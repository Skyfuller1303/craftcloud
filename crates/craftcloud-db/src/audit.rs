//! Audit events (CC-F11): an append-only record of security-relevant actions.
//!
//! **Write semantics.** A sensitive change records its event in the *same
//! transaction* as the change ([`record_in`], [`crate::members::OrgTx::audit`]
//! or the `*_with_audit` repository functions): either both commit or neither
//! does, a rolled-back request leaves no event, and a request that finds
//! nothing to change (an idempotent replay) records nothing. If the insert
//! fails, the transaction fails and the change is not made (fail closed).
//! Events about things that did not change data (denied requests, failed
//! logins) have no transaction to join; they are written afterwards with
//! [`record`] / [`record_aggregated`] and the caller treats a failure as
//! best-effort (log and continue).
//!
//! **Data minimisation (ADR-0016).** Events hold identifiers, stable action
//! codes, enum-like result codes and allowlisted details. Instance-setting
//! audit events additionally retain old/new scalar values for a fixed set of
//! non-secret fields, including configured mail addresses and usernames, so
//! operators can see the exact configuration change. Those values are bounded
//! and checked by setting key and type. Secrets, tokens, passwords, file
//! content, IP addresses and user agents are not stored. This limited retention
//! of mail identity fields is a deliberate AC-13 exception to the earlier rule
//! against recording e-mail addresses.
//!
//! **Append-only.** Database triggers (migration 0011) reject `UPDATE`
//! (except the counter of an aggregated row), `DELETE` (except through
//! `audit_purge`, see [`purge`]) and `TRUNCATE`.
//!
//! **Aggregation.** Repeated failures that carry no new information (the
//! login failures of one account, the same denied action of one actor) share
//! one row per 15-minute window, with a counter, so a password-guessing run
//! cannot flood the log.

use craftcloud_core::policy::Action;
use serde_json::{Map, Number, Value};
use sqlx::{FromRow, PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::organizations::AfterKey;
use crate::{DbError, OrgScope};

/// Most events an export page reads at once.
pub const EXPORT_PAGE: i64 = 500;
/// Most events one export request streams.
pub const EXPORT_MAX_ROWS: i64 = 100_000;
/// Smallest accepted retention in days.
pub const MIN_RETENTION_DAYS: u32 = 30;
/// Default retention in days (ADR-0016: one year).
pub const DEFAULT_RETENTION_DAYS: u32 = 365;
/// Aggregation window in seconds.
pub const AGGREGATION_WINDOW_SECS: i64 = 900;

/// Outcome recorded with an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditResult {
    /// The action happened.
    Success,
    /// The policy refused the actor.
    Denied,
    /// The action was attempted and failed (for example a wrong password).
    Failed,
}

impl AuditResult {
    /// The stored / wire representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Denied => "denied",
            Self::Failed => "failed",
        }
    }

    /// Parses the stored / wire representation.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "success" => Some(Self::Success),
            "denied" => Some(Self::Denied),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// A value allowed in an event's `details`: identifiers, fixed codes,
/// counters, flags and strictly validated non-secret setting scalars. There
/// is deliberately no general free-text variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detail {
    /// An identifier.
    Id(Uuid),
    /// A code chosen by the program (a role, a state), never user input.
    Code(&'static str),
    /// Canonical hostname or IP address, without credentials, port or path.
    EndpointHost(String),
    /// SHA-256 fingerprint used to compare redacted configuration snapshots.
    Fingerprint([u8; 32]),
    /// A counter or size.
    Count(i64),
    /// A flag.
    Flag(bool),
    /// A scalar from a fixed allowlist of non-secret instance settings.
    InstanceSetting { key: String, value: Value },
    /// The opaque `ds_<hex>` reference of a device session: a domain-separated
    /// SHA-256 digest, never the session identifier itself.
    DeviceRef([u8; 32]),
}

/// Who acted and under which request; shared by the events of one request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditCtx {
    /// The acting user, `None` for the system (maintenance) or an
    /// unauthenticated actor.
    pub actor_id: Option<Uuid>,
    /// The request id (`x-request-id`), if the event belongs to a request.
    pub request_id: Option<String>,
}

impl AuditCtx {
    /// A context for the system (no actor, no request).
    #[must_use]
    pub fn system() -> Self {
        Self::default()
    }
}

/// An event to store.
#[derive(Debug, Clone)]
pub struct NewEvent<'a> {
    /// The tenant, `None` for instance-level events.
    pub org_id: Option<Uuid>,
    /// Stable dotted action code (`member.add`).
    pub action: std::borrow::Cow<'a, str>,
    /// Kind and id of the object acted upon.
    pub target: Option<(&'static str, Uuid)>,
    /// The workspace the event belongs to, if any.
    pub workspace_id: Option<Uuid>,
    /// The outcome.
    pub result: AuditResult,
    /// Ids, codes and counters (flat).
    pub details: Vec<(&'static str, Detail)>,
}

impl<'a> NewEvent<'a> {
    /// A successful event of `action`.
    #[must_use]
    pub fn new(action: impl Into<std::borrow::Cow<'a, str>>) -> Self {
        Self {
            org_id: None,
            action: action.into(),
            target: None,
            workspace_id: None,
            result: AuditResult::Success,
            details: Vec::new(),
        }
    }

    /// The same event with owned text, so it can outlive the request (events
    /// written by a spawned task).
    #[must_use]
    pub fn into_owned(self) -> NewEvent<'static> {
        NewEvent {
            org_id: self.org_id,
            action: std::borrow::Cow::Owned(self.action.into_owned()),
            target: self.target,
            workspace_id: self.workspace_id,
            result: self.result,
            details: self.details,
        }
    }

    /// Sets the tenant.
    #[must_use]
    pub fn org(mut self, org_id: Uuid) -> Self {
        self.org_id = Some(org_id);
        self
    }

    /// Sets the tenant if there is one.
    #[must_use]
    pub fn org_opt(mut self, org_id: Option<Uuid>) -> Self {
        self.org_id = org_id;
        self
    }

    /// Sets the object acted upon.
    #[must_use]
    pub fn target(mut self, kind: &'static str, id: Uuid) -> Self {
        self.target = Some((kind, id));
        self
    }

    /// Sets the object acted upon if there is one.
    #[must_use]
    pub fn target_opt(mut self, kind: &'static str, id: Option<Uuid>) -> Self {
        self.target = id.map(|id| (kind, id));
        self
    }

    /// Sets the workspace.
    #[must_use]
    pub fn workspace(mut self, workspace_id: Uuid) -> Self {
        self.workspace_id = Some(workspace_id);
        self
    }

    /// Sets the workspace if there is one.
    #[must_use]
    pub fn workspace_opt(mut self, workspace_id: Option<Uuid>) -> Self {
        self.workspace_id = workspace_id;
        self
    }

    /// Sets the outcome.
    #[must_use]
    pub fn result(mut self, result: AuditResult) -> Self {
        self.result = result;
        self
    }

    /// Adds a detail.
    #[must_use]
    pub fn detail(mut self, key: &'static str, value: Detail) -> Self {
        self.details.push((key, value));
        self
    }

    fn details_json(&self) -> Result<String, DbError> {
        let mut map = Map::new();
        for (key, value) in &self.details {
            if key.is_empty() || !key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
                return Err(DbError::CheckViolation {
                    constraint: Some("audit_events_details_object".to_owned()),
                });
            }
            let value = match value {
                Detail::Id(id) => Value::String(id.to_string()),
                Detail::Code(code) => Value::String((*code).to_owned()),
                Detail::EndpointHost(host) if valid_endpoint_host(host) => {
                    Value::String(host.clone())
                }
                Detail::EndpointHost(_) => {
                    return Err(DbError::CheckViolation {
                        constraint: Some("audit_events_endpoint_host".to_owned()),
                    });
                }
                Detail::Fingerprint(digest) => {
                    use std::fmt::Write as _;
                    let mut text = String::with_capacity(64);
                    for byte in digest {
                        let _ = write!(text, "{byte:02x}");
                    }
                    Value::String(text)
                }
                Detail::Count(n) => Value::Number(Number::from(*n)),
                Detail::Flag(flag) => Value::Bool(*flag),
                Detail::InstanceSetting { key, value }
                    if valid_instance_setting_value(key, value) =>
                {
                    value.clone()
                }
                Detail::InstanceSetting { .. } => {
                    return Err(DbError::CheckViolation {
                        constraint: Some("audit_events_instance_setting".to_owned()),
                    });
                }
                Detail::DeviceRef(digest) => {
                    use std::fmt::Write as _;
                    let mut text = String::with_capacity(67);
                    text.push_str("ds_");
                    for byte in digest {
                        let _ = write!(text, "{byte:02x}");
                    }
                    Value::String(text)
                }
            };
            map.insert((*key).to_owned(), value);
        }
        Ok(Value::Object(map).to_string())
    }
}

pub(crate) fn valid_instance_setting_value(key: &str, value: &Value) -> bool {
    match key {
        "mail.transport" => value.as_str().is_some_and(|v| {
            matches!(
                v,
                "none" | "smtp" | "graph" | "smtp_oauth_microsoft" | "smtp_oauth_google"
            )
        }),
        "mail.tls" => value
            .as_str()
            .is_some_and(|v| matches!(v, "starttls" | "implicit")),
        "mail.port" => value.as_u64().is_some_and(|v| (1..=65535).contains(&v)),
        "mail.save_to_sent" => value.is_boolean(),
        "retention.version_days" | "retention.trash_days" | "retention.quarantine_days" => {
            value.as_u64().is_some_and(|v| (1..=3650).contains(&v))
        }
        "retention.audit_days" => value.as_u64().is_some_and(|v| (30..=3650).contains(&v)),
        "versions.keep_all_hours" => value.as_u64().is_some_and(|v| v <= 168),
        "versions.daily_days" => value.as_u64().is_some_and(|v| v <= 90),
        "versions.weekly_weeks" => value.as_u64().is_some_and(|v| v <= 52),
        "versions.max_per_file" => value
            .as_u64()
            .is_some_and(|v| v == 0 || (2..=10_000).contains(&v)),
        "storage.compression_level" => value.as_u64().is_some_and(|v| v <= 19),
        "mail.host" => value.as_str().is_some_and(valid_setting_host),
        "mail.username" | "mail.from" | "mail.tenant_id" | "mail.client_id" | "mail.sender" => {
            value
                .as_str()
                .is_some_and(|v| v.len() <= 320 && !v.chars().any(char::is_control))
        }
        _ => false,
    }
}

fn valid_setting_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 || !host.is_ascii() || host.chars().any(char::is_control)
    {
        return false;
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
            && label
                .as_bytes()
                .last()
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

fn valid_endpoint_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 || !host.is_ascii() || host.chars().any(char::is_control)
    {
        return false;
    }
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        return address.to_string() == host;
    }
    if host != host.to_ascii_lowercase() {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && label
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

const INSERT_COLUMNS: &str = "id, org_id, actor_id, action, target_type, target_id, workspace_id, \
     result, request_id, details";

/// Stores an event on `conn`, which is normally the connection of the
/// transaction that makes the change.
///
/// # Errors
///
/// Database failures; the caller must fail the surrounding change.
pub async fn record_in(
    conn: &mut PgConnection,
    ctx: &AuditCtx,
    event: &NewEvent<'_>,
) -> Result<(), DbError> {
    let details = event.details_json()?;
    sqlx::query(&format!(
        "INSERT INTO audit_events ({INSERT_COLUMNS}) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::text::jsonb)"
    ))
    .bind(Uuid::new_v4())
    .bind(event.org_id)
    .bind(ctx.actor_id)
    .bind(&*event.action)
    .bind(event.target.map(|(kind, _)| kind))
    .bind(event.target.map(|(_, id)| id))
    .bind(event.workspace_id)
    .bind(event.result.as_str())
    .bind(ctx.request_id.as_deref())
    .bind(details)
    .execute(conn)
    .await?;
    Ok(())
}

/// Stores an event in its own statement (events without a transaction to join).
///
/// # Errors
///
/// Database failures.
pub async fn record(pool: &PgPool, ctx: &AuditCtx, event: &NewEvent<'_>) -> Result<(), DbError> {
    let mut conn = pool.acquire().await?;
    record_in(&mut conn, ctx, event).await
}

/// What an aggregated row is keyed by besides action, result, actor, tenant
/// and the 15-minute window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggScope {
    /// Nothing more: repeated attempts against many objects share one row (a
    /// denied actor probing random ids must not create one row per id).
    Actor,
    /// Also the target: one row per object (downloads of one version, login
    /// failures of one account).
    Target,
}

/// Stores an event that is folded into one row per action, result, actor,
/// tenant, 15-minute window and, with [`AggScope::Target`], target (the first occurrence creates the row, later
/// ones raise its counter). Used for failures that repeat: login failures and
/// denials.
///
/// # Errors
///
/// Database failures.
pub async fn record_aggregated(
    pool: &PgPool,
    ctx: &AuditCtx,
    event: &NewEvent<'_>,
    scope: AggScope,
) -> Result<(), DbError> {
    let details = event.details_json()?;
    let target = match scope {
        AggScope::Actor => None,
        AggScope::Target => event.target.map(|(_, id)| id),
    };
    let dash = |id: Option<Uuid>| id.map_or_else(|| "-".to_owned(), |id| id.to_string());
    let key = format!(
        "{}:{}:{}:{}:{}",
        event.action,
        event.result.as_str(),
        dash(ctx.actor_id),
        dash(event.org_id),
        dash(target),
    );
    sqlx::query(&format!(
        "INSERT INTO audit_events ({INSERT_COLUMNS}, agg_key) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::text::jsonb, \
                 $11 || ':' || (floor(extract(epoch FROM now()) / {AGGREGATION_WINDOW_SECS}))::bigint::text) \
         ON CONFLICT (agg_key) WHERE agg_key IS NOT NULL DO UPDATE \
            SET event_count = audit_events.event_count + 1, \
                last_at = GREATEST(audit_events.last_at, now())"
    ))
    .bind(Uuid::new_v4())
    .bind(event.org_id)
    .bind(ctx.actor_id)
    .bind(&*event.action)
    .bind(event.target.map(|(kind, _)| kind))
    .bind(event.target.map(|(_, id)| id))
    .bind(event.workspace_id)
    .bind(event.result.as_str())
    .bind(ctx.request_id.as_deref())
    .bind(details)
    .bind(key)
    .execute(pool)
    .await?;
    Ok(())
}

/// A stored event.
#[derive(Debug, Clone, FromRow)]
pub struct AuditRecord {
    /// Identifier.
    pub id: Uuid,
    /// Tenant, `None` for instance-level events.
    pub org_id: Option<Uuid>,
    /// Acting user, `None` for the system or an unknown actor.
    pub actor_id: Option<Uuid>,
    /// Action code.
    pub action: String,
    /// Kind of the target.
    pub target_type: Option<String>,
    /// Id of the target.
    pub target_id: Option<Uuid>,
    /// Workspace.
    pub workspace_id: Option<Uuid>,
    /// `success`, `denied` or `failed`.
    pub result: String,
    /// Request id.
    pub request_id: Option<String>,
    /// When the event happened (first occurrence for aggregated rows).
    pub created_at: OffsetDateTime,
    /// Latest occurrence (equals `created_at` for ordinary events).
    pub last_at: OffsetDateTime,
    /// Number of occurrences folded into the row.
    pub event_count: i32,
    /// The details as JSON text.
    pub details: String,
}

/// Filters of a listing. All are optional and combined with AND.
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    /// Matches this action and everything below it (`member` matches
    /// `member.add`). Must only contain `a-z`, `0-9`, `_` and `.`.
    pub action_prefix: Option<String>,
    /// The acting user.
    pub actor_id: Option<Uuid>,
    /// The target id.
    pub target_id: Option<Uuid>,
    /// The workspace.
    pub workspace_id: Option<Uuid>,
    /// The result.
    pub result: Option<AuditResult>,
    /// Events at or after this time.
    pub from: Option<OffsetDateTime>,
    /// Events before this time.
    pub to: Option<OffsetDateTime>,
}

/// Whether `prefix` is a valid action prefix filter.
#[must_use]
pub fn valid_action_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && prefix.len() <= 64
        && prefix
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
}

const SELECT: &str = "SELECT id, org_id, actor_id, action, target_type, target_id, workspace_id, \
     result, request_id, created_at, last_at, event_count, details::text AS details \
     FROM audit_events";

fn filter_sql(first_param: usize) -> String {
    let p = |n: usize| first_param + n;
    format!(
        "AND ({a}::text IS NULL OR action = {a} OR action LIKE {b} ESCAPE '\\') \
         AND ({c}::uuid IS NULL OR actor_id = {c}) \
         AND ({d}::uuid IS NULL OR target_id = {d}) \
         AND ({e}::uuid IS NULL OR workspace_id = {e}) \
         AND ({f}::text IS NULL OR result = {f}) \
         AND ({g}::timestamptz IS NULL OR created_at >= {g}) \
         AND ({h}::timestamptz IS NULL OR created_at < {h}) \
         AND ({i}::timestamptz IS NULL OR (created_at, id) < ({i}, {j}::uuid)) \
         ORDER BY created_at DESC, id DESC LIMIT {k}",
        a = format_args!("${}", p(0)),
        b = format_args!("${}", p(1)),
        c = format_args!("${}", p(2)),
        d = format_args!("${}", p(3)),
        e = format_args!("${}", p(4)),
        f = format_args!("${}", p(5)),
        g = format_args!("${}", p(6)),
        h = format_args!("${}", p(7)),
        i = format_args!("${}", p(8)),
        j = format_args!("${}", p(9)),
        k = format_args!("${}", p(10)),
    )
}

/// `_` and `%` are wildcards in `LIKE`; valid prefixes contain `_`.
fn like_pattern(prefix: &str) -> String {
    let mut pattern = String::with_capacity(prefix.len() + 3);
    for ch in prefix.chars() {
        if matches!(ch, '_' | '%' | '\\') {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push_str(".%");
    pattern
}

async fn page(
    pool: &PgPool,
    org_id: Option<Uuid>,
    filter: &AuditFilter,
    limit: i64,
    after: Option<AfterKey>,
) -> Result<Vec<AuditRecord>, DbError> {
    if filter
        .action_prefix
        .as_deref()
        .is_some_and(|p| !valid_action_prefix(p))
    {
        return Err(DbError::CheckViolation {
            constraint: Some("audit_events_action_valid".to_owned()),
        });
    }
    let scope_sql = if org_id.is_some() {
        "WHERE org_id = $1"
    } else {
        "WHERE org_id IS NULL AND $1::uuid IS NULL"
    };
    let sql = format!("{SELECT} {scope_sql} {}", filter_sql(2));
    Ok(sqlx::query_as::<_, AuditRecord>(&sql)
        .bind(org_id)
        .bind(filter.action_prefix.as_deref())
        .bind(filter.action_prefix.as_deref().map(like_pattern))
        .bind(filter.actor_id)
        .bind(filter.target_id)
        .bind(filter.workspace_id)
        .bind(filter.result.map(AuditResult::as_str))
        .bind(filter.from)
        .bind(filter.to)
        .bind(after.map(|(at, _)| at))
        .bind(after.map(|(_, id)| id))
        .bind(limit)
        .fetch_all(pool)
        .await?)
}

/// Lists the events of the scoped organization, newest first, one page after
/// `after` (`limit` is clamped to `1..=101`, one more than the largest API
/// page so the caller can tell whether another page follows).
///
/// # Errors
///
/// Database failures; [`DbError::CheckViolation`] for an invalid prefix.
pub async fn list(
    pool: &PgPool,
    scope: &OrgScope,
    filter: &AuditFilter,
    limit: u32,
    after: Option<AfterKey>,
) -> Result<Vec<AuditRecord>, DbError> {
    scope.require(Action::AuditRead)?;
    page(
        pool,
        Some(scope.org_id()),
        filter,
        crate::organizations::clamp_page(limit),
        after,
    )
    .await
}

/// Lists the instance-level events (no organization), newest first.
/// Operator-only by policy (`Action::InstanceAuditRead`).
///
/// # Errors
///
/// Database failures; [`DbError::CheckViolation`] for an invalid prefix.
pub async fn list_instance(
    pool: &PgPool,
    filter: &AuditFilter,
    limit: u32,
    after: Option<AfterKey>,
) -> Result<Vec<AuditRecord>, DbError> {
    page(
        pool,
        None,
        filter,
        crate::organizations::clamp_page(limit),
        after,
    )
    .await
}

/// One page of an export (up to [`EXPORT_PAGE`] events) for the scoped
/// organization; the caller streams pages until [`EXPORT_MAX_ROWS`].
///
/// # Errors
///
/// Database failures; [`DbError::CheckViolation`] for an invalid prefix.
pub async fn export_page(
    pool: &PgPool,
    scope: &OrgScope,
    filter: &AuditFilter,
    after: Option<AfterKey>,
    limit: i64,
) -> Result<Vec<AuditRecord>, DbError> {
    scope.require(Action::AuditRead)?;
    page(
        pool,
        Some(scope.org_id()),
        filter,
        limit.clamp(1, EXPORT_PAGE + 1),
        after,
    )
    .await
}

/// One page of an export of the instance-level events.
///
/// # Errors
///
/// Database failures; [`DbError::CheckViolation`] for an invalid prefix.
pub async fn export_page_instance(
    pool: &PgPool,
    filter: &AuditFilter,
    after: Option<AfterKey>,
    limit: i64,
) -> Result<Vec<AuditRecord>, DbError> {
    page(pool, None, filter, limit.clamp(1, EXPORT_PAGE + 1), after).await
}

/// Retention: deletes at most `batch` events older than `retention_days`
/// (measured on the database clock) and returns how many were deleted. Safe
/// to call repeatedly and concurrently; it never touches newer events.
///
/// # Errors
///
/// Database failures.
pub async fn purge(pool: &PgPool, retention_days: u32, batch: i64) -> Result<u64, DbError> {
    let removed: i64 =
        sqlx::query_scalar("SELECT audit_purge(now() - make_interval(days => $1), $2::integer)")
            .bind(i32::try_from(retention_days).unwrap_or(i32::MAX))
            .bind(i32::try_from(batch.clamp(1, 100_000)).unwrap_or(1000))
            .fetch_one(pool)
            .await?;
    Ok(u64::try_from(removed).unwrap_or(0))
}

/// How many events are older than `retention_days` (dry runs).
///
/// # Errors
///
/// Database failures.
pub async fn count_expired(pool: &PgPool, retention_days: u32) -> Result<u64, DbError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE created_at < now() - make_interval(days => $1)",
    )
    .bind(i32::try_from(retention_days).unwrap_or(i32::MAX))
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// The time of the newest successful instance-level event (no tenant) with
/// exactly this `action`, on the database clock, or `None` if there is none. The admin
/// health view uses it for the age of the last backup (`backup.created`).
/// Events removed by the retention job are gone, so a retention shorter than
/// the backup interval makes the age disappear.
///
/// # Errors
///
/// Database failures.
pub async fn latest_success_at(
    pool: &PgPool,
    action: &str,
) -> Result<Option<OffsetDateTime>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT max(created_at) FROM audit_events \
         WHERE action = $1 AND org_id IS NULL AND result = 'success'",
    )
    .bind(action)
    .fetch_one(pool)
    .await?)
}

#[cfg(test)]
mod endpoint_host_tests {
    use super::*;

    #[test]
    fn endpoint_host_accepts_canonical_hosts_and_rejects_url_components() {
        for host in ["archive.example.test", "127.0.0.1", "2001:db8::1"] {
            assert!(valid_endpoint_host(host), "{host}");
        }
        for host in [
            "user@archive.example.test",
            "archive.example.test:443",
            "archive.example.test/path",
            "archive.example.test\r\nsecret",
            &"a".repeat(254),
        ] {
            assert!(!valid_endpoint_host(host), "{host:?}");
        }
        let event = NewEvent::new("archive.configuration.enabled").detail(
            "endpoint_host",
            Detail::EndpointHost("archive.example.test".into()),
        );
        assert_eq!(
            event.details_json().unwrap(),
            r#"{"endpoint_host":"archive.example.test"}"#
        );
        let invalid = NewEvent::new("archive.configuration.enabled").detail(
            "endpoint_host",
            Detail::EndpointHost("user@host.test".into()),
        );
        assert!(invalid.details_json().is_err());
    }

    #[test]
    fn instance_setting_audit_values_are_key_and_type_limited() {
        let event = NewEvent::new("instance_setting.updated")
            .detail(
                "old",
                Detail::InstanceSetting {
                    key: "mail.transport".into(),
                    value: Value::String("smtp".into()),
                },
            )
            .detail(
                "new",
                Detail::InstanceSetting {
                    key: "mail.transport".into(),
                    value: Value::String("graph".into()),
                },
            );
        assert_eq!(
            serde_json::from_str::<Value>(&event.details_json().unwrap()).unwrap(),
            serde_json::json!({"old": "smtp", "new": "graph"})
        );
        let invalid = NewEvent::new("instance_setting.updated").detail(
            "new",
            Detail::InstanceSetting {
                key: "mail.transport".into(),
                value: Value::String("smtp://user:secret@host".into()),
            },
        );
        assert!(invalid.details_json().is_err());
        let uppercase_host = NewEvent::new("instance_setting.updated").detail(
            "new",
            Detail::InstanceSetting {
                key: "mail.host".into(),
                value: Value::String("Mail.Example.COM".into()),
            },
        );
        assert_eq!(
            uppercase_host.details_json().unwrap(),
            r#"{"new":"Mail.Example.COM"}"#
        );
        let compression = NewEvent::new("instance_setting.updated").detail(
            "new",
            Detail::InstanceSetting {
                key: "storage.compression_level".into(),
                value: Value::from(19),
            },
        );
        assert_eq!(compression.details_json().unwrap(), r#"{"new":19}"#);
        let invalid_compression = NewEvent::new("instance_setting.updated").detail(
            "new",
            Detail::InstanceSetting {
                key: "storage.compression_level".into(),
                value: Value::from(20),
            },
        );
        assert!(invalid_compression.details_json().is_err());
    }
}
