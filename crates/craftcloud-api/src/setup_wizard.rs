//! Operator-only setup wizard state and database-backed instance settings.

use std::collections::BTreeMap;

use axum::{
    Extension, Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use base64::Engine;
use craftcloud_auth::encrypt_secret_for;
use craftcloud_core::policy::Action;
use craftcloud_db::{instance_settings as db, setup};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};
use tungstenite::client::IntoClientRequest;
use uuid::Uuid;
use zeroize::Zeroize;

use crate::{
    ApiState, error_response,
    orgs::{self, rfc3339},
    responses::{internal, rejection_response, unavailable, validation_failed},
    session::{AuthenticatedSession, CredentialKind},
};

const STEP_IDS: &[&str] = &[
    "account",
    "system_check",
    "two_factor",
    "email_delivery",
    "organization",
    "workspace_project",
    "invitations",
    "optional_services",
    "summary",
];
const MUTABLE_STEPS: &[&str] = &[
    "system_check",
    "two_factor",
    "email_delivery",
    "organization",
    "workspace_project",
    "invitations",
    "optional_services",
    "summary",
];
const REQUIRED_STEPS: &[&str] = &[
    "account",
    "system_check",
    "two_factor",
    "email_delivery",
    "organization",
    "workspace_project",
    "invitations",
    "optional_services",
    "summary",
];
const SECRET_KEYS: &[&str] = db::SECRET_KEYS;
const SETTING_KEYS: &[&str] = &[
    "mail.transport",
    "mail.host",
    "mail.port",
    "mail.tls",
    "mail.username",
    "mail.from",
    "mail.tenant_id",
    "mail.client_id",
    "mail.sender",
    "mail.save_to_sent",
    "retention.version_days",
    "retention.trash_days",
    "retention.audit_days",
    "retention.quarantine_days",
];

fn failed(error: &craftcloud_db::DbError, headers: &HeaderMap, action: &'static str) -> Response {
    orgs::db_failure(error, headers, action)
}

async fn wizard_complete(pool: &craftcloud_db::PgPool) -> Result<bool, craftcloud_db::DbError> {
    if !setup::is_completed(pool).await? {
        return Ok(false);
    }
    let rows = db::setup_steps(pool).await?;
    Ok(REQUIRED_STEPS
        .iter()
        .filter(|id| **id != "account")
        .all(|id| {
            rows.iter().find(|row| row.step == *id).is_some_and(|row| {
                if *id == "summary" {
                    row.status == "done"
                } else {
                    row.status == "done" || row.status == "skipped"
                }
            })
        }))
}

async fn operator(
    state: &ApiState,
    session: &AuthenticatedSession,
    headers: &HeaderMap,
    action: &'static str,
) -> Result<std::sync::Arc<crate::Services>, Box<Response>> {
    let Some(services) = state.services.clone() else {
        return Err(Box::new(unavailable(headers, "AUTH_UNAVAILABLE")));
    };
    orgs::operator_gate(
        &services,
        session,
        Action::PasswordResetIssue,
        action,
        None,
        headers,
    )
    .await?;
    Ok(services)
}

fn setting_code(key: &str) -> Option<&'static str> {
    match key {
        "mail.transport" => Some("mail.transport"),
        "mail.host" => Some("mail.host"),
        "mail.port" => Some("mail.port"),
        "mail.tls" => Some("mail.tls"),
        "mail.username" => Some("mail.username"),
        "mail.from" => Some("mail.from"),
        "mail.tenant_id" => Some("mail.tenant_id"),
        "mail.client_id" => Some("mail.client_id"),
        "mail.sender" => Some("mail.sender"),
        "mail.save_to_sent" => Some("mail.save_to_sent"),
        "mail.password" => Some("mail.password"),
        "mail.client_secret" => Some("mail.client_secret"),
        "mail.certificate" => Some("mail.certificate"),
        "mail.private_key" => Some("mail.private_key"),
        "mail.service_account" => Some("mail.service_account"),
        "retention.version_days" => Some("retention.version_days"),
        "retention.trash_days" => Some("retention.trash_days"),
        "retention.audit_days" => Some("retention.audit_days"),
        "retention.quarantine_days" => Some("retention.quarantine_days"),
        _ => None,
    }
}

fn setting_requires_fresh_auth(key: &str) -> bool {
    key.starts_with("mail.")
}

fn is_retention_key(key: &str) -> bool {
    key.starts_with("retention.")
}

/// True when the new value is shorter than the period currently in force
/// (the stored value including a running grace period, else the default).
fn shortens_retention(
    key: &str,
    new_days: u64,
    rows: &[db::InstanceSetting],
    now: time::OffsetDateTime,
) -> bool {
    let current = rows
        .iter()
        .find(|row| row.key == key)
        .and_then(|row| db::effective_retention_days(key, &row.value, row.updated_at, now))
        .or_else(|| default_setting_value(key).as_u64());
    current.is_some_and(|current| new_days < current)
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

fn default_setting_value(key: &str) -> Value {
    match key {
        "mail.transport" => json!("none"),
        "retention.version_days" | "retention.trash_days" | "retention.quarantine_days" => {
            json!(30)
        }
        "retention.audit_days" => json!(365),
        _ => Value::Null,
    }
}

fn setting_value_valid(key: &str, value: &Value) -> bool {
    match key {
        "mail.transport" => value.as_str().is_some_and(|v| {
            matches!(
                v,
                "none" | "smtp" | "graph" | "smtp_oauth_microsoft" | "smtp_oauth_google"
            )
        }),
        "mail.port" => value
            .as_u64()
            .is_some_and(|v| (1..=u16::MAX as u64).contains(&v)),
        "mail.tls" => value
            .as_str()
            .is_some_and(|v| matches!(v, "starttls" | "implicit")),
        "mail.save_to_sent" => value.is_boolean(),
        "retention.version_days" | "retention.trash_days" | "retention.quarantine_days" => {
            value.as_u64().is_some_and(|v| (1..=3650).contains(&v))
        }
        "retention.audit_days" => value.as_u64().is_some_and(|v| (30..=3650).contains(&v)),
        "mail.host" => value.as_str().is_some_and(valid_setting_host),
        "mail.username" | "mail.from" | "mail.tenant_id" | "mail.client_id" | "mail.sender" => {
            value.is_null() && key != "mail.host"
                || value
                    .as_str()
                    .is_some_and(|v| v.len() <= 320 && !v.chars().any(char::is_control))
        }
        _ => false,
    }
}

pub(crate) async fn status(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
) -> Response {
    let services = match operator(&state, &session, &headers, "setup.status").await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let setup_complete = match setup::is_completed(&services.pool).await {
        Ok(value) => value,
        Err(error) => return failed(&error, &headers, "setup state lookup failed"),
    };
    let first = match sqlx::query_as::<_, (Uuid, String, String)>(
        "SELECT o.id, o.name, o.slug FROM instance_setup s JOIN organizations o ON o.owner_user_id=s.operator_user_id ORDER BY o.created_at, o.id LIMIT 1",
    ).fetch_optional(&services.pool).await {
        Ok(row) => row,
        Err(error) => return failed(&error.into(), &headers, "setup organization lookup failed"),
    };
    let rows = match db::setup_steps(&services.pool).await {
        Ok(rows) => rows,
        Err(error) => return failed(&error, &headers, "setup step lookup failed"),
    };
    let mut steps = Vec::new();
    for id in STEP_IDS {
        let row = rows.iter().find(|row| row.step == *id);
        let account_complete = *id == "account" && setup_complete;
        let status = if account_complete {
            "done"
        } else {
            row.map_or("pending", |row| row.status.as_str())
        };
        steps.push(json!({"id":id,"status":status,"updated_at":row.map(|row|rfc3339(row.updated_at)),"updated_by":row.and_then(|row|row.updated_by).map(|id|id.to_string())}));
    }
    let complete = wizard_complete(&services.pool).await.unwrap_or(false);
    let organization =
        first.map(|(id, name, slug)| json!({"id":id.to_string(),"name":name,"slug":slug}));
    Json(json!({"organization":organization,"steps":steps,"complete":complete})).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepUpdate {
    status: String,
}

pub(crate) async fn update_step(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(step): Path<String>,
    body: Result<Json<StepUpdate>, JsonRejection>,
) -> Response {
    let services = match operator(&state, &session, &headers, "setup.step.update").await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    if let Err(response) = services.rate_limit_admin(session.user_id, &headers) {
        return *response;
    }
    match wizard_complete(&services.pool).await {
        Ok(true) => {
            return error_response(
                StatusCode::CONFLICT,
                "SETUP_ALREADY_COMPLETE",
                "Setup is already complete.",
                &headers,
            );
        }
        Err(error) => return failed(&error, &headers, "setup state lookup failed"),
        _ => {}
    }
    if !MUTABLE_STEPS.contains(&step.as_str()) {
        return orgs::not_found(&headers);
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return rejection_response(&error, &headers),
    };
    if !matches!(body.status.as_str(), "done" | "skipped")
        || (matches!(step.as_str(), "system_check" | "summary") && body.status != "done")
    {
        return validation_failed(&["status"], &headers);
    }
    let step_code = match step.as_str() {
        "system_check" => "system_check",
        "two_factor" => "two_factor",
        "email_delivery" => "email_delivery",
        "organization" => "organization",
        "workspace_project" => "workspace_project",
        "invitations" => "invitations",
        "summary" => "summary",
        _ => "optional_services",
    };
    match db::set_setup_step_audited(
        &services.pool,
        &step,
        step_code,
        &body.status,
        session.user_id,
        &crate::audit::ctx(&headers, Some(session.user_id)),
    )
    .await
    {
        Ok(row) => {
            let complete = match wizard_complete(&services.pool).await {
                Ok(value) => value,
                Err(error) => return failed(&error, &headers, "setup state lookup failed"),
            };
            Json(json!({"step":{"id":row.step,"status":row.status,"updated_at":rfc3339(row.updated_at),"updated_by":row.updated_by.map(|id|id.to_string())},"complete":complete})).into_response()
        }
        Err(db::StepUpdateError::AlreadyComplete) => error_response(
            StatusCode::CONFLICT,
            "SETUP_ALREADY_COMPLETE",
            "Setup is already complete.",
            &headers,
        ),
        Err(db::StepUpdateError::Incomplete) => error_response(
            StatusCode::CONFLICT,
            "SETUP_STEPS_INCOMPLETE",
            "Complete or skip all setup steps before finishing.",
            &headers,
        ),
        Err(db::StepUpdateError::Db(error)) => failed(&error, &headers, "setup step update failed"),
    }
}

pub(crate) async fn system_check(
    State(state): State<ApiState>,
    headers: HeaderMap,
    proxy: Option<Extension<crate::client_ip::TrustedProxy>>,
    session: AuthenticatedSession,
) -> Response {
    let services = match operator(&state, &session, &headers, "setup.system_check").await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let public_url = services.settings.public_url.as_deref();
    let request_origin = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok());
    let origin_matches = public_origin_matches(
        services.settings.public_origin.as_deref(),
        request_origin,
        host,
    );
    let version_num =
        sqlx::query_scalar::<_, String>("SELECT current_setting('server_version_num')")
            .fetch_one(&services.pool)
            .await
            .ok()
            .and_then(|v| v.parse::<i64>().ok());
    let db_version = version_num.and_then(craftcloud_db::postgres_major);
    let (postgres_status, postgres_message) = postgres_check(db_version);
    let drift_seconds = sqlx::query_scalar::<_, time::OffsetDateTime>("SELECT clock_timestamp()")
        .fetch_one(&services.pool)
        .await
        .ok()
        .map(|database_now| {
            (services.clock.now() - database_now)
                .whole_seconds()
                .unsigned_abs()
        });
    let free_bytes = services.storage_free_bytes.as_ref().and_then(|read| read());
    let trusted_https = https_verified(
        proxy.is_some_and(|Extension(proxy)| proxy.0),
        headers.get_all("x-forwarded-proto").iter().count(),
        headers
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok()),
    );
    let figcraft_probe = probe_figcraft(services.figcraft_internal_url.clone()).await;
    let items = vec![
        json!({"id":"public_url","status":if public_url.is_none(){"warning"}else if origin_matches==Some(true){"ok"}else if origin_matches.is_none(){"warning"}else{"error"},"message":if public_url.is_none(){"setup.check.public_url.unconfigured"}else if origin_matches==Some(true){"setup.check.public_url.ok"}else if origin_matches.is_none(){"setup.check.public_url.origin_unavailable"}else{"setup.check.public_url.mismatch"},"docs_path":"docs/06-operations/install.md#public-url"}),
        json!({"id":"https","status":if trusted_https{"ok"}else{"warning"},"message":if trusted_https{"setup.check.https.ok"}else{"setup.check.https.unverified"},"docs_path":"docs/06-operations/install.md#reverse-proxy"}),
        json!({"id":"postgres_version","status":postgres_status,"message":postgres_message,"docs_path":"docs/06-operations/install.md#postgresql"}),
        json!({"id":"storage_free_space","status":if free_bytes.is_some_and(|bytes|!low_storage(Some(bytes))){"ok"}else{"warning"},"message":if free_bytes.is_some_and(|bytes|low_storage(Some(bytes))){"setup.check.storage_free_space.low"}else if free_bytes.is_some(){"setup.check.storage_free_space.ok"}else{"setup.check.storage_free_space.unavailable"},"free_bytes":free_bytes,"docs_path":"docs/06-operations/install.md#storage"}),
        json!({"id":"figcraft","status":match figcraft_probe {FigcraftProbe::Reachable=>"ok",FigcraftProbe::HandshakeFailed=>"warning",FigcraftProbe::Unreachable=>"error",FigcraftProbe::NotConfigured=>"not_configured"},"message":match figcraft_probe {FigcraftProbe::Reachable=>"setup.check.figcraft.reachable",FigcraftProbe::HandshakeFailed=>"setup.check.figcraft.handshake_failed",FigcraftProbe::Unreachable=>"setup.check.figcraft.unreachable",FigcraftProbe::NotConfigured=>"setup.check.figcraft.not_configured"},"docs_path":"docs/06-operations/install.md#figcraft"}),
        json!({"id":"clock_drift","status":if drift_seconds.is_some_and(|seconds|seconds<=300){"ok"}else{"warning"},"message":if drift_seconds.is_some_and(|seconds|seconds<=300){"setup.check.clock_drift.ok"}else{"setup.check.clock_drift.warning"},"drift_seconds":drift_seconds,"docs_path":"docs/06-operations/install.md#time-synchronization"}),
        json!({"id":"maintenance","status":if services.maintenance_enabled{"ok"}else{"warning"},"message":if services.maintenance_enabled{"setup.check.maintenance.enabled"}else{"setup.check.maintenance.disabled"},"docs_path":"docs/06-operations/install.md#maintenance"}),
        json!({"id":"archive","status":if services.archive_operator_metadata.is_some(){"warning"}else{"not_configured"},"message":if services.archive_operator_metadata.is_some(){"setup.check.archive.configured_unverified"}else{"setup.check.archive.not_configured"},"docs_path":"docs/06-operations/install.md#archive"}),
    ];
    Json(json!({"checked_at":rfc3339(services.clock.now()),"items":items})).into_response()
}

pub(crate) async fn get_settings(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
) -> Response {
    let services = match operator(&state, &session, &headers, "instance_settings.read").await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    match get_settings_value(&services, &headers).await {
        Ok(body) => Json(body).into_response(),
        Err(response) => *response,
    }
}

async fn get_settings_value(
    services: &crate::Services,
    headers: &HeaderMap,
) -> Result<Value, Box<Response>> {
    let rows = match db::settings(&services.pool).await {
        Ok(rows) => rows,
        Err(error) => {
            return Err(Box::new(failed(
                &error,
                headers,
                "instance settings lookup failed",
            )));
        }
    };
    let values: BTreeMap<_, _> = rows
        .iter()
        .map(|row| (row.key.as_str(), &row.value))
        .collect();
    let mut settings = serde_json::Map::new();
    for key in SETTING_KEYS {
        let stored = values.get(key).copied();
        let locked = services.instance_setting_locked.contains(*key);
        let (value, source) = if locked {
            (
                services
                    .instance_setting_overrides
                    .get(*key)
                    .cloned()
                    .unwrap_or(Value::Null),
                "environment",
            )
        } else if let Some(value) = stored.and_then(|v| v.get("value")) {
            (value.clone(), "database")
        } else {
            (default_setting_value(key), "default")
        };
        settings.insert(
            (*key).to_owned(),
            json!({"value":value,"source":source,"locked":locked}),
        );
    }
    for key in SECRET_KEYS {
        let stored = values.get(key).copied();
        let locked = services.instance_setting_locked.contains(*key);
        let source = if locked {
            "environment"
        } else if stored.is_some() {
            "database"
        } else {
            "default"
        };
        settings.insert((*key).to_owned(),json!({"configured_at":if locked {Value::Null} else {stored.and_then(|v|v.get("configured_at")).cloned().unwrap_or(Value::Null)},"source":source,"locked":locked}));
    }
    let updated_at = rows.iter().map(|row| row.updated_at).max().map(rfc3339);
    Ok(json!({"settings":settings,"updated_at":updated_at}))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SettingWrite {
    value: Option<Value>,
    set: Option<String>,
    remove: Option<bool>,
}

impl Drop for SettingWrite {
    fn drop(&mut self) {
        if let Some(secret) = &mut self.set {
            secret.zeroize();
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsWrite {
    settings: BTreeMap<String, SettingWrite>,
}

#[derive(Clone, Copy)]
enum FreshAuthFailure {
    Stale,
    MfaRequired,
    Database,
}

fn fresh_mfa_timestamp(
    verified: Option<time::OffsetDateTime>,
    enabled: Option<time::OffsetDateTime>,
    session_created: time::OffsetDateTime,
    now: time::OffsetDateTime,
) -> bool {
    verified.zip(enabled).is_some_and(|(verified, enabled)| {
        verified >= enabled
            && verified >= session_created
            && verified <= now
            && now - verified <= time::Duration::minutes(5)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FigcraftProbe {
    NotConfigured,
    Reachable,
    HandshakeFailed,
    Unreachable,
}

const LOW_STORAGE_BYTES: u64 = 1024 * 1024 * 1024;

fn public_origin_matches(
    expected: Option<&str>,
    request_origin: Option<&str>,
    host: Option<&str>,
) -> Option<bool> {
    let expected = url::Url::parse(expected?).ok()?;
    if let Some(origin) = request_origin {
        let actual = url::Url::parse(origin).ok()?;
        return Some(expected.origin() == actual.origin());
    }
    let host = host?;
    let actual_host = if host.starts_with('[') {
        host.split(']').next()?.trim_start_matches('[')
    } else {
        host.split(':').next()?
    };
    Some(expected.host_str()?.eq_ignore_ascii_case(actual_host))
}

fn low_storage(free_bytes: Option<u64>) -> bool {
    free_bytes.is_none_or(|bytes| bytes < LOW_STORAGE_BYTES)
}

fn https_verified(proxy_trusted: bool, proto_header_count: usize, proto: Option<&str>) -> bool {
    proxy_trusted && proto_header_count == 1 && proto == Some("https")
}

fn postgres_check(major: Option<u32>) -> (&'static str, &'static str) {
    match major {
        Some(version)
            if (craftcloud_db::MIN_POSTGRES_MAJOR..=craftcloud_db::MAX_POSTGRES_MAJOR)
                .contains(&version) =>
        {
            ("ok", "setup.check.postgres_version.ok")
        }
        Some(_) => ("warning", "setup.check.postgres_version.unsupported"),
        None => ("error", "setup.check.postgres_version.unavailable"),
    }
}

async fn probe_figcraft(url: Option<String>) -> FigcraftProbe {
    let Some(url) = url else {
        return FigcraftProbe::NotConfigured;
    };
    tokio::time::timeout(
        Duration::from_secs(4),
        tokio::task::spawn_blocking(move || {
            let Ok(endpoint) = url::Url::parse(&url) else {
                return FigcraftProbe::Unreachable;
            };
            let Some(host) = endpoint.host_str() else {
                return FigcraftProbe::Unreachable;
            };
            let Some(port) = endpoint.port_or_known_default() else {
                return FigcraftProbe::Unreachable;
            };
            let Ok(addresses) = (host, port).to_socket_addrs() else {
                return FigcraftProbe::Unreachable;
            };
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            for address in addresses {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let Ok(stream) = TcpStream::connect_timeout(&address, remaining) else {
                    continue;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                let Ok(request) = url.as_str().into_client_request() else {
                    return FigcraftProbe::Unreachable;
                };
                return match tungstenite::client_tls_with_config(request, stream, None, None) {
                    Ok(_) => FigcraftProbe::Reachable,
                    Err(_) => FigcraftProbe::HandshakeFailed,
                };
            }
            FigcraftProbe::Unreachable
        }),
    )
    .await
    .map_or(FigcraftProbe::Unreachable, |result| {
        result.unwrap_or(FigcraftProbe::Unreachable)
    })
}

async fn fresh_auth(
    services: &crate::Services,
    session: &AuthenticatedSession,
) -> Result<(), FreshAuthFailure> {
    let now = services.clock.now();
    if session.credential_kind != CredentialKind::Cookie
        || session.created_at > now
        || now - session.created_at > time::Duration::minutes(5)
    {
        return Err(FreshAuthFailure::Stale);
    }
    let mfa_required: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL) OR EXISTS(SELECT 1 FROM organization_members m JOIN organizations o ON o.id=m.org_id WHERE m.user_id=$1 AND m.status='active' AND o.require_two_factor)").bind(session.user_id).fetch_one(&services.pool).await.map_err(|_|FreshAuthFailure::Database)?;
    if mfa_required {
        let proof: Option<(Option<time::OffsetDateTime>, Option<time::OffsetDateTime>)> = sqlx::query_as(
            "SELECT s.mfa_verified_at, f.enabled_at FROM sessions s LEFT JOIN user_two_factor f ON f.user_id=s.user_id AND f.enabled_at IS NOT NULL WHERE s.id=$1 AND s.user_id=$2 AND s.revoked_at IS NULL",
        ).bind(session.session_id).bind(session.user_id).fetch_optional(&services.pool).await.map_err(|_|FreshAuthFailure::Database)?;
        let fresh_factor = proof.is_some_and(|(verified, enabled)| {
            fresh_mfa_timestamp(verified, enabled, session.created_at, now)
        });
        if !fresh_factor {
            return Err(FreshAuthFailure::MfaRequired);
        }
    }
    Ok(())
}

pub(crate) async fn update_settings(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    body: Result<Json<SettingsWrite>, JsonRejection>,
) -> Response {
    let services = match operator(&state, &session, &headers, "instance_settings.update").await {
        Ok(s) => s,
        Err(r) => return *r,
    };
    if let Err(response) = services.rate_limit_admin(session.user_id, &headers) {
        return *response;
    }
    let Json(mut body) = match body {
        Ok(body) => body,
        Err(error) => return rejection_response(&error, &headers),
    };
    let mut invalid = Vec::new();
    let mut requires_fresh_auth = false;
    for (key, entry) in &body.settings {
        if setting_requires_fresh_auth(key) {
            requires_fresh_auth = true;
        }
        if services.instance_setting_locked.contains(key) {
            return error_response(
                StatusCode::CONFLICT,
                "SETTING_LOCKED",
                "This setting is controlled by the server configuration.",
                &headers,
            );
        }
        if SECRET_KEYS.contains(&key.as_str()) {
            requires_fresh_auth = true;
            let max = match key.as_str() {
                "mail.password" | "mail.client_secret" => 16_384,
                _ => 65_536,
            };
            if entry.set.as_ref().is_some_and(|v| v.len() > max)
                || (entry.set.is_some() && entry.remove == Some(true))
                || entry.value.is_some()
            {
                invalid.push(key.as_str());
            }
        } else if !SETTING_KEYS.contains(&key.as_str())
            || entry
                .value
                .as_ref()
                .is_none_or(|value| !setting_value_valid(key, value))
            || entry.set.is_some()
            || entry.remove.is_some()
        {
            invalid.push(key.as_str());
        }
    }
    if !invalid.is_empty() {
        return validation_failed(&invalid, &headers);
    }
    let now_time = services.clock.now();
    if body.settings.keys().any(|key| is_retention_key(key)) {
        let rows = match db::settings(&services.pool).await {
            Ok(rows) => rows,
            Err(error) => return failed(&error, &headers, "instance settings lookup failed"),
        };
        if body.settings.iter().any(|(key, entry)| {
            is_retention_key(key)
                && entry
                    .value
                    .as_ref()
                    .and_then(Value::as_u64)
                    .is_some_and(|days| shortens_retention(key, days, &rows, now_time))
        }) {
            requires_fresh_auth = true;
        }
    }
    if requires_fresh_auth {
        match fresh_auth(&services, &session).await {
            Ok(()) => {}
            Err(FreshAuthFailure::Stale) => {
                return error_response(
                    StatusCode::FORBIDDEN,
                    "FRESH_AUTH_REQUIRED",
                    "Fresh browser authentication is required.",
                    &headers,
                );
            }
            Err(FreshAuthFailure::MfaRequired) => {
                return error_response(
                    StatusCode::FORBIDDEN,
                    "MFA_REAUTH_REQUIRED",
                    "Fresh browser authentication with the required second factor is required.",
                    &headers,
                );
            }
            Err(FreshAuthFailure::Database) => return internal(&headers),
        }
    }
    let now = rfc3339(services.clock.now());
    let mut changes = Vec::new();
    let mut instance_id = None;
    for (key, mut entry) in std::mem::take(&mut body.settings) {
        let Some(key_code) = setting_code(&key) else {
            return validation_failed(&["settings"], &headers);
        };
        if SECRET_KEYS.contains(&key.as_str()) {
            if entry.set.as_deref().is_some_and(|v| v.is_empty()) {
                continue;
            }
            if entry.remove == Some(true) {
                changes.push(db::SettingMutation {
                    key,
                    key_code,
                    value: None,
                    secret: true,
                    baseline: None,
                });
                continue;
            }
            if let Some(mut plaintext) = entry.set.take() {
                let Some(key_bytes) = services.two_factor_keys.current.as_ref() else {
                    plaintext.zeroize();
                    return unavailable(&headers, "SECRET_KEY_UNAVAILABLE");
                };
                if instance_id.is_none() {
                    instance_id = match sqlx::query_scalar(
                        "SELECT operator_user_id FROM instance_setup WHERE id=true",
                    )
                    .fetch_optional(&services.pool)
                    .await
                    {
                        Ok(Some(id)) => Some(id),
                        Ok(None) => {
                            plaintext.zeroize();
                            return unavailable(&headers, "SETUP_UNAVAILABLE");
                        }
                        Err(error) => {
                            plaintext.zeroize();
                            return failed(
                                &error.into(),
                                &headers,
                                "instance identity lookup failed",
                            );
                        }
                    };
                }
                let Some(instance_id) = instance_id else {
                    plaintext.zeroize();
                    return unavailable(&headers, "SETUP_UNAVAILABLE");
                };
                let purpose = format!("instance-setting:{key}");
                let encrypted = match encrypt_secret_for(
                    key_bytes,
                    instance_id,
                    purpose.as_bytes(),
                    plaintext.as_bytes(),
                ) {
                    Ok(v) => v,
                    Err(_) => {
                        plaintext.zeroize();
                        return internal(&headers);
                    }
                };
                plaintext.zeroize();
                let value = json!({"ciphertext":base64::engine::general_purpose::STANDARD.encode(encrypted),"configured_at":now});
                changes.push(db::SettingMutation {
                    key,
                    key_code,
                    value: Some(value),
                    secret: true,
                    baseline: None,
                });
            }
        } else if let Some(value) = entry.value.take() {
            let saved = json!({"value":value});
            let baseline = Some(default_setting_value(&key)).filter(|value| !value.is_null());
            changes.push(db::SettingMutation {
                key,
                key_code,
                value: Some(saved),
                secret: false,
                baseline,
            });
        }
    }
    let mut cleared = match db::update_settings_audited(
        &services.pool,
        session.user_id,
        &crate::audit::ctx(&headers, Some(session.user_id)),
        &changes,
        now_time,
    )
    .await
    {
        Ok(cleared) => cleared,
        Err(error) => return failed(&error, &headers, "instance settings transaction failed"),
    };
    cleared.sort();
    let refresh_mail =
        !cleared.is_empty() || changes.iter().any(|change| change.key.starts_with("mail."));
    if refresh_mail
        && let Some(notify) = &services.instance_settings_changed
        && !notify().await
    {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "MAIL_CONFIGURATION_INVALID",
            "The saved mail settings could not be activated; delivery is disabled.",
            &headers,
        );
    }
    let mut response = get_settings_value(&services, &headers).await;
    if let Ok(body) = &mut response {
        body["credentials_cleared"] = json!(cleared);
    }
    match response {
        Ok(body) => Json(body).into_response(),
        Err(response) => *response,
    }
}

pub(crate) async fn test_email(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
) -> Response {
    let services = match operator(&state, &session, &headers, "instance_settings.email_test").await
    {
        Ok(s) => s,
        Err(r) => return *r,
    };
    if let Err(response) = services.rate_limit_admin(session.user_id, &headers) {
        return *response;
    }
    if !services.email_delivery_enabled() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "EMAIL_DELIVERY_UNAVAILABLE",
            "Email delivery is not configured.",
            &headers,
        );
    }
    crate::email::test_email(State(state), headers, session).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_validation_is_typed_and_bounded() {
        assert!(setting_value_valid(
            "mail.transport",
            &json!("smtp_oauth_google")
        ));
        assert!(!setting_value_valid(
            "mail.transport",
            &json!("smtp+arbitrary")
        ));
        assert!(setting_value_valid("mail.port", &json!(65535)));
        assert!(!setting_value_valid("mail.port", &json!(0)));
        assert!(setting_value_valid("retention.audit_days", &json!(30)));
        assert!(!setting_value_valid("retention.audit_days", &json!(29)));
        assert!(!setting_value_valid(
            "mail.host",
            &json!("mail.example\nforged")
        ));
        assert!(setting_value_valid("mail.host", &json!("Mail.Example.COM")));
        assert!(!setting_value_valid("mail.host", &json!("a".repeat(254))));
        assert!(setting_value_valid("mail.from", &json!("a".repeat(320))));
        assert!(!setting_value_valid("mail.from", &json!("a".repeat(321))));
    }

    #[test]
    fn encrypted_settings_bind_to_stable_instance_and_key_name() {
        use craftcloud_auth::decrypt_secret_for;
        let key = [0x5a; 32];
        let instance = Uuid::from_u128(0x1234);
        let secret = b"synthetic secret";
        let ciphertext =
            encrypt_secret_for(&key, instance, b"instance-setting:mail.password", secret).unwrap();
        assert_eq!(
            decrypt_secret_for(
                &key,
                &ciphertext,
                instance,
                b"instance-setting:mail.password",
            )
            .unwrap(),
            secret
        );
        assert!(
            decrypt_secret_for(
                &key,
                &ciphertext,
                instance,
                b"instance-setting:mail.client_secret",
            )
            .is_err()
        );
        assert!(
            decrypt_secret_for(
                &key,
                &ciphertext,
                Uuid::from_u128(0x5678),
                b"instance-setting:mail.password"
            )
            .is_err()
        );
    }

    #[test]
    fn reenabled_factor_requires_a_new_mfa_session_marker() {
        let session = time::OffsetDateTime::from_unix_timestamp(1_000).unwrap();
        let verified = time::OffsetDateTime::from_unix_timestamp(1_010).unwrap();
        let enabled = time::OffsetDateTime::from_unix_timestamp(1_020).unwrap();
        let now = time::OffsetDateTime::from_unix_timestamp(1_030).unwrap();
        assert!(!fresh_mfa_timestamp(
            Some(verified),
            Some(enabled),
            session,
            now
        ));
        assert!(fresh_mfa_timestamp(Some(now), Some(enabled), session, now));
    }

    #[test]
    fn setup_checks_warn_for_low_storage_and_compare_host_without_origin() {
        assert!(low_storage(Some(0)));
        assert!(low_storage(Some(LOW_STORAGE_BYTES - 1)));
        assert!(!low_storage(Some(LOW_STORAGE_BYTES)));
        assert_eq!(
            public_origin_matches(Some("https://cloud.example"), None, Some("cloud.example")),
            Some(true)
        );
        assert_eq!(
            public_origin_matches(Some("https://cloud.example"), None, Some("other.example")),
            Some(false)
        );
    }

    #[test]
    fn https_check_rejects_http_and_untrusted_forwarded_headers() {
        assert!(https_verified(true, 1, Some("https")));
        assert!(!https_verified(true, 1, Some("http")));
        assert!(!https_verified(false, 1, Some("https")));
        assert!(!https_verified(true, 2, Some("https")));
    }

    #[test]
    fn postgres_check_warns_for_unsupported_server_versions() {
        assert_eq!(
            postgres_check(None),
            ("error", "setup.check.postgres_version.unavailable")
        );
        assert_eq!(
            postgres_check(Some(craftcloud_db::MIN_POSTGRES_MAJOR.saturating_sub(1))),
            ("warning", "setup.check.postgres_version.unsupported")
        );
        assert_eq!(
            postgres_check(Some(craftcloud_db::MIN_POSTGRES_MAJOR)),
            ("ok", "setup.check.postgres_version.ok")
        );
    }

    #[test]
    fn mail_transport_changes_require_fresh_auth() {
        assert!(setting_requires_fresh_auth("mail.host"));
        assert!(setting_requires_fresh_auth("mail.transport"));
        assert!(setting_requires_fresh_auth("mail.password"));
        assert!(!setting_requires_fresh_auth("retention.trash_days"));
    }

    #[tokio::test]
    async fn figcraft_probe_reports_live_and_unreachable_local_websocket_services() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = tungstenite::accept(stream);
        });
        assert_eq!(
            probe_figcraft(Some(format!("ws://{address}/"))).await,
            FigcraftProbe::Reachable
        );
        server.join().unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        assert_eq!(
            probe_figcraft(Some(format!("ws://{address}/"))).await,
            FigcraftProbe::Unreachable
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = std::io::Read::read(&mut stream, &mut request);
            let _ = std::io::Write::write_all(
                &mut stream,
                b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        });
        assert_eq!(
            probe_figcraft(Some(format!("ws://{address}/"))).await,
            FigcraftProbe::HandshakeFailed
        );
        server.join().unwrap();
    }
}
