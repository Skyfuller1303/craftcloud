//! Minimal process supervisor for the `all-in-one` role.
//!
//! The supervisor starts a fixed list of child processes, restarts a child that
//! exits on its own with an exponential backoff, and on the stop request asks
//! every child to terminate (SIGTERM on unix), waiting up to a grace period
//! before it kills what is left. A child that exits with the configuration
//! error code ends the whole group: restarting cannot fix a wrong
//! configuration, and the container runtime shows the failure.

use std::path::PathBuf;
use std::process::Stdio;
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
}

impl Policy {
    /// Production values: 1 s to 30 s backoff, 60 s counts as stable and
    /// 25 s of grace (below the 30 s stop timeout the documentation uses).
    #[must_use]
    pub const fn standard() -> Self {
        Self {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            stable_after: Duration::from_secs(60),
            grace: Duration::from_secs(25),
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
    let (stop_tx, stop_rx) = watch::channel(false);
    let (fatal_tx, mut fatal_rx) = tokio::sync::mpsc::unbounded_channel::<u8>();
    let mut tasks = Vec::new();
    for spec in specs {
        tasks.push(tokio::spawn(supervise(
            spec,
            policy,
            stop_rx.clone(),
            fatal_tx.clone(),
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
    outcome
}

async fn supervise(
    spec: ChildSpec,
    policy: Policy,
    mut stop: watch::Receiver<bool>,
    fatal: tokio::sync::mpsc::UnboundedSender<u8>,
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
        tokio::select! {
            status = child.wait() => {
                let code = status.ok().and_then(|status| status.code());
                tracing::warn!(child = spec.name, code = ?code, "exited");
                if code == Some(i32::from(EXIT_CONFIG)) {
                    let _ = fatal.send(EXIT_CONFIG);
                    return;
                }
            }
            () = stop_requested(&mut stop) => {
                terminate(&spec, &mut child, policy.grace).await;
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

    const ROLE_ENV: &str = "CC_SUPERVISOR_TEST_ROLE";
    const FILE_ENV: &str = "CC_SUPERVISOR_TEST_FILE";

    fn quick() -> Policy {
        Policy {
            initial_backoff: Duration::from_millis(20),
            max_backoff: Duration::from_millis(80),
            stable_after: Duration::from_secs(60),
            grace: Duration::from_secs(20),
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
        let spec = ChildSpec {
            name: "missing",
            program: PathBuf::from("/definitely/not/a/program"),
            args: Vec::new(),
            envs: Vec::new(),
        };
        let stop = tokio::time::sleep(Duration::from_millis(300));
        assert_eq!(run(vec![spec], quick(), stop).await, Outcome::Stopped);
    }
}
