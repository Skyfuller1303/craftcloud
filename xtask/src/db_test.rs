//! `cargo xtask db-test`: runs the ignored PostgreSQL integration tests
//! against a throwaway Docker container.
//!
//! The container is bound to 127.0.0.1 on a free port, limited to 1 GiB of
//! memory and 2 CPUs (the rest of the 16 GiB agent budget is reserved for the
//! Windows build), uses a random per-run password that only lives in this
//! process and the environment of the child processes, and is always removed.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Pinned PostgreSQL image (exact minor version). The digest is printed at
/// start so test logs record what actually ran.
const IMAGE: &str = "postgres:18.6";
const READY_TIMEOUT: Duration = Duration::from_secs(90);

/// Removes the container when dropped, also on failure paths.
struct Container {
    name: String,
}

impl Drop for Container {
    fn drop(&mut self) {
        let removed = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        println!(
            "==> container {} {}",
            self.name,
            if removed {
                "removed"
            } else {
                "REMOVAL FAILED (check `docker ps -a`)"
            }
        );
    }
}

pub fn run(root: &Path) -> ExitCode {
    match run_inner(root) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("db-test: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run_inner(root: &Path) -> Result<ExitCode, String> {
    println!("==> checking docker");
    let version = docker_output(&["version", "--format", "{{.Server.Version}}"])
        .map_err(|e| format!("docker is not available ({e}); start Docker Desktop first"))?;
    println!("docker server {}", version.trim());

    if docker_output(&["image", "inspect", IMAGE, "--format", "{{.Id}}"]).is_err() {
        println!("==> pulling {IMAGE}");
        let status = Command::new("docker")
            .args(["pull", IMAGE])
            .status()
            .map_err(|e| format!("cannot run docker pull: {e}"))?;
        if !status.success() {
            return Err(format!("docker pull {IMAGE} failed"));
        }
    }
    let digest = docker_output(&[
        "image",
        "inspect",
        IMAGE,
        "--format",
        "{{index .RepoDigests 0}}",
    ])
    .unwrap_or_else(|_| "unknown".to_owned());
    println!("image {IMAGE} digest {}", digest.trim());

    let port = free_port()?;
    let password = random_hex(32);
    let name = format!("craftcloud-dbtest-{}", std::process::id());

    println!("==> starting {name} (memory 1g, cpus 2, 127.0.0.1:{port})");
    let status = Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "--name",
            &name,
            "--memory",
            "1g",
            "--cpus",
            "2",
            "-e",
            "POSTGRES_PASSWORD",
            "-p",
            &format!("127.0.0.1:{port}:5432"),
            IMAGE,
            "postgres",
            "-c",
            "fsync=off",
            "-c",
            "max_connections=200",
        ])
        .env("POSTGRES_PASSWORD", &password)
        .stdout(Stdio::null())
        .status()
        .map_err(|e| format!("cannot run docker run: {e}"))?;
    if !status.success() {
        return Err("docker run failed".to_owned());
    }
    let _guard = Container { name: name.clone() };

    println!("==> waiting for PostgreSQL");
    wait_ready(&name)?;

    let url = format!("postgres://postgres:{password}@127.0.0.1:{port}/postgres");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    println!(
        "==> cargo test -p craftcloud-db -p craftcloud-api -p craftcloud-server --features craftcloud-server/fault-injection -- --ignored"
    );
    let status = Command::new(&cargo)
        .args([
            "test",
            "-p",
            "craftcloud-db",
            "-p",
            "craftcloud-server",
            "-p",
            "craftcloud-api",
            // The crash tests need a server binary that can abort itself.
            "--features",
            "craftcloud-server/fault-injection",
            "--",
            "--ignored",
        ])
        .env("CC_TEST_DATABASE_URL", &url)
        .current_dir(root)
        .status()
        .map_err(|e| format!("cannot start cargo: {e}"))?;
    if status.success() {
        println!("<== db-test: PASS");
        Ok(ExitCode::SUCCESS)
    } else {
        println!("<== db-test: FAIL ({status})");
        let code = status
            .code()
            .and_then(|c| u8::try_from(c).ok())
            .filter(|c| *c != 0);
        Ok(code.map_or(ExitCode::FAILURE, ExitCode::from))
    }
}

fn docker_output(args: &[&str]) -> Result<String, String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!("exit status {}", output.status))
    }
}

fn free_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no free port: {e}"))?;
    listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| format!("no free port: {e}"))
}

fn wait_ready(name: &str) -> Result<(), String> {
    let start = Instant::now();
    while start.elapsed() < READY_TIMEOUT {
        // TCP check: the entrypoint's temporary server only listens on a
        // unix socket, so success over 127.0.0.1 means the final server is up.
        let ready = Command::new("docker")
            .args([
                "exec",
                name,
                "pg_isready",
                "-h",
                "127.0.0.1",
                "-U",
                "postgres",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ready {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "PostgreSQL not ready after {} s",
        READY_TIMEOUT.as_secs()
    ))
}

/// Random hex string from the OS-seeded `RandomState` plus the clock. Good
/// enough for a throwaway password on a loopback-only test container.
fn random_hex(len: usize) -> String {
    let mut out = String::new();
    let mut counter: u64 = 0;
    while out.len() < len {
        let mut hasher = RandomState::new().build_hasher();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        hasher.write_u128(nanos);
        hasher.write_u64(counter);
        hasher.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", hasher.finish()));
        counter = counter.wrapping_add(1);
    }
    out.truncate(len);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_hex_is_hex_of_requested_length_and_varies() {
        let a = random_hex(32);
        let b = random_hex(32);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn free_port_is_nonzero() {
        assert!(free_port().unwrap() > 0);
    }
}
