//! Request logging and metrics (CC-F15).
//!
//! One middleware, [`observe`], sits right behind the request-id layers. For
//! every request it
//!
//! - opens the `request` span (method, route template, request id) so that all
//!   log lines of the handler carry them,
//! - counts the request in [`Metrics`] (route template, method, status class)
//!   and records its latency in a histogram,
//! - writes one `craftcloud::request` log line when the response is ready:
//!   status, latency, and the pseudonymous `actor_id` and `org_id` when the
//!   handler learned them.
//!
//! **What is logged, and what never is.** Only identifiers and numbers:
//! request id, HTTP method, the route *template* (for example
//! `/api/v1/files/{file_id}`, never the concrete path, so ids and any token in
//! a path do not appear), status, latency, actor id and organization id
//! (random UUIDs). Not logged: the query string, URL fragments (never sent),
//! any header (cookies, `Authorization`, CSRF token), request or response
//! bodies, e-mail addresses, display or file names, tokens. Requests that match
//! no route are logged with the route `unmatched`; their path is dropped.
//!
//! Metrics carry no user or organization ids as labels (cardinality and
//! privacy); route labels come from a fixed table, so a client cannot create
//! new label values.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use axum::{
    extract::{MatchedPath, Request},
    middleware::Next,
    response::Response,
};
use tracing::Instrument as _;
use uuid::Uuid;

use crate::{REQUEST_ID_HEADER, ROUTES};

/// Upper bounds of the latency histogram buckets, in seconds.
const BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Rate limit classes with a counter of their own (label `class`).
const RATE_CLASSES: [&str; 9] = [
    "login",
    "setup",
    "reset",
    "accept",
    "pwchange",
    "invite",
    "member_search",
    "failed_login",
    "two_factor_management",
];

/// Routes that are not part of [`ROUTES`] but are served and measured.
const EXTRA_ROUTES: [&str; 5] = [
    "/admin",
    "/admin/",
    "/admin/{*path}",
    "/invite",
    "/reset-password",
];

#[derive(Debug, Default, Clone)]
struct Histogram {
    /// Observations per bucket (not cumulative); the last slot is `+Inf`.
    buckets: [u64; BUCKETS.len() + 1],
    sum_micros: u64,
    count: u64,
}

#[derive(Debug, Default)]
struct HttpTable {
    requests: BTreeMap<(&'static str, &'static str, &'static str), u64>,
    latency: BTreeMap<&'static str, Histogram>,
}

/// The in-process metrics registry. Cheap to share; every counter is an atomic
/// or sits behind a short-lived lock. Hand-written Prometheus text exposition
/// (no metrics dependency).
#[derive(Debug, Default)]
pub struct Metrics {
    http: Mutex<HttpTable>,
    in_flight: AtomicI64,
    uploaded_bytes: AtomicU64,
    downloaded_bytes: AtomicU64,
    rate_limited: [AtomicU64; RATE_CLASSES.len()],
    maintenance_ok: AtomicU64,
    maintenance_failed: AtomicU64,
    maintenance_last_success: AtomicI64,
    /// Whether the last answer of `/health/ready` was a failure (for the
    /// one-line log of state changes).
    ready_failing: std::sync::atomic::AtomicBool,
}

impl Metrics {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn request_started(&self) -> InFlight<'_> {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        InFlight(self)
    }

    fn record_request(&self, route: &'static str, method: &'static str, status: u16, seconds: f64) {
        let class = status_class(status);
        let mut table = self.http.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = table.requests.entry((route, method, class)).or_insert(0);
        *slot = slot.saturating_add(1);
        let histogram = table.latency.entry(route).or_default();
        let index = BUCKETS
            .iter()
            .position(|bound| seconds <= *bound)
            .unwrap_or(BUCKETS.len());
        if let Some(bucket) = histogram.buckets.get_mut(index) {
            *bucket = bucket.saturating_add(1);
        }
        // Saturating float-to-int conversion; latency is far below u64::MAX.
        histogram.sum_micros = histogram
            .sum_micros
            .saturating_add((seconds * 1_000_000.0) as u64);
        histogram.count = histogram.count.saturating_add(1);
    }

    /// Records the outcome of a readiness probe; true when the state changed
    /// (ok to failing or back). The first success is not a change.
    pub(crate) fn note_ready(&self, failing: bool) -> bool {
        self.ready_failing.swap(failing, Ordering::Relaxed) != failing
    }

    /// Counts bytes of upload bodies received.
    pub(crate) fn add_uploaded(&self, bytes: u64) {
        self.uploaded_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Counts bytes of file content sent to download clients.
    pub(crate) fn add_downloaded(&self, bytes: u64) {
        self.downloaded_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Counts a request rejected by a rate limit of `class`.
    pub(crate) fn rate_limited(&self, class: &str) {
        if let Some(index) = RATE_CLASSES.iter().position(|c| *c == class)
            && let Some(counter) = self.rate_limited.get(index)
        {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Records the end of a maintenance run. `finished_unix` is only used for
    /// a run without errors.
    pub(crate) fn maintenance_finished(&self, ok: bool, finished_unix: i64) {
        if ok {
            self.maintenance_ok.fetch_add(1, Ordering::Relaxed);
            self.maintenance_last_success
                .store(finished_unix, Ordering::Relaxed);
        } else {
            self.maintenance_failed.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Total of rejected requests over all rate limit classes (tests).
    #[must_use]
    pub fn rate_limit_rejections(&self) -> u64 {
        self.rate_limited
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .sum()
    }

    /// Requests currently being handled.
    #[must_use]
    pub fn in_flight(&self) -> i64 {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Renders the in-process part of the exposition.
    pub(crate) fn render_into(&self, out: &mut String) {
        let table = self.http.lock().unwrap_or_else(PoisonError::into_inner);
        header(
            out,
            "craftcloud_http_requests_total",
            "counter",
            "HTTP requests by route template, method and status class.",
        );
        for ((route, method, class), count) in &table.requests {
            let _ = writeln!(
                out,
                "craftcloud_http_requests_total{{route=\"{route}\",method=\"{method}\",status_class=\"{class}\"}} {count}"
            );
        }
        header(
            out,
            "craftcloud_http_request_duration_seconds",
            "histogram",
            "Time until the response head was produced, by route template.",
        );
        for (route, histogram) in &table.latency {
            let mut cumulative = 0_u64;
            for (index, bound) in BUCKETS.iter().enumerate() {
                cumulative =
                    cumulative.saturating_add(histogram.buckets.get(index).copied().unwrap_or(0));
                let _ = writeln!(
                    out,
                    "craftcloud_http_request_duration_seconds_bucket{{route=\"{route}\",le=\"{bound}\"}} {cumulative}"
                );
            }
            let _ = writeln!(
                out,
                "craftcloud_http_request_duration_seconds_bucket{{route=\"{route}\",le=\"+Inf\"}} {}",
                histogram.count
            );
            let _ = writeln!(
                out,
                "craftcloud_http_request_duration_seconds_sum{{route=\"{route}\"}} {}",
                histogram.sum_micros as f64 / 1_000_000.0
            );
            let _ = writeln!(
                out,
                "craftcloud_http_request_duration_seconds_count{{route=\"{route}\"}} {}",
                histogram.count
            );
        }
        drop(table);
        gauge(
            out,
            "craftcloud_http_requests_in_flight",
            "Requests being handled, counted until the response head is produced (a streamed body is not included).",
            self.in_flight.load(Ordering::Relaxed),
        );
        counter(
            out,
            "craftcloud_upload_bytes_received_total",
            "Bytes of upload bodies received.",
            self.uploaded_bytes.load(Ordering::Relaxed),
        );
        counter(
            out,
            "craftcloud_download_bytes_sent_total",
            "Bytes of file content sent to clients.",
            self.downloaded_bytes.load(Ordering::Relaxed),
        );
        header(
            out,
            "craftcloud_rate_limit_rejections_total",
            "counter",
            "Requests rejected with 429 by a rate limit, by limit class.",
        );
        for (class, count) in RATE_CLASSES.iter().zip(&self.rate_limited) {
            let _ = writeln!(
                out,
                "craftcloud_rate_limit_rejections_total{{class=\"{class}\"}} {}",
                count.load(Ordering::Relaxed)
            );
        }
        header(
            out,
            "craftcloud_maintenance_runs_total",
            "counter",
            "Maintenance runs finished by this process, by result.",
        );
        let _ = writeln!(
            out,
            "craftcloud_maintenance_runs_total{{result=\"ok\"}} {}",
            self.maintenance_ok.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            out,
            "craftcloud_maintenance_runs_total{{result=\"failed\"}} {}",
            self.maintenance_failed.load(Ordering::Relaxed)
        );
        gauge(
            out,
            "craftcloud_maintenance_last_success_timestamp_seconds",
            "Unix time of the last maintenance run of this process without errors (0: none yet).",
            self.maintenance_last_success.load(Ordering::Relaxed),
        );
    }
}

/// Decrements the in-flight gauge when dropped (also when the request future
/// is cancelled).
pub(crate) struct InFlight<'a>(&'a Metrics);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) fn header(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

pub(crate) fn gauge(out: &mut String, name: &str, help: &str, value: i64) {
    header(out, name, "gauge", help);
    let _ = writeln!(out, "{name} {value}");
}

pub(crate) fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    header(out, name, "counter", help);
    let _ = writeln!(out, "{name} {value}");
}

fn status_class(status: u16) -> &'static str {
    match status {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        500..=599 => "5xx",
        _ => "other",
    }
}

fn method_label(method: &axum::http::Method) -> &'static str {
    match method.as_str() {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        _ => "OTHER",
    }
}

/// The static route template for a matched path, or `unmatched`. Looking the
/// value up in the route table keeps the label set closed.
pub(crate) fn route_label(matched: Option<&str>) -> &'static str {
    let Some(matched) = matched else {
        return "unmatched";
    };
    ROUTES
        .iter()
        .map(|(path, _)| *path)
        .chain(EXTRA_ROUTES)
        .find(|path| *path == matched)
        .unwrap_or("unmatched")
}

// ---- who made the request ----------------------------------------------------

/// Identifiers a handler learned while serving one request.
#[derive(Debug, Default)]
struct Learned {
    actor: Mutex<Option<Uuid>>,
    org: Mutex<Option<Uuid>>,
}

tokio::task_local! {
    static LEARNED: Arc<Learned>;
}

/// Remembers the authenticated account for the request log (pseudonymous id).
pub(crate) fn note_actor(user_id: Uuid) {
    let _ = LEARNED.try_with(|learned| {
        *learned.actor.lock().unwrap_or_else(PoisonError::into_inner) = Some(user_id);
    });
}

/// Remembers the organization the request was authorized for.
pub(crate) fn note_org(org_id: Uuid) {
    let _ = LEARNED.try_with(|learned| {
        *learned.org.lock().unwrap_or_else(PoisonError::into_inner) = Some(org_id);
    });
}

fn shown(id: Option<Uuid>) -> String {
    id.map_or_else(|| "-".to_owned(), |id| id.to_string())
}

/// Request span, metrics and the one request log line. See the module docs.
pub(crate) async fn observe(
    axum::extract::State(metrics): axum::extract::State<Arc<Metrics>>,
    request: Request,
    next: Next,
) -> Response {
    let route = route_label(
        request
            .extensions()
            .get::<MatchedPath>()
            .map(MatchedPath::as_str),
    );
    let method = method_label(request.method());
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_owned();
    let span =
        tracing::info_span!("request", method = %method, route = %route, request_id = %request_id);
    let learned = Arc::new(Learned::default());
    let started = Instant::now();
    let in_flight = metrics.request_started();
    let response = LEARNED
        .scope(
            Arc::clone(&learned),
            next.run(request).instrument(span.clone()),
        )
        .await;
    drop(in_flight);
    let elapsed = started.elapsed();
    let status = response.status().as_u16();
    metrics.record_request(route, method, status, elapsed.as_secs_f64());

    let actor = *learned.actor.lock().unwrap_or_else(PoisonError::into_inner);
    let org = *learned.org.lock().unwrap_or_else(PoisonError::into_inner);
    let latency_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    let actor_id = shown(actor);
    let org_id = shown(org);
    let _entered = span.enter();
    if route.starts_with("/health") {
        // Probes run every few seconds, also while the service is failing:
        // keep every one of them (5xx included) out of the default log and
        // log only the change of the readiness state, once.
        tracing::debug!(
            target: "craftcloud::request",
            status, latency_ms, actor_id = %actor_id, org_id = %org_id, "request"
        );
        if route == "/health/ready" && metrics.note_ready(status >= 500) {
            if status >= 500 {
                tracing::warn!(
                    target: "craftcloud::health",
                    status,
                    "readiness is failing: /health/ready answers 5xx (see /api/v1/admin/health)"
                );
            } else {
                tracing::info!(target: "craftcloud::health", "readiness recovered");
            }
        }
    } else if status >= 500 {
        tracing::warn!(
            target: "craftcloud::request",
            status, latency_ms, actor_id = %actor_id, org_id = %org_id, "request"
        );
    } else {
        tracing::info!(
            target: "craftcloud::request",
            status, latency_ms, actor_id = %actor_id, org_id = %org_id, "request"
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_labels_come_from_the_route_table_only() {
        assert_eq!(
            route_label(Some("/api/v1/files/{file_id}")),
            "/api/v1/files/{file_id}"
        );
        assert_eq!(route_label(Some("/api/v1/files/1234")), "unmatched");
        assert_eq!(route_label(None), "unmatched");
    }

    #[test]
    fn status_classes() {
        assert_eq!(status_class(200), "2xx");
        assert_eq!(status_class(404), "4xx");
        assert_eq!(status_class(503), "5xx");
        assert_eq!(status_class(99), "other");
    }

    #[test]
    fn histogram_buckets_are_cumulative_in_the_output() {
        let metrics = Metrics::new();
        metrics.record_request("/health/live", "GET", 200, 0.004);
        metrics.record_request("/health/live", "GET", 200, 0.2);
        metrics.record_request("/health/live", "GET", 500, 60.0);
        let mut out = String::new();
        metrics.render_into(&mut out);
        assert!(out.contains(
            "craftcloud_http_requests_total{route=\"/health/live\",method=\"GET\",status_class=\"2xx\"} 2"
        ));
        assert!(out.contains("le=\"0.005\"} 1"), "{out}");
        assert!(out.contains("le=\"0.25\"} 2"), "{out}");
        assert!(out.contains("le=\"10\"} 2"), "{out}");
        assert!(out.contains("le=\"+Inf\"} 3"), "{out}");
        assert!(out.contains("_count{route=\"/health/live\"} 3"), "{out}");
    }

    #[test]
    fn unknown_rate_limit_classes_are_ignored() {
        let metrics = Metrics::new();
        metrics.rate_limited("login");
        metrics.rate_limited("made-up");
        assert_eq!(metrics.rate_limit_rejections(), 1);
    }

    #[test]
    fn readiness_state_changes_are_reported_once() {
        let metrics = Metrics::new();
        assert!(!metrics.note_ready(false));
        assert!(metrics.note_ready(true));
        assert!(!metrics.note_ready(true));
        assert!(!metrics.note_ready(true));
        assert!(metrics.note_ready(false));
        assert!(!metrics.note_ready(false));
    }

    #[test]
    fn in_flight_guard_counts_down_on_drop() {
        let metrics = Metrics::new();
        let guard = metrics.request_started();
        assert_eq!(metrics.in_flight(), 1);
        drop(guard);
        assert_eq!(metrics.in_flight(), 0);
    }
}
