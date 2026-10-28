//! Local developer tasks. `cargo xtask ci` runs the same checks a CI job
//! would run, sequentially and failing fast.

mod docs_check;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("ci") if args.next().is_none() => ci(),
        _ => {
            eprintln!("usage: cargo xtask ci");
            ExitCode::from(2)
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
    let steps: [(&str, &[&str]); 3] = [
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
