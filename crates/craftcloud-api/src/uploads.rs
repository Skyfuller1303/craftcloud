//! Upload sessions and storage quota routes (CC-F07, CC-F10).
//!
//! An upload ends as a verified, published but uncommitted blob; the version
//! commit follows in a later milestone. The organization, workspace and
//! creator of every request come from the session or file row, never from the
//! client. Callers who cannot see the workspace get the uniform 404. Audit
//! lines carry ids only.
//!
//! The content route streams the request body into object storage with a
//! hard byte cap (the declared size), a per-read idle timeout and no global
//! body limit or handler timeout (it is registered on a separate router).
//! Whatever happens, a session that does not end `stored` releases its
//! reservation, and an object that was published but cannot be recorded is
//! deleted again.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use craftcloud_core::policy::{Action, Actor, OrgMembership, Resource, WorkspaceMembership, can};
use craftcloud_db::audit::Detail;
use craftcloud_db::members::OrgTx;
use craftcloud_db::uploads::{
    self, AbortOutcome, BeginOutcome, NewUpload, QuotaSetOutcome, StoreOutcome,
    UploadCreateOutcome, UploadSession,
};
use craftcloud_db::{OrgScope, files, organizations, workspaces};
use craftcloud_storage::{ObjectKey, ObjectStore, Sha256Digest, StorageError, WriteSink};
use http_body_util::BodyExt;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit::{Change, Denied, commit_audited};
use crate::orgs::{
    Gate, audit, conflict, db_failure, gate, not_found, operator_gate, page_error, parse_id,
    rfc3339,
};
use crate::pagination::{PageQuery, decode_time_cursor, encode_time_cursor};
use crate::projects::{Resolved, write_gate};
use crate::responses::{internal, rejection_response, unavailable, validation_failed};
use crate::session::AuthenticatedSession;
use crate::workspaces::{Locked, lock, read_actor};
use crate::{ApiState, Services, error_response};

// ---- helpers -----------------------------------------------------------------

fn session_json(s: &UploadSession) -> Value {
    json!({
        "upload_id": s.id.to_string(),
        "file_id": s.file_id.to_string(),
        "project_id": s.project_id.to_string(),
        "workspace_id": s.workspace_id.to_string(),
        "state": s.state,
        "size_bytes": s.declared_size,
        "sha256": hex(&s.declared_sha256),
        "max_bytes": s.declared_size,
        "created_by": s.created_by_user_id.to_string(),
        "created_at": rfc3339(s.created_at),
        "expires_at": rfc3339(s.expires_at),
    })
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Decodes 64 hex characters (either case) into a digest.
fn parse_sha256(text: &str) -> Option<Sha256Digest> {
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut out = [0_u8; 32];
    for (slot, pair) in out.iter_mut().zip(text.as_bytes().chunks(2)) {
        let pair = std::str::from_utf8(pair).ok()?;
        *slot = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

fn storage_unavailable(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "STORAGE_UNAVAILABLE",
        "File storage is not configured on this server.",
        headers,
    )
}

fn storage_error(error: &StorageError, headers: &HeaderMap) -> Response {
    match error {
        StorageError::Full => error_response(
            StatusCode::INSUFFICIENT_STORAGE,
            "INSUFFICIENT_STORAGE",
            "The server has no space left for this upload.",
            headers,
        ),
        StorageError::TooLarge => error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "PAYLOAD_TOO_LARGE",
            "The upload is larger than the declared size.",
            headers,
        ),
        StorageError::LengthMismatch => error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "LENGTH_MISMATCH",
            "The uploaded size differs from the declared size.",
            headers,
        ),
        StorageError::ChecksumMismatch => error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "CHECKSUM_MISMATCH",
            "The uploaded data does not match the declared SHA-256.",
            headers,
        ),
        other => {
            tracing::error!(error = ?other, "storage failure");
            internal(headers)
        }
    }
}

async fn services_of(
    state: &ApiState,
    headers: &HeaderMap,
) -> Result<Arc<Services>, Box<Response>> {
    state
        .services
        .clone()
        .ok_or_else(|| Box::new(unavailable(headers, "AUTH_UNAVAILABLE")))
}

/// Marks a session failed and releases its reservation; errors are logged
/// (the session then expires on its own).
async fn fail_session(services: &Services, org_id: Uuid, id: Uuid) {
    let now = services.clock.now();
    match OrgTx::begin(&services.pool, org_id).await {
        Ok(Some(mut tx)) => {
            if let Err(error) = tx.upload_fail(id, now).await {
                tracing::error!(%error, "cannot fail upload session");
                return;
            }
            if let Err(error) = tx.commit().await {
                tracing::error!(%error, "cannot fail upload session");
            }
        }
        Ok(None) => {}
        Err(error) => tracing::error!(%error, "cannot fail upload session"),
    }
}

/// Fails the session inside the open transaction. If that or the commit fails,
/// the transaction is dropped (releasing the organization lock) and the session
/// is failed in a new one, so a rejected upload never keeps its reservation.
async fn fail_in_tx(
    mut tx: OrgTx,
    services: &Services,
    org_id: Uuid,
    id: Uuid,
    now: time::OffsetDateTime,
) {
    let recorded = match tx.upload_fail(id, now).await {
        Ok(_) => tx.commit().await.is_ok(),
        Err(_) => {
            drop(tx);
            false
        }
    };
    if !recorded {
        fail_session(services, org_id, id).await;
    }
}

/// Fails the session when the request future is dropped (client gone) before
/// it finished on its own.
struct FailOnDrop {
    services: Arc<Services>,
    org_id: Uuid,
    id: Uuid,
    armed: bool,
}

impl Drop for FailOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (services, org_id, id) = (Arc::clone(&self.services), self.org_id, self.id);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move { fail_session(&services, org_id, id).await });
        }
    }
}

async fn resolve_upload(
    state: &ApiState,
    headers: &HeaderMap,
    raw: &str,
) -> Result<(Resolved, Uuid, Uuid), Box<Response>> {
    let services = services_of(state, headers).await?;
    let Some(id) = parse_id(raw) else {
        return Err(Box::new(not_found(headers)));
    };
    match uploads::locate(&services.pool, id).await {
        Ok(Some(location)) => Ok((
            Resolved {
                services,
                org_id: location.org_id,
                workspace_id: location.workspace_id,
                id,
            },
            location.creator,
            location.file_id,
        )),
        Ok(None) => Err(Box::new(not_found(headers))),
        Err(error) => Err(Box::new(db_failure(
            &error,
            headers,
            "upload lookup failed",
        ))),
    }
}

/// Read-side decision for an upload session.
async fn upload_read_gate(
    resolved: &Resolved,
    session: &AuthenticatedSession,
    headers: &HeaderMap,
    action: Action,
    name: &str,
    creator: Uuid,
    file_id: Uuid,
) -> Result<OrgScope, Box<Response>> {
    let actor = read_actor(
        &resolved.services.pool,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
    )
    .await
    .map_err(|error| Box::new(db_failure(&error, headers, "actor lookup failed")))?;
    let object = crate::projects::policy_resource(
        &resolved.services.pool,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        file_id,
        Action::FileRead,
    )
    .await
    .map_err(|error| Box::new(db_failure(&error, headers, "file grant lookup failed")))?;
    let resource = upload_object_resource(object, resolved.org_id, resolved.workspace_id, creator);
    let decision = can(&actor, action, &resource);
    match gate(
        decision,
        headers,
        session.user_id,
        name,
        Some(resolved.org_id),
        Some(resolved.id),
    ) {
        Gate::Open(permit) => {
            OrgScope::from_permit(&permit).ok_or_else(|| Box::new(internal(headers)))
        }
        Gate::Closed(response) => Err(Box::new(response)),
    }
}

fn upload_object_resource(
    resource: Resource,
    org_id: Uuid,
    workspace_id: Uuid,
    creator: Uuid,
) -> Resource {
    match resource {
        Resource::Object { target, .. } => Resource::UploadObject {
            org_id,
            workspace_id,
            creator,
            target,
        },
        _ => Resource::Upload {
            org_id,
            workspace_id,
            creator,
        },
    }
}

/// Locks the organization, loads fresh roles and decides for an upload session.
pub(crate) async fn upload_write_gate(
    resolved: &Resolved,
    session: &AuthenticatedSession,
    headers: &HeaderMap,
    action: Action,
    name: &str,
    creator: Uuid,
    file_id: Uuid,
) -> Result<(OrgTx, OrgScope), Box<Response>> {
    let Locked { mut tx, actor } = lock(
        &resolved.services.pool,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        headers,
    )
    .await?;
    let resource = if matches!(action, Action::UploadWrite | Action::VersionCommit) {
        tx.policy_resource(session.user_id, resolved.workspace_id, file_id, action)
            .await
            .map_err(|error| Box::new(db_failure(&error, headers, "file grant lookup failed")))?
    } else if action == Action::UploadAbort && creator == session.user_id {
        let object = tx
            .policy_resource(
                session.user_id,
                resolved.workspace_id,
                file_id,
                Action::FileRead,
            )
            .await
            .map_err(|error| Box::new(db_failure(&error, headers, "file grant lookup failed")))?;
        upload_object_resource(object, resolved.org_id, resolved.workspace_id, creator)
    } else {
        Resource::Upload {
            org_id: resolved.org_id,
            workspace_id: resolved.workspace_id,
            creator,
        }
    };
    let decision = if matches!(action, Action::UploadWrite | Action::VersionCommit)
        && creator != session.user_id
    {
        can(
            &actor,
            action,
            &Resource::Upload {
                org_id: resolved.org_id,
                workspace_id: resolved.workspace_id,
                creator,
            },
        )
    } else {
        can(&actor, action, &resource)
    };
    match gate(
        decision,
        headers,
        session.user_id,
        name,
        Some(resolved.org_id),
        Some(resolved.id),
    ) {
        Gate::Open(permit) => {
            if action == Action::VersionCommit {
                match tx.file_lock(file_id).await {
                    Ok(Some(lock)) if lock.holder_session_id != session.session_id => {
                        return Err(Box::new(crate::projects::locked_response(
                            &lock,
                            session.session_id,
                            session.user_id,
                            headers,
                        )));
                    }
                    Ok(_) => {}
                    Err(error) => {
                        return Err(Box::new(db_failure(
                            &error,
                            headers,
                            "file lock lookup failed",
                        )));
                    }
                }
            }
            OrgScope::from_permit(&permit)
                .map(|scope| (tx, scope))
                .ok_or_else(|| Box::new(internal(headers)))
        }
        Gate::Closed(response) => Err(Box::new(response)),
    }
}

// ---- POST /files/{id}/uploads ------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateUploadRequest {
    size_bytes: u64,
    sha256: String,
}

fn idempotency_key(headers: &HeaderMap) -> Result<Option<String>, ()> {
    let Some(raw) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    let text = raw.to_str().map_err(|_| ())?;
    if text.is_empty() || text.len() > 128 || !text.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(());
    }
    Ok(Some(text.to_owned()))
}

pub(crate) async fn create_upload(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(file_raw): Path<String>,
    body: Result<Json<CreateUploadRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    let services = match services_of(&state, &headers).await {
        Ok(services) => services,
        Err(response) => return *response,
    };
    let Some(file_id) = parse_id(&file_raw) else {
        return not_found(&headers);
    };
    let location = match files::locate(&services.pool, file_id).await {
        Ok(Some(location)) => location,
        Ok(None) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "file lookup failed"),
    };
    let resolved = Resolved {
        services: Arc::clone(&services),
        org_id: location.org_id,
        workspace_id: location.workspace_id,
        id: file_id,
    };
    let (mut tx, scope) = match write_gate(
        &resolved,
        &session,
        &headers,
        Action::UploadCreate,
        "upload.create",
        file_id,
    )
    .await
    {
        Ok(opened) => opened,
        Err(response) => return *response,
    };
    if services.storage.is_none() {
        return storage_unavailable(&headers);
    }
    let Ok(key) = idempotency_key(&headers) else {
        return validation_failed(&["Idempotency-Key"], &headers);
    };
    let digest = parse_sha256(&request.sha256);
    let size = i64::try_from(request.size_bytes).ok();
    let (Some(digest), Some(size)) = (digest, size) else {
        let mut fields = Vec::new();
        if size.is_none() {
            fields.push("size_bytes");
        }
        if digest.is_none() {
            fields.push("sha256");
        }
        return validation_failed(&fields, &headers);
    };
    if request.size_bytes > services.settings.max_file_size_bytes {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "PAYLOAD_TOO_LARGE",
            "The declared size exceeds the maximum file size of this server.",
            &headers,
        );
    }
    let now = services.clock.now();
    let ttl = time::Duration::try_from(services.settings.upload_ttl).unwrap_or(time::Duration::DAY);
    let new = NewUpload {
        file_id,
        user_id: session.user_id,
        size,
        sha256: digest,
        idempotency_key: key,
        now,
        expires_at: now + ttl,
        max_per_user: services.settings.uploads_per_user,
        max_per_org: services.settings.uploads_per_org,
    };
    let outcome = match tx.upload_create(&scope, &new).await {
        Ok(outcome) => outcome,
        Err(error) => return db_failure(&error, &headers, "upload creation failed"),
    };
    // Expiry done on the way is committed with every outcome.
    if let Err(error) = tx.commit().await {
        return db_failure(&error, &headers, "upload creation failed");
    }
    match outcome {
        UploadCreateOutcome::Created(created) => {
            audit(
                &headers,
                session.user_id,
                "upload.create",
                Some(resolved.org_id),
                Some(created.id),
                "success",
            );
            (StatusCode::CREATED, Json(session_json(&created))).into_response()
        }
        UploadCreateOutcome::Replayed(existing) => {
            (StatusCode::OK, Json(session_json(&existing))).into_response()
        }
        UploadCreateOutcome::KeyReused => conflict(
            "IDEMPOTENCY_KEY_REUSED",
            "The Idempotency-Key was used with a different request.",
            &headers,
        ),
        UploadCreateOutcome::FileGone => not_found(&headers),
        UploadCreateOutcome::FileDeleted => conflict(
            "FILE_DELETED",
            "The file is in the trash; restore it first.",
            &headers,
        ),
        UploadCreateOutcome::ProjectDeleted => conflict(
            "PROJECT_DELETED",
            "The project is in the trash; restore it first.",
            &headers,
        ),
        UploadCreateOutcome::UserLimit | UploadCreateOutcome::OrgLimit => conflict(
            "UPLOAD_LIMIT",
            "Too many uploads are open; finish or abort some first.",
            &headers,
        ),
        UploadCreateOutcome::QuotaExceeded => error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "QUOTA_EXCEEDED",
            "The storage quota does not allow this upload.",
            &headers,
        ),
    }
}

// ---- GET /uploads/{id} --------------------------------------------------------------

pub(crate) async fn get_upload(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(raw): Path<String>,
) -> Response {
    let (resolved, creator, file_id) = match resolve_upload(&state, &headers, &raw).await {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let scope = match upload_read_gate(
        &resolved,
        &session,
        &headers,
        Action::UploadRead,
        "upload.read",
        creator,
        file_id,
    )
    .await
    {
        Ok(scope) => scope,
        Err(response) => return *response,
    };
    // Lazy expiry: an open session past its time is closed before it is shown.
    let now = resolved.services.clock.now();
    match uploads::get(&resolved.services.pool, &scope, resolved.id).await {
        Ok(Some(mut upload)) => {
            if upload.expires_at <= now
                && matches!(upload.state.as_str(), "created" | "receiving" | "stored")
                && let Ok(Some(mut tx)) =
                    OrgTx::begin(&resolved.services.pool, resolved.org_id).await
                && tx.expire_due(now).await.is_ok()
                && tx.commit().await.is_ok()
                && let Ok(Some(fresh)) =
                    uploads::get(&resolved.services.pool, &scope, resolved.id).await
            {
                upload = fresh;
            }
            Json(session_json(&upload)).into_response()
        }
        Ok(None) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "upload read failed"),
    }
}

// ---- DELETE /uploads/{id} ------------------------------------------------------------

pub(crate) async fn abort_upload(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(raw): Path<String>,
) -> Response {
    let (resolved, creator, file_id) = match resolve_upload(&state, &headers, &raw).await {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let (mut tx, scope) = match upload_write_gate(
        &resolved,
        &session,
        &headers,
        Action::UploadAbort,
        "upload.abort",
        creator,
        file_id,
    )
    .await
    {
        Ok(opened) => opened,
        Err(response) => return *response,
    };
    let now = resolved.services.clock.now();
    let outcome = match tx.upload_abort(&scope, resolved.id, now).await {
        Ok(outcome) => outcome,
        Err(error) => return db_failure(&error, &headers, "upload abort failed"),
    };
    if let Err(error) = tx.commit().await {
        return db_failure(&error, &headers, "upload abort failed");
    }
    match outcome {
        AbortOutcome::Aborted(_) => {
            audit(
                &headers,
                session.user_id,
                "upload.abort",
                Some(resolved.org_id),
                Some(resolved.id),
                "success",
            );
            StatusCode::NO_CONTENT.into_response()
        }
        AbortOutcome::Unchanged(_) => StatusCode::NO_CONTENT.into_response(),
        AbortOutcome::InProgress => conflict(
            "UPLOAD_IN_PROGRESS",
            "The content is being received; wait for it to finish.",
            &headers,
        ),
        AbortOutcome::Committed => conflict(
            "UPLOAD_CLOSED",
            "The upload was already committed.",
            &headers,
        ),
        AbortOutcome::Gone => not_found(&headers),
    }
}

// ---- PUT /uploads/{id}/content ----------------------------------------------------------

fn deadline_error(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::REQUEST_TIMEOUT,
        "REQUEST_TIMEOUT",
        "The upload stalled and was cancelled.",
        headers,
    )
}

fn interrupted(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "UPLOAD_INTERRUPTED",
        "The upload body could not be read completely.",
        headers,
    )
}

fn expired(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::GONE,
        "UPLOAD_EXPIRED",
        "The upload session has expired; create a new one.",
        headers,
    )
}

async fn delete_object(services: &Services, key: &ObjectKey) {
    if let Some(store) = &services.storage
        && let Err(error) = store.delete(key).await
    {
        tracing::error!(%error, "cannot delete an object that could not be recorded");
    }
}

/// After this long a receiving upload must have shown the minimum average
/// rate, or it is cancelled (slow-drip protection; the session would
/// otherwise hold quota and a slot until it expires).
pub(crate) const RATE_GRACE_SECS: i64 = 300;
/// Minimum average rate in bytes per second once the grace period is over.
pub(crate) const MIN_RATE_BYTES_PER_SEC: u64 = 16 * 1024;

/// Deletes a published object and fails its session unless disarmed: covers
/// a task that ends abnormally between publishing and recording the blob.
struct ObjectGuard {
    services: Arc<Services>,
    org_id: Uuid,
    id: Uuid,
    key: Option<ObjectKey>,
}

impl Drop for ObjectGuard {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        let (services, org_id, id) = (Arc::clone(&self.services), self.org_id, self.id);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                delete_object(&services, &key).await;
                fail_session(&services, org_id, id).await;
            });
        }
    }
}

/// Everything the second half of an upload needs; moved into a detached task.
struct Finish {
    services: Arc<Services>,
    temp: craftcloud_storage::TempObject,
    size: u64,
    expected: Sha256Digest,
    user_id: Uuid,
    login_session_id: Uuid,
    credential_kind: crate::session::CredentialKind,
    org_id: Uuid,
    workspace_id: Uuid,
    upload_id: Uuid,
    file_id: Uuid,
    creator: Uuid,
    headers: HeaderMap,
}

/// Verifies and publishes the object, then records the blob only if
/// everything still holds, judged with fresh roles under the organization
/// lock (a revoked editor stores nothing) and with the login session and the
/// account re-checked in the same transaction (a disabled account or a
/// revoked session stores nothing). It runs in its own task, so a
/// cancelled request cannot leave a published object behind unaccounted.
async fn finish_upload(finish: Finish) -> Response {
    let Finish {
        services,
        temp,
        size,
        expected,
        user_id,
        login_session_id,
        credential_kind,
        org_id,
        workspace_id,
        upload_id,
        file_id,
        creator,
        headers,
    } = finish;
    let Some(store) = services.storage.as_ref() else {
        fail_session(&services, org_id, upload_id).await;
        return storage_unavailable(&headers);
    };
    let key = match store.finalize(temp, size, &expected).await {
        Ok(key) => key,
        Err(error) => {
            fail_session(&services, org_id, upload_id).await;
            return storage_error(&error, &headers);
        }
    };
    let mut guard = ObjectGuard {
        services: Arc::clone(&services),
        org_id,
        id: upload_id,
        key: Some(key.clone()),
    };
    let Locked { mut tx, actor } =
        match lock(&services.pool, user_id, org_id, workspace_id, &headers).await {
            Ok(locked) => locked,
            Err(response) => {
                guard.key = None;
                delete_object(&services, &key).await;
                fail_session(&services, org_id, upload_id).await;
                return *response;
            }
        };
    // The request was authenticated when it started; the account may have been
    // disabled or the session revoked while the body streamed in.
    let now = services.clock.now();
    match tx
        .session_valid(
            login_session_id,
            user_id,
            now,
            credential_kind == crate::session::CredentialKind::Device,
        )
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            fail_in_tx(tx, &services, org_id, upload_id, now).await;
            guard.key = None;
            delete_object(&services, &key).await;
            audit(
                &headers,
                user_id,
                "upload.content",
                Some(org_id),
                Some(upload_id),
                "unauthenticated",
            );
            let mut response = error_response(
                StatusCode::UNAUTHORIZED,
                "UNAUTHENTICATED",
                "Authentication is required.",
                &headers,
            );
            // Stored as an aggregated `denied` audit row by `record_denials`.
            response.extensions_mut().insert(Denied {
                actor: user_id,
                action: "upload.content".to_owned(),
                org: Some(org_id),
                target: Some(upload_id),
            });
            return response;
        }
        Err(error) => {
            guard.key = None;
            drop(tx);
            delete_object(&services, &key).await;
            fail_session(&services, org_id, upload_id).await;
            return db_failure(&error, &headers, "session check failed");
        }
    }
    let write_resource = if creator == user_id {
        match tx
            .policy_resource(user_id, workspace_id, file_id, Action::UploadWrite)
            .await
        {
            Ok(resource) => resource,
            Err(error) => {
                fail_in_tx(tx, &services, org_id, upload_id, now).await;
                guard.key = None;
                delete_object(&services, &key).await;
                return db_failure(&error, &headers, "file grant lookup failed");
            }
        }
    } else {
        Resource::Upload {
            org_id,
            workspace_id,
            creator,
        }
    };
    let decision = can(&actor, Action::UploadWrite, &write_resource);
    let scope = match gate(
        decision,
        &headers,
        user_id,
        "upload.content",
        Some(org_id),
        Some(upload_id),
    ) {
        Gate::Open(permit) => OrgScope::from_permit(&permit),
        Gate::Closed(response) => {
            fail_in_tx(tx, &services, org_id, upload_id, now).await;
            guard.key = None;
            delete_object(&services, &key).await;
            return response;
        }
    };
    let Some(scope) = scope else {
        guard.key = None;
        delete_object(&services, &key).await;
        fail_session(&services, org_id, upload_id).await;
        return internal(&headers);
    };
    let stored = match tx.upload_store(&scope, upload_id, key.as_str(), now).await {
        Ok(outcome) => outcome,
        Err(error) => {
            guard.key = None;
            drop(tx);
            delete_object(&services, &key).await;
            fail_session(&services, org_id, upload_id).await;
            return db_failure(&error, &headers, "upload store failed");
        }
    };
    match stored {
        StoreOutcome::Stored(upload, _blob) => {
            // Crash test hook: object published, blob row not committed yet.
            craftcloud_storage::crash_point("upload-before-record", 0);
            if let Err(error) = tx.commit().await {
                guard.key = None;
                delete_object(&services, &key).await;
                fail_session(&services, org_id, upload_id).await;
                return db_failure(&error, &headers, "upload store failed");
            }
            guard.key = None;
            audit(
                &headers,
                user_id,
                "upload.content",
                Some(org_id),
                Some(upload_id),
                "success",
            );
            Json(session_json(&upload)).into_response()
        }
        StoreOutcome::NotReceiving(state) => {
            // Expired (or otherwise closed) while the bytes were arriving.
            let _ = tx.commit().await;
            guard.key = None;
            delete_object(&services, &key).await;
            if state == "expired" {
                expired(&headers)
            } else {
                conflict(
                    "UPLOAD_CLOSED",
                    "The upload was closed meanwhile.",
                    &headers,
                )
            }
        }
        StoreOutcome::FileDeleted | StoreOutcome::ProjectDeleted => {
            fail_in_tx(tx, &services, org_id, upload_id, now).await;
            guard.key = None;
            delete_object(&services, &key).await;
            conflict(
                "FILE_DELETED",
                "The file or its project was deleted while the upload was running.",
                &headers,
            )
        }
        StoreOutcome::Gone => {
            drop(tx);
            guard.key = None;
            delete_object(&services, &key).await;
            not_found(&headers)
        }
    }
}

pub(crate) async fn put_content(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(raw): Path<String>,
    body: Body,
) -> Response {
    let (resolved, creator, file_id) = match resolve_upload(&state, &headers, &raw).await {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let services = Arc::clone(&resolved.services);
    let org_id = resolved.org_id;
    let upload_id = resolved.id;

    // 1. created -> receiving under the organization lock, with fresh roles.
    let (mut tx, scope) = match upload_write_gate(
        &resolved,
        &session,
        &headers,
        Action::UploadWrite,
        "upload.content",
        creator,
        file_id,
    )
    .await
    {
        Ok(opened) => opened,
        Err(response) => return *response,
    };
    // Only an authorized caller learns about the server's storage state.
    if services.storage.is_none() {
        return storage_unavailable(&headers);
    }
    let started_at = services.clock.now();
    let begun = match tx
        .upload_begin_receiving(&scope, upload_id, started_at)
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return db_failure(&error, &headers, "upload start failed"),
    };
    if let Err(error) = tx.commit().await {
        return db_failure(&error, &headers, "upload start failed");
    }
    let declared = match begun {
        BeginOutcome::Started(started) => started,
        BeginOutcome::InProgress => {
            return conflict(
                "UPLOAD_IN_PROGRESS",
                "The content of this upload is already being received.",
                &headers,
            );
        }
        BeginOutcome::Expired => return expired(&headers),
        BeginOutcome::Closed(_) => {
            return conflict(
                "UPLOAD_CLOSED",
                "This upload cannot receive content any more; create a new one.",
                &headers,
            );
        }
        BeginOutcome::FileDeleted => {
            fail_session(&services, org_id, upload_id).await;
            return conflict("FILE_DELETED", "The file is in the trash.", &headers);
        }
        BeginOutcome::ProjectDeleted => {
            fail_session(&services, org_id, upload_id).await;
            return conflict("PROJECT_DELETED", "The project is in the trash.", &headers);
        }
        BeginOutcome::Gone => return not_found(&headers),
    };
    let mut guard = FailOnDrop {
        services: Arc::clone(&services),
        org_id,
        id: upload_id,
        armed: true,
    };
    let size = u64::try_from(declared.declared_size).unwrap_or(0);
    let Ok(expected) = <Sha256Digest>::try_from(declared.declared_sha256.as_slice()) else {
        guard.armed = false;
        fail_session(&services, org_id, upload_id).await;
        return internal(&headers);
    };
    let Some(store) = services.storage.as_ref() else {
        guard.armed = false;
        fail_session(&services, org_id, upload_id).await;
        return storage_unavailable(&headers);
    };

    // 2. stream the body into a temporary object. The whole loop ends at the
    //    session's expiry (the reservation is freed then), and a body that
    //    arrives slower than the minimum rate is cancelled.
    let declared_length = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared_length.is_some_and(|len| len > size) {
        guard.armed = false;
        fail_session(&services, org_id, upload_id).await;
        return storage_error(&StorageError::TooLarge, &headers);
    }
    let mut temp = match store.begin_write(size).await {
        Ok(temp) => temp,
        Err(error) => {
            guard.armed = false;
            fail_session(&services, org_id, upload_id).await;
            return storage_error(&error, &headers);
        }
    };
    let expires_at = declared.expires_at;
    let until_expiry = (expires_at - started_at)
        .try_into()
        .unwrap_or(Duration::ZERO);
    let deadline = tokio::time::Instant::now() + until_expiry;
    let idle: Duration = services.settings.upload_idle_timeout;
    let mut received: u64 = 0;
    let mut body = body;
    loop {
        let now = services.clock.now();
        if now >= expires_at {
            guard.armed = false;
            fail_session(&services, org_id, upload_id).await;
            return expired(&headers);
        }
        let elapsed = (now - started_at).whole_seconds();
        if elapsed >= RATE_GRACE_SECS
            && received < MIN_RATE_BYTES_PER_SEC.saturating_mul(u64::try_from(elapsed).unwrap_or(0))
        {
            guard.armed = false;
            fail_session(&services, org_id, upload_id).await;
            return slow_error(&headers);
        }
        let wait = idle.min(deadline.saturating_duration_since(tokio::time::Instant::now()));
        let frame = match tokio::time::timeout(wait, body.frame()).await {
            Err(_) => {
                guard.armed = false;
                fail_session(&services, org_id, upload_id).await;
                return if tokio::time::Instant::now() >= deadline {
                    expired(&headers)
                } else {
                    deadline_error(&headers)
                };
            }
            Ok(None) => break,
            Ok(Some(Err(_))) => {
                guard.armed = false;
                fail_session(&services, org_id, upload_id).await;
                return interrupted(&headers);
            }
            Ok(Some(Ok(frame))) => frame,
        };
        if let Ok(data) = frame.into_data() {
            received = received.saturating_add(data.len() as u64);
            services.metrics.add_uploaded(data.len() as u64);
            if let Err(error) = temp.write(&data).await {
                guard.armed = false;
                fail_session(&services, org_id, upload_id).await;
                return storage_error(&error, &headers);
            }
        }
    }

    // 3. verify, publish and record in a detached task: from here on a
    //    cancelled request must not leave a published object behind.
    guard.armed = false;
    let finish = Finish {
        services: Arc::clone(&services),
        temp,
        size,
        expected,
        user_id: session.user_id,
        login_session_id: session.session_id,
        credential_kind: session.credential_kind,
        org_id,
        workspace_id: resolved.workspace_id,
        upload_id,
        file_id,
        creator,
        headers: headers.clone(),
    };
    match tokio::spawn(finish_upload(finish)).await {
        Ok(response) => response,
        Err(_) => {
            fail_session(&services, org_id, upload_id).await;
            internal(&headers)
        }
    }
}

fn slow_error(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::REQUEST_TIMEOUT,
        "REQUEST_TIMEOUT",
        "The upload is arriving too slowly and was cancelled.",
        headers,
    )
}

// ---- quota -----------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QuotaRequest {
    /// A non-negative number of bytes, or `null` for unlimited. The key is
    /// required so that omitting it can never mean "unlimited".
    quota_bytes: Value,
}

fn parse_quota(value: &Value) -> Result<Option<i64>, ()> {
    match value {
        Value::Null => Ok(None),
        Value::Number(n) => n
            .as_u64()
            .and_then(|v| i64::try_from(v).ok())
            .map(Some)
            .ok_or(()),
        _ => Err(()),
    }
}

pub(crate) async fn set_org_quota(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(org_raw): Path<String>,
    body: Result<Json<QuotaRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    let services = match services_of(&state, &headers).await {
        Ok(services) => services,
        Err(response) => return *response,
    };
    if let Err(response) = operator_gate(
        &services,
        &session,
        Action::OrganizationQuotaSet,
        "organization.quota",
        parse_id(&org_raw),
        &headers,
    )
    .await
    {
        return *response;
    }
    let Ok(quota) = parse_quota(&request.quota_bytes) else {
        return validation_failed(&["quota_bytes"], &headers);
    };
    let Some(org_id) = parse_id(&org_raw) else {
        return not_found(&headers);
    };
    let mut tx = match OrgTx::begin(&services.pool, org_id).await {
        Ok(Some(tx)) => tx,
        Ok(None) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "quota update failed"),
    };
    let changed = match tx.org_quota_set(quota).await {
        Ok(changed) => changed,
        Err(error) => return db_failure(&error, &headers, "quota update failed"),
    };
    // Setting the value it already has changes nothing and records nothing.
    let done = if changed {
        let change = Change::new(
            &headers,
            session.user_id,
            "organization.quota",
            org_id,
            Some(org_id),
        )
        .detail("unlimited", Detail::Flag(quota.is_none()))
        .detail("quota_bytes", Detail::Count(quota.unwrap_or(0)));
        commit_audited(tx, change).await
    } else {
        tx.commit().await
    };
    if let Err(error) = done {
        return db_failure(&error, &headers, "quota update failed");
    }
    Json(json!({ "quota_bytes": quota })).into_response()
}

pub(crate) async fn set_workspace_quota(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(ws_raw): Path<String>,
    body: Result<Json<QuotaRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    let services = match services_of(&state, &headers).await {
        Ok(services) => services,
        Err(response) => return *response,
    };
    let Some(workspace_id) = parse_id(&ws_raw) else {
        return not_found(&headers);
    };
    let org_id = match workspaces::org_of(&services.pool, workspace_id).await {
        Ok(Some(org_id)) => org_id,
        Ok(None) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "workspace lookup failed"),
    };
    let resolved = Resolved {
        services: Arc::clone(&services),
        org_id,
        workspace_id,
        id: workspace_id,
    };
    let (mut tx, scope) = match write_gate(
        &resolved,
        &session,
        &headers,
        Action::WorkspaceQuotaSet,
        "workspace.quota",
        workspace_id,
    )
    .await
    {
        Ok(opened) => opened,
        Err(response) => return *response,
    };
    let Ok(quota) = parse_quota(&request.quota_bytes) else {
        return validation_failed(&["quota_bytes"], &headers);
    };
    match tx.workspace_quota_set(&scope, workspace_id, quota).await {
        Ok(outcome @ (QuotaSetOutcome::Set | QuotaSetOutcome::Unchanged)) => {
            let done = if outcome == QuotaSetOutcome::Set {
                let change = Change::new(
                    &headers,
                    session.user_id,
                    "workspace.quota",
                    org_id,
                    Some(workspace_id),
                )
                .workspace(workspace_id)
                .detail("unlimited", Detail::Flag(quota.is_none()))
                .detail("quota_bytes", Detail::Count(quota.unwrap_or(0)));
                commit_audited(tx, change).await
            } else {
                tx.commit().await
            };
            if let Err(error) = done {
                return db_failure(&error, &headers, "quota update failed");
            }
            Json(json!({ "quota_bytes": quota })).into_response()
        }
        Ok(QuotaSetOutcome::ExceedsOrganization) => validation_failed(&["quota_bytes"], &headers),
        Ok(QuotaSetOutcome::Gone) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "quota update failed"),
    }
}

pub(crate) async fn get_usage(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(org_raw): Path<String>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let services = match services_of(&state, &headers).await {
        Ok(services) => services,
        Err(response) => return *response,
    };
    let Some(org_id) = parse_id(&org_raw) else {
        return not_found(&headers);
    };
    let role = match organizations::active_role(&services.pool, org_id, session.user_id).await {
        Ok(role) => role,
        Err(error) => return db_failure(&error, &headers, "actor lookup failed"),
    };
    let actor = Actor {
        user_id: session.user_id,
        instance_operator: false,
        membership: role.map(|role| OrgMembership { org_id, role }),
        workspace: None::<WorkspaceMembership>,
    };
    let decision = can(
        &actor,
        Action::UsageRead,
        &Resource::Organization { org_id },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "usage.read",
        Some(org_id),
        None,
    ) {
        Gate::Open(permit) => permit,
        Gate::Closed(response) => return response,
    };
    let Some(scope) = OrgScope::from_permit(&permit) else {
        return internal(&headers);
    };
    let Ok(Query(page)) = query else {
        return validation_failed(&["query"], &headers);
    };
    let limit = match page.limit() {
        Ok(limit) => limit,
        Err(error) => return page_error(error, &headers),
    };
    let scope_text = org_id.to_string();
    let after = match page
        .cursor()
        .map(|c| decode_time_cursor(c, "usage", &scope_text))
    {
        Some(Ok(key)) => Some(key),
        Some(Err(error)) => return page_error(error, &headers),
        None => None,
    };
    let usage = match uploads::org_usage(&services.pool, &scope).await {
        Ok(Some(usage)) => usage,
        Ok(None) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "usage read failed"),
    };
    let mut rows = match uploads::workspace_usage(&services.pool, &scope, limit + 1, after).await {
        Ok(rows) => rows,
        Err(error) => return db_failure(&error, &headers, "usage read failed"),
    };
    let next = if rows.len() > limit as usize {
        rows.truncate(limit as usize);
        rows.last()
            .map(|last| encode_time_cursor("usage", &scope_text, last.created_at, last.id))
    } else {
        None
    };
    let items: Vec<_> = rows
        .iter()
        .map(|w| {
            // The effective limit is the smaller of the workspace and the
            // organization quota (lowering the organization quota does not
            // touch workspace quotas).
            let effective = match (w.quota_bytes, usage.quota_bytes) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            json!({
                "workspace_id": w.id.to_string(),
                "effective_limit_bytes": effective,
                "quota_bytes": w.quota_bytes,
                "reserved_bytes": w.reserved_bytes,
                "committed_bytes": w.committed_bytes,
            })
        })
        .collect();
    Json(json!({
        "organization": {
            "quota_bytes": usage.quota_bytes,
            "reserved_bytes": usage.reserved_bytes,
            "committed_bytes": usage.committed_bytes,
        },
        "workspaces": items,
        "next_cursor": next,
    }))
    .into_response()
}
