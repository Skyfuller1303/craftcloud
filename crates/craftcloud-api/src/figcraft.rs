//! FigCraft live publishing, personal capabilities, snapshots and revocation worker.

use std::{sync::Arc, time::Duration};

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use craftcloud_auth::{decrypt_secret_for, encrypt_secret_for};
use craftcloud_core::policy::{Action, Decision, can};
use craftcloud_db::{
    OrgScope, figcraft as db,
    uploads::{BeginOutcome, NewUpload, StoreOutcome, UploadCreateOutcome},
    versions::{self, CommitOutcome, DownloadTarget, Precondition},
};
use craftcloud_figcraft::Access;
use craftcloud_storage::{ObjectKey, ObjectStore, ReadSource, WriteSink};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, pool::PoolConnection};
use tokio::sync::watch;
use uuid::Uuid;

use crate::files::resolve_file;
use crate::orgs::{db_failure, not_found, rfc3339};
use crate::projects::{Resolved, policy_resource, read_gate};
use crate::responses::{internal, unavailable, validation_failed};
use crate::session::{AuthenticatedSession, CredentialKind};
use crate::workspaces::{lock, read_actor};
use crate::{ApiState, Services, error_response};

const NATIVE_MAX_BYTES: u64 = 64 * 1024 * 1024;
const LINK_PURPOSE: &[u8] = b"figcraft-live-link-v1";

struct FileGuard {
    connection: Option<PoolConnection<Postgres>>,
    file_id: Uuid,
}

impl FileGuard {
    async fn acquire(pool: &PgPool, file_id: Uuid) -> Result<Self, ()> {
        let mut connection = tokio::time::timeout(Duration::from_secs(5), pool.acquire())
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        // Advisory locks are session scoped. Never return this connection to
        // the pool after the guard is dropped, even if async unlock cannot run.
        connection.close_on_drop();
        let locked: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1,0))")
                .bind(file_id.to_string())
                .fetch_one(&mut *connection)
                .await
                .map_err(|_| ())?;
        if !locked {
            return Err(());
        }
        Ok(Self {
            connection: Some(connection),
            file_id,
        })
    }
}

impl Drop for FileGuard {
    fn drop(&mut self) {
        if let Some(mut connection) = self.connection.take() {
            let key = self.file_id.to_string();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = sqlx::query("SELECT pg_advisory_unlock(hashtextextended($1,0))")
                        .bind(key)
                        .execute(&mut *connection)
                        .await;
                });
            }
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct OpenRequest {
    #[serde(default)]
    renew: bool,
}

#[allow(clippy::result_large_err)]
async fn ready(services: &Arc<Services>, headers: &HeaderMap) -> Result<(), Response> {
    if services.figcraft_available() {
        Ok(())
    } else {
        Err(unavailable(headers, "FIGCRAFT_LIVE_UNAVAILABLE"))
    }
}

#[allow(clippy::result_large_err)]
async fn source_file(
    resolved: &Resolved,
    scope: &craftcloud_db::OrgScope,
    headers: &HeaderMap,
    version: Option<Uuid>,
) -> Result<(Vec<u8>, String, Uuid), Response> {
    let target = versions::download_target(&resolved.services.pool, scope, resolved.id, version)
        .await
        .map_err(|e| db_failure(&e, headers, "FigCraft source lookup failed"))?;
    let DownloadTarget::Ready(info) = target else {
        return Err(not_found(headers));
    };
    if info.size_bytes < 0 || info.size_bytes as u64 > NATIVE_MAX_BYTES {
        return Err(error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "FILE_TOO_LARGE",
            "FigCraft documents may not exceed 64 MiB.",
            headers,
        ));
    }
    let Some(size) = u64::try_from(info.size_bytes).ok() else {
        return Err(internal(headers));
    };
    let mut bytes = Vec::with_capacity(info.size_bytes as usize);
    let mut digest = Sha256::new();
    if info.storage_tier == "archive" {
        let (Some(archive), Some(crypto)) = (
            resolved.services.archive_store.as_deref(),
            resolved.services.archive_crypto.as_deref(),
        ) else {
            return Err(crate::versions::archive_unavailable(headers));
        };
        let (Some(raw_key), Some(raw_envelope)) = (
            info.archive_object_key.as_deref(),
            info.archive_wrapped_key.as_deref(),
        ) else {
            let _ = versions::mark_blob_corrupt(&resolved.services.pool, info.blob_id).await;
            return Err(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BLOB_CORRUPT",
                "The stored document failed integrity verification.",
                headers,
            ));
        };
        let (Ok(key), Ok(envelope)) = (
            ObjectKey::parse(raw_key),
            craftcloud_storage::WrappedDataKey::from_bytes(raw_envelope),
        ) else {
            let _ = versions::mark_blob_corrupt(&resolved.services.pool, info.blob_id).await;
            return Err(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BLOB_CORRUPT",
                "The stored document failed integrity verification.",
                headers,
            ));
        };
        let mut source = match crate::archive_transport::open_archive_range(
            archive,
            crypto,
            &key,
            &envelope,
            info.blob_id,
            size,
            None,
        )
        .await
        {
            Ok(source) => source,
            Err(crate::archive_transport::ArchiveTransportError::Storage(_)) => {
                return Err(crate::versions::archive_unavailable(headers));
            }
            Err(_) => {
                let _ = versions::mark_blob_corrupt(&resolved.services.pool, info.blob_id).await;
                return Err(error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "BLOB_CORRUPT",
                    "The stored document failed integrity verification.",
                    headers,
                ));
            }
        };
        if source.remaining() != size {
            let _ = versions::mark_blob_corrupt(&resolved.services.pool, info.blob_id).await;
            return Err(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BLOB_CORRUPT",
                "The stored document failed integrity verification.",
                headers,
            ));
        }
        loop {
            let Some(chunk) = (match source.next_chunk().await {
                Ok(chunk) => chunk,
                Err(crate::archive_transport::ArchiveTransportError::Storage(_)) => {
                    return Err(crate::versions::archive_unavailable(headers));
                }
                Err(_) => {
                    let _ =
                        versions::mark_blob_corrupt(&resolved.services.pool, info.blob_id).await;
                    return Err(error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "BLOB_CORRUPT",
                        "The stored document failed integrity verification.",
                        headers,
                    ));
                }
            }) else {
                break;
            };
            if bytes.len().saturating_add(chunk.len()) as u64 > NATIVE_MAX_BYTES {
                return Err(error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "FILE_TOO_LARGE",
                    "FigCraft documents may not exceed 64 MiB.",
                    headers,
                ));
            }
            digest.update(&chunk);
            bytes.extend_from_slice(&chunk);
        }
    } else {
        let Some(store) = &resolved.services.storage else {
            return Err(unavailable(headers, "STORAGE_UNAVAILABLE"));
        };
        let key = ObjectKey::parse(&info.object_key).map_err(|_| internal(headers))?;
        let mut source = store
            .open_read(&key, None)
            .await
            .map_err(|_| unavailable(headers, "STORAGE_UNAVAILABLE"))?;
        while let Some(chunk) = source
            .next_chunk()
            .await
            .map_err(|_| unavailable(headers, "STORAGE_UNAVAILABLE"))?
        {
            if bytes.len().saturating_add(chunk.len()) as u64 > NATIVE_MAX_BYTES {
                return Err(error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "FILE_TOO_LARGE",
                    "FigCraft documents may not exceed 64 MiB.",
                    headers,
                ));
            }
            digest.update(&chunk);
            bytes.extend_from_slice(&chunk);
        }
    }
    let hash = digest.finalize();
    if bytes.len() as i64 != info.size_bytes || hash.as_slice() != info.sha256 {
        if info.storage_tier == "archive" {
            let _ = versions::mark_blob_corrupt(&resolved.services.pool, info.blob_id).await;
        }
        return Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "BLOB_CORRUPT",
            "The stored document failed integrity verification.",
            headers,
        ));
    }
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT name,app FROM files WHERE org_id=$1 AND id=$2 AND deleted_at IS NULL",
    )
    .bind(resolved.org_id)
    .bind(resolved.id)
    .fetch_optional(&resolved.services.pool)
    .await
    .map_err(|_| internal(headers))?;
    let Some((name, app)) = row else {
        return Err(not_found(headers));
    };
    if app != "figcraft" || !name.to_lowercase().ends_with(".figcraft") {
        return Err(error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "INVALID_FIGCRAFT_FILE",
            "Only native .figcraft files can be published.",
            headers,
        ));
    }
    Ok((bytes, name, info.version_id))
}

fn access_for(
    actor: &craftcloud_core::policy::Actor,
    resource: &craftcloud_core::policy::Resource,
) -> Option<Access> {
    let can_edit = can(actor, Action::VersionCommit, resource).is_allow();
    let can_view = can(actor, Action::FileRead, resource).is_allow();
    if can_edit {
        Some(Access::Edit)
    } else if can_view {
        Some(Access::View)
    } else {
        None
    }
}

fn purpose(file_id: Uuid, link_id: Uuid, access: &str) -> Vec<u8> {
    let mut value = Vec::with_capacity(LINK_PURPOSE.len() + 33 + access.len());
    value.extend_from_slice(LINK_PURPOSE);
    value.extend_from_slice(file_id.as_bytes());
    value.extend_from_slice(link_id.as_bytes());
    value.push(0);
    value.extend_from_slice(access.as_bytes());
    value
}

#[allow(clippy::result_large_err)]
async fn verify_existing_link(
    resolved: &Resolved,
    session: &AuthenticatedSession,
    link_id: Uuid,
    access: &str,
    headers: &HeaderMap,
) -> Result<(), Response> {
    let mut locked = lock(
        &resolved.services,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        headers,
        session.app_id.as_deref(),
    )
    .await
    .map_err(|e| *e)?;
    locked
        .restrict_tool_session(session.user_id, &session.client_kind)
        .await
        .map_err(|e| db_failure(&e, headers, "device client lookup failed"))?;
    let crate::workspaces::Locked { mut tx, actor } = locked;
    if !actor.may_use_live_links() {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "ACCESS_CHANGED",
            "Access changed before issuing the link.",
            headers,
        ));
    }
    let valid = tx
        .session_valid(
            session.session_id,
            session.user_id,
            resolved.services.clock.now(),
            session.credential_kind == CredentialKind::Device,
        )
        .await
        .map_err(|e| db_failure(&e, headers, "FigCraft session recheck failed"))?;
    if !valid {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "ACCESS_CHANGED",
            "Access changed before issuing the link.",
            headers,
        ));
    }
    let resource = tx
        .policy_resource(
            session.user_id,
            resolved.workspace_id,
            resolved.id,
            Action::FileRead,
        )
        .await
        .map_err(|e| db_failure(&e, headers, "FigCraft permission recheck failed"))?;
    let action = if access == "edit" {
        Action::VersionCommit
    } else {
        Action::FileRead
    };
    if !can(&actor, action, &resource).is_allow() {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "ACCESS_CHANGED",
            "Access changed before issuing the link.",
            headers,
        ));
    }
    if !tx
        .figcraft_link_active(link_id, resolved.id, session.user_id, access)
        .await
        .map_err(|e| db_failure(&e, headers, "FigCraft link recheck failed"))?
    {
        return Err(unavailable(headers, "FIGCRAFT_LIVE_UNAVAILABLE"));
    }
    tx.commit()
        .await
        .map_err(|e| db_failure(&e, headers, "FigCraft link recheck failed"))?;
    Ok(())
}

struct PendingLink<'a> {
    id: Uuid,
    access: &'a str,
    remote_file: &'a str,
    remote_link: &'a str,
    ciphertext: &'a [u8],
}

#[allow(clippy::result_large_err)]
async fn persist_link_with_current_authority(
    resolved: &Resolved,
    session: &AuthenticatedSession,
    link: PendingLink<'_>,
    headers: &HeaderMap,
) -> Result<(), Response> {
    let mut locked = lock(
        &resolved.services,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        headers,
        session.app_id.as_deref(),
    )
    .await
    .map_err(|e| *e)?;
    locked
        .restrict_tool_session(session.user_id, &session.client_kind)
        .await
        .map_err(|e| db_failure(&e, headers, "device client lookup failed"))?;
    let crate::workspaces::Locked { mut tx, actor } = locked;
    if !actor.may_use_live_links() {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "ACCESS_CHANGED",
            "Access changed before issuing the link.",
            headers,
        ));
    }
    let now = resolved.services.clock.now();
    let valid = tx
        .session_valid(
            session.session_id,
            session.user_id,
            now,
            session.credential_kind == CredentialKind::Device,
        )
        .await
        .map_err(|e| db_failure(&e, headers, "FigCraft session recheck failed"))?;
    if !valid {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "ACCESS_CHANGED",
            "Access changed before issuing the link.",
            headers,
        ));
    }
    let resource = tx
        .policy_resource(
            session.user_id,
            resolved.workspace_id,
            resolved.id,
            Action::FileRead,
        )
        .await
        .map_err(|e| db_failure(&e, headers, "FigCraft permission recheck failed"))?;
    let action = if link.access == "edit" {
        Action::VersionCommit
    } else {
        Action::FileRead
    };
    if !can(&actor, action, &resource).is_allow() {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "ACCESS_CHANGED",
            "Access changed before issuing the link.",
            headers,
        ));
    }
    let Some(file_permit) = can(&actor, Action::FileRead, &resource).permit() else {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "ACCESS_CHANGED",
            "Access changed before issuing the link.",
            headers,
        ));
    };
    let Some(scope) = OrgScope::from_permit(&file_permit) else {
        return Err(internal(headers));
    };
    let ctx = craftcloud_db::audit::AuditCtx {
        actor_id: Some(session.user_id),
        request_id: headers
            .get(crate::REQUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
    };
    let inserted = tx
        .figcraft_insert_link(
            &scope,
            db::NewFigcraftLink {
                id: link.id,
                file_id: resolved.id,
                user_id: session.user_id,
                access: link.access,
                remote_file: link.remote_file,
                remote_link: link.remote_link,
                ciphertext: link.ciphertext,
                now,
                workspace_id: resolved.workspace_id,
                source_kind: &session.client_kind,
                source_app: session.app_id.as_deref(),
            },
            &ctx,
        )
        .await
        .map_err(|e| db_failure(&e, headers, "FigCraft link persistence failed"))?;
    if !inserted {
        return Err(error_response(
            StatusCode::CONFLICT,
            "FIGCRAFT_BUSY",
            "FigCraft Live stopped before the link could be issued.",
            headers,
        ));
    }
    tx.commit()
        .await
        .map_err(|e| db_failure(&e, headers, "FigCraft link persistence failed"))?;
    Ok(())
}

fn pct(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/' | b':') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn join_url(public: &str, remote_file: &str, token: &str) -> String {
    format!(
        "figcraft://join?server={}&file={}&token={}",
        pct(public),
        pct(remote_file),
        pct(token)
    )
}

fn key(services: &Services) -> Option<[u8; 32]> {
    services.two_factor_keys.current
}

fn user_audit_ctx(
    session: &AuthenticatedSession,
    headers: &HeaderMap,
) -> craftcloud_db::audit::AuditCtx {
    craftcloud_db::audit::AuditCtx {
        actor_id: Some(session.user_id),
        request_id: headers
            .get(crate::REQUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
    }
}

pub(crate) async fn status(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(raw): Path<String>,
) -> Response {
    let resolved = match resolve_file(&state, &headers, &raw).await {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let scope = match read_gate(
        &resolved,
        &session,
        &headers,
        Action::FileRead,
        "figcraft.live.read",
        resolved.id,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let actor = match read_actor(
        &resolved.services,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        session.app_id.as_deref(),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "actor lookup failed"),
    };
    let resource = match policy_resource(
        &resolved.services.pool,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        resolved.id,
        Action::FileRead,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "file permission lookup failed"),
    };
    let Some(access) = access_for(&actor, &resource) else {
        return not_found(&headers);
    };
    let live = match db::get(&resolved.services.pool, &scope, resolved.id).await {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "FigCraft status lookup failed"),
    };
    let (state_text, last_at) = live.map_or(("inactive", None), |l| {
        (
            match l.state.as_str() {
                "publishing" => "publishing",
                "live" => "active",
                "unpublishing" => "stopping",
                _ => "republish",
            },
            l.last_snapshot_at.map(rfc3339),
        )
    });
    let can_stop =
        craftcloud_core::policy::can(&actor, Action::FileLockForce, &resource).is_allow();
    Json(json!({"state":state_text,"access":if access==Access::Edit{"edit"}else{"view"},"can_snapshot":access==Access::Edit&&state_text=="active","can_stop":can_stop,"last_snapshot_at":last_at,"participants":Value::Null})).into_response()
}

/// Authorizes and rate limits a live operation before it may take a global job
/// permit, so callers without read access or over budget cannot occupy capacity.
/// The operation itself repeats the stricter checks.
pub(crate) async fn preauthorize(
    state: &ApiState,
    headers: &HeaderMap,
    session: &AuthenticatedSession,
    raw: &str,
    name: &str,
) -> Result<(), Box<Response>> {
    let resolved = resolve_file(state, headers, raw).await?;
    read_gate(
        &resolved,
        session,
        headers,
        Action::FileRead,
        name,
        resolved.id,
    )
    .await?;
    resolved
        .services
        .rate_limit_figcraft(session.user_id, headers)
}

pub(crate) async fn open(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(raw): Path<String>,
    body: Bytes,
) -> Response {
    let request = if body.is_empty() {
        OpenRequest::default()
    } else {
        match serde_json::from_slice::<OpenRequest>(&body) {
            Ok(v) => v,
            Err(_) => return validation_failed(&["renew"], &headers),
        }
    };
    let resolved = match resolve_file(&state, &headers, &raw).await {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let scope = match read_gate(
        &resolved,
        &session,
        &headers,
        Action::FileRead,
        "figcraft.live.open",
        resolved.id,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return *e,
    };
    if let Err(response) = ready(&resolved.services, &headers).await {
        return response;
    }
    let _file_guard = match FileGuard::acquire(&resolved.services.pool, resolved.id).await {
        Ok(v) => v,
        Err(()) => {
            return error_response(
                StatusCode::CONFLICT,
                "FIGCRAFT_BUSY",
                "Another FigCraft operation is in progress.",
                &headers,
            );
        }
    };
    let actor = match read_actor(
        &resolved.services,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        session.app_id.as_deref(),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "actor lookup failed"),
    };
    let mut actor = actor;
    actor.tool_access = match crate::workspaces::tool_access(
        &resolved.services,
        session.user_id,
        resolved.org_id,
        &session.client_kind,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "device client lookup failed"),
    };
    if !actor.may_use_live_links() {
        return error_response(
            StatusCode::FORBIDDEN,
            "APP_NOT_ASSIGNED",
            "This client may not use live links in this organization.",
            &headers,
        );
    }
    let resource = match policy_resource(
        &resolved.services.pool,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        resolved.id,
        Action::FileRead,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "file permission lookup failed"),
    };
    let Some(access) = access_for(&actor, &resource) else {
        return not_found(&headers);
    };
    let access_text = if access == Access::Edit {
        "edit"
    } else {
        "view"
    };
    let current_link = match db::live_link(
        &resolved.services.pool,
        resolved.id,
        session.user_id,
        access_text,
        &session.client_kind,
        session.app_id.as_deref(),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "FigCraft link lookup failed"),
    };
    if !request.renew
        && let Some(link) = current_link.as_ref()
    {
        if let Err(response) =
            verify_existing_link(&resolved, &session, link.id, &link.access, &headers).await
        {
            return response;
        }
        let Some(key) = key(&resolved.services) else {
            return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE");
        };
        let cipher = &link.token_ciphertext;
        let decrypted = decrypt_secret_for(
            &key,
            cipher,
            session.user_id,
            &purpose(resolved.id, link.id, &link.access),
        );
        let (token, old_key) = match decrypted {
            Ok(token) => (token, false),
            Err(_) => match resolved
                .services
                .two_factor_keys
                .old
                .as_ref()
                .and_then(|old| {
                    decrypt_secret_for(
                        old,
                        cipher,
                        session.user_id,
                        &purpose(resolved.id, link.id, &link.access),
                    )
                    .ok()
                }) {
                Some(token) => (token, true),
                None => return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE"),
            },
        };
        if old_key {
            let Ok(reencrypted) = encrypt_secret_for(
                &key,
                session.user_id,
                &purpose(resolved.id, link.id, &link.access),
                &token,
            ) else {
                return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE");
            };
            if db::update_link_ciphertext(&resolved.services.pool, link.id, &reencrypted)
                .await
                .is_err()
            {
                return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE");
            }
        }
        let token = String::from_utf8_lossy(&token).into_owned();
        let current = match db::get(&resolved.services.pool, &scope, resolved.id).await {
            Ok(Some(v)) if v.state == "live" => v,
            Ok(_) => return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE"),
            Err(e) => return db_failure(&e, &headers, "FigCraft status lookup failed"),
        };
        let Some(remote) = current.figcraft_file_id.as_deref() else {
            return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE");
        };
        let Some(public) = resolved.services.figcraft_public_url.as_deref() else {
            return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE");
        };
        return Json(json!({"join_url":join_url(public,remote,&token),"access":access_text}))
            .into_response();
    }
    let link_id = Uuid::new_v4();
    let live = match db::get(&resolved.services.pool, &scope, resolved.id).await {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "FigCraft status lookup failed"),
    };
    if live
        .as_ref()
        .is_some_and(|l| l.state != "live" && l.state != "republish")
    {
        return error_response(
            StatusCode::CONFLICT,
            "FIGCRAFT_BUSY",
            "Another FigCraft operation is in progress.",
            &headers,
        );
    }
    let mut published_now = false;
    let remote_id = if let Some(live) = live.filter(|l| l.state == "live") {
        let Some(id) = live.figcraft_file_id else {
            return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE");
        };
        id
    } else {
        let (original, _name, version_id) =
            match source_file(&resolved, &scope, &headers, None).await {
                Ok(v) => v,
                Err(e) => return e,
            };
        let (mut tx, fresh_actor) = match lock(
            &resolved.services,
            session.user_id,
            resolved.org_id,
            resolved.workspace_id,
            &headers,
            session.app_id.as_deref(),
        )
        .await
        {
            Ok(v) => (v.tx, v.actor),
            Err(e) => return *e,
        };
        let valid = tx
            .session_valid(
                session.session_id,
                session.user_id,
                resolved.services.clock.now(),
                session.credential_kind == CredentialKind::Device,
            )
            .await;
        if !matches!(valid, Ok(true)) {
            return error_response(
                StatusCode::FORBIDDEN,
                "ACCESS_CHANGED",
                "Access changed before publishing.",
                &headers,
            );
        }
        let fresh_resource = match tx
            .policy_resource(
                session.user_id,
                resolved.workspace_id,
                resolved.id,
                Action::FileRead,
            )
            .await
        {
            Ok(v) => v,
            Err(e) => return db_failure(&e, &headers, "FigCraft publish permission lookup failed"),
        };
        let Some(fresh_access) = access_for(&fresh_actor, &fresh_resource) else {
            return not_found(&headers);
        };
        if fresh_access != access {
            return error_response(
                StatusCode::FORBIDDEN,
                "ACCESS_CHANGED",
                "Access changed before publishing.",
                &headers,
            );
        }
        let Some(permit) = can(&fresh_actor, Action::FileRead, &fresh_resource).permit() else {
            return not_found(&headers);
        };
        let Some(intent_scope) = OrgScope::from_permit(&permit) else {
            return internal(&headers);
        };
        let outcome = match tx
            .figcraft_begin_publish(
                &intent_scope,
                resolved.id,
                resolved.workspace_id,
                version_id,
                session.user_id,
                resolved.services.clock.now(),
            )
            .await
        {
            Ok(v) => v,
            Err(e) => return db_failure(&e, &headers, "FigCraft publish intent failed"),
        };
        match outcome {
            db::PublishIntentOutcome::Started => (),
            db::PublishIntentOutcome::HeadChanged => {
                return error_response(
                    StatusCode::CONFLICT,
                    "VERSION_CONFLICT",
                    "The file changed while FigCraft Live was starting.",
                    &headers,
                );
            }
            db::PublishIntentOutcome::Busy => {
                return error_response(
                    StatusCode::CONFLICT,
                    "FIGCRAFT_BUSY",
                    "Another FigCraft operation is in progress.",
                    &headers,
                );
            }
            db::PublishIntentOutcome::Gone => return not_found(&headers),
        }
        if let Err(e) = tx.commit().await {
            return db_failure(&e, &headers, "FigCraft publish intent failed");
        }
        let Some(backend) = resolved.services.figcraft.as_ref().map(Arc::clone) else {
            return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE");
        };
        let opaque_name = resolved.id.to_string();
        let result = resolved
            .services
            .run_figcraft(move || backend.publish(&original, &opaque_name))
            .await;
        let Some(Ok(id)) = result else {
            let _ = db::clear_publish_intent(
                &resolved.services.pool,
                resolved.id,
                resolved.services.clock.now(),
            )
            .await;
            return unavailable(&headers, "FIGCRAFT_UNAVAILABLE");
        };
        // Re-check permissions after external work before making the remote id visible.
        if read_gate(
            &resolved,
            &session,
            &headers,
            Action::FileRead,
            "figcraft.live.publish.recheck",
            resolved.id,
        )
        .await
        .is_err()
        {
            let Some(backend) = resolved.services.figcraft.as_ref().map(Arc::clone) else {
                return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE");
            };
            let remove = id.clone();
            let _ = resolved
                .services
                .run_figcraft(move || backend.remove(&remove))
                .await;
            let _ = db::clear_publish_intent(
                &resolved.services.pool,
                resolved.id,
                resolved.services.clock.now(),
            )
            .await;
            return error_response(
                StatusCode::FORBIDDEN,
                "FORBIDDEN",
                "Access changed while publishing.",
                &headers,
            );
        }
        let publish_ctx = craftcloud_db::audit::AuditCtx {
            actor_id: Some(session.user_id),
            request_id: headers
                .get(crate::REQUEST_ID_HEADER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
        };
        if let Err(e) = db::mark_published_audited(
            &resolved.services.pool,
            resolved.id,
            &id,
            resolved.services.clock.now(),
            &publish_ctx,
        )
        .await
        {
            if let Some(backend) = resolved.services.figcraft.as_ref().map(Arc::clone) {
                let remove = id.clone();
                let _ = resolved
                    .services
                    .run_figcraft(move || backend.remove(&remove))
                    .await;
            }
            let _ = db::clear_publish_intent(
                &resolved.services.pool,
                resolved.id,
                resolved.services.clock.now(),
            )
            .await;
            return db_failure(&e, &headers, "FigCraft publish result persistence failed");
        }
        published_now = true;
        id
    };
    let Some(backend) = resolved.services.figcraft.as_ref().map(Arc::clone) else {
        return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE");
    };
    let file_for_call = remote_id.clone();
    let backend_for_create = Arc::clone(&backend);
    let created = resolved
        .services
        .run_figcraft(move || backend_for_create.create_link(&file_for_call, access))
        .await;
    let Some(Ok(link)) = created else {
        return unavailable(&headers, "FIGCRAFT_UNAVAILABLE");
    };
    let old_links = match db::links_for_source(
        &resolved.services.pool,
        resolved.id,
        session.user_id,
        access_text,
        &session.client_kind,
        session.app_id.as_deref(),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            let backend = Arc::clone(&backend);
            let file = remote_id.clone();
            let id = link.id.clone();
            let _ = resolved
                .services
                .run_figcraft(move || backend.revoke_link(&file, &id))
                .await;
            return db_failure(&e, &headers, "FigCraft link lookup failed");
        }
    };
    for old in old_links {
        if let Some(old_remote) = old.figcraft_link_id.clone() {
            let backend_revoke = Arc::clone(&backend);
            let file = remote_id.clone();
            if !matches!(
                resolved
                    .services
                    .run_figcraft(move || backend_revoke.revoke_link(&file, &old_remote))
                    .await,
                Some(Ok(()))
            ) {
                let backend = Arc::clone(&backend);
                let file = remote_id.clone();
                let id = link.id.clone();
                let _ = resolved
                    .services
                    .run_figcraft(move || backend.revoke_link(&file, &id))
                    .await;
                return unavailable(&headers, "FIGCRAFT_UNAVAILABLE");
            }
        }
        if db::revoke_link_audited(
            &resolved.services.pool,
            old.id,
            resolved.services.clock.now(),
            &user_audit_ctx(&session, &headers),
        )
        .await
        .is_err()
        {
            let backend = Arc::clone(&backend);
            let file = remote_id.clone();
            let id = link.id.clone();
            let _ = resolved
                .services
                .run_figcraft(move || backend.revoke_link(&file, &id))
                .await;
            return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE");
        }
    }
    let Some(key) = key(&resolved.services) else {
        return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE");
    };
    let cipher = match encrypt_secret_for(
        &key,
        session.user_id,
        &purpose(resolved.id, link_id, access_text),
        link.token.as_bytes(),
    ) {
        Ok(v) => v,
        Err(_) => return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE"),
    };
    if let Err(response) = persist_link_with_current_authority(
        &resolved,
        &session,
        PendingLink {
            id: link_id,
            access: access_text,
            remote_file: &remote_id,
            remote_link: &link.id,
            ciphertext: &cipher,
        },
        &headers,
    )
    .await
    {
        let file = remote_id.clone();
        let rid = link.id.clone();
        let backend_revoke = Arc::clone(&backend);
        let _ = resolved
            .services
            .run_figcraft(move || backend_revoke.revoke_link(&file, &rid))
            .await;
        if published_now
            && db::mark_unpublishing(
                &resolved.services.pool,
                resolved.id,
                resolved.services.clock.now(),
            )
            .await
            .is_err()
        {
            let file = remote_id.clone();
            let backend = Arc::clone(&backend);
            let _ = resolved
                .services
                .run_figcraft(move || backend.remove(&file))
                .await;
        }
        return response;
    }
    let Some(public) = resolved.services.figcraft_public_url.as_deref() else {
        return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE");
    };
    Json(json!({"join_url":join_url(public,&remote_id,&link.token),"access":access_text}))
        .into_response()
}

pub(crate) async fn snapshot(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(raw): Path<String>,
) -> Response {
    let resolved = match resolve_file(&state, &headers, &raw).await {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let scope = match read_gate(
        &resolved,
        &session,
        &headers,
        Action::VersionCommit,
        "figcraft.live.snapshot",
        resolved.id,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return *e,
    };
    if let Err(response) = ready(&resolved.services, &headers).await {
        return response;
    }
    let _file_guard = match FileGuard::acquire(&resolved.services.pool, resolved.id).await {
        Ok(v) => v,
        Err(()) => {
            return error_response(
                StatusCode::CONFLICT,
                "FIGCRAFT_BUSY",
                "Another FigCraft operation is in progress.",
                &headers,
            );
        }
    };
    let Some(live) = (match db::get(&resolved.services.pool, &scope, resolved.id).await {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "FigCraft state lookup failed"),
    }) else {
        return error_response(
            StatusCode::CONFLICT,
            "FIGCRAFT_NOT_LIVE",
            "Publish this file before taking a snapshot.",
            &headers,
        );
    };
    if live.state != "live" {
        return error_response(
            StatusCode::CONFLICT,
            "FIGCRAFT_BUSY",
            "FigCraft Live is changing state; retry shortly.",
            &headers,
        );
    }
    let (original, name, _published_version) = match source_file(
        &resolved,
        &scope,
        &headers,
        Some(live.published_from_version_id),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return e,
    };
    let current_version =
        match versions::download_target(&resolved.services.pool, &scope, resolved.id, None).await {
            Ok(DownloadTarget::Ready(v)) => v.version_id,
            Ok(_) => return not_found(&headers),
            Err(e) => return db_failure(&e, &headers, "FigCraft head lookup failed"),
        };
    let Some(remote) = live.figcraft_file_id.clone() else {
        return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE");
    };
    let Some(backend) = resolved.services.figcraft.as_ref().map(Arc::clone) else {
        return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE");
    };
    let remote_export = remote.clone();
    let export = resolved
        .services
        .run_figcraft(move || backend.export(&remote_export, &original))
        .await;
    let Some(Ok(export)) = export else {
        return unavailable(&headers, "FIGCRAFT_UNAVAILABLE");
    };
    if export.bytes.is_empty() || export.bytes.len() as u64 > NATIVE_MAX_BYTES {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "FILE_TOO_LARGE",
            "FigCraft snapshot exceeds 64 MiB.",
            &headers,
        );
    }
    let digest: [u8; 32] = Sha256::digest(&export.bytes).into();
    let now = resolved.services.clock.now();
    let (mut tx, actor) = match lock(
        &resolved.services,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        &headers,
        session.app_id.as_deref(),
    )
    .await
    {
        Ok(v) => (v.tx, v.actor),
        Err(e) => return *e,
    };
    let valid = tx
        .session_valid(
            session.session_id,
            session.user_id,
            now,
            session.credential_kind == CredentialKind::Device,
        )
        .await;
    if !matches!(valid, Ok(true)) {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    }
    let resource = match tx
        .policy_resource(
            session.user_id,
            resolved.workspace_id,
            resolved.id,
            Action::UploadCreate,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "file permissions changed"),
    };
    let Some(create_permit) = can(&actor, Action::UploadCreate, &resource).permit() else {
        return error_response(
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
            "Editing access is required to snapshot this file.",
            &headers,
        );
    };
    let Some(create_scope) = OrgScope::from_permit(&create_permit) else {
        return internal(&headers);
    };
    let size = match i64::try_from(export.bytes.len()) {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "FILE_TOO_LARGE",
                "FigCraft snapshot is too large.",
                &headers,
            );
        }
    };
    let ttl = time::Duration::try_from(resolved.services.settings.upload_ttl)
        .unwrap_or(time::Duration::DAY);
    let new = NewUpload {
        file_id: resolved.id,
        user_id: session.user_id,
        size,
        sha256: digest,
        idempotency_key: None,
        now,
        expires_at: now + ttl,
        max_per_user: resolved.services.settings.uploads_per_user,
        max_per_org: resolved.services.settings.uploads_per_org,
    };
    let upload = match tx.upload_create(&create_scope, &new).await {
        Ok(UploadCreateOutcome::Created(v)) => v,
        Ok(UploadCreateOutcome::QuotaExceeded) => {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "QUOTA_EXCEEDED",
                "The storage quota does not allow this snapshot.",
                &headers,
            );
        }
        Ok(UploadCreateOutcome::UserLimit | UploadCreateOutcome::OrgLimit) => {
            return error_response(
                StatusCode::CONFLICT,
                "UPLOAD_LIMIT",
                "Too many uploads are open.",
                &headers,
            );
        }
        Ok(_) => {
            return error_response(
                StatusCode::CONFLICT,
                "UPLOAD_CLOSED",
                "Could not create the snapshot upload.",
                &headers,
            );
        }
        Err(e) => return db_failure(&e, &headers, "snapshot upload reservation failed"),
    };
    if let Err(e) = tx.commit().await {
        return db_failure(&e, &headers, "snapshot upload reservation failed");
    }
    let Some(store) = resolved.services.storage.as_ref() else {
        return unavailable(&headers, "STORAGE_UNAVAILABLE");
    };
    let mut temp = match store.begin_write(export.bytes.len() as u64).await {
        Ok(v) => v,
        Err(_) => return unavailable(&headers, "STORAGE_UNAVAILABLE"),
    };
    if temp.write(&export.bytes).await.is_err() {
        store.abort(temp).await;
        return unavailable(&headers, "STORAGE_UNAVAILABLE");
    }
    let key = match store
        .finalize(temp, export.bytes.len() as u64, &digest)
        .await
    {
        Ok(v) => v,
        Err(_) => return unavailable(&headers, "STORAGE_UNAVAILABLE"),
    };
    let now = resolved.services.clock.now();
    let (mut tx, actor) = match lock(
        &resolved.services,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        &headers,
        session.app_id.as_deref(),
    )
    .await
    {
        Ok(v) => (v.tx, v.actor),
        Err(e) => {
            let _ = store.delete(&key).await;
            return *e;
        }
    };
    let valid = tx
        .session_valid(
            session.session_id,
            session.user_id,
            now,
            session.credential_kind == CredentialKind::Device,
        )
        .await;
    let resource = match tx
        .policy_resource(
            session.user_id,
            resolved.workspace_id,
            resolved.id,
            Action::VersionCommit,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => {
            let _ = store.delete(&key).await;
            return db_failure(&e, &headers, "snapshot permissions changed");
        }
    };
    let Some(write_permit) = can(&actor, Action::UploadWrite, &resource)
        .permit()
        .filter(|_| matches!(valid, Ok(true)))
    else {
        drop(tx);
        let _ = store.delete(&key).await;
        return error_response(
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
            "Editing access changed while creating the snapshot.",
            &headers,
        );
    };
    let Some(write_scope) = OrgScope::from_permit(&write_permit) else {
        let _ = store.delete(&key).await;
        return internal(&headers);
    };
    match tx
        .upload_begin_receiving(&write_scope, upload.id, now)
        .await
    {
        Ok(BeginOutcome::Started(_)) => (),
        Ok(_) => {
            let _ = store.delete(&key).await;
            return error_response(
                StatusCode::CONFLICT,
                "UPLOAD_CLOSED",
                "Snapshot upload is no longer available.",
                &headers,
            );
        }
        Err(e) => {
            let _ = store.delete(&key).await;
            return db_failure(&e, &headers, "snapshot upload start failed");
        }
    };
    match tx
        .upload_store(&write_scope, upload.id, key.as_str(), now)
        .await
    {
        Ok(StoreOutcome::Stored(_, _)) => (),
        Ok(_) => {
            drop(tx);
            let _ = store.delete(&key).await;
            return error_response(
                StatusCode::CONFLICT,
                "FILE_IS_LIVE",
                "Snapshot could not be stored.",
                &headers,
            );
        }
        Err(e) => {
            drop(tx);
            let _ = store.delete(&key).await;
            return db_failure(&e, &headers, "snapshot content storage failed");
        }
    };
    if let Err(e) = tx.commit().await {
        let _ = store.delete(&key).await;
        return db_failure(&e, &headers, "snapshot content storage failed");
    }
    let now = resolved.services.clock.now();
    let (mut tx, actor) = match lock(
        &resolved.services,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        &headers,
        session.app_id.as_deref(),
    )
    .await
    {
        Ok(v) => (v.tx, v.actor),
        Err(e) => return *e,
    };
    let valid = tx
        .session_valid(
            session.session_id,
            session.user_id,
            now,
            session.credential_kind == CredentialKind::Device,
        )
        .await;
    if !matches!(valid, Ok(true)) {
        return error_response(
            StatusCode::FORBIDDEN,
            "ACCESS_CHANGED",
            "Access changed before snapshot commit.",
            &headers,
        );
    }
    let resource = match tx
        .policy_resource(
            session.user_id,
            resolved.workspace_id,
            resolved.id,
            Action::VersionCommit,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "snapshot permission lookup failed"),
    };
    let Some(permit) = can(&actor, Action::VersionCommit, &resource).permit() else {
        return error_response(
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
            "Editing access changed before snapshot commit.",
            &headers,
        );
    };
    let Some(commit_scope) = OrgScope::from_permit(&permit) else {
        return internal(&headers);
    };
    let outcome = match tx
        .version_commit(
            &commit_scope,
            resolved.id,
            upload.id,
            session.user_id,
            Some(Precondition::Head(Some(current_version))),
            now,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "snapshot version commit failed"),
    };
    let mut version = match outcome {
        CommitOutcome::Committed(v) => v,
        CommitOutcome::PreconditionFailed { .. } => {
            return error_response(
                StatusCode::CONFLICT,
                "VERSION_CONFLICT",
                "The file changed while FigCraft Live was exporting.",
                &headers,
            );
        }
        _ => {
            return error_response(
                StatusCode::CONFLICT,
                "SNAPSHOT_FAILED",
                "The FigCraft snapshot could not be committed.",
                &headers,
            );
        }
    };
    if let Err(e) = tx.mark_figcraft_live_snapshot(version.id).await {
        return db_failure(&e, &headers, "snapshot source marking failed");
    }
    version.actor_kind = "figcraft_live".to_owned();
    if !matches!(
        tx.figcraft_record_snapshot(resolved.id, version.id, now, false)
            .await,
        Ok(true)
    ) {
        return error_response(
            StatusCode::CONFLICT,
            "FIGCRAFT_NOT_LIVE",
            "FigCraft Live stopped while the snapshot was being committed.",
            &headers,
        );
    }
    let mut event = craftcloud_db::audit::NewEvent::new("figcraft.snapshot")
        .org(resolved.org_id)
        .target("file", resolved.id)
        .workspace(resolved.workspace_id);
    event
        .details
        .push(("version_id", craftcloud_db::audit::Detail::Id(version.id)));
    event.details.push((
        "source",
        craftcloud_db::audit::Detail::Code("figcraft_live"),
    ));
    let ctx = craftcloud_db::audit::AuditCtx {
        actor_id: Some(session.user_id),
        request_id: headers
            .get(crate::REQUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
    };
    if let Err(e) = tx.audit(&ctx, event).await {
        return db_failure(&e, &headers, "snapshot audit insert failed");
    }
    if let Err(e) = tx.commit().await {
        return db_failure(&e, &headers, "snapshot version commit failed");
    }
    let _ = name;
    (
        StatusCode::CREATED,
        Json(crate::versions::version_json(&version)),
    )
        .into_response()
}

pub(crate) async fn stop(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(raw): Path<String>,
) -> Response {
    let resolved = match resolve_file(&state, &headers, &raw).await {
        Ok(v) => v,
        Err(e) => return *e,
    };
    let scope = match read_gate(
        &resolved,
        &session,
        &headers,
        Action::FileLockForce,
        "figcraft.live.stop",
        resolved.id,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return *e,
    };
    if let Err(response) = ready(&resolved.services, &headers).await {
        return response;
    }
    let Some(live) = (match db::get(&resolved.services.pool, &scope, resolved.id).await {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "FigCraft state lookup failed"),
    }) else {
        return (StatusCode::NO_CONTENT, ()).into_response();
    };
    if live.state == "republish" {
        return (StatusCode::NO_CONTENT, ()).into_response();
    }
    if live.state != "live" {
        return error_response(
            StatusCode::CONFLICT,
            "FIGCRAFT_BUSY",
            "Another FigCraft operation is in progress.",
            &headers,
        );
    }
    let _file_guard = match FileGuard::acquire(&resolved.services.pool, resolved.id).await {
        Ok(v) => v,
        Err(()) => {
            return error_response(
                StatusCode::CONFLICT,
                "FIGCRAFT_BUSY",
                "Another FigCraft operation is in progress.",
                &headers,
            );
        }
    };
    let (mut tx, actor) = match lock(
        &resolved.services,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        &headers,
        session.app_id.as_deref(),
    )
    .await
    {
        Ok(v) => (v.tx, v.actor),
        Err(e) => return *e,
    };
    let now = resolved.services.clock.now();
    let valid = tx
        .session_valid(
            session.session_id,
            session.user_id,
            now,
            session.credential_kind == CredentialKind::Device,
        )
        .await;
    if !matches!(valid, Ok(true)) {
        return error_response(
            StatusCode::FORBIDDEN,
            "ACCESS_CHANGED",
            "Access changed before stopping FigCraft Live.",
            &headers,
        );
    }
    let resource = match tx
        .policy_resource(
            session.user_id,
            resolved.workspace_id,
            resolved.id,
            Action::FileLockForce,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "FigCraft stop permission lookup failed"),
    };
    let Some(permit) = can(&actor, Action::FileLockForce, &resource).permit() else {
        return error_response(
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
            "You cannot stop FigCraft Live.",
            &headers,
        );
    };
    let Some(stop_scope) = OrgScope::from_permit(&permit) else {
        return internal(&headers);
    };
    match tx
        .figcraft_mark_unpublishing(&stop_scope, resolved.id, now)
        .await
    {
        Ok(true) => (),
        Ok(false) => {
            return error_response(
                StatusCode::CONFLICT,
                "FIGCRAFT_BUSY",
                "FigCraft Live changed before stopping.",
                &headers,
            );
        }
        Err(e) => return db_failure(&e, &headers, "FigCraft stop state update failed"),
    }
    if let Err(e) = tx.commit().await {
        return db_failure(&e, &headers, "FigCraft stop state update failed");
    }
    let Some(remote) = live.figcraft_file_id.clone() else {
        return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE");
    };
    let Some(backend) = resolved.services.figcraft.as_ref().map(Arc::clone) else {
        return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE");
    };
    let links = match db::links(&resolved.services.pool, resolved.id).await {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "FigCraft link lookup failed"),
    };
    for link in links {
        if let Some(remote_link) = link.figcraft_link_id.as_deref() {
            let file = remote.clone();
            let link_id = remote_link.to_owned();
            let backend = Arc::clone(&backend);
            if !matches!(
                resolved
                    .services
                    .run_figcraft(move || backend.revoke_link(&file, &link_id))
                    .await,
                Some(Ok(()))
            ) {
                return unavailable(&headers, "FIGCRAFT_UNAVAILABLE");
            }
        }
        if db::revoke_link_audited(
            &resolved.services.pool,
            link.id,
            resolved.services.clock.now(),
            &user_audit_ctx(&session, &headers),
        )
        .await
        .is_err()
        {
            return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE");
        }
    }
    drop(_file_guard);
    if let Err(response) = service_snapshot(Arc::clone(&resolved.services), resolved.id, true).await
    {
        return response;
    }
    if read_gate(
        &resolved,
        &session,
        &headers,
        Action::FileLockForce,
        "figcraft.live.stop.recheck",
        resolved.id,
    )
    .await
    .is_err()
    {
        return error_response(
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
            "Access changed while stopping FigCraft Live.",
            &headers,
        );
    }
    let _file_guard = match FileGuard::acquire(&resolved.services.pool, resolved.id).await {
        Ok(v) => v,
        Err(()) => {
            return error_response(
                StatusCode::CONFLICT,
                "FIGCRAFT_BUSY",
                "Another FigCraft operation is in progress.",
                &headers,
            );
        }
    };
    let file = remote.clone();
    if !matches!(
        resolved
            .services
            .run_figcraft(move || backend.remove(&file))
            .await,
        Some(Ok(()))
    ) {
        return unavailable(&headers, "FIGCRAFT_UNAVAILABLE");
    }
    let ctx = craftcloud_db::audit::AuditCtx {
        actor_id: Some(session.user_id),
        request_id: headers
            .get(crate::REQUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
    };
    if db::finish_unpublish_audited(
        &resolved.services.pool,
        resolved.id,
        resolved.services.clock.now(),
        &ctx,
    )
    .await
    .is_err()
    {
        return unavailable(&headers, "FIGCRAFT_LIVE_UNAVAILABLE");
    }
    (StatusCode::NO_CONTENT, ()).into_response()
}

/// Snapshot using authority derived from the locked live-file row. `force` is
/// used by stop so the final native state always becomes a version.
#[allow(clippy::result_large_err)]
async fn service_snapshot(
    services: Arc<Services>,
    file_id: Uuid,
    force: bool,
) -> Result<bool, Response> {
    let _guard = match FileGuard::acquire(&services.pool, file_id).await {
        Ok(v) => v,
        Err(()) => {
            return Err(error_response(
                StatusCode::CONFLICT,
                "FIGCRAFT_BUSY",
                "Another FigCraft operation is in progress.",
                &HeaderMap::new(),
            ));
        }
    };
    let live = db::get_any(&services.pool, file_id)
        .await
        .map_err(|e| db_failure(&e, &HeaderMap::new(), "FigCraft live lookup failed"))?
        .ok_or_else(|| not_found(&HeaderMap::new()))?;
    if !matches!(live.state.as_str(), "live" | "unpublishing") {
        return Err(error_response(
            StatusCode::CONFLICT,
            "FIGCRAFT_BUSY",
            "FigCraft Live is changing state; retry shortly.",
            &HeaderMap::new(),
        ));
    }
    let resolved = Resolved {
        services: Arc::clone(&services),
        org_id: live.org_id,
        workspace_id: live.workspace_id,
        id: file_id,
    };
    let mut tx = craftcloud_db::members::OrgTx::begin(&services.pool, live.org_id)
        .await
        .map_err(|e| {
            db_failure(
                &e,
                &HeaderMap::new(),
                "FigCraft snapshot transaction failed",
            )
        })?
        .ok_or_else(|| not_found(&HeaderMap::new()))?;
    let read_scope = tx
        .figcraft_service_scope(file_id, Action::FileRead)
        .await
        .map_err(|e| {
            db_failure(
                &e,
                &HeaderMap::new(),
                "FigCraft snapshot authority lookup failed",
            )
        })?
        .ok_or_else(|| {
            error_response(
                StatusCode::FORBIDDEN,
                "FORBIDDEN",
                "FigCraft snapshot authority is no longer valid.",
                &HeaderMap::new(),
            )
        })?;
    tx.commit().await.map_err(|e| {
        db_failure(
            &e,
            &HeaderMap::new(),
            "FigCraft snapshot authority commit failed",
        )
    })?;
    let (original, _name, _) = source_file(
        &resolved,
        &read_scope,
        &HeaderMap::new(),
        Some(live.published_from_version_id),
    )
    .await?;
    let current_info = match versions::download_target(&services.pool, &read_scope, file_id, None)
        .await
        .map_err(|e| db_failure(&e, &HeaderMap::new(), "FigCraft head lookup failed"))?
    {
        DownloadTarget::Ready(v) => v,
        _ => return Err(not_found(&HeaderMap::new())),
    };
    let current_version = current_info.version_id;
    let Some(remote) = live.figcraft_file_id.clone() else {
        return Err(unavailable(&HeaderMap::new(), "FIGCRAFT_LIVE_UNAVAILABLE"));
    };
    let Some(backend) = services.figcraft.as_ref().map(Arc::clone) else {
        return Err(unavailable(&HeaderMap::new(), "FIGCRAFT_LIVE_UNAVAILABLE"));
    };
    let original_export = original.clone();
    let export = services
        .run_figcraft(move || backend.export(&remote, &original_export))
        .await
        .and_then(Result::ok)
        .ok_or_else(|| unavailable(&HeaderMap::new(), "FIGCRAFT_UNAVAILABLE"))?;
    if export.bytes.is_empty() || export.bytes.len() as u64 > NATIVE_MAX_BYTES {
        return Err(error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "FILE_TOO_LARGE",
            "FigCraft snapshot exceeds 64 MiB.",
            &HeaderMap::new(),
        ));
    }
    let now = services.clock.now();
    let export_digest: [u8; 32] = Sha256::digest(&export.bytes).into();
    if !force
        && current_info.size_bytes == export.bytes.len() as i64
        && current_info.sha256.as_slice() == export_digest
    {
        db::mark_snapshot_unchanged(&services.pool, file_id, current_version, now)
            .await
            .map_err(|e| {
                db_failure(
                    &e,
                    &HeaderMap::new(),
                    "FigCraft snapshot timestamp update failed",
                )
            })?;
        return Ok(false);
    }
    let digest: [u8; 32] = Sha256::digest(&export.bytes).into();
    let mut tx = craftcloud_db::members::OrgTx::begin(&services.pool, live.org_id)
        .await
        .map_err(|e| db_failure(&e, &HeaderMap::new(), "FigCraft upload reservation failed"))?
        .ok_or_else(|| not_found(&HeaderMap::new()))?;
    let create_scope = tx
        .figcraft_service_scope(file_id, Action::UploadCreate)
        .await
        .map_err(|e| {
            db_failure(
                &e,
                &HeaderMap::new(),
                "FigCraft upload authority lookup failed",
            )
        })?
        .ok_or_else(|| {
            error_response(
                StatusCode::FORBIDDEN,
                "FORBIDDEN",
                "FigCraft snapshot authority is no longer valid.",
                &HeaderMap::new(),
            )
        })?;
    let size = i64::try_from(export.bytes.len()).map_err(|_| {
        error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "FILE_TOO_LARGE",
            "FigCraft snapshot is too large.",
            &HeaderMap::new(),
        )
    })?;
    let now = services.clock.now();
    let new = NewUpload {
        file_id,
        user_id: live.published_by_user_id,
        size,
        sha256: digest,
        idempotency_key: None,
        now,
        expires_at: now
            + time::Duration::try_from(services.settings.upload_ttl).unwrap_or(time::Duration::DAY),
        max_per_user: services.settings.uploads_per_user,
        max_per_org: services.settings.uploads_per_org,
    };
    let upload = match tx
        .upload_create(&create_scope, &new)
        .await
        .map_err(|e| db_failure(&e, &HeaderMap::new(), "FigCraft upload reservation failed"))?
    {
        UploadCreateOutcome::Created(v) => v,
        UploadCreateOutcome::QuotaExceeded => {
            return Err(error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "QUOTA_EXCEEDED",
                "The storage quota does not allow this snapshot.",
                &HeaderMap::new(),
            ));
        }
        _ => {
            return Err(error_response(
                StatusCode::CONFLICT,
                "UPLOAD_LIMIT",
                "Could not reserve a snapshot upload.",
                &HeaderMap::new(),
            ));
        }
    };
    tx.commit()
        .await
        .map_err(|e| db_failure(&e, &HeaderMap::new(), "FigCraft upload reservation failed"))?;
    let Some(store) = services.storage.as_ref() else {
        return Err(unavailable(&HeaderMap::new(), "STORAGE_UNAVAILABLE"));
    };
    let mut temp = store
        .begin_write(export.bytes.len() as u64)
        .await
        .map_err(|_| unavailable(&HeaderMap::new(), "STORAGE_UNAVAILABLE"))?;
    if temp.write(&export.bytes).await.is_err() {
        store.abort(temp).await;
        return Err(unavailable(&HeaderMap::new(), "STORAGE_UNAVAILABLE"));
    }
    let key = store
        .finalize(temp, export.bytes.len() as u64, &digest)
        .await
        .map_err(|_| unavailable(&HeaderMap::new(), "STORAGE_UNAVAILABLE"))?;
    let now = services.clock.now();
    let mut tx = match craftcloud_db::members::OrgTx::begin(&services.pool, live.org_id).await {
        Ok(Some(tx)) => tx,
        Ok(None) => {
            let _ = store.delete(&key).await;
            return Err(not_found(&HeaderMap::new()));
        }
        Err(e) => {
            let _ = store.delete(&key).await;
            return Err(db_failure(
                &e,
                &HeaderMap::new(),
                "FigCraft snapshot store failed",
            ));
        }
    };
    let write_scope = tx
        .figcraft_service_scope(file_id, Action::UploadWrite)
        .await
        .map_err(|e| {
            db_failure(
                &e,
                &HeaderMap::new(),
                "FigCraft snapshot authority lookup failed",
            )
        })?
        .ok_or_else(|| {
            error_response(
                StatusCode::FORBIDDEN,
                "FORBIDDEN",
                "FigCraft snapshot authority is no longer valid.",
                &HeaderMap::new(),
            )
        })?;
    match tx
        .upload_begin_receiving(&write_scope, upload.id, now)
        .await
        .map_err(|e| db_failure(&e, &HeaderMap::new(), "FigCraft upload start failed"))?
    {
        BeginOutcome::Started(_) => (),
        _ => {
            return Err(error_response(
                StatusCode::CONFLICT,
                "UPLOAD_CLOSED",
                "Snapshot upload is no longer available.",
                &HeaderMap::new(),
            ));
        }
    }
    match tx
        .upload_store(&write_scope, upload.id, key.as_str(), now)
        .await
        .map_err(|e| db_failure(&e, &HeaderMap::new(), "FigCraft upload store failed"))?
    {
        StoreOutcome::Stored(_, _) => (),
        _ => {
            return Err(error_response(
                StatusCode::CONFLICT,
                "UPLOAD_CLOSED",
                "Snapshot upload is no longer available.",
                &HeaderMap::new(),
            ));
        }
    }
    tx.commit()
        .await
        .map_err(|e| db_failure(&e, &HeaderMap::new(), "FigCraft upload store failed"))?;
    let now = services.clock.now();
    let mut tx = craftcloud_db::members::OrgTx::begin(&services.pool, live.org_id)
        .await
        .map_err(|e| db_failure(&e, &HeaderMap::new(), "FigCraft version commit failed"))?
        .ok_or_else(|| not_found(&HeaderMap::new()))?;
    let commit_scope = tx
        .figcraft_service_scope(file_id, Action::VersionCommit)
        .await
        .map_err(|e| {
            db_failure(
                &e,
                &HeaderMap::new(),
                "FigCraft snapshot authority lookup failed",
            )
        })?
        .ok_or_else(|| {
            error_response(
                StatusCode::FORBIDDEN,
                "FORBIDDEN",
                "FigCraft snapshot authority is no longer valid.",
                &HeaderMap::new(),
            )
        })?;
    let version = match tx
        .version_commit(
            &commit_scope,
            file_id,
            upload.id,
            live.published_by_user_id,
            Some(Precondition::Head(Some(current_version))),
            now,
        )
        .await
        .map_err(|e| db_failure(&e, &HeaderMap::new(), "FigCraft version commit failed"))?
    {
        CommitOutcome::Committed(v) => v,
        CommitOutcome::PreconditionFailed { .. } => {
            return Err(error_response(
                StatusCode::CONFLICT,
                "VERSION_CONFLICT",
                "The file changed while FigCraft Live was exporting.",
                &HeaderMap::new(),
            ));
        }
        _ => {
            return Err(error_response(
                StatusCode::CONFLICT,
                "SNAPSHOT_FAILED",
                "The FigCraft snapshot could not be committed.",
                &HeaderMap::new(),
            ));
        }
    };
    tx.mark_figcraft_live_snapshot(version.id)
        .await
        .map_err(|e| db_failure(&e, &HeaderMap::new(), "FigCraft source marking failed"))?;
    if !tx
        .figcraft_record_snapshot(file_id, version.id, now, force)
        .await
        .map_err(|e| db_failure(&e, &HeaderMap::new(), "FigCraft state update failed"))?
    {
        return Err(error_response(
            StatusCode::CONFLICT,
            "FIGCRAFT_NOT_LIVE",
            "FigCraft Live stopped while snapshot was committed.",
            &HeaderMap::new(),
        ));
    }
    let mut event = craftcloud_db::audit::NewEvent::new("figcraft.snapshot")
        .org(live.org_id)
        .target("file", file_id)
        .workspace(live.workspace_id);
    event
        .details
        .push(("version_id", craftcloud_db::audit::Detail::Id(version.id)));
    event.details.push((
        "source",
        craftcloud_db::audit::Detail::Code("figcraft_live"),
    ));
    tx.audit(&craftcloud_db::audit::AuditCtx::system(), event)
        .await
        .map_err(|e| db_failure(&e, &HeaderMap::new(), "FigCraft snapshot audit failed"))?;
    tx.commit()
        .await
        .map_err(|e| db_failure(&e, &HeaderMap::new(), "FigCraft snapshot commit failed"))?;
    Ok(true)
}

/// Runs durable link invalidations until `stop` becomes true. Failed work is retained with backoff.
pub async fn run_figcraft_worker(services: Arc<Services>, mut stop: watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut state = FigcraftWorkerState::default();
    loop {
        tokio::select! { _=tick.tick()=>(), changed=stop.changed()=>{if changed.is_err()||*stop.borrow(){break;}} };
        figcraft_worker_tick(Arc::clone(&services), &mut state).await;
    }
}

/// Stateful worker cursor; public only so integration tests can drive one poll without sleeping.
#[doc(hidden)]
pub struct FigcraftWorkerState {
    reconcile_after: Option<Uuid>,
    reconcile_active: bool,
    reconcile_next: tokio::time::Instant,
}

impl Default for FigcraftWorkerState {
    fn default() -> Self {
        Self {
            reconcile_after: None,
            reconcile_active: false,
            reconcile_next: tokio::time::Instant::now() + Duration::from_secs(5 * 60),
        }
    }
}

impl FigcraftWorkerState {
    /// Makes the next poll begin a full entitlement reconciliation cycle.
    #[doc(hidden)]
    pub fn reconcile_now(&mut self) {
        self.reconcile_next = tokio::time::Instant::now();
    }
}

/// Executes one production worker pass; exposed as a hidden integration-test seam.
#[doc(hidden)]
pub async fn figcraft_worker_tick(services: Arc<Services>, state: &mut FigcraftWorkerState) {
    if !services.figcraft_available() {
        return;
    }
    if let Err(e)=sqlx::query("UPDATE figcraft_live_files SET state='republish',updated_at=$1 WHERE state='publishing' AND figcraft_file_id IS NULL AND updated_at<=$1-interval '1 minute'").bind(services.clock.now()).execute(&services.pool).await{tracing::warn!(%e,"FigCraft stale publish intent cleanup failed");}
    if !state.reconcile_active && tokio::time::Instant::now() >= state.reconcile_next {
        state.reconcile_active = true;
    }
    if state.reconcile_active && reconcile_links(&services, &mut state.reconcile_after).await {
        state.reconcile_active = false;
        state.reconcile_next = tokio::time::Instant::now() + Duration::from_secs(5 * 60);
    }
    if let Err(e) = db::prune_revoked_links(
        &services.pool,
        services.clock.now() - time::Duration::hours(24),
    )
    .await
    {
        tracing::warn!(%e,"FigCraft revoked link pruning failed");
    }
    process_revocations(&services).await;
    let snapshot_files:Vec<Uuid>=match sqlx::query_scalar("SELECT file_id FROM figcraft_live_files WHERE state='live' AND ((last_snapshot_at IS NOT NULL AND last_snapshot_at<=$1-interval '15 minutes') OR (last_snapshot_at IS NULL AND updated_at<=$1-interval '15 minutes')) AND (last_snapshot_attempt_at IS NULL OR last_snapshot_attempt_at<=$1-interval '1 minute') ORDER BY last_snapshot_at NULLS FIRST LIMIT 16")
            .bind(services.clock.now()).fetch_all(&services.pool).await{Ok(v)=>v,Err(e)=>{tracing::warn!(%e,"FigCraft automatic snapshot lookup failed");Vec::new()}};
    for file_id in snapshot_files {
        if let Err(e) =
            db::mark_snapshot_attempt(&services.pool, file_id, services.clock.now()).await
        {
            tracing::warn!(%e,"FigCraft automatic snapshot attempt update failed");
            continue;
        }
        if let Err(response) = service_snapshot(Arc::clone(&services), file_id, false).await {
            tracing::warn!(status=%response.status(),"FigCraft automatic snapshot failed");
        }
        // Revocations queued while the export ran must not wait for the next item or tick.
        process_revocations(&services).await;
    }
    let pending:Vec<(Uuid,Option<String>,Option<time::OffsetDateTime>)>=match sqlx::query_as("SELECT file_id,figcraft_file_id,last_snapshot_attempt_at FROM figcraft_live_files WHERE state='unpublishing' ORDER BY last_snapshot_attempt_at NULLS FIRST,updated_at LIMIT 32").fetch_all(&services.pool).await{Ok(v)=>v,Err(e)=>{tracing::warn!(%e,"FigCraft stop recovery lookup failed");return}};
    for (file_id, remote, last_attempt) in pending {
        process_revocations(&services).await;
        let _file_guard = match FileGuard::acquire(&services.pool, file_id).await {
            Ok(v) => v,
            Err(()) => continue,
        };
        let active: bool = match sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM figcraft_links WHERE file_id=$1 AND revoked_at IS NULL)",
        )
        .bind(file_id)
        .fetch_one(&services.pool)
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(%e,"FigCraft stop recovery link lookup failed");
                continue;
            }
        };
        if active {
            continue;
        }
        let final_done:bool=match sqlx::query_scalar("SELECT final_snapshot_done FROM figcraft_live_files WHERE file_id=$1 AND state='unpublishing'").bind(file_id).fetch_optional(&services.pool).await{Ok(Some(v))=>v,Ok(None)=>continue,Err(e)=>{tracing::warn!(%e,"FigCraft final snapshot state lookup failed");continue}};
        if !final_done {
            drop(_file_guard);
            // A permanent failure (for example QUOTA_EXCEEDED) must not re-export on every tick.
            let now = services.clock.now();
            if last_attempt
                .is_some_and(|at| at > now - time::Duration::seconds(FINAL_SNAPSHOT_RETRY_SECS))
            {
                continue;
            }
            if let Err(e) = db::mark_final_snapshot_attempt(&services.pool, file_id, now).await {
                tracing::warn!(%e,"FigCraft final snapshot attempt update failed");
                continue;
            }
            if let Err(response) = service_snapshot(Arc::clone(&services), file_id, true).await {
                tracing::warn!(status=%response.status(),"FigCraft final snapshot recovery failed");
            }
            continue;
        }
        let Some(remote) = remote else {
            let _ = db::finish_unpublish_audited(
                &services.pool,
                file_id,
                services.clock.now(),
                &craftcloud_db::audit::AuditCtx::system(),
            )
            .await;
            continue;
        };
        let Some(backend) = services.figcraft.as_ref().map(Arc::clone) else {
            continue;
        };
        if matches!(
            services.run_figcraft(move || backend.remove(&remote)).await,
            Some(Ok(()))
        ) {
            let _ = db::finish_unpublish_audited(
                &services.pool,
                file_id,
                services.clock.now(),
                &craftcloud_db::audit::AuditCtx::system(),
            )
            .await;
        }
    }
}

/// Delay before a revocation is retried after the per-file guard was busy.
const GUARD_BUSY_RETRY_SECS: i64 = 2;
/// Pause between final-snapshot attempts of one unpublishing file.
const FINAL_SNAPSHOT_RETRY_SECS: i64 = 60;

/// Claims and executes due link invalidations. Called ahead of and between the
/// slow snapshot and stop items so revocations never wait behind an export.
pub(crate) async fn process_app_access_revocations(services: &Arc<Services>) {
    if services.figcraft_available() {
        process_revocations(services).await;
    }
}

async fn process_revocations(services: &Arc<Services>) {
    let rows = match db::claim_revocations(&services.pool, 32, services.clock.now()).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(%e,"FigCraft outbox claim failed");
            return;
        }
    };
    for row in rows {
        let _file_guard = match FileGuard::acquire(&services.pool, row.file_id).await {
            Ok(v) => v,
            Err(_) => {
                // Local contention is not a remote failure: retry shortly and keep the attempt count.
                let _ = db::defer_revocation(
                    &services.pool,
                    row.id,
                    services.clock.now(),
                    time::Duration::seconds(GUARD_BUSY_RETRY_SECS),
                )
                .await;
                continue;
            }
        };
        let app_source = match db::parse_app_revocation_reason(&row.reason) {
            Ok(source) => source,
            Err(_) => {
                tracing::error!(reason = %row.reason, "invalid FigCraft app revocation reason; keeping fail-closed retry");
                let _ = db::retry_revocation(&services.pool, row.id, services.clock.now()).await;
                continue;
            }
        };
        let links = match match app_source.as_ref() {
            Some(source) => match row.user_id {
                Some(user_id) => match source {
                    db::AppRevocationSource::Legacy => {
                        db::app_access_revocation_links(&services.pool, row.file_id, user_id, None)
                            .await
                    }
                    db::AppRevocationSource::App(app) => {
                        db::app_access_revocation_links(
                            &services.pool,
                            row.file_id,
                            user_id,
                            Some(app.as_str()),
                        )
                        .await
                    }
                    db::AppRevocationSource::Tool(kind) => {
                        db::tool_revocation_links(&services.pool, row.file_id, user_id, kind).await
                    }
                },
                None => Err(craftcloud_db::DbError::Internal),
            },
            None => db::links(&services.pool, row.file_id).await,
        } {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(%e,"FigCraft outbox lookup failed");
                let _ = db::retry_revocation(&services.pool, row.id, services.clock.now()).await;
                continue;
            }
        };
        let mut failed = false;
        for link in links
            .into_iter()
            .filter(|l| row.user_id.is_none_or(|u| u == l.user_id))
        {
            let Some(remote) = link.figcraft_link_id else {
                continue;
            };
            let Some(file) = db::get_any(&services.pool, row.file_id)
                .await
                .ok()
                .flatten()
                .and_then(|l| l.figcraft_file_id)
            else {
                failed = true;
                break;
            };
            let Some(backend) = services.figcraft.as_ref().map(Arc::clone) else {
                failed = true;
                break;
            };
            if !matches!(
                services
                    .run_figcraft(move || backend.revoke_link(&file, &remote))
                    .await,
                Some(Ok(()))
            ) {
                failed = true;
                break;
            }
        }
        if failed {
            let _ = db::retry_revocation(&services.pool, row.id, services.clock.now()).await;
        } else {
            let _ = db::finish_revocation(&services.pool, row.id, services.clock.now()).await;
        }
    }
}

type ReconcileLinkRow = (Uuid, Uuid, Uuid, Uuid, Uuid, String, String, Option<String>);

async fn reconcile_links(services: &Arc<Services>, after: &mut Option<Uuid>) -> bool {
    let rows:Vec<ReconcileLinkRow>=match sqlx::query_as("SELECT l.id,l.file_id,l.user_id,f.org_id,f.workspace_id,l.access,l.source_kind,l.source_app FROM figcraft_links l JOIN files f ON f.id=l.file_id JOIN figcraft_live_files lf ON lf.file_id=l.file_id WHERE l.revoked_at IS NULL AND lf.state='live' AND ($1::uuid IS NULL OR l.id>$1) ORDER BY l.id LIMIT 1000")
        .bind(*after).fetch_all(&services.pool).await {Ok(v)=>v,Err(e)=>{tracing::warn!(%e,"FigCraft reconciliation query failed");return false}};
    if rows.is_empty() {
        *after = None;
        return true;
    }
    *after = rows.last().map(|r| r.0);
    for (_, file_id, user_id, org_id, workspace_id, access, source_kind, source_app) in rows {
        let policy_app = match source_kind.as_str() {
            "app" => source_app.as_deref(),
            "legacy" => Some("figcraft"),
            _ => None,
        };
        let allowed:bool=match sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1 AND disabled_at IS NULL) AND (NOT (SELECT require_two_factor FROM organizations WHERE id=$2) OR EXISTS(SELECT 1 FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL))")
            .bind(user_id).bind(org_id).fetch_one(&services.pool).await {Ok(v)=>v,Err(e)=>{tracing::warn!(%e,"FigCraft user reconciliation failed");continue}};
        let mut app_denied = false;
        let policy = if allowed {
            let actor = match read_actor(services, user_id, org_id, workspace_id, policy_app).await
            {
                Ok(mut actor) => {
                    match crate::workspaces::tool_access(services, user_id, org_id, &source_kind)
                        .await
                    {
                        Ok(tool_access) => {
                            actor.tool_access = tool_access;
                            Some(actor)
                        }
                        Err(_) => None,
                    }
                }
                Err(_) => None,
            };
            match actor {
                Some(actor) if !actor.may_use_live_links() => {
                    app_denied = true;
                    false
                }
                Some(actor) => match policy_resource(
                    &services.pool,
                    user_id,
                    org_id,
                    workspace_id,
                    file_id,
                    Action::FileRead,
                )
                .await
                {
                    Ok(resource) => {
                        let read = can(&actor, Action::FileRead, &resource);
                        if matches!(read, Decision::AppNotAssigned) {
                            app_denied = true;
                        }
                        if !read.is_allow() {
                            false
                        } else if access == "edit" {
                            let write = can(&actor, Action::VersionCommit, &resource);
                            if matches!(write, Decision::AppNotAssigned) {
                                app_denied = true;
                            }
                            write.is_allow()
                        } else {
                            true
                        }
                    }
                    Err(_) => false,
                },
                None => false,
            }
        } else {
            false
        };
        if !policy {
            let enqueue = if app_denied && matches!(source_kind.as_str(), "launcher" | "cli") {
                db::enqueue_file_tool_revocations_pool(
                    &services.pool,
                    file_id,
                    user_id,
                    &source_kind,
                    services.clock.now(),
                )
                .await
            } else if app_denied {
                let source_app = (source_kind != "legacy").then_some(policy_app).flatten();
                db::enqueue_file_app_revocations_pool(
                    &services.pool,
                    org_id,
                    file_id,
                    user_id,
                    source_app,
                    services.clock.now(),
                )
                .await
            } else {
                db::enqueue_file_user_revocations_pool(
                    &services.pool,
                    file_id,
                    user_id,
                    "security_changed",
                    services.clock.now(),
                )
                .await
            };
            if let Err(e) = enqueue {
                tracing::warn!(%e,"FigCraft entitlement revocation enqueue failed");
            }
        }
    }
    false
}
