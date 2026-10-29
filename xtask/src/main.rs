//! Local developer tasks. `cargo xtask ci` runs the same checks a CI job
//! would run, sequentially and failing fast.

mod db_test;
mod docs_check;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("ci") if args.next().is_none() => ci(),
        Some("db-test") if args.next().is_none() => db_test::run(&repo_root()),
        Some("openapi-snapshot") => match (args.next().as_deref(), args.next()) {
            (None, None) => openapi_snapshot(false),
            (Some("--update"), None) => openapi_snapshot(true),
            _ => usage(),
        },
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: cargo xtask <ci|db-test|openapi-snapshot [--update]>");
    ExitCode::from(2)
}

/// Checks `contracts/openapi.snapshot` against `contracts/openapi.yaml`, or
/// rewrites it with `--update`. The normalization lives in the contract test
/// (`crates/craftcloud-api/tests/contract.rs`), so there is one implementation.
fn openapi_snapshot(update: bool) -> ExitCode {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .args([
            "test",
            "-p",
            "craftcloud-api",
            "--test",
            "contract",
            "openapi_matches_the_committed_snapshot",
        ])
        .current_dir(repo_root());
    if update {
        command.env("UPDATE_OPENAPI_SNAPSHOT", "1");
    } else {
        command.env_remove("UPDATE_OPENAPI_SNAPSHOT");
    }
    match command.status() {
        Ok(status) if status.success() => {
            println!(
                "{}",
                if update {
                    "contracts/openapi.snapshot updated; review and commit the diff"
                } else {
                    "contracts/openapi.snapshot matches contracts/openapi.yaml"
                }
            );
            ExitCode::SUCCESS
        }
        Ok(status) => exit_code_from(status.code()),
        Err(err) => {
            eprintln!("cannot start cargo: {err}");
            ExitCode::FAILURE
        }
    }
}

/// The repository root: the parent of the `xtask` crate directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn ci() -> ExitCode {
    let root = repo_root();
    let steps: [(&str, &[&str]); 4] = [
        ("cargo fmt", &["fmt", "--all", "--", "--check"]),
        (
            "cargo clippy",
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        ),
        // The crash tests (CC-P02-08) are only built with the feature; lint them too.
        (
            "cargo clippy (fault-injection)",
            &[
                "clippy",
                "-p",
                "craftcloud-server",
                "--all-targets",
                "--features",
                "fault-injection",
                "--",
                "-D",
                "warnings",
            ],
        ),
        ("cargo test", &["test", "--workspace"]),
    ];
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    for (name, args) in steps {
        println!("==> {name}");
        let status = Command::new(&cargo).args(args).current_dir(&root).status();
        match status {
            Ok(status) if status.success() => println!("<== {name}: PASS"),
            Ok(status) => {
                println!("<== {name}: FAIL ({status})");
                return exit_code_from(status.code());
            }
            Err(err) => {
                println!("<== {name}: FAIL (cannot start cargo: {err})");
                return ExitCode::FAILURE;
            }
        }
    }

    println!("==> docs links");
    match docs_check::check_repository(&root) {
        Ok(checked) => println!("<== docs links: PASS ({checked} relative links resolved)"),
        Err(problems) => {
            for problem in &problems {
                println!("  {problem}");
            }
            println!("<== docs links: FAIL ({} broken)", problems.len());
            return ExitCode::FAILURE;
        }
    }

    println!("ci: all steps passed");
    ExitCode::SUCCESS
}

fn exit_code_from(code: Option<i32>) -> ExitCode {
    match code.and_then(|c| u8::try_from(c).ok()) {
        Some(c) if c != 0 => ExitCode::from(c),
        _ => ExitCode::FAILURE,
    }
}
