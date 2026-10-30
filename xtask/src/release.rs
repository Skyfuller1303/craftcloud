//! Build source release archives and dependency inventories.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

pub(super) fn run(root: &Path, version: &str) -> ExitCode {
    match create_release(root, version) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("release: {message}");
            ExitCode::FAILURE
        }
    }
}

fn create_release(root: &Path, version: &str) -> Result<(), String> {
    validate_version(version)?;
    let dirty = git_output(root, &["status", "--porcelain", "--untracked-files=all"])?;
    ensure_clean_tree(&dirty)?;
    let commit = git_output(root, &["rev-parse", "HEAD"])?;
    let commit = commit.trim();
    if commit.len() < 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("git returned an invalid commit hash".to_owned());
    }

    let output = root.join("dist");
    fs::create_dir_all(&output).map_err(|err| format!("cannot create dist: {err}"))?;
    let _lock = ReleaseLock::acquire(&output)?;
    let name = format!("craftcloud-{version}");
    let archive = output.join(format!("{name}.tar.gz"));
    let sbom = output.join(format!("{name}.cdx.json"));
    if archive.exists() || sbom.exists() {
        return Err(format!("release {version} already exists in dist"));
    }
    let staging = StagingDir::create(&output)?;
    let staging_path = &staging.path;
    let raw_tar = staging_path.join("source.tar");

    let mut archive_command = Command::new("git");
    archive_command
        .args([
            "archive",
            "--format=tar",
            &format!("--prefix={name}/"),
            commit,
        ])
        .current_dir(root)
        .stdout(Stdio::from(
            File::create(&raw_tar).map_err(|err| format!("cannot create archive: {err}"))?,
        ));
    run_status(archive_command, "git archive")?;

    let helper = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/release_archive.py");
    let status = Command::new("python")
        .args([
            "-I",
            helper.to_string_lossy().as_ref(),
            raw_tar.to_string_lossy().as_ref(),
            staging_path.to_string_lossy().as_ref(),
            output.to_string_lossy().as_ref(),
            version,
            commit,
            format!("{name}.tar.gz").as_str(),
        ])
        .current_dir(root)
        .status()
        .map_err(|err| format!("cannot start release archive generator: {err}"))?;
    if !status.success() {
        return Err(format!("release archive generator failed: {status}"));
    }
    println!("created {}", archive.display());
    Ok(())
}

struct ReleaseLock {
    path: PathBuf,
}

impl ReleaseLock {
    fn acquire(output: &Path) -> Result<Self, String> {
        let path = output.join(".release.lock");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::AlreadyExists {
                    "release lock exists; another release may be running, or remove stale dist/.release.lock after verifying no release process is active".to_owned()
                } else {
                    format!("cannot acquire release lock: {err}")
                }
            })?;
        use std::io::Write;
        if let Err(err) = writeln!(file, "{}", std::process::id()) {
            let _ = fs::remove_file(&path);
            return Err(format!("cannot write release lock: {err}"));
        }
        Ok(Self { path })
    }
}

impl Drop for ReleaseLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct StagingDir {
    path: PathBuf,
}

impl StagingDir {
    fn create(output: &Path) -> Result<Self, String> {
        for attempt in 0..100 {
            let path = output.join(format!(".release-stage-{}-{attempt}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(format!("cannot create release staging directory: {err}")),
            }
        }
        Err("cannot allocate a unique release staging directory".to_owned())
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn validate_version(version: &str) -> Result<(), String> {
    let mut core_and_pre = version
        .split_once('+')
        .map_or(version, |(core, _)| core)
        .splitn(2, '-');
    let core = core_and_pre.next().unwrap_or_default();
    let pre = core_and_pre.next();
    let fields: Vec<_> = core.split('.').collect();
    if fields.len() != 3 || fields.iter().any(|part| !numeric_identifier(part)) {
        return Err(format!("invalid SemVer version {version:?}"));
    }
    if let Some(pre) = pre
        && (pre.is_empty()
            || !pre.split('.').all(|part| {
                valid_identifier(part)
                    && (!part.bytes().all(|byte| byte.is_ascii_digit())
                        || part == "0"
                        || !part.starts_with('0'))
            }))
    {
        return Err(format!("invalid SemVer version {version:?}"));
    }
    if let Some((_, build)) = version.split_once('+')
        && (build.is_empty() || !build.split('.').all(valid_identifier))
    {
        return Err(format!("invalid SemVer version {version:?}"));
    }
    Ok(())
}

fn numeric_identifier(part: &str) -> bool {
    !part.is_empty()
        && part.bytes().all(|byte| byte.is_ascii_digit())
        && (part == "0" || !part.starts_with('0'))
}

fn valid_identifier(part: &str) -> bool {
    !part.is_empty()
        && part
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn ensure_clean_tree(status: &str) -> Result<(), String> {
    if status.trim().is_empty() {
        Ok(())
    } else {
        Err("working tree is not clean".to_owned())
    }
}

fn git_output(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|err| format!("cannot run git: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout).map_err(|err| format!("git output is not UTF-8: {err}"))
}

fn run_status(mut command: Command, name: &str) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|err| format!("cannot start {name}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{name} failed: {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_accepts_release_and_rejects_invalid_values() {
        for version in ["0.1.0", "1.2.3-rc.1", "2.0.0+build.7"] {
            assert!(validate_version(version).is_ok());
        }
        for version in [
            "", "v1.2.3", "01.2.3", "1.2", "1.2.3-", "1.2.3+", "1.2.3-01",
        ] {
            assert!(validate_version(version).is_err());
        }
    }

    #[test]
    fn release_requires_clean_tree() {
        assert!(ensure_clean_tree("").is_ok());
        assert!(ensure_clean_tree("?? untracked\n").is_err());
        assert!(ensure_clean_tree(" M changed\n").is_err());
        assert!(validate_version("not-a-version").is_err());
    }

    #[test]
    fn release_lock_serializes_publication() {
        let output = std::env::temp_dir().join(format!(
            "craftcloud-release-lock-test-{}",
            std::process::id()
        ));
        assert!(fs::create_dir_all(&output).is_ok());
        let first = ReleaseLock::acquire(&output);
        assert!(first.is_ok());
        assert!(ReleaseLock::acquire(&output).is_err());
        drop(first);
        assert!(ReleaseLock::acquire(&output).is_ok());
        let _ = fs::remove_dir_all(output);
    }

    #[test]
    fn git_archive_uses_index_tree_and_omits_untracked_files() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent();
        assert!(root.is_some(), "workspace root missing");
        let Some(root) = root else { return };
        let scratch = root.join("dist/xtask-archive-test");
        assert!(fs::create_dir_all(&scratch).is_ok());
        let index = scratch.join("index");
        let tracked = scratch.join("tracked.txt");
        let untracked = scratch.join("untracked.txt");
        assert!(fs::write(&tracked, "tracked fixture\n").is_ok());
        assert!(fs::write(&untracked, "must not ship\n").is_ok());
        let ignored = Command::new("git")
            .args(["check-ignore", untracked.to_string_lossy().as_ref()])
            .current_dir(root)
            .status();
        assert!(
            ignored.is_ok_and(|status| status.success()),
            "fixture must be ignored by repository rules"
        );

        let blob = Command::new("git")
            .args([
                "hash-object",
                "-w",
                "--",
                tracked.to_string_lossy().as_ref(),
            ])
            .current_dir(root)
            .output();
        assert!(blob.is_ok(), "git hash-object did not start");
        let Ok(blob) = blob else { return };
        assert!(blob.status.success());
        let blob_id = String::from_utf8_lossy(&blob.stdout).trim().to_owned();
        let tree = Command::new("git")
            .args([
                "update-index",
                "--add",
                "--cacheinfo",
                "100644",
                &blob_id,
                "tracked.txt",
            ])
            .env("GIT_INDEX_FILE", &index)
            .current_dir(root)
            .status();
        assert!(tree.is_ok_and(|status| status.success()));
        let tree = Command::new("git")
            .args(["write-tree"])
            .env("GIT_INDEX_FILE", &index)
            .current_dir(root)
            .output();
        assert!(tree.is_ok(), "git write-tree did not start");
        let Ok(tree) = tree else { return };
        assert!(tree.status.success());
        let tree_id = String::from_utf8_lossy(&tree.stdout).trim().to_owned();
        let archive = Command::new("git")
            .args(["archive", "--format=tar", "--prefix=fixture/", &tree_id])
            .current_dir(root)
            .output();
        let _ = fs::remove_dir_all(&scratch);
        assert!(archive.is_ok(), "git archive did not start");
        let Ok(archive) = archive else { return };
        assert!(archive.status.success());
        assert!(
            archive
                .stdout
                .windows(b"tracked.txt".len())
                .any(|bytes| bytes == b"tracked.txt")
        );
        assert!(
            !archive
                .stdout
                .windows(b"untracked.txt".len())
                .any(|bytes| bytes == b"untracked.txt")
        );
        assert!(
            !archive
                .stdout
                .windows(b"must not ship".len())
                .any(|bytes| bytes == b"must not ship")
        );
    }

    #[test]
    fn python_archive_and_sbom_pipeline_tests_run_under_cargo_test() {
        let Some(root) = Path::new(env!("CARGO_MANIFEST_DIR")).parent() else {
            return;
        };
        let test_script = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/test_release_archive.py");
        let status = Command::new("python")
            .args(["-I", test_script.to_string_lossy().as_ref()])
            .current_dir(root)
            .status();
        assert!(
            status.is_ok_and(|status| status.success()),
            "Python release generator tests failed"
        );
    }
}
