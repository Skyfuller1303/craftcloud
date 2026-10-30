//! Session-bound file edit leases.
use crate::audit::{Change, commit_audited};
use crate::orgs::{db_failure, not_found, parse_id};
use crate::responses::{rejection_response, unavailable, validation_failed};
use crate::workspaces::lock;
use crate::{
    ApiState,
    projects::{self, Resolved},
    session::AuthenticatedSession,
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
        &resolved.services.pool,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        &headers,
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
            Action::FileLockAcquire,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "file grant lookup failed"),
    };
    if can(&opened.actor, Action::FileLockAcquire, &resource)
        .permit()
        .is_none()
    {
        return not_found(&headers);
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
                kind: LockSessionKind::Browser,
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
        &resolved.services.pool,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        &headers,
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
    if can(&opened.actor, Action::FileLockRelease, &resource)
        .permit()
        .is_none()
    {
        return not_found(&headers);
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
        && can(&opened.actor, Action::FileLockForce, &resource)
            .permit()
            .is_none()
    {
        return not_found(&headers);
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
                kind: LockSessionKind::Browser,
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
