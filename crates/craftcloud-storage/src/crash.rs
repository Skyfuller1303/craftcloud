//! Process-level crash points for crash-recovery tests.
//!
//! The in-process fault plan ([`crate::FaultPlan`]) injects errors; a crash
//! point instead kills the whole process with [`std::process::abort`] at a
//! named place, the way a power cut or `kill -9` would, so that tests can
//! start the real server binary again and look at what is left on disk and in
//! the database.
//!
//! Everything armed here exists only with the `fault-injection` feature. In a
//! normal build [`crash_point`] is an empty function and neither the
//! environment variable nor the abort call is compiled in.

/// Name of the environment variable that arms a crash point. The value is
/// `<point>` or `<point>:<minimum progress>` (for example
/// `upload-mid-stream:262144`). It is read once at startup by the server
/// binary (feature `fault-injection` only); an unknown point is a startup
/// error, so that a typo can never silently disable a test.
#[cfg(feature = "fault-injection")]
pub const CRASH_ENV: &str = "CC_FAULT_ABORT_AT";

/// The known crash points.
///
/// - `upload-mid-stream`: after the bytes written to the temporary `.part`
///   file reach the armed number (progress = bytes flushed to the file).
/// - `upload-before-record`: the object is published, the transaction that
///   records the blob is still open.
/// - `version-after-commit`: the version commit transaction has committed,
///   the response was not sent yet.
/// - `gc-after-claim`: a garbage-collection claim (`deleting`) is committed,
///   the object is not deleted yet.
/// - `archive-after-intent`: the archive worker persisted its immutable key and
///   envelope, then entered archive transport.
/// - `archive-upload-started`, `archive-upload-mid-stream`,
///   `archive-upload-published`, `archive-upload-verified`: transport stages.
/// - `archive-db-committed`: verified archive reference committed, local copy
///   remains.
/// - `archive-local-deleted`: local bytes removed, pending-delete state remains.
/// - `archive-recall-claimed`, `archive-recall-download-started`,
///   `archive-recall-download-complete`, `archive-recall-local-written`,
///   `archive-recall-verified`, `archive-recall-committed`: operator recall
///   stages. Recall never deletes the archive object.
#[cfg(feature = "fault-injection")]
pub const CRASH_POINTS: [&str; 17] = [
    "upload-mid-stream",
    "upload-before-record",
    "version-after-commit",
    "gc-after-claim",
    "archive-after-intent",
    "archive-upload-started",
    "archive-upload-mid-stream",
    "archive-upload-published",
    "archive-upload-verified",
    "archive-db-committed",
    "archive-local-deleted",
    "archive-recall-claimed",
    "archive-recall-download-started",
    "archive-recall-download-complete",
    "archive-recall-local-written",
    "archive-recall-verified",
    "archive-recall-committed",
];

#[cfg(feature = "fault-injection")]
static ARMED: std::sync::OnceLock<Option<(String, u64)>> = std::sync::OnceLock::new();

/// Reads [`CRASH_ENV`] once and arms the named crash point.
///
/// # Errors
///
/// A message when the value names an unknown point or has a bad number.
#[cfg(feature = "fault-injection")]
pub fn arm_crash_point_from_env() -> Result<(), String> {
    let armed = match std::env::var(CRASH_ENV) {
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(format!("{CRASH_ENV} is not valid unicode"));
        }
        Ok(raw) => {
            let (name, min) = match raw.split_once(':') {
                Some((name, number)) => (
                    name,
                    number
                        .parse::<u64>()
                        .map_err(|_| format!("{CRASH_ENV}: bad number {number:?}"))?,
                ),
                None => (raw.as_str(), 0),
            };
            if !CRASH_POINTS.contains(&name) {
                return Err(format!(
                    "{CRASH_ENV}: unknown crash point {name:?}; known: {}",
                    CRASH_POINTS.join(", ")
                ));
            }
            Some((name.to_owned(), min))
        }
    };
    ARMED
        .set(armed)
        .map_err(|_| format!("{CRASH_ENV} was already read"))
}

/// Aborts the process if `point` is the armed crash point and `progress` has
/// reached the armed minimum. Does nothing otherwise, and nothing at all
/// without the `fault-injection` feature.
#[cfg(feature = "fault-injection")]
pub fn crash_point(point: &str, progress: u64) {
    if let Some(Some((name, min))) = ARMED.get()
        && name == point
        && progress >= *min
    {
        eprintln!(
            "fault injection: aborting the process at crash point {point} (progress {progress})"
        );
        std::process::abort();
    }
}

/// Without the `fault-injection` feature a crash point is an empty function.
#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
pub fn crash_point(_point: &str, _progress: u64) {}
