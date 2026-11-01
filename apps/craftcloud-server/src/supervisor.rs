//! Minimal process supervisor for the `all-in-one` role.
//!
//! The supervisor starts a fixed list of child processes, restarts a child that
//! exits on its own with an exponential backoff, and on the stop request asks
//! every child to terminate (SIGTERM on unix), waiting up to a grace period
//! before it kills what is left. A child that exits with the configuration
//! error code ends the whole group: restarting cannot fix a wrong
//! configuration, and the container runtime shows the failure.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::process::{Child, Command};
use tokio::sync::watch;

/// Exit code of `craftcloud-server` for an invalid configuration. A child that
/// exits with it is never restarted.
pub const EXIT_CONFIG: u8 = 2;

/// One child process of the group.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    /// Name used in log lines.
    pub name: &'static str,
    /// Executable to start.
    pub program: PathBuf,
    /// Arguments of the executable.
    pub args: Vec<String>,
    /// Variables added to (or replacing) the inherited environment.
    pub envs: Vec<(String, String)>,
}

/// Timing of restarts and of the shutdown.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Wait before the first restart.
    pub initial_backoff: Duration,
    /// Upper bound of the wait between restarts.
    pub max_backoff: Duration,
    /// A child that ran at least this long starts over with `initial_backoff`.
    pub stable_after: Duration,
    /// Time children get after the stop request before they are killed.
    pub grace: Duration,
    /// Reap processes that were re-parented to this process (orphaned grandchildren).
    /// Needed when the supervisor is PID 1 of a container, where nobody else would.
    pub reap_orphans: bool,
}

impl Policy {
    /// Production values: 1 s to 30 s backoff, 60 s counts as stable and
    /// 25 s of grace (below the 30 s stop timeout the documentation uses).
    #[must_use]
    pub fn standard() -> Self {
        Self {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            stable_after: Duration::from_secs(60),
            grace: Duration::from_secs(25),
            reap_orphans: cfg!(target_os = "linux") && std::process::id() == 1,
        }
    }
}

/// How the group ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The stop request arrived and every child was stopped.
    Stopped,
    /// A child reported a configuration error; the exit code to return.
    Fatal(u8),
}

/// The wait before the restart that follows a run of `ran` with the previous
/// wait `current`: it doubles up to the maximum and starts over after a stable run.
#[must_use]
pub fn next_backoff(current: Duration, ran: Duration, policy: &Policy) -> Duration {
    if ran >= policy.stable_after {
        policy.initial_backoff
    } else {
        current.saturating_mul(2).min(policy.max_backoff)
    }
}

/// Runs the children until `stop` resolves or a child reports a configuration error.
pub async fn run<F>(specs: Vec<ChildSpec>, policy: Policy, stop: F) -> Outcome
where
    F: std::future::Future<Output = ()>,
{
    let known: Known = Arc::new(Mutex::new(HashSet::new()));
    let reaper = if policy.reap_orphans {
        Some(tokio::spawn(reap_loop(Arc::clone(&known))))
    } else {
        None
    };
    let (stop_tx, stop_rx) = watch::channel(false);
    let (fatal_tx, mut fatal_rx) = tokio::sync::mpsc::unbounded_channel::<u8>();
    let mut tasks = Vec::new();
    for spec in specs {
        tasks.push(tokio::spawn(supervise(
            spec,
            policy,
            stop_rx.clone(),
            fatal_tx.clone(),
            Arc::clone(&known),
        )));
    }
    drop(fatal_tx);
    let outcome = tokio::select! {
        () = stop => Outcome::Stopped,
        code = fatal_rx.recv() => code.map_or(Outcome::Stopped, Outcome::Fatal),
    };
    let _ = stop_tx.send(true);
    for task in tasks {
        let _ = task.await;
    }
    if let Some(reaper) = reaper {
        // One last pass for orphans that appeared while the children stopped.
        reap_orphans(&known);
        reaper.abort();
    }
    outcome
}

async fn supervise(
    spec: ChildSpec,
    policy: Policy,
    mut stop: watch::Receiver<bool>,
    fatal: tokio::sync::mpsc::UnboundedSender<u8>,
    known: Known,
) {
    let mut backoff = policy.initial_backoff;
    loop {
        if *stop.borrow() {
            return;
        }
        let started = Instant::now();
        let mut child = match start(&spec) {
            Ok(child) => child,
            Err(err) => {
                tracing::error!(child = spec.name, "cannot start: {err}");
                if wait_or_stop(&mut stop, backoff).await {
                    return;
                }
                backoff = next_backoff(backoff, started.elapsed(), &policy);
                continue;
            }
        };
        tracing::info!(child = spec.name, "started");
        let pid = child.id();
        remember(&known, pid, true);
        tokio::select! {
            status = child.wait() => {
                remember(&known, pid, false);
                let code = status.ok().and_then(|status| status.code());
                tracing::warn!(child = spec.name, code = ?code, "exited");
                if code == Some(i32::from(EXIT_CONFIG)) {
                    let _ = fatal.send(EXIT_CONFIG);
                    return;
                }
            }
            () = stop_requested(&mut stop) => {
                terminate(&spec, &mut child, policy.grace).await;
                remember(&known, pid, false);
                return;
            }
        }
        backoff = next_backoff(backoff, started.elapsed(), &policy);
        tracing::info!(child = spec.name, delay = ?backoff, "restarting");
        if wait_or_stop(&mut stop, backoff).await {
            return;
        }
    }
}

/// Process ids of the children this supervisor started and has not yet waited for.
type Known = Arc<Mutex<HashSet<u32>>>;

fn remember(known: &Known, pid: Option<u32>, add: bool) {
    if let (Some(pid), Ok(mut set)) = (pid, known.lock()) {
        if add {
            set.insert(pid);
        } else {
            set.remove(&pid);
        }
    }
}

/// Waits for SIGCHLD (and a one second tick) and reaps orphans each time.
#[cfg(unix)]
async fn reap_loop(known: Known) {
    use tokio::signal::unix::{SignalKind, signal};
    let mut child_signal = signal(SignalKind::child()).ok();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        match child_signal.as_mut() {
            Some(stream) => {
                tokio::select! {
                    _ = stream.recv() => {}
                    _ = tick.tick() => {}
                }
            }
            None => {
                tick.tick().await;
            }
        }
        reap_orphans(&known);
    }
}

#[cfg(not(unix))]
async fn reap_loop(_known: Known) {
    std::future::pending::<()>().await;
}

/// Reaps zombie children of this process that the supervisor did not start (for
/// example grandchildren that were re-parented to PID 1). A child the supervisor
/// started is never touched here, so its exit status still reaches `wait`.
/// Returns how many zombies were reaped. Linux only (reads `/proc`).
pub fn reap_orphans(known: &Known) -> usize {
    #[cfg(target_os = "linux")]
    {
        use rustix::process::{Pid, WaitOptions, waitpid};
        let skip: HashSet<u32> = known.lock().map(|set| set.clone()).unwrap_or_default();
        let mut reaped = 0;
        for pid in zombie_children(std::process::id()) {
            if skip.contains(&pid) {
                continue;
            }
            let Some(target) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
                continue;
            };
            if matches!(waitpid(Some(target), WaitOptions::NOHANG), Ok(Some(_))) {
                reaped += 1;
            }
        }
        reaped
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = known;
        0
    }
}

/// Process ids of the zombies whose parent is `parent`. Linux only.
#[cfg(target_os = "linux")]
pub fn zombie_children(parent: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // "pid (comm) S ppid ...": the name may contain spaces and parentheses.
        let Some(rest) = stat.rfind(')').and_then(|end| stat.get(end + 1..)) else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let state = fields.next();
        let ppid = fields.next().and_then(|field| field.parse::<u32>().ok());
        if state == Some("Z") && ppid == Some(parent) {
            found.push(pid);
        }
    }
    found
}

fn start(spec: &ChildSpec) -> std::io::Result<Child> {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .envs(spec.envs.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command.spawn()
}

/// Sleeps for `delay`; returns true if the stop request came first.
async fn wait_or_stop(stop: &mut watch::Receiver<bool>, delay: Duration) -> bool {
    tokio::select! {
        () = tokio::time::sleep(delay) => false,
        () = stop_requested(stop) => true,
    }
}

/// Resolves once the stop request was given (or the sender is gone). The
/// borrowed value is dropped inside, so the future stays `Send`.
async fn stop_requested(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopped| *stopped).await;
}

/// Asks the child to terminate and kills it when the grace period runs out.
async fn terminate(spec: &ChildSpec, child: &mut Child, grace: Duration) {
    request_termination(child);
    match tokio::time::timeout(grace, child.wait()).await {
        Ok(_) => tracing::info!(child = spec.name, "stopped"),
        Err(_) => {
            tracing::warn!(child = spec.name, "did not stop in time; killing it");
            let _ = child.kill().await;
        }
    }
}

#[cfg(unix)]
fn request_termination(child: &mut Child) {
    use rustix::process::{Pid, Signal, kill_process};
    let pid = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw);
    // No pid means the child has already been reaped.
    if let Some(pid) = pid
        && let Err(err) = kill_process(pid, Signal::TERM)
    {
        tracing::warn!("cannot send SIGTERM: {err}");
    }
}

#[cfg(not(unix))]
fn request_termination(child: &mut Child) {
    // Windows has no SIGTERM; the role is meant for Linux containers.
    let _ = child.start_kill();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests start supervisors in one process. A supervisor that reaps orphans
    /// would also collect the zombies of another test's children, so the tests
    /// that start a supervisor run one at a time.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    const ROLE_ENV: &str = "CC_SUPERVISOR_TEST_ROLE";
    const FILE_ENV: &str = "CC_SUPERVISOR_TEST_FILE";

    fn quick() -> Policy {
        Policy {
            initial_backoff: Duration::from_millis(20),
            max_backoff: Duration::from_millis(80),
            stable_after: Duration::from_secs(60),
            grace: Duration::from_secs(20),
            reap_orphans: cfg!(target_os = "linux"),
        }
    }

    /// Entry point of the helper child: the test binary starts itself with
    /// `--exact supervisor::tests::helper_child` and a role in the environment.
    /// Without the variable the test does nothing.
    #[test]
    fn helper_child() {
        let Ok(role) = std::env::var(ROLE_ENV) else {
            return;
        };
        match role.as_str() {
            "wait" => loop {
                std::thread::sleep(Duration::from_millis(50));
            },
            // Starts a grandchild that exits at once, records its pid and exits itself,
            // so that the grandchild is orphaned and re-parented.
            "orphan" => {
                let program = std::env::current_exe().unwrap_or_default();
                let grandchild = std::process::Command::new(program)
                    .args([
                        "--exact",
                        "supervisor::tests::helper_child",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env(ROLE_ENV, "quick")
                    .spawn();
                if let (Ok(child), Ok(path)) = (grandchild, std::env::var(FILE_ENV)) {
                    use std::io::Write;
                    if let Ok(mut file) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                    {
                        let _ = writeln!(file, "{}", child.id());
                    }
                }
                std::thread::sleep(Duration::from_millis(300));
                std::process::exit(0);
            }
            "quick" => std::process::exit(0),
            "crash" | "fatal" => {
                if let Ok(path) = std::env::var(FILE_ENV) {
                    use std::io::Write;
                    if let Ok(mut file) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                    {
                        let _ = writeln!(file, "start");
                    }
                }
                std::process::exit(if role == "fatal" { 2 } else { 1 });
            }
            _ => {}
        }
    }

    fn helper(role: &str, file: Option<&std::path::Path>) -> ChildSpec {
        let mut envs = vec![(ROLE_ENV.to_owned(), role.to_owned())];
        if let Some(file) = file {
            envs.push((FILE_ENV.to_owned(), file.to_string_lossy().into_owned()));
        }
        ChildSpec {
            name: "helper",
            program: std::env::current_exe().unwrap_or_default(),
            args: [
                "--exact",
                "supervisor::tests::helper_child",
                "--nocapture",
                "--test-threads=1",
            ]
            .iter()
            .map(|arg| (*arg).to_owned())
            .collect(),
            envs,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "craftcloud-supervisor-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn count_starts(file: &std::path::Path) -> usize {
        std::fs::read_to_string(file)
            .unwrap_or_default()
            .lines()
            .count()
    }

    #[test]
    fn backoff_doubles_up_to_the_maximum_and_resets_after_a_stable_run() {
        let policy = quick();
        let mut wait = policy.initial_backoff;
        let mut seen = Vec::new();
        for _ in 0..5 {
            wait = next_backoff(wait, Duration::from_millis(1), &policy);
            seen.push(wait.as_millis());
        }
        assert_eq!(seen, [40, 80, 80, 80, 80]);
        let reset = next_backoff(wait, Duration::from_secs(61), &policy);
        assert_eq!(reset, policy.initial_backoff);
    }

    #[tokio::test]
    async fn a_crashing_child_is_restarted_with_backoff() {
        let _serial = SERIAL.lock().await;
        let dir = scratch("restart");
        let file = dir.join("starts.txt");
        let stop = tokio::time::sleep(Duration::from_millis(2500));
        let outcome = run(vec![helper("crash", Some(&file))], quick(), stop).await;
        assert_eq!(outcome, Outcome::Stopped);
        // Starting the test binary takes a moment; the point is that the child
        // ran more than once and that the backoff bounded the rate.
        let starts = count_starts(&file);
        assert!(starts >= 2, "child was started {starts} time(s)");
        assert!(starts <= 60, "child was started {starts} times");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_stop_request_ends_a_running_child_before_the_grace_period() {
        let _serial = SERIAL.lock().await;
        let started = Instant::now();
        let stop = tokio::time::sleep(Duration::from_millis(800));
        let outcome = run(vec![helper("wait", None)], quick(), stop).await;
        assert_eq!(outcome, Outcome::Stopped);
        // The grace period is 20 s: returning sooner proves the child ended
        // on the termination request and was not waited out or killed late.
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "stop took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_configuration_error_ends_the_group_without_restart() {
        let _serial = SERIAL.lock().await;
        let dir = scratch("fatal");
        let file = dir.join("starts.txt");
        let outcome = run(
            vec![helper("wait", None), helper("fatal", Some(&file))],
            quick(),
            std::future::pending::<()>(),
        )
        .await;
        assert_eq!(outcome, Outcome::Fatal(EXIT_CONFIG));
        assert_eq!(count_starts(&file), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_missing_program_is_retried_until_stopped() {
        let _serial = SERIAL.lock().await;
        let spec = ChildSpec {
            name: "missing",
            program: PathBuf::from("/definitely/not/a/program"),
            args: Vec::new(),
            envs: Vec::new(),
        };
        let stop = tokio::time::sleep(Duration::from_millis(300));
        assert_eq!(run(vec![spec], quick(), stop).await, Outcome::Stopped);
    }

    /// Linux only: the supervisor reaps orphaned grandchildren but never a child it
    /// started (the restart and fatal tests above depend on seeing their statuses).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn orphaned_grandchildren_are_reaped_only_when_asked() {
        let _serial = SERIAL.lock().await;
        // Become the subreaper so that orphans come to this test process, as they
        // would come to a supervisor that runs as PID 1.
        assert!(rustix::process::set_child_subreaper(Some(rustix::process::getpid())).is_ok());
        let read_pids = |file: &std::path::Path| -> Vec<u32> {
            std::fs::read_to_string(file)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| line.trim().parse().ok())
                .collect()
        };
        let exists = |pid: u32| std::path::Path::new(&format!("/proc/{pid}")).exists();

        let dir = scratch("orphans-off");
        let file = dir.join("pids.txt");
        let mut off = quick();
        off.reap_orphans = false;
        let stop = tokio::time::sleep(Duration::from_millis(1200));
        assert_eq!(
            run(vec![helper("orphan", Some(&file))], off, stop).await,
            Outcome::Stopped
        );
        let left = read_pids(&file);
        assert!(!left.is_empty(), "the helper started no grandchild");
        assert!(
            left.iter().any(|pid| exists(*pid)),
            "without reaping the orphans stay as zombies (test would prove nothing)"
        );

        let dir_on = scratch("orphans-on");
        let file_on = dir_on.join("pids.txt");
        let stop = tokio::time::sleep(Duration::from_millis(1500));
        assert_eq!(
            run(vec![helper("orphan", Some(&file_on))], quick(), stop).await,
            Outcome::Stopped
        );
        let reaped = read_pids(&file_on);
        assert!(!reaped.is_empty());
        // The reaper also collects the zombies of the first run, which it did not start either.
        for pid in reaped.iter().chain(left.iter()) {
            assert!(!exists(*pid), "orphan {pid} was not reaped");
        }
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir_on);
    }
}
