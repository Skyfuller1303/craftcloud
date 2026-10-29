//! `cargo xtask web`: checks and builds the browser admin UI
//! (`apps/craftcloud-admin`) with a local Node.js installation.
//!
//! Steps (stop at the first failure): `npm ci --ignore-scripts`, type check,
//! production build, unit and component tests, translation catalogue check.
//!
//! Node.js is searched in this order: the directory in `CRAFTCLOUD_NODE_DIR`, then `PATH`.
//! npm is started as `node npm-cli.js`, never through a shell. A missing Node.js is an
//! error with instructions: `cargo xtask ci` only continues without this step
//! when `--skip-web` was passed explicitly.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Environment variable naming the Node.js installation directory.
pub const NODE_DIR_ENV: &str = "CRAFTCLOUD_NODE_DIR";

/// A usable Node.js installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub node: PathBuf,
    pub npm_cli: PathBuf,
}

/// Recognizes the Windows zip layout (`node.exe` next to `node_modules/npm`)
/// and the unix tarball layout (`bin/node`, `lib/node_modules/npm`).
pub fn node_in(dir: &Path) -> Option<Node> {
    let candidates = [
        (
            dir.join("node.exe"),
            dir.join("node_modules/npm/bin/npm-cli.js"),
        ),
        (
            dir.join("bin/node"),
            dir.join("lib/node_modules/npm/bin/npm-cli.js"),
        ),
        (
            dir.join("node"),
            dir.join("node_modules/npm/bin/npm-cli.js"),
        ),
    ];
    candidates
        .into_iter()
        .find(|(node, npm)| node.is_file() && npm.is_file())
        .map(|(node, npm_cli)| Node { node, npm_cli })
}

/// Finds Node.js; `Err` carries the instructions shown to the developer.
pub fn find_node(env_dir: Option<OsString>, path_var: Option<OsString>) -> Result<Node, String> {
    if let Some(dir) = env_dir.filter(|d| !d.is_empty()) {
        let dir = PathBuf::from(dir);
        return node_in(&dir).ok_or_else(|| {
            format!(
                "{NODE_DIR_ENV}={} does not contain a Node.js installation \
                 (expected node.exe or bin/node with npm next to it)",
                dir.display()
            )
        });
    }
    if let Some(path_var) = path_var {
        for dir in std::env::split_paths(&path_var) {
            if let Some(found) = node_in(&dir) {
                return Ok(found);
            }
            // Layout of a prefix installation: <prefix>/bin/node.
            if let Some(found) = dir.parent().and_then(node_in) {
                return Ok(found);
            }
        }
    }
    Err(format!(
        "Node.js was not found. Install the current LTS release (zip/tarball, no installer needed), \
         set {NODE_DIR_ENV} to its directory, then run `cargo xtask web`. \
         Pass `--skip-web` to `cargo xtask ci` only if you deliberately do not check the admin UI."
    ))
}

fn app_dir(root: &Path) -> PathBuf {
    root.join("apps").join("craftcloud-admin")
}

fn run_step(node: &Node, app: &Path, name: &str, npm_args: &[&str]) -> Result<(), String> {
    println!("==> web: {name}");
    let mut path = OsString::new();
    if let Some(dir) = node.node.parent() {
        path.push(dir);
        path.push(if cfg!(windows) { ";" } else { ":" });
    }
    path.push(std::env::var_os("PATH").unwrap_or_default());
    let status = Command::new(&node.node)
        .arg(&node.npm_cli)
        .args(npm_args)
        .current_dir(app)
        .env("PATH", path)
        .env("npm_config_update_notifier", "false")
        .env("npm_config_fund", "false")
        .env("CI", "1")
        .status();
    match status {
        Ok(status) if status.success() => {
            println!("<== web: {name}: PASS");
            Ok(())
        }
        Ok(status) => Err(format!("{name} failed ({status})")),
        Err(err) => Err(format!("{name}: cannot start node: {err}")),
    }
}

/// Runs all web steps. `Err` is a message for the developer.
pub fn check(root: &Path) -> Result<(), String> {
    let node = find_node(std::env::var_os(NODE_DIR_ENV), std::env::var_os("PATH"))?;
    let app = app_dir(root);
    if !app.join("package.json").is_file() {
        return Err(format!("{} has no package.json", app.display()));
    }
    println!("web: using node {}", node.node.display());
    let steps: [(&str, &[&str]); 5] = [
        (
            "npm ci",
            &["ci", "--ignore-scripts", "--no-audit", "--no-fund"],
        ),
        ("typecheck", &["run", "typecheck"]),
        ("build", &["run", "build"]),
        ("tests", &["test"]),
        ("i18n check", &["run", "i18n:check"]),
    ];
    for (name, args) in steps {
        run_step(&node, &app, name, args)?;
    }
    Ok(())
}

/// Entry point of `cargo xtask web`.
pub fn run(root: &Path) -> ExitCode {
    match check(root) {
        Ok(()) => {
            println!("web: all steps passed");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("web: {message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "craftcloud-xtask-test-{}-{tag}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }

    #[test]
    fn recognizes_windows_and_unix_layouts() {
        let windows = TempDir::new("win");
        touch(&windows.0.join("node.exe"));
        assert_eq!(node_in(&windows.0), None, "npm is missing");
        touch(&windows.0.join("node_modules/npm/bin/npm-cli.js"));
        assert_eq!(
            node_in(&windows.0).unwrap().node,
            windows.0.join("node.exe")
        );

        let unix = TempDir::new("unix");
        touch(&unix.0.join("bin/node"));
        touch(&unix.0.join("lib/node_modules/npm/bin/npm-cli.js"));
        assert_eq!(node_in(&unix.0).unwrap().node, unix.0.join("bin/node"));
    }

    #[test]
    fn explicit_directory_wins_and_errors_name_the_variable() {
        let empty = TempDir::new("empty");
        let error = find_node(Some(empty.0.clone().into_os_string()), None).unwrap_err();
        assert!(error.contains(NODE_DIR_ENV), "{error}");

        let good = TempDir::new("good");
        touch(&good.0.join("node.exe"));
        touch(&good.0.join("node_modules/npm/bin/npm-cli.js"));
        assert!(find_node(Some(good.0.clone().into_os_string()), None).is_ok());
    }

    #[test]
    fn missing_node_explains_what_to_do() {
        let nothing = TempDir::new("nothing");
        let path = std::env::join_paths([nothing.0.clone()]).unwrap();
        let error = find_node(None, Some(path)).unwrap_err();
        assert!(error.contains("Node.js was not found"), "{error}");
        assert!(error.contains("--skip-web"), "{error}");
        assert!(error.contains(NODE_DIR_ENV), "{error}");
    }

    #[test]
    fn finds_node_on_path() {
        let dir = TempDir::new("pathdir");
        touch(&dir.0.join("bin/node"));
        touch(&dir.0.join("lib/node_modules/npm/bin/npm-cli.js"));
        let path = std::env::join_paths([dir.0.join("bin")]).unwrap();
        assert_eq!(
            find_node(None, Some(path)).unwrap().node,
            dir.0.join("bin/node")
        );
    }
}
