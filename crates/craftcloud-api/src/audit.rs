//! Audit events (CC-F11): recording helpers and the read / export routes.
//!
//! **Recording.** Sensitive writes record their event in the transaction of
//! the change (see `craftcloud_db::audit`); [`Change`] and [`commit_audited`]
//! do that for the handlers that hold an `OrgTx`, the `*_audited` repository
//! functions for the handlers whose repository call owns its transaction. A
//! failing insert fails the request and the change is rolled back (fail
//! closed). The structured `craftcloud::audit` log lines stay as they were.
//!
//! **Denials.** `orgs::gate` marks a 403 answer with a [`Denied`] response
//! extension; [`record_denials`] (innermost middleware) turns it into an
//! aggregated audit row after the handler is done: one row per actor, action
//! and organization and 15-minute window, with a counter. That write happens
//! outside any transaction and is best-effort: it has a short timeout, a
//! failure is logged and the 403 is still sent. Unknown resources and
//! non-members (404) are not recorded, so an outsider cannot fill the log of
//! an organization they cannot see.
//!
//! **Reading.** `GET /organizations/{org_id}/audit` (owner and admin, policy
//! action `AuditRead`) and `GET /admin/audit` (operator, `InstanceAuditRead`,
//! instance-level events only) list events with filters and a keyset cursor
//! over `(created_at, id)`, newest first; the `/export` variants stream the
//! same filtered events as NDJSON, at most [`EXPORT_MAX_ROWS`] per request.
//! Events carry ids and codes only, never names or e-mail addresses.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{Path, Query, Request, State, rejection::QueryRejection},
    http::{HeaderMap, HeaderValue, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use craftcloud_core::policy::{Action, Resource};
use craftcloud_db::audit::{
    self as db, AggScope, AuditCtx, AuditFilter, AuditRecord, AuditResult, Detail, EXPORT_MAX_ROWS,
    EXPORT_PAGE, NewEvent,
};
use craftcloud_db::members::OrgTx;
use craftcloud_db::{DbError, OrgScope, PgPool};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use uuid::Uuid;

use crate::orgs::{Gate, db_failure, gate, load_actor, not_found, page_error, parse_id, rfc3339};
use crate::pagination::{PageError, decode_time_cursor, encode_time_cursor};
use crate::responses::{internal, unavailable, validation_failed};
use crate::services::Services;
use crate::session::AuthenticatedSession;
use crate::{ApiState, REQUEST_ID_HEADER};

/// Longest wait for a best-effort audit write (denials, downloads).
const BEST_EFFORT_TIMEOUT: Duration = Duration::from_secs(2);

/// Default and largest page size of the listing.
const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 100;

// ---- recording ---------------------------------------------------------------

/// The audit context of a request: acting user and request id. A request id
/// the database would refuse is dropped rather than failing the request.
pub(crate) fn ctx(headers: &HeaderMap, actor: Option<Uuid>) -> AuditCtx {
    ctx_from(
        headers.get(REQUEST_ID_HEADER).and_then(|v| v.to_str().ok()),
        actor,
    )
}

fn ctx_from(request_id: Option<&str>, actor: Option<Uuid>) -> AuditCtx {
    let request_id = request_id
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 64
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
        .map(str::to_owned);
    AuditCtx {
        actor_id: actor,
        request_id,
    }
}

/// The kind of object an action is about, from its code. `None` for actions
/// without a single target.
pub(crate) fn target_type(action: &str) -> Option<&'static str> {
    let kind = if action.starts_with("workspace.member.") || action.starts_with("member.") {
        "user"
    } else {
        match action.split('.').next()? {
            "user" | "password_reset" | "auth" => "user",
            "invitation" => "invitation",
            "workspace" => "workspace",
            "project" => "project",
            "file" => "file",
            "version" => "file_version",
            "upload" => "upload",
            "organization" => "organization",
            _ => return None,
        }
    };
    Some(kind)
}

/// One audit event of a change, to be recorded in the change's transaction.
pub(crate) struct Change<'a> {
    headers: &'a HeaderMap,
    actor: Uuid,
    action: &'static str,
    org: Uuid,
    target: Option<Uuid>,
    workspace: Option<Uuid>,
    details: Vec<(&'static str, Detail)>,
}

impl<'a> Change<'a> {
    /// A change of `action` made by `actor` in `org`, about `target`.
    pub(crate) fn new(
        headers: &'a HeaderMap,
        actor: Uuid,
        action: &'static str,
        org: Uuid,
        target: Option<Uuid>,
    ) -> Self {
        Self {
            headers,
            actor,
            action,
            org,
            target,
            workspace: None,
            details: Vec::new(),
        }
    }

    /// The workspace the change belongs to.
    pub(crate) fn workspace(mut self, workspace: Uuid) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// Adds an id, code, counter or flag.
    pub(crate) fn detail(mut self, key: &'static str, value: Detail) -> Self {
        self.details.push((key, value));
        self
    }

    fn event(&self) -> NewEvent<'static> {
        let mut event = NewEvent::new(self.action)
            .target_opt(target_type(self.action).unwrap_or("object"), self.target)
            .workspace_opt(self.workspace);
        event.details = self.details.clone();
        event
    }
}

/// Records the event in the open transaction. On error the caller returns and
/// drops the transaction (rollback).
pub(crate) async fn record_change(tx: &mut OrgTx, change: &Change<'_>) -> Result<(), DbError> {
    tx.audit(&ctx(change.headers, Some(change.actor)), change.event())
        .await
}

/// Records the event and commits the transaction; also emits the structured
/// log line. Nothing is committed if the event cannot be stored.
pub(crate) async fn commit_audited(mut tx: OrgTx, change: Change<'_>) -> Result<(), DbError> {
    record_change(&mut tx, &change).await?;
    tx.commit().await?;
    crate::orgs::audit(
        change.headers,
        change.actor,
        change.action,
        Some(change.org),
        change.target,
        "success",
    );
    Ok(())
}

/// How many best-effort audit writes may be in flight, in two separate budgets
/// so a flood of one kind cannot starve the other. Together they are about a
/// quarter of the database pool (at least one each), so audit writes never
/// take the connections the requests themselves need.
///
/// * *lossy* (denials, failed logins): an attacker can produce these at will,
///   so when the budget is used up the event is dropped and counted.
/// * *important* (downloads, exports): produced by authorized actions, so the
///   request waits up to [`IMPORTANT_WAIT`] for a slot before the event is
///   dropped.
#[derive(Clone)]
pub(crate) struct AuditBudget {
    pub(crate) lossy: Arc<Semaphore>,
    pub(crate) important: Arc<Semaphore>,
    dropped: Arc<AtomicU64>,
    dropped_lossy: Arc<AtomicU64>,
    dropped_important: Arc<AtomicU64>,
}

/// Longest wait of an important audit write for a free slot.
const IMPORTANT_WAIT: Duration = Duration::from_secs(2);

impl AuditBudget {
    pub(crate) fn for_pool(max_connections: u32) -> Self {
        let total = usize::try_from(max_connections / 4).unwrap_or(1).max(1);
        let lossy = (total / 2).max(1);
        let important = total.saturating_sub(lossy).max(1);
        Self {
            lossy: Arc::new(Semaphore::new(lossy)),
            important: Arc::new(Semaphore::new(important)),
            dropped: Arc::new(AtomicU64::new(0)),
            dropped_lossy: Arc::new(AtomicU64::new(0)),
            dropped_important: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Events dropped so far as `(lossy, important)`. Counters of this
    /// process; they start at zero after a restart.
    pub(crate) fn dropped_counts(&self) -> (u64, u64) {
        (
            self.dropped_lossy.load(Ordering::Relaxed),
            self.dropped_important.load(Ordering::Relaxed),
        )
    }

    fn note_drop(&self, event: &NewEvent<'_>, class: &str) {
        let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        let by_class = if class == "important" {
            &self.dropped_important
        } else {
            &self.dropped_lossy
        };
        by_class.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            action = %event.action,
            class,
            dropped_total = total,
            "audit event dropped: too many audit writes in flight"
        );
    }
}

fn spawn_write(
    pool: &PgPool,
    permit: OwnedSemaphorePermit,
    ctx: &AuditCtx,
    event: &NewEvent<'_>,
    aggregate: Option<AggScope>,
) {
    let (pool, ctx, event) = (pool.clone(), ctx.clone(), event.clone().into_owned());
    tokio::spawn(async move {
        let write = async {
            match aggregate {
                Some(scope) => db::record_aggregated(&pool, &ctx, &event, scope).await,
                None => db::record(&pool, &ctx, &event).await,
            }
        };
        match tokio::time::timeout(BEST_EFFORT_TIMEOUT, write).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, action = %event.action, "audit event could not be stored");
            }
            Err(_) => {
                tracing::warn!(action = %event.action, "audit event timed out");
            }
        }
        drop(permit);
    });
}

/// Best-effort write of an event an attacker can provoke (denials, failed
/// logins). The write runs in a task of its own, so the response is never
/// delayed; when the lossy budget is used up the event is dropped and counted.
/// `aggregate` folds repeats into one row (see [`AggScope`]).
pub(crate) fn record_lossy(
    services: &Services,
    ctx: &AuditCtx,
    event: &NewEvent<'_>,
    aggregate: Option<AggScope>,
) {
    let budget = &services.audit_budget;
    match Arc::clone(&budget.lossy).try_acquire_owned() {
        Ok(permit) => spawn_write(&services.pool, permit, ctx, event, aggregate),
        Err(_) => budget.note_drop(event, "lossy"),
    }
}

/// Best-effort write of an event of an authorized action (downloads,
/// exports). Waits up to two seconds for a slot in the separate important
/// budget, so denial floods cannot push it out; then the write runs in a task
/// of its own. A failure is logged, never reported to the client.
pub(crate) async fn record_important(
    services: &Services,
    ctx: &AuditCtx,
    event: &NewEvent<'_>,
    aggregate: Option<AggScope>,
) {
    let budget = &services.audit_budget;
    let acquired = tokio::time::timeout(
        IMPORTANT_WAIT,
        Arc::clone(&budget.important).acquire_owned(),
    )
    .await;
    match acquired {
        Ok(Ok(permit)) => spawn_write(&services.pool, permit, ctx, event, aggregate),
        _ => budget.note_drop(event, "important"),
    }
}

/// Writes an event inline with a short timeout; for callers that are
/// background jobs and have no response to protect (maintenance).
pub(crate) async fn record_inline(pool: &PgPool, ctx: &AuditCtx, event: &NewEvent<'_>) {
    match tokio::time::timeout(BEST_EFFORT_TIMEOUT, db::record(pool, ctx, event)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, action = %event.action, "audit event could not be stored");
        }
        Err(_) => tracing::warn!(action = %event.action, "audit event timed out"),
    }
}

// ---- denials -------------------------------------------------------------------

/// Response extension set by `orgs::gate` on a 403.
#[derive(Debug, Clone)]
pub(crate) struct Denied {
    pub(crate) actor: Uuid,
    pub(crate) action: String,
    pub(crate) org: Option<Uuid>,
    pub(crate) target: Option<Uuid>,
}

/// Middleware: stores the policy denials of a request as aggregated audit rows
/// after the handler returned.
pub(crate) async fn record_denials(
    State(services): State<Option<Arc<Services>>>,
    request: Request,
    next: Next,
) -> Response {
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let mut response = next.run(request).await;
    if let (Some(denied), Some(services)) = (response.extensions_mut().remove::<Denied>(), services)
    {
        let mut event = NewEvent::new(&denied.action)
            .org_opt(denied.org)
            .result(AuditResult::Denied);
        if let Some(target) = denied.target {
            event = event.target(target_type(&denied.action).unwrap_or("object"), target);
        }
        let context = ctx_from(request_id.as_deref(), Some(denied.actor));
        record_lossy(&services, &context, &event, Some(AggScope::Actor));
    }
    response
}

// ---- JSON ----------------------------------------------------------------------

fn record_json(record: &AuditRecord) -> Value {
    let target = match (&record.target_type, record.target_id) {
        (kind, Some(id)) => json!({ "type": kind, "id": id.to_string() }),
        _ => Value::Null,
    };
    let details: Value = serde_json::from_str(&record.details).unwrap_or_else(|_| json!({}));
    json!({
        "id": record.id.to_string(),
        "created_at": rfc3339(record.created_at),
        "last_at": rfc3339(record.last_at),
        "count": record.event_count,
        "org_id": record.org_id.map(|id| id.to_string()),
        "actor_id": record.actor_id.map(|id| id.to_string()),
        "action": record.action,
        "target": target,
        "workspace_id": record.workspace_id.map(|id| id.to_string()),
        "result": record.result,
        "request_id": record.request_id,
        "details": details,
    })
}

// ---- queries -------------------------------------------------------------------

/// Query parameters of the listing and the export.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuditQuery {
    limit: Option<String>,
    cursor: Option<String>,
    action: Option<String>,
    actor: Option<String>,
    target: Option<String>,
    workspace: Option<String>,
    result: Option<String>,
    from: Option<String>,
    to: Option<String>,
}

struct Parsed {
    filter: AuditFilter,
    limit: u32,
    cursor: Option<String>,
}

fn parse_uuid_param(
    raw: Option<&str>,
    name: &'static str,
    bad: &mut Vec<&'static str>,
) -> Option<Uuid> {
    let raw = raw?;
    let parsed = Uuid::parse_str(raw).ok();
    if parsed.is_none() {
        bad.push(name);
    }
    parsed
}

fn parse_time_param(
    raw: Option<&str>,
    name: &'static str,
    bad: &mut Vec<&'static str>,
) -> Option<OffsetDateTime> {
    let raw = raw?;
    let parsed = OffsetDateTime::parse(raw, &Rfc3339).ok();
    if parsed.is_none() {
        bad.push(name);
    }
    parsed
}

fn parse_query(
    query: AuditQuery,
    export: bool,
    headers: &HeaderMap,
) -> Result<Parsed, Box<Response>> {
    let mut bad: Vec<&'static str> = Vec::new();
    let limit = match (&query.limit, export) {
        (None, _) => DEFAULT_LIMIT,
        (Some(_), true) => {
            return Err(Box::new(validation_failed(&["limit"], headers)));
        }
        (Some(raw), false) => match raw.parse::<u32>() {
            Ok(0) | Err(_) => return Err(Box::new(page_error(PageError::Limit, headers))),
            Ok(n) => n.min(MAX_LIMIT),
        },
    };
    let action_prefix = query.action.clone();
    if action_prefix
        .as_deref()
        .is_some_and(|prefix| !db::valid_action_prefix(prefix))
    {
        bad.push("action");
    }
    let result = match query.result.as_deref() {
        None => None,
        Some(raw) => {
            let parsed = AuditResult::parse(raw);
            if parsed.is_none() {
                bad.push("result");
            }
            parsed
        }
    };
    let filter = AuditFilter {
        action_prefix,
        actor_id: parse_uuid_param(query.actor.as_deref(), "actor", &mut bad),
        target_id: parse_uuid_param(query.target.as_deref(), "target", &mut bad),
        workspace_id: parse_uuid_param(query.workspace.as_deref(), "workspace", &mut bad),
        result,
        from: parse_time_param(query.from.as_deref(), "from", &mut bad),
        to: parse_time_param(query.to.as_deref(), "to", &mut bad),
    };
    if let (Some(from), Some(to)) = (filter.from, filter.to)
        && from >= to
    {
        bad.push("to");
    }
    if !bad.is_empty() {
        return Err(Box::new(validation_failed(&bad, headers)));
    }
    Ok(Parsed {
        filter,
        limit,
        cursor: query.cursor,
    })
}

fn decode_cursor(
    cursor: Option<&str>,
    scope: &str,
    headers: &HeaderMap,
) -> Result<Option<(OffsetDateTime, Uuid)>, Box<Response>> {
    match cursor.map(|c| decode_time_cursor(c, "audit", scope)) {
        Some(Ok(key)) => Ok(Some(key)),
        Some(Err(error)) => Err(Box::new(page_error(error, headers))),
        None => Ok(None),
    }
}

// ---- routes --------------------------------------------------------------------

/// Where a listing reads from.
#[derive(Clone)]
enum Source {
    Org(OrgScope),
    Instance,
}

impl Source {
    fn scope_text(&self) -> String {
        match self {
            Self::Org(scope) => scope.org_id().to_string(),
            Self::Instance => "instance".to_owned(),
        }
    }
}

async fn list_impl(
    services: &Services,
    source: &Source,
    query: Result<Query<AuditQuery>, QueryRejection>,
    headers: &HeaderMap,
) -> Response {
    let Ok(Query(query)) = query else {
        return validation_failed(&["query"], headers);
    };
    let parsed = match parse_query(query, false, headers) {
        Ok(parsed) => parsed,
        Err(response) => return *response,
    };
    let scope_text = source.scope_text();
    let after = match decode_cursor(parsed.cursor.as_deref(), &scope_text, headers) {
        Ok(after) => after,
        Err(response) => return *response,
    };
    let rows = match source {
        Source::Org(scope) => {
            db::list(
                &services.pool,
                scope,
                &parsed.filter,
                parsed.limit + 1,
                after,
            )
            .await
        }
        Source::Instance => {
            db::list_instance(&services.pool, &parsed.filter, parsed.limit + 1, after).await
        }
    };
    let mut rows = match rows {
        Ok(rows) => rows,
        Err(error) => return db_failure(&error, headers, "audit list failed"),
    };
    let limit = parsed.limit as usize;
    let next = if rows.len() > limit {
        rows.truncate(limit);
        rows.last()
            .map(|last| encode_time_cursor("audit", &scope_text, last.created_at, last.id))
    } else {
        None
    };
    let items: Vec<Value> = rows.iter().map(record_json).collect();
    Json(json!({ "items": items, "next_cursor": next })).into_response()
}

/// Streams the filtered events as NDJSON through a small channel, one page of
/// [`EXPORT_PAGE`] events at a time (bounded memory), at most
/// [`EXPORT_MAX_ROWS`] events. If more exist, the last line is
/// `{"truncated":true,"next_cursor":...}`; the cursor continues the export.
async fn export_impl(
    services: &Services,
    source: Source,
    query: Result<Query<AuditQuery>, QueryRejection>,
    headers: &HeaderMap,
    actor: Uuid,
) -> Response {
    let Ok(Query(query)) = query else {
        return validation_failed(&["query"], headers);
    };
    let parsed = match parse_query(query, true, headers) {
        Ok(parsed) => parsed,
        Err(response) => return *response,
    };
    let scope_text = source.scope_text();
    let after = match decode_cursor(parsed.cursor.as_deref(), &scope_text, headers) {
        Ok(after) => after,
        Err(response) => return *response,
    };
    // The export itself is an auditable act (best-effort: it is not a change).
    let org = match &source {
        Source::Org(scope) => Some(scope.org_id()),
        Source::Instance => None,
    };
    record_important(
        services,
        &ctx(headers, Some(actor)),
        &NewEvent::new("audit.export").org_opt(org),
        None,
    )
    .await;

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(2);
    let pool = services.pool.clone();
    let filter = parsed.filter;
    tokio::spawn(async move {
        let mut after = after;
        let mut sent: i64 = 0;
        loop {
            let want = (EXPORT_MAX_ROWS - sent).min(EXPORT_PAGE);
            let page = match &source {
                Source::Org(scope) => db::export_page(&pool, scope, &filter, after, want + 1).await,
                Source::Instance => db::export_page_instance(&pool, &filter, after, want + 1).await,
            };
            let mut rows = match page {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::error!(%error, "audit export failed");
                    let _ = tx
                        .send(Err(std::io::Error::other("audit export failed")))
                        .await;
                    return;
                }
            };
            let more = rows.len() as i64 > want;
            rows.truncate(usize::try_from(want).unwrap_or(0));
            let mut chunk = Vec::new();
            for row in &rows {
                chunk.extend_from_slice(record_json(row).to_string().as_bytes());
                chunk.push(b'\n');
            }
            let last = rows.last().map(|row| (row.created_at, row.id));
            sent += rows.len() as i64;
            if !chunk.is_empty() && tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                return;
            }
            if !more {
                return;
            }
            after = last;
            if sent >= EXPORT_MAX_ROWS {
                let next =
                    last.map(|(at, id)| encode_time_cursor("audit", &source.scope_text(), at, id));
                let line = format!("{}\n", json!({ "truncated": true, "next_cursor": next }));
                let _ = tx.send(Ok(Bytes::from(line))).await;
                return;
            }
        }
    });
    let mut response = Response::new(Body::new(crate::versions::ChannelBody::new(rx)));
    let out = response.headers_mut();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    out.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"audit.ndjson\""),
    );
    out.insert(
        axum::http::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// Policy check of the organization audit routes: the permit's scope.
async fn org_scope(
    services: &Services,
    session: &AuthenticatedSession,
    org_raw: &str,
    action_name: &str,
    headers: &HeaderMap,
) -> Result<OrgScope, Box<Response>> {
    let Some(org_id) = parse_id(org_raw) else {
        return Err(Box::new(not_found(headers)));
    };
    let actor = load_actor(services, session.user_id, Some(org_id))
        .await
        .map_err(|error| Box::new(db_failure(&error, headers, "actor lookup failed")))?;
    let decision = craftcloud_core::policy::can(
        &actor,
        Action::AuditRead,
        &Resource::Organization { org_id },
    );
    match gate(
        decision,
        headers,
        session.user_id,
        action_name,
        Some(org_id),
        None,
    ) {
        Gate::Open(permit) => {
            OrgScope::from_permit(&permit).ok_or_else(|| Box::new(internal(headers)))
        }
        Gate::Closed(response) => Err(Box::new(response)),
    }
}

pub(crate) async fn list_org_audit(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(org_raw): Path<String>,
    query: Result<Query<AuditQuery>, QueryRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    match org_scope(&services, &session, &org_raw, "audit.read", &headers).await {
        Ok(scope) => list_impl(&services, &Source::Org(scope), query, &headers).await,
        Err(response) => *response,
    }
}

pub(crate) async fn export_org_audit(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(org_raw): Path<String>,
    query: Result<Query<AuditQuery>, QueryRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    match org_scope(&services, &session, &org_raw, "audit.export", &headers).await {
        Ok(scope) => {
            export_impl(
                &services,
                Source::Org(scope),
                query,
                &headers,
                session.user_id,
            )
            .await
        }
        Err(response) => *response,
    }
}

pub(crate) async fn list_instance_audit(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    query: Result<Query<AuditQuery>, QueryRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if let Err(response) = crate::orgs::operator_gate(
        &services,
        &session,
        Action::InstanceAuditRead,
        "audit.read",
        None,
        &headers,
    )
    .await
    {
        return *response;
    }
    list_impl(&services, &Source::Instance, query, &headers).await
}

pub(crate) async fn export_instance_audit(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    query: Result<Query<AuditQuery>, QueryRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if let Err(response) = crate::orgs::operator_gate(
        &services,
        &session,
        Action::InstanceAuditRead,
        "audit.export",
        None,
        &headers,
    )
    .await
    {
        return *response;
    }
    export_impl(
        &services,
        Source::Instance,
        query,
        &headers,
        session.user_id,
    )
    .await
}
