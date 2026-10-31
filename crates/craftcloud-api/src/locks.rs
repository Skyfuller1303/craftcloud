//! Session-bound file edit leases.
use crate::audit::{Change, commit_audited};
use crate::orgs::{Gate, db_failure, gate, not_found, parse_id};
use crate::pagination::{PageQuery, decode_time_cursor, encode_time_cursor};
use crate::responses::{rejection_response, unavailable, validation_failed};
use crate::workspaces::lock;
use crate::{
    ApiState,
    projects::{self, Resolved},
    session::{AuthenticatedSession, CredentialKind},
};
use axum::http::StatusCode;
use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use craftcloud_core::policy::{Action, can};
use craftcloud_db::locks::{self, AcquireOutcome, LockSessionContext, LockSessionKind};
use craftcloud_db::members::OrgTx;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OrganizationLockQuery {
    limit: Option<String>,
    cursor: Option<String>,
    workspace_id: Option<String>,
    user_id: Option<String>,
}

pub(crate) async fn list_organization(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(org_raw): Path<String>,
    query: Result<
        axum::extract::Query<OrganizationLockQuery>,
        axum::extract::rejection::QueryRejection,
    >,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let Some(org_id) = parse_id(&org_raw) else {
        return not_found(&headers);
    };
    let Ok(axum::extract::Query(query)) = query else {
        return validation_failed(&["query"], &headers);
    };
    let workspace_filter = match query.workspace_id.as_deref().map(parse_id) {
        Some(Some(id)) => Some(id),
        Some(None) => return validation_failed(&["workspace_id"], &headers),
        None => None,
    };
    let user_filter = match query.user_id.as_deref().map(parse_id) {
        Some(Some(id)) => Some(id),
        Some(None) => return validation_failed(&["user_id"], &headers),
        None => None,
    };
    let page = PageQuery {
        limit: query.limit,
        cursor: query.cursor,
    };
    let limit = match page.limit() {
        Ok(v) => v,
        Err(e) => return crate::orgs::page_error(e, &headers),
    };
    let (_scope, workspace_ids) = match crate::grants::organization_listing_scope(
        &services,
        session.user_id,
        org_id,
        workspace_filter,
        Action::OrganizationLockList,
        "organization.locks.list",
        &headers,
    )
    .await
    {
        Ok(v) => v,
        Err(response) => return *response,
    };
    let cursor_scope = format!(
        "{org_id}:{}:{}",
        workspace_filter.map_or_else(|| "*".to_owned(), |id| id.to_string()),
        user_filter.map_or_else(|| "*".to_owned(), |id| id.to_string())
    );
    let after = match page
        .cursor()
        .map(|cursor| decode_time_cursor(cursor, "organization-locks", &cursor_scope))
    {
        Some(Ok(key)) => Some(key),
        Some(Err(e)) => return crate::orgs::page_error(e, &headers),
        None => None,
    };
    let mut rows = match locks::list_organization(
        &services.pool,
        org_id,
        &workspace_ids,
        workspace_filter,
        user_filter,
        limit + 1,
        after,
    )
    .await
    {
        Ok(rows) => rows,
        Err(error) => return db_failure(&error, &headers, "organization locks listing failed"),
    };
    let next = if rows.len() > limit as usize {
        rows.truncate(limit as usize);
        rows.last().map(|row| {
            encode_time_cursor("organization-locks", &cursor_scope, row.since, row.file_id)
        })
    } else {
        None
    };
    let items: Vec<_> = rows.iter().map(|row| json!({"file_id":row.file_id.to_string(),"file_name":row.file_name,"workspace_id":row.workspace_id.to_string(),"holder_user_id":row.holder_user_id.to_string(),"holder_display_name":row.holder_display_name,"since":crate::orgs::rfc3339(row.since),"expires_at":crate::orgs::rfc3339(row.expires_at),"session_kind":row.session_kind})).collect();
    Json(json!({"items":items,"next_cursor":next})).into_response()
}
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcquireRequest {
    client_id: String,
    #[serde(default)]
    takeover: bool,
}
#[derive(Deserialize, Default)]
pub(crate) struct LockQuery {
    force: Option<bool>,
}

fn session_kind(session: &AuthenticatedSession) -> LockSessionKind {
    match session.credential_kind {
        CredentialKind::Cookie => LockSessionKind::Browser,
        CredentialKind::Device => LockSessionKind::Device,
    }
}

fn lock_json(lock: &locks::FileLock, user_id: Uuid, session_id: Uuid) -> serde_json::Value {
    json!({
        "holder_display_name": lock.display_name,
        "since": crate::orgs::rfc3339(lock.acquired_at),
        "expires_at": crate::orgs::rfc3339(lock.expires_at),
        "mine": lock.holder_user_id == user_id,
        "held_by_current_session": lock.holder_session_id == session_id && lock.holder_user_id == user_id
    })
}

async fn unsupported(tx: &mut OrgTx, file_id: Uuid) -> Result<bool, craftcloud_db::DbError> {
    Ok(tx.file_app(file_id).await?.as_deref() == Some("figcraft"))
}

async fn resolve(
    state: &ApiState,
    headers: &HeaderMap,
    raw: &str,
) -> Result<Resolved, Box<Response>> {
    let Some(services) = state.services.clone() else {
        return Err(Box::new(unavailable(headers, "AUTH_UNAVAILABLE")));
    };
    let Some(id) = parse_id(raw) else {
        return Err(Box::new(not_found(headers)));
    };
    match craftcloud_db::files::locate(&services.pool, id).await {
        Ok(Some(row)) => Ok(Resolved {
            services,
            org_id: row.org_id,
            workspace_id: row.workspace_id,
            id,
        }),
        Ok(None) => Err(Box::new(not_found(headers))),
        Err(error) => Err(Box::new(db_failure(&error, headers, "file lookup failed"))),
    }
}

pub(crate) async fn acquire(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(raw): Path<String>,
    body: Result<Json<AcquireRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(v)) => v,
        Err(e) => return rejection_response(&e, &headers),
    };
    if request.client_id.is_empty()
        || request.client_id.len() > 32
        || !request
            .client_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return validation_failed(&["client_id"], &headers);
    }
    let resolved = match resolve(&state, &headers, &raw).await {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let mut opened = match lock(
        &resolved.services,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        &headers,
        session.app_id.as_deref(),
    )
    .await
    {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if let Err(e) = opened
        .restrict_tool_session(session.user_id, &session.client_kind)
        .await
    {
        return db_failure(&e, &headers, "device client lookup failed");
    }
    let resource = match opened
        .tx
        .policy_resource(
            session.user_id,
            resolved.workspace_id,
            resolved.id,
            Action::FileLockAcquire,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "file grant lookup failed"),
    };
    if let Gate::Closed(r) = gate(
        can(&opened.actor, Action::FileLockAcquire, &resource),
        &headers,
        session.user_id,
        "file.locked",
        Some(resolved.org_id),
        Some(resolved.id),
    ) {
        return r;
    }
    match unsupported(&mut opened.tx, resolved.id).await {
        Ok(true) => {
            return crate::error_response(
                StatusCode::CONFLICT,
                "LOCK_NOT_SUPPORTED",
                "This file type does not support editing leases.",
                &headers,
            );
        }
        Ok(false) => {}
        Err(e) => return db_failure(&e, &headers, "file metadata lookup failed"),
    }
    let outcome = match opened
        .tx
        .lock_acquire(
            resolved.id,
            LockSessionContext {
                user_id: session.user_id,
                session_id: session.session_id,
                expires_at: session.expires_at,
                idle_timeout_secs: resolved.services.settings.idle_timeout.as_secs() as i64,
                kind: session_kind(&session),
            },
            &request.client_id,
            request.takeover,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "file lock acquire failed"),
    };
    let audit_action = match &outcome {
        AcquireOutcome::Acquired {
            replaced_session: true,
            ..
        } => Some("file.lock_taken_over"),
        AcquireOutcome::Acquired { created: true, .. } => Some("file.locked"),
        _ => None,
    };
    if let Some(action) = audit_action {
        if let Err(e) = commit_audited(
            opened.tx,
            Change::new(
                &headers,
                session.user_id,
                action,
                resolved.org_id,
                Some(resolved.id),
            )
            .workspace(resolved.workspace_id),
        )
        .await
        {
            return db_failure(&e, &headers, "file lock acquire failed");
        }
    } else if let Err(e) = opened.tx.commit().await {
        return db_failure(&e, &headers, "file lock acquire failed");
    }
    match outcome {
        AcquireOutcome::Acquired { current, .. } => {
            Json(lock_json(&current, session.user_id, session.session_id)).into_response()
        }
        AcquireOutcome::Locked { current, .. } => {
            projects::locked_response(&current, session.session_id, session.user_id, &headers)
        }
        AcquireOutcome::FileDeleted => crate::orgs::conflict(
            "FILE_DELETED",
            "The file is in the trash; restore it first.",
            &headers,
        ),
        AcquireOutcome::ProjectDeleted => crate::orgs::conflict(
            "PROJECT_DELETED",
            "The project is in the trash; restore it first.",
            &headers,
        ),
        AcquireOutcome::SessionExpired => crate::error_response(
            axum::http::StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
            "Authentication is required.",
            &headers,
        ),
    }
}

pub(crate) async fn release(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(raw): Path<String>,
    query: Result<Query<LockQuery>, QueryRejection>,
) -> Response {
    let force = match query {
        Ok(Query(q)) => q.force.unwrap_or(false),
        Err(_) => return validation_failed(&["force"], &headers),
    };
    let resolved = match resolve(&state, &headers, &raw).await {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let mut opened = match lock(
        &resolved.services,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        &headers,
        session.app_id.as_deref(),
    )
    .await
    {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let resource = match opened
        .tx
        .policy_resource(
            session.user_id,
            resolved.workspace_id,
            resolved.id,
            Action::FileLockRelease,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "file grant lookup failed"),
    };
    if let Gate::Closed(r) = gate(
        can(&opened.actor, Action::FileLockRelease, &resource),
        &headers,
        session.user_id,
        "file.unlocked",
        Some(resolved.org_id),
        Some(resolved.id),
    ) {
        return r;
    }
    match unsupported(&mut opened.tx, resolved.id).await {
        Ok(true) => {
            return crate::error_response(
                StatusCode::CONFLICT,
                "LOCK_NOT_SUPPORTED",
                "This file type does not support editing leases.",
                &headers,
            );
        }
        Ok(false) => {}
        Err(e) => return db_failure(&e, &headers, "file metadata lookup failed"),
    }
    if force
        && let Gate::Closed(r) = gate(
            can(&opened.actor, Action::FileLockForce, &resource),
            &headers,
            session.user_id,
            "file.lock_forced",
            Some(resolved.org_id),
            Some(resolved.id),
        )
    {
        return r;
    }
    let released = match opened
        .tx
        .lock_release(
            resolved.id,
            LockSessionContext {
                user_id: session.user_id,
                session_id: session.session_id,
                expires_at: session.expires_at,
                idle_timeout_secs: resolved.services.settings.idle_timeout.as_secs() as i64,
                kind: session_kind(&session),
            },
            force,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "file lock release failed"),
    };
    if released {
        let action = if force {
            "file.lock_forced"
        } else {
            "file.unlocked"
        };
        if let Err(e) = commit_audited(
            opened.tx,
            Change::new(
                &headers,
                session.user_id,
                action,
                resolved.org_id,
                Some(resolved.id),
            )
            .workspace(resolved.workspace_id),
        )
        .await
        {
            return db_failure(&e, &headers, "file lock release failed");
        }
    } else if let Err(e) = opened.tx.commit().await {
        return db_failure(&e, &headers, "file lock release failed");
    }
    if released {
        axum::http::StatusCode::NO_CONTENT.into_response()
    } else {
        not_found(&headers)
    }
}
