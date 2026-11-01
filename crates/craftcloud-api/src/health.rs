//! Readiness, the operator health view and the Prometheus exposition
//! (CC-F15).
//!
//! - `GET /health/live` stays in `lib.rs`: no dependency is checked, so a
//!   transient database outage never restarts the process.
//! - `GET /health/ready` (public, for the reverse proxy and container
//!   health checks): 200 `{"status":"ready"}`, 200 `{"status":"degraded"}`
//!   (serving works but the database and the object store disagree about their
//!   epoch, so destructive maintenance is blocked) or 503 `NOT_READY` in the
//!   common error shape. No component names, versions, counts or error texts:
//!   an anonymous caller learns only whether to send traffic.
//! - `GET /api/v1/admin/health` (operator only): the detailed view.
//! - [`metrics_router`]: `GET /metrics` for a separate internal listener.
//!
//! The readiness probe does real work (a query, a migration check, a file
//! write in the store). It runs as a single detached task: probes that arrive
//! while it runs wait for its result, the result is cached for
//! [`READY_CACHE`], and the task completes and caches even if every caller
//! gives up. Health traffic therefore cannot become a load on the database or
//! the disk. The whole check has a deadline ([`READY_DEADLINE`]) below the
//! default request timeout. During shutdown ([`Services::begin_draining`])
//! readiness answers 503 at once.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use craftcloud_auth::BootstrapToken;
use craftcloud_core::policy::Action;
use craftcloud_db::PgPool;
use craftcloud_db::maintenance as db;
use serde_json::{Value, json};
use tokio::time::{Instant, timeout};

use crate::maintenance::{Binding, check_store_binding, run_json};
use crate::observe::{self, Metrics};
use crate::orgs::{operator_gate, rfc3339};
use crate::responses::unavailable;
use crate::services::Services;
use crate::session::AuthenticatedSession;
use crate::{ApiState, error_response};

/// How long one readiness result is reused.
const READY_CACHE: Duration = Duration::from_secs(3);

/// Longest wait for any single dependency check.
const CHECK_TIMEOUT: Duration = Duration::from_millis(1500);

/// Deadline of the whole readiness check (the steps run one after the other);
/// below the default request timeout of 10 s.
const READY_DEADLINE: Duration = Duration::from_secs(4);

/// How long the measured free disk space is reused (it costs a process).
const FREE_CACHE: Duration = Duration::from_secs(30);

/// Condition of one component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    Ok,
    Degraded,
    Down,
    NotConfigured,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Degraded => "degraded",
            Self::Down => "down",
            Self::NotConfigured => "not_configured",
        }
    }
}

/// Result of one run of the dependency checks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Snapshot {
    pub(crate) database: Level,
    pub(crate) migrations: Level,
    pub(crate) storage: Level,
    /// `matches`, `unbound`, `epoch_conflict`, `mismatch`, `error`, `unknown`
    /// or `not_configured`.
    pub(crate) binding: &'static str,
    pub(crate) epoch_conflict: bool,
}

impl Snapshot {
    /// The worst condition of the components that matter for serving.
    pub(crate) fn overall(&self) -> Level {
        let parts = [self.database, self.migrations, self.storage];
        if parts.contains(&Level::Down) {
            Level::Down
        } else if parts.contains(&Level::Degraded) || self.epoch_conflict {
            Level::Degraded
        } else {
            Level::Ok
        }
    }
}

/// Cached results shared by all requests of one [`Services`].
#[derive(Default)]
pub(crate) struct HealthCache {
    ready: Mutex<ReadyState>,
    free: tokio::sync::Mutex<Option<(Instant, Option<u64>)>>,
    /// Readiness checks completed (for the metrics and the tests).
    checks: AtomicU64,
}

#[derive(Default)]
struct ReadyState {
    last: Option<(Instant, Snapshot)>,
    running: Option<tokio::sync::watch::Receiver<Option<Snapshot>>>,
}

async fn ping(pool: &PgPool) -> bool {
    matches!(
        timeout(CHECK_TIMEOUT, craftcloud_db::ping(pool)).await,
        Ok(Ok(()))
    )
}

impl Services {
    /// Runs the dependency checks now.
    pub(crate) async fn check_now(&self) -> Snapshot {
        let database = if ping(&self.pool).await {
            Level::Ok
        } else {
            Level::Down
        };
        let migrations = if database == Level::Ok {
            match timeout(CHECK_TIMEOUT, craftcloud_db::migrate::verify(&self.pool)).await {
                Ok(Ok(())) => Level::Ok,
                _ => Level::Down,
            }
        } else {
            Level::Down
        };
        let (storage, binding, epoch_conflict) = match &self.storage {
            // Refused at startup: uploads and downloads answer 503, everything
            // else works, so this is degraded and not "do not send traffic".
            None if self.storage_unusable => (Level::Degraded, "mismatch", false),
            None => (Level::NotConfigured, "not_configured", false),
            Some(store) => match timeout(CHECK_TIMEOUT, store.probe()).await {
                Ok(Ok(())) if database == Level::Ok => {
                    let now = self.clock.now();
                    match timeout(
                        CHECK_TIMEOUT,
                        check_store_binding(&self.pool, store, now, false),
                    )
                    .await
                    {
                        Ok(Ok(Binding::Matches(_))) => (Level::Ok, "matches", false),
                        Ok(Ok(Binding::Created(_))) => (Level::Ok, "unbound", false),
                        Ok(Ok(Binding::EpochConflict(_))) => {
                            (Level::Degraded, "epoch_conflict", true)
                        }
                        Ok(Ok(Binding::Mismatch(_))) => (Level::Degraded, "mismatch", false),
                        _ => (Level::Down, "error", false),
                    }
                }
                Ok(Ok(())) => (Level::Ok, "unknown", false),
                _ => (Level::Down, "error", false),
            },
        };
        Snapshot {
            database,
            migrations,
            storage,
            binding,
            epoch_conflict,
        }
    }

    /// The readiness result, reused for [`READY_CACHE`]. At most one check
    /// runs at a time, in a task of its own: when callers are cancelled the
    /// check still completes and fills the cache. While draining the answer
    /// is "down" without any check.
    pub(crate) async fn check_cached(self: &Arc<Self>) -> Snapshot {
        if self.is_draining() {
            return DOWN;
        }
        let mut waiting = {
            let mut state = self
                .health
                .ready
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some((at, snapshot)) = state.last
                && at.elapsed() < READY_CACHE
            {
                return snapshot;
            }
            if let Some(running) = &state.running {
                running.clone()
            } else {
                let (tx, rx) = tokio::sync::watch::channel(None);
                state.running = Some(rx.clone());
                let services = Arc::clone(self);
                tokio::spawn(async move {
                    let snapshot = timeout(READY_DEADLINE, services.check_now())
                        .await
                        .unwrap_or(DOWN);
                    {
                        let mut state = services
                            .health
                            .ready
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        state.last = Some((Instant::now(), snapshot));
                        state.running = None;
                    }
                    services.health.checks.fetch_add(1, Ordering::Relaxed);
                    // Nobody may be waiting any more; the cache is filled.
                    let _ = tx.send(Some(snapshot));
                });
                rx
            }
        };
        loop {
            if let Some(snapshot) = *waiting.borrow_and_update() {
                return snapshot;
            }
            if waiting.changed().await.is_err() {
                // The check task died without a result (a panic): do not
                // leave the slot blocked.
                self.health
                    .ready
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .running = None;
                return DOWN;
            }
        }
    }

    /// Number of readiness checks that have completed (each one is a database
    /// query, a migration check and a file probe).
    #[must_use]
    pub fn readiness_checks_run(&self) -> u64 {
        self.health.checks.load(Ordering::Relaxed)
    }

    /// Starts draining: `/health/ready` answers 503 from now on, so that the
    /// proxy stops sending new requests while running ones finish. Call it
    /// when the shutdown signal arrives.
    pub fn begin_draining(&self) {
        self.draining.store(true, Ordering::Relaxed);
    }

    pub(crate) fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Relaxed)
    }

    /// Free bytes on the volume of the object store, if it can be measured.
    /// One measurement at a time, in a task of its own (callers may give up);
    /// the result, also "unknown", is reused for [`FREE_CACHE`].
    pub(crate) async fn storage_free_bytes(self: &Arc<Self>) -> Option<u64> {
        self.storage.as_ref()?;
        let services = Arc::clone(self);
        tokio::spawn(async move {
            let store = services.storage.as_ref()?;
            let mut slot = services.health.free.lock().await;
            if let Some((at, value)) = *slot
                && at.elapsed() < FREE_CACHE
            {
                return value;
            }
            let value = disk_free_bytes(store.root()).await;
            *slot = Some((Instant::now(), value));
            value
        })
        .await
        .ok()
        .flatten()
    }
}

const DOWN: Snapshot = Snapshot {
    database: Level::Down,
    migrations: Level::Down,
    storage: Level::Down,
    binding: "unknown",
    epoch_conflict: false,
};

/// Free space for unprivileged users on the volume holding `path`, in bytes.
/// The standard library has no such call and `unsafe` code is forbidden, so
/// this asks `df -Pk` (POSIX). `None` when `df` is missing, fails or times
/// out, and on platforms without it.
#[cfg(unix)]
async fn disk_free_bytes(path: &Path) -> Option<u64> {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut child = Command::new("df")
            .arg("-Pk")
            .arg("--")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        // A hung mount must not hang the caller: poll, then kill. A killed
        // child is reaped by a detached thread, never waited for here.
        let reap = |mut child: std::process::Child| {
            let _ = child.kill();
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        };
        let deadline = std::time::Instant::now() + CHECK_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(_) => {
                    reap(child);
                    return None;
                }
            }
            if std::time::Instant::now() >= deadline {
                reap(child);
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if !status.success() {
            return None;
        }
        let mut text = String::new();
        // `df -P` prints one short line per file system; bound the read anyway.
        child
            .stdout
            .take()?
            .take(16 * 1024)
            .read_to_string(&mut text)
            .ok()?;
        parse_df(&text)
    })
    .await
    .ok()
    .flatten()
}

#[cfg(not(unix))]
async fn disk_free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Reads the available kilobytes from the output of `df -Pk`. The line must
/// have exactly the six POSIX columns `Filesystem 1024-blocks Used Available
/// Capacity Mounted-on`; a file system or mount point with spaces gives more
/// fields and is reported as unknown rather than guessed.
#[cfg_attr(not(unix), allow(dead_code))]
fn parse_df(text: &str) -> Option<u64> {
    let line = text.lines().nth(1)?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    let [_, _, _, available, capacity, _] = fields.as_slice() else {
        return None;
    };
    if !capacity.ends_with('%') {
        return None;
    }
    available.parse::<u64>().ok()?.checked_mul(1024)
}

// ---- GET /health/ready ----------------------------------------------------------

pub(crate) async fn health_ready(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    let Some(services) = state.services.clone() else {
        return not_ready(&headers);
    };
    match services.check_cached().await.overall() {
        Level::Ok | Level::NotConfigured => Json(json!({ "status": "ready" })).into_response(),
        Level::Degraded => Json(json!({ "status": "degraded" })).into_response(),
        Level::Down => not_ready(&headers),
    }
}

fn not_ready(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "NOT_READY",
        "The server is not ready to take traffic.",
        headers,
    )
}

// ---- GET /api/v1/admin/health ---------------------------------------------------

/// Operator only: the detailed health view. Only numbers, flags and error
/// codes; nothing that identifies tenants, files or people.
pub(crate) async fn get_health(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if let Err(response) = operator_gate(
        &services,
        &session,
        Action::HealthRead,
        "health.read",
        None,
        &headers,
    )
    .await
    {
        return *response;
    }
    let snapshot = services.check_now().await;
    let now = services.clock.now();
    // The view must stay usable while the database is slow or down: queries
    // are skipped when the ping failed and are bounded by a timeout otherwise;
    // what cannot be read is reported as unknown (null).
    let database_up = snapshot.database == Level::Ok;
    let last_run = if database_up {
        match timeout(CHECK_TIMEOUT, db::last_run(&services.pool)).await {
            Ok(Ok(record)) => record.as_ref().map(run_json),
            _ => None,
        }
    } else {
        None
    };
    let open_sessions = if database_up {
        timeout(CHECK_TIMEOUT, db::count_open_sessions(&services.pool))
            .await
            .ok()
            .and_then(Result::ok)
    } else {
        None
    };
    let overdue_sessions = if database_up {
        timeout(CHECK_TIMEOUT, db::count_due_sessions(&services.pool, now))
            .await
            .ok()
            .and_then(Result::ok)
    } else {
        None
    };
    // The newest successful `backup.created` event (written by
    // `craftcloud-server admin backup create`); unknown (null) if there is none
    // or it cannot be read.
    let backup_age_seconds = if database_up {
        match timeout(
            CHECK_TIMEOUT,
            craftcloud_db::audit::latest_success_at(&services.pool, "backup.created"),
        )
        .await
        {
            Ok(Ok(Some(at))) => Some(u64::try_from((now - at).whole_seconds().max(0)).unwrap_or(0)),
            _ => None,
        }
    } else {
        None
    };
    let archive_status = if database_up && services.archive_operator_metadata.is_some() {
        timeout(
            CHECK_TIMEOUT,
            db::archive_status(&services.pool, services.archive_after),
        )
        .await
        .ok()
        .and_then(Result::ok)
    } else {
        None
    };
    let archive = services.archive_operator_metadata.as_ref();
    let (dropped_lossy, dropped_important) = services.audit_budget.dropped_counts();
    let pool = &services.pool;
    let size = pool.size();
    let idle = u32::try_from(pool.num_idle()).unwrap_or(u32::MAX);
    let last_run_ok = last_run.as_ref().map(|run| {
        run.get("error_codes")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
    });
    Json(json!({
        "status": snapshot.overall().as_str(),
        "checked_at": rfc3339(now),
        "components": {
            "database": {
                "status": snapshot.database.as_str(),
                "pool": {
                    "size": size,
                    "idle": idle,
                    "in_use": size.saturating_sub(idle),
                    "max": pool.options().get_max_connections(),
                },
            },
            "migrations": { "status": snapshot.migrations.as_str() },
            "two_factor": { "status": if services.two_factor_available() { "ok" } else { "not_configured" } },
            "storage": {
                "status": snapshot.storage.as_str(),
                "binding": snapshot.binding,
                "free_bytes": services.storage_free_bytes().await,
            },
        },
        "uploads": {
            "open_sessions": open_sessions,
            "overdue_sessions": overdue_sessions,
        },
        "maintenance": {
            "last_run": last_run,
            "last_run_ok": last_run_ok,
            "epoch_conflict": snapshot.epoch_conflict,
        },
        "audit": {
            "dropped_events_total": dropped_lossy.saturating_add(dropped_important),
            "dropped_events_lossy": dropped_lossy,
            "dropped_events_important": dropped_important,
        },
        "server": {
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_seconds": services.started.elapsed().as_secs(),
        },
        "backup": { "last_success_age_seconds": backup_age_seconds },
        "archive": {
            "configured": archive.is_some(),
            "provider_type": archive.map(|metadata| metadata.provider_type.as_str()),
            "endpoint": archive.and_then(|metadata| metadata.endpoint.as_deref()),
            "local_bytes": archive_status.as_ref().map(|status| status.local_bytes),
            "archive_bytes": archive_status.as_ref().map(|status| status.archive_bytes),
            "pending_archives": archive_status.as_ref().map(|status| status.pending_archives),
            "failed_deletions": archive_status.as_ref().map(|status| status.archive_delete_errors),
            "last_verified_at": archive_status.as_ref().and_then(|status| status.last_verified_at.map(rfc3339)),
            "last_error_code": archive_status.as_ref().and_then(|status| status.last_error_code.as_deref()),
        },
    }))
    .into_response()
}

// ---- GET /metrics (internal listener) -------------------------------------------

/// The complete exposition: in-process counters plus values read from the
/// database and the store at scrape time. A source that cannot be read within
/// the timeout is left out (the series disappears instead of showing a lie).
pub(crate) async fn render_metrics(metrics: &Metrics, services: Option<&Arc<Services>>) -> String {
    let mut out = String::new();
    metrics.render_into(&mut out);
    observe::header(
        &mut out,
        "craftcloud_build_info",
        "gauge",
        "Build information; the value is always 1.",
    );
    out.push_str(&format!(
        "craftcloud_build_info{{version=\"{}\"}} 1\n",
        env!("CARGO_PKG_VERSION")
    ));
    let Some(services) = services else {
        return out;
    };
    observe::gauge(
        &mut out,
        "craftcloud_uptime_seconds",
        "Seconds since the server started.",
        i64::try_from(services.started.elapsed().as_secs()).unwrap_or(i64::MAX),
    );
    let pool = &services.pool;
    let size = pool.size();
    let idle = u32::try_from(pool.num_idle()).unwrap_or(u32::MAX);
    observe::gauge(
        &mut out,
        "craftcloud_db_pool_connections",
        "Connections currently open in the database pool.",
        i64::from(size),
    );
    observe::gauge(
        &mut out,
        "craftcloud_db_pool_idle_connections",
        "Idle connections in the database pool.",
        i64::from(idle),
    );
    observe::gauge(
        &mut out,
        "craftcloud_db_pool_max_connections",
        "Configured maximum size of the database pool.",
        i64::from(pool.options().get_max_connections()),
    );
    let snapshot = services.check_cached().await;
    observe::gauge(
        &mut out,
        "craftcloud_ready",
        "1 when /health/ready answers 200 (also degraded), else 0 (also while draining).",
        i64::from(snapshot.overall() != Level::Down),
    );
    observe::counter(
        &mut out,
        "craftcloud_readiness_checks_total",
        "Readiness checks that ran (results are cached for a few seconds).",
        services.readiness_checks_run(),
    );
    observe::gauge(
        &mut out,
        "craftcloud_storage_epoch_conflict",
        "1 when database and object store disagree about their epoch (restore suspected).",
        i64::from(snapshot.epoch_conflict),
    );
    if let Ok(Ok(open)) = timeout(CHECK_TIMEOUT, db::count_open_sessions(pool)).await {
        observe::gauge(
            &mut out,
            "craftcloud_upload_sessions_open",
            "Upload sessions that are created, receiving or stored.",
            open,
        );
    }
    if let Ok(Ok(Some(run))) = timeout(CHECK_TIMEOUT, db::last_run(pool)).await {
        observe::gauge(
            &mut out,
            "craftcloud_maintenance_last_run_timestamp_seconds",
            "Unix time at which the newest recorded maintenance run (any process, any trigger) finished.",
            run.finished_at.unix_timestamp(),
        );
    }
    let (lossy, important) = services.audit_budget.dropped_counts();
    observe::header(
        &mut out,
        "craftcloud_audit_events_dropped_total",
        "counter",
        "Best-effort audit events dropped under load, by class.",
    );
    out.push_str(&format!(
        "craftcloud_audit_events_dropped_total{{class=\"lossy\"}} {lossy}\n\
         craftcloud_audit_events_dropped_total{{class=\"important\"}} {important}\n"
    ));
    if let Some(free) = services.storage_free_bytes().await {
        observe::gauge(
            &mut out,
            "craftcloud_storage_free_bytes",
            "Free bytes on the volume of the object store.",
            i64::try_from(free).unwrap_or(i64::MAX),
        );
    }
    out
}

#[derive(Clone)]
struct MetricsState {
    metrics: Arc<Metrics>,
    services: Option<Arc<Services>>,
    token: Option<Arc<BootstrapToken>>,
}

/// The router of the internal metrics listener: `GET /metrics` only. With a
/// `token`, requests need `Authorization: Bearer <token>` (compared in
/// constant time); a wrong or missing token answers 401 with no detail. This
/// router is never merged into the public one.
pub fn metrics_router(
    metrics: Arc<Metrics>,
    services: Option<Arc<Services>>,
    token: Option<BootstrapToken>,
) -> Router {
    Router::new()
        .route("/metrics", get(serve_metrics))
        .with_state(MetricsState {
            metrics,
            services,
            token: token.map(Arc::new),
        })
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token)
}

async fn serve_metrics(State(state): State<MetricsState>, headers: HeaderMap) -> Response {
    if let Some(expected) = &state.token
        && !bearer(&headers).is_some_and(|given| expected.verify(given))
    {
        let mut response = StatusCode::UNAUTHORIZED.into_response();
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return response;
    }
    let body = render_metrics(&state.metrics, state.services.as_ref()).await;
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn df_output_is_parsed_from_the_available_column() {
        let text = "Filesystem     1024-blocks      Used Available Capacity Mounted on\n\
                    /dev/sda1        102400000  51200000  40960000      56% /srv/craft\n";
        assert_eq!(parse_df(text), Some(40_960_000 * 1024));
    }

    #[test]
    fn df_garbage_and_unusual_lines_yield_none() {
        for text in [
            "",
            "only a header\n",
            "h\nno percent column here\n",
            "h\n% x\n",
            // spaces in the mount point or file system: more than six fields
            "h\n/dev/sda1 102 51 40 56% /srv/craft cloud\n",
            "h\nmy share 100 50 25 50% /mnt/x\n",
            // fewer than six fields
            "h\nfs 1 1 40 56%\n",
            // the capacity column is not a percentage
            "h\nfs 1 1 40 56 /\n",
        ] {
            assert_eq!(parse_df(text), None, "{text:?}");
        }
        assert_eq!(parse_df("h\nfs 1 1 notanumber 1% /\n"), None);
        assert_eq!(parse_df("h\nfs 1 1 18446744073709551615 1% /\n"), None);
    }

    #[test]
    fn bearer_scheme_is_case_insensitive_and_required() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer(&headers), None);
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc"),
        );
        assert_eq!(bearer(&headers), Some("abc"));
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer abc"),
        );
        assert_eq!(bearer(&headers), Some("abc"));
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(bearer(&headers), None);
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("abc"));
        assert_eq!(bearer(&headers), None);
    }

    #[test]
    fn overall_level_is_the_worst_serving_component() {
        let ok = Snapshot {
            database: Level::Ok,
            migrations: Level::Ok,
            storage: Level::NotConfigured,
            binding: "not_configured",
            epoch_conflict: false,
        };
        assert_eq!(ok.overall(), Level::Ok);
        let conflict = Snapshot {
            storage: Level::Degraded,
            epoch_conflict: true,
            ..ok
        };
        assert_eq!(conflict.overall(), Level::Degraded);
        let down = Snapshot {
            database: Level::Down,
            ..conflict
        };
        assert_eq!(down.overall(), Level::Down);
    }
}
