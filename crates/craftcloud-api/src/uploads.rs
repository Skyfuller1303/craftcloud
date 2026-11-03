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
    self, AbortOutcome, BeginOutcome, ChunkReuseCandidate, NewChunk, NewUpload, QuotaSetOutcome,
    StoreOutcome, UploadCreateOutcome, UploadSession,
};
use craftcloud_db::{OrgScope, files, organizations, workspaces};
use craftcloud_storage::{
    ObjectKey, ObjectStore, ReadSource, Sha256Digest, StorageError, WriteSink,
};
use http_body_util::BodyExt;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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

/// A bounded request body containing at most 32,768 manifest entries.
pub(crate) const CHUNK_MANIFEST_MAX_JSON_BYTES: usize = 4 * 1024 * 1024;
const CHUNK_MANIFEST_MAX_ENTRIES: usize = 32_768;

// ---- helpers -----------------------------------------------------------------

fn session_json(s: &UploadSession, missing_chunks: Option<Vec<u32>>) -> Value {
    let mut value = json!({
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
    });
    if let Some(missing_chunks) = missing_chunks
        && let Some(object) = value.as_object_mut()
    {
        object.insert("mode".to_owned(), json!("chunked"));
        object.insert("missing_chunks".to_owned(), json!(missing_chunks));
    }
    value
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
        &resolved.services,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        session.app_id.as_deref(),
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
        &resolved.services,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        headers,
        session.app_id.as_deref(),
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
            if matches!(action, Action::UploadWrite | Action::VersionCommit) {
                let live = tx.figcraft_blocks_write(file_id).await;
                match live {
                    Ok(true) => {
                        return Err(Box::new(conflict(
                            "FILE_IS_LIVE",
                            "Stop FigCraft Live before committing another version.",
                            headers,
                        )));
                    }
                    Ok(false) => {}
                    Err(_) => return Err(Box::new(internal(headers))),
                }
            }
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
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    chunks: Option<Vec<CreateUploadChunk>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateUploadChunk {
    size_bytes: u64,
    sha256: String,
}

fn parse_chunk_manifest(
    request: &CreateUploadRequest,
    size: i64,
    file_id: Uuid,
    identity_keys: &[[u8; 32]],
) -> Result<Option<Vec<uploads::ChunkManifestSpec>>, ()> {
    match (request.mode.as_deref(), request.chunks.as_ref()) {
        (None, None) => return Ok(None),
        (Some("chunked"), Some(_)) => {}
        _ => return Err(()),
    }
    let Some(chunks) = request.chunks.as_ref() else {
        return Err(());
    };
    if chunks.len() > CHUNK_MANIFEST_MAX_ENTRIES || identity_keys.is_empty() {
        return Err(());
    }
    let mut offset = 0i64;
    let key = identity_keys.first().ok_or(())?;
    let mut manifest = Vec::with_capacity(chunks.len());
    for (ordinal, chunk) in chunks.iter().enumerate() {
        let block_size = i64::try_from(chunk.size_bytes).map_err(|_| ())?;
        if !(1..=i64::from(craftcloud_storage::MAX_CHUNK_BYTES)).contains(&block_size)
            || (ordinal + 1 < chunks.len()
                && block_size < i64::from(craftcloud_storage::MIN_CHUNK_BYTES))
        {
            return Err(());
        }
        let sha256 = parse_sha256(&chunk.sha256).ok_or(())?;
        let digest =
            craftcloud_storage::chunk_digest_from_sha256(key, file_id, &sha256).ok_or(())?;
        manifest.push(uploads::ChunkManifestSpec {
            ordinal: i32::try_from(ordinal).map_err(|_| ())?,
            byte_offset: offset,
            size_bytes: block_size,
            sha256,
            digest,
        });
        offset = offset.checked_add(block_size).ok_or(())?;
    }
    if offset != size || (size == 0 && !chunks.is_empty()) || (size > 0 && chunks.is_empty()) {
        return Err(());
    }
    Ok(Some(manifest))
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
    let identity_keys = services.chunk_identity_keys();
    let chunk_manifest = match parse_chunk_manifest(&request, size, file_id, &identity_keys) {
        Ok(manifest) => manifest,
        Err(()) => return validation_failed(&["chunks"], &headers),
    };
    if request.mode.is_some() && (identity_keys.is_empty() || services.storage.is_none()) {
        return conflict(
            "CHUNKED_UPLOAD_UNAVAILABLE",
            "Chunked uploads are not enabled on this server.",
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
    let mut outcome = match tx.upload_create(&scope, &new).await {
        Ok(outcome) => outcome,
        Err(error) => return db_failure(&error, &headers, "upload creation failed"),
    };
    let mut missing_chunks = None;
    match (&chunk_manifest, outcome.clone()) {
        (Some(entries), UploadCreateOutcome::Created(created)) => {
            if let Err(error) = tx
                .upload_manifest_create(&scope, created.id, entries, now)
                .await
            {
                return db_failure(&error, &headers, "upload manifest creation failed");
            }
            let Some(store) = services.storage.as_ref() else {
                return storage_unavailable(&headers);
            };
            let mut verified_candidates = std::collections::HashMap::<Uuid, bool>::new();
            for entry in entries {
                let expected_digests: Vec<[u8; 32]> = identity_keys
                    .iter()
                    .filter_map(|identity_key| {
                        craftcloud_storage::chunk_digest_from_sha256(
                            identity_key,
                            file_id,
                            &entry.sha256,
                        )
                    })
                    .collect();
                let candidate = match tx
                    .upload_manifest_candidate(
                        &scope,
                        created.id,
                        entry.ordinal,
                        &expected_digests,
                        now,
                        Action::UploadCreate,
                    )
                    .await
                {
                    Ok(candidate) => candidate,
                    Err(error) => {
                        return db_failure(&error, &headers, "upload manifest lookup failed");
                    }
                };
                let Some(candidate) = candidate else {
                    continue;
                };
                let matched_digest: [u8; 32] = match candidate.digest.as_slice().try_into() {
                    Ok(digest) => digest,
                    Err(_) => return internal(&headers),
                };
                if let Err(error) = tx
                    .upload_manifest_set_digest(&scope, created.id, entry.ordinal, &matched_digest)
                    .await
                {
                    return db_failure(&error, &headers, "upload manifest update failed");
                }
                if candidate.state == "deleting"
                    || candidate.storage_tier != "local"
                    || candidate.size_bytes != entry.size_bytes
                    || candidate.sha256.as_slice() != entry.sha256
                {
                    continue;
                }
                let key = match ObjectKey::parse(&candidate.object_key) {
                    Ok(key) => key,
                    Err(_) => continue,
                };
                let size = u64::try_from(entry.size_bytes).unwrap_or(0);
                let verified = if let Some(verified) = verified_candidates.get(&candidate.id) {
                    *verified
                } else {
                    let verified = matches!(store.head(&key).await, Ok(head) if head.len == size)
                        && store.verify(&key, &entry.sha256).await.is_ok();
                    verified_candidates.insert(candidate.id, verified);
                    verified
                };
                if !verified {
                    continue;
                }
                if !matches!(
                    tx.upload_manifest_attach(
                        &scope,
                        created.id,
                        entry.ordinal,
                        candidate.id,
                        now,
                        Action::UploadCreate,
                    )
                    .await,
                    Ok(true)
                ) {
                    return internal(&headers);
                }
            }
            let rows = match tx
                .upload_manifest_rows(&scope, created.id, Action::UploadCreate)
                .await
            {
                Ok(Some(rows)) => rows,
                Ok(None) => return internal(&headers),
                Err(error) => return db_failure(&error, &headers, "upload manifest read failed"),
            };
            missing_chunks = Some(
                rows.into_iter()
                    .filter(|row| row.chunk_id.is_none())
                    .filter_map(|row| u32::try_from(row.ordinal).ok())
                    .collect(),
            );
        }
        (Some(entries), UploadCreateOutcome::Replayed(existing))
            if !matches!(existing.state.as_str(), "created" | "receiving" | "stored") =>
        {
            // Terminal session transitions remove the temporary manifest. The
            // database replay check already matched file, size and whole SHA.
            // Preserve that replay instead of treating deleted staging rows as
            // an idempotency-key conflict.
            missing_chunks = Some(if existing.state == "committed" {
                Vec::new()
            } else {
                entries
                    .iter()
                    .filter_map(|entry| u32::try_from(entry.ordinal).ok())
                    .collect()
            });
        }
        (Some(entries), UploadCreateOutcome::Replayed(existing)) => {
            let rows = match tx
                .upload_manifest_rows(&scope, existing.id, Action::UploadCreate)
                .await
            {
                Ok(Some(rows)) => rows,
                Ok(None) => {
                    if matches!(existing.state.as_str(), "created" | "receiving" | "stored") {
                        outcome = UploadCreateOutcome::KeyReused;
                    }
                    Vec::new()
                }
                Err(error) => return db_failure(&error, &headers, "upload manifest read failed"),
            };
            if rows.len() != entries.len()
                || rows.iter().zip(entries).any(|(row, entry)| {
                    row.ordinal != entry.ordinal
                        || row.byte_offset != entry.byte_offset
                        || row.size_bytes != entry.size_bytes
                        || row.sha256.as_slice() != entry.sha256
                })
            {
                outcome = UploadCreateOutcome::KeyReused;
            } else {
                missing_chunks = Some(
                    rows.into_iter()
                        .filter(|row| row.chunk_id.is_none())
                        .filter_map(|row| u32::try_from(row.ordinal).ok())
                        .collect(),
                );
            }
        }
        (None, UploadCreateOutcome::Created(_)) => {}
        (None, UploadCreateOutcome::Replayed(created)) => {
            if matches!(
                tx.upload_manifest_rows(&scope, created.id, Action::UploadCreate)
                    .await,
                Ok(Some(_))
            ) {
                outcome = UploadCreateOutcome::KeyReused;
            }
        }
        _ => {}
    }
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
            (
                StatusCode::CREATED,
                Json(session_json(&created, missing_chunks)),
            )
                .into_response()
        }
        UploadCreateOutcome::Replayed(existing) => (
            StatusCode::OK,
            Json(session_json(&existing, missing_chunks)),
        )
            .into_response(),
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
            let missing =
                match uploads::chunk_manifest_missing(&resolved.services.pool, &scope, resolved.id)
                    .await
                {
                    Ok(missing) => missing,
                    Err(error) => {
                        return db_failure(&error, &headers, "upload manifest read failed");
                    }
                };
            Json(session_json(&upload, missing)).into_response()
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

pub(crate) async fn local_chunk_matches(
    store: &craftcloud_storage::LocalStore,
    reuse: &ChunkReuseCandidate,
) -> bool {
    let Ok(key) = ObjectKey::parse(&reuse.existing_object_key) else {
        return false;
    };
    let Ok(size) = u64::try_from(reuse.size_bytes) else {
        return false;
    };
    matches!(store.head(&key).await, Ok(head) if head.len == size)
        && store.verify(&key, &reuse.sha256).await.is_ok()
}

/// After this long a receiving upload must have shown the minimum average
/// rate, or it is cancelled (slow-drip protection; the session would
/// otherwise hold quota and a slot until it expires).
pub(crate) const RATE_GRACE_SECS: i64 = 300;
/// Minimum average rate in bytes per second once the grace period is over.
pub(crate) const MIN_RATE_BYTES_PER_SEC: u64 = 16 * 1024;

pub(crate) async fn stage_chunk_manifest(
    store: &craftcloud_storage::LocalStore,
    source_key: &ObjectKey,
    size: u64,
    expected: Sha256Digest,
    file_id: Uuid,
    identity_keys: &[[u8; 32]],
) -> Result<(Vec<NewChunk>, Vec<ObjectKey>), StorageError> {
    let mut manifest = Vec::new();
    let mut keys = Vec::new();
    let result = async {
        store.verify(source_key, &expected).await?;
        let mut reader = store.open_read(source_key, None).await?;
        if reader.remaining() != size {
            return Err(StorageError::LengthMismatch);
        }
        let mut buffer =
            Vec::with_capacity(craftcloud_storage::MAX_CHUNK_BYTES as usize + 64 * 1024);
        let mut offset = 0u64;
        while let Some(bytes) = reader.next_chunk().await? {
            buffer.extend_from_slice(&bytes);
            write_ready_chunks(
                store,
                file_id,
                identity_keys,
                &mut buffer,
                false,
                &mut offset,
                &mut manifest,
                &mut keys,
            )
            .await?;
        }
        write_ready_chunks(
            store,
            file_id,
            identity_keys,
            &mut buffer,
            true,
            &mut offset,
            &mut manifest,
            &mut keys,
        )
        .await?;
        if offset != size || !buffer.is_empty() {
            return Err(StorageError::LengthMismatch);
        }
        Ok(())
    }
    .await;
    if result.is_err() {
        for key in &keys {
            delete_object_key(store, key).await;
        }
    }
    result?;
    Ok((manifest, keys))
}

async fn delete_object_key(store: &craftcloud_storage::LocalStore, key: &ObjectKey) {
    if let Err(error) = store.delete(key).await {
        tracing::error!(%error, "cannot delete an unpublished chunk candidate");
    }
}

#[allow(clippy::too_many_arguments)]
async fn write_ready_chunks(
    store: &craftcloud_storage::LocalStore,
    file_id: Uuid,
    identity_keys: &[[u8; 32]],
    buffer: &mut Vec<u8>,
    final_input: bool,
    offset: &mut u64,
    manifest: &mut Vec<NewChunk>,
    keys: &mut Vec<ObjectKey>,
) -> Result<(), StorageError> {
    let spans =
        craftcloud_storage::fastcdc_chunks(buffer).map_err(|_| StorageError::LengthMismatch)?;
    let ready = if final_input {
        spans.len()
    } else {
        spans.len().saturating_sub(1)
    };
    let mut consumed = 0usize;
    for span in spans.iter().take(ready) {
        let Some(content) = buffer.get(span.offset..span.offset + span.length) else {
            return Err(StorageError::LengthMismatch);
        };
        let Some(current_key) = identity_keys.first() else {
            return Err(StorageError::ChecksumMismatch);
        };
        let digest = craftcloud_storage::chunk_digest(current_key, file_id, content)
            .ok_or(StorageError::ChecksumMismatch)?;
        let mut digest_candidates = Vec::with_capacity(identity_keys.len());
        for identity_key in identity_keys {
            if let Some(candidate) =
                craftcloud_storage::chunk_digest(identity_key, file_id, content)
            {
                digest_candidates.push(candidate);
            }
        }
        let object_key = ObjectKey::generate();
        let chunk_len = u64::try_from(content.len()).map_err(|_| StorageError::TooLarge)?;
        let mut temp = store.begin_write_as(object_key.clone(), chunk_len).await?;
        if let Err(error) = temp.write(content).await {
            store.abort(temp).await;
            return Err(error);
        }
        let chunk_sha: Sha256Digest = Sha256::digest(content).into();
        let published = store.finalize(temp, chunk_len, &chunk_sha).await?;
        if published != object_key {
            return Err(StorageError::ChecksumMismatch);
        }
        keys.push(published.clone());
        let byte_offset = i64::try_from(*offset).map_err(|_| StorageError::TooLarge)?;
        let size_bytes = i64::try_from(chunk_len).map_err(|_| StorageError::TooLarge)?;
        manifest.push(NewChunk {
            digest,
            digest_candidates,
            sha256: chunk_sha,
            object_key: object_key.as_str().to_owned(),
            size_bytes,
            byte_offset,
        });
        *offset = offset
            .checked_add(chunk_len)
            .ok_or(StorageError::TooLarge)?;
        consumed = consumed
            .checked_add(span.length)
            .ok_or(StorageError::TooLarge)?;
    }
    if consumed > 0 {
        buffer.drain(..consumed);
    }
    Ok(())
}

/// Deletes a published object and fails its session unless disarmed: covers
/// a task that ends abnormally between publishing and recording the blob.
struct ObjectGuard {
    services: Arc<Services>,
    org_id: Uuid,
    id: Uuid,
    key: Option<ObjectKey>,
    chunk_keys: Vec<ObjectKey>,
}

/// Deletes an uploaded chunk candidate unless a successful or ambiguous DB
/// commit may have made it reachable.
struct ChunkObjectGuard {
    services: Arc<Services>,
    key: Option<ObjectKey>,
}

impl ChunkObjectGuard {
    fn disarm(&mut self) {
        self.key = None;
    }
}

impl Drop for ChunkObjectGuard {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            let services = Arc::clone(&self.services);
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move { delete_object(&services, &key).await });
            }
        }
    }
}

impl ObjectGuard {
    fn disarm(&mut self) {
        self.key = None;
        self.chunk_keys.clear();
    }
}

impl Drop for ObjectGuard {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else {
            if self.chunk_keys.is_empty() {
                return;
            }
            let services = Arc::clone(&self.services);
            let chunk_keys = std::mem::take(&mut self.chunk_keys);
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    for key in chunk_keys {
                        delete_object(&services, &key).await;
                    }
                });
            }
            return;
        };
        let (services, org_id, id, chunk_keys) = (
            Arc::clone(&self.services),
            self.org_id,
            self.id,
            std::mem::take(&mut self.chunk_keys),
        );
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                delete_object(&services, &key).await;
                for key in chunk_keys {
                    delete_object(&services, &key).await;
                }
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
    app_id: Option<String>,
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
        app_id,
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
    let identity_keys = services.chunk_identity_keys();
    let (chunk_manifest, chunk_keys) = if identity_keys.is_empty() {
        (None, Vec::new())
    } else {
        match stage_chunk_manifest(store, &key, size, expected, file_id, &identity_keys).await {
            Ok((manifest, keys)) => (Some(manifest), keys),
            Err(error) => {
                delete_object(&services, &key).await;
                fail_session(&services, org_id, upload_id).await;
                return storage_error(&error, &headers);
            }
        }
    };
    let mut guard = ObjectGuard {
        services: Arc::clone(&services),
        org_id,
        id: upload_id,
        key: Some(key.clone()),
        chunk_keys,
    };
    let Locked { mut tx, actor } = match lock(
        &services,
        user_id,
        org_id,
        workspace_id,
        &headers,
        app_id.as_deref(),
    )
    .await
    {
        Ok(locked) => locked,
        Err(response) => {
            guard.key = None;
            if chunk_manifest.is_some() {
                delete_object(&services, &key).await;
            }
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
            let organization_policy = match tx
                .session_organization_policy(
                    login_session_id,
                    user_id,
                    credential_kind == crate::session::CredentialKind::Device,
                    now,
                )
                .await
            {
                Ok(policy) => policy,
                Err(error) => {
                    guard.key = None;
                    drop(tx);
                    delete_object(&services, &key).await;
                    fail_session(&services, org_id, upload_id).await;
                    return db_failure(
                        &error,
                        &headers,
                        "organization session policy check failed",
                    );
                }
            };
            let global_session_valid = organization_policy.is_some();
            let organization_failure = match organization_policy {
                Some((craftcloud_db::sso::SessionPolicy::Revoked, _)) => Some((
                    "ORGANIZATION_REAUTHENTICATION_REQUIRED",
                    "Sign in again to access this organization.",
                )),
                Some((craftcloud_db::sso::SessionPolicy::SsoRequired, _)) => Some((
                    "SSO_REQUIRED",
                    "Sign in with this organization's company account.",
                )),
                Some((
                    craftcloud_db::sso::SessionPolicy::Allowed {
                        mfa: false,
                        sso: true,
                    },
                    true,
                )) => Some((
                    "SSO_MFA_REQUIRED",
                    "Sign in again with the required additional factor.",
                )),
                _ => None,
            };
            let setup_required = match if global_session_valid {
                tx.two_factor_setup_required(user_id).await
            } else {
                Ok(false)
            } {
                Ok(value) => value,
                Err(error) => {
                    guard.key = None;
                    drop(tx);
                    delete_object(&services, &key).await;
                    fail_session(&services, org_id, upload_id).await;
                    return db_failure(&error, &headers, "two-factor policy check failed");
                }
            };
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
            let mut response = if let Some((code, message)) = organization_failure {
                error_response(StatusCode::FORBIDDEN, code, message, &headers)
            } else if setup_required {
                error_response(
                    StatusCode::FORBIDDEN,
                    "TWO_FACTOR_SETUP_REQUIRED",
                    "Set up two-factor authentication to access this organization.",
                    &headers,
                )
            } else {
                error_response(
                    StatusCode::UNAUTHORIZED,
                    "UNAUTHENTICATED",
                    "Authentication is required.",
                    &headers,
                )
            };
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
    // A lease taken by another session while the body streamed in still blocks
    // the write: decided here under the organization lock, like the write gate.
    match tx.file_lock(file_id).await {
        Ok(Some(lock)) if lock.holder_session_id != login_session_id => {
            fail_in_tx(tx, &services, org_id, upload_id, now).await;
            guard.key = None;
            delete_object(&services, &key).await;
            return crate::projects::locked_response(&lock, login_session_id, user_id, &headers);
        }
        Ok(_) => {}
        Err(error) => {
            guard.key = None;
            drop(tx);
            delete_object(&services, &key).await;
            fail_session(&services, org_id, upload_id).await;
            return db_failure(&error, &headers, "file lock lookup failed");
        }
    }
    let (stored, mut unused_chunk_keys, reuse_candidates) = match chunk_manifest.as_deref() {
        Some(chunks) => match tx
            .upload_store_chunked(&scope, upload_id, key.as_str(), now, chunks)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                guard.key = None;
                drop(tx);
                delete_object(&services, &key).await;
                fail_session(&services, org_id, upload_id).await;
                return db_failure(&error, &headers, "upload store failed");
            }
        },
        None => match tx.upload_store(&scope, upload_id, key.as_str(), now).await {
            Ok(outcome) => (outcome, Vec::new(), Vec::new()),
            Err(error) => {
                guard.key = None;
                drop(tx);
                delete_object(&services, &key).await;
                fail_session(&services, org_id, upload_id).await;
                return db_failure(&error, &headers, "upload store failed");
            }
        },
    };
    match stored {
        StoreOutcome::Stored(upload, _blob) => {
            for reuse in &reuse_candidates {
                if local_chunk_matches(store, reuse).await {
                    continue;
                }
                match tx
                    .upload_replace_chunk_object(&scope, upload_id, reuse)
                    .await
                {
                    Ok(true) => {
                        unused_chunk_keys
                            .retain(|candidate| candidate != &reuse.candidate_object_key);
                        unused_chunk_keys.push(reuse.existing_object_key.clone());
                    }
                    Ok(false) | Err(_) => {
                        guard.key = None;
                        drop(tx);
                        delete_object(&services, &key).await;
                        fail_session(&services, org_id, upload_id).await;
                        return internal(&headers);
                    }
                }
            }
            // Crash test hook: object published, blob row not committed yet.
            craftcloud_storage::crash_point("upload-before-record", 0);
            if let Err(error) = tx.commit().await {
                // COMMIT acknowledgement can fail after PostgreSQL committed.
                // Preserve every candidate object; reconciliation and GC can
                // remove true strays after observing durable references.
                guard.disarm();
                fail_session(&services, org_id, upload_id).await;
                return db_failure(&error, &headers, "upload store failed");
            }
            for unused in unused_chunk_keys {
                if let Ok(unused) = ObjectKey::parse(&unused) {
                    guard.chunk_keys.retain(|candidate| candidate != &unused);
                    delete_object(&services, &unused).await;
                }
            }
            if chunk_manifest.is_some() {
                delete_object(&services, &key).await;
            }
            guard.disarm();
            audit(
                &headers,
                user_id,
                "upload.content",
                Some(org_id),
                Some(upload_id),
                "success",
            );
            Json(session_json(&upload, None)).into_response()
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
    match tx
        .upload_manifest_rows(&scope, upload_id, Action::UploadWrite)
        .await
    {
        Ok(Some(_)) => {
            return conflict(
                "UPLOAD_MODE_MISMATCH",
                "This session accepts chunk blocks, not whole-file content.",
                &headers,
            );
        }
        Ok(None) => {}
        Err(error) => return db_failure(&error, &headers, "upload manifest read failed"),
    }
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
        app_id: session.app_id.clone(),
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

/// PUT one missing block of a client-provided chunk manifest.
pub(crate) async fn put_chunk(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path((raw, ordinal_raw)): Path<(String, String)>,
    body: Body,
) -> Response {
    let Ok(ordinal) = ordinal_raw.parse::<u32>() else {
        return not_found(&headers);
    };
    let Ok(ordinal_i32) = i32::try_from(ordinal) else {
        return not_found(&headers);
    };
    let (resolved, creator, file_id) = match resolve_upload(&state, &headers, &raw).await {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let services = Arc::clone(&resolved.services);
    let now = services.clock.now();
    let (mut tx, scope) = match upload_write_gate(
        &resolved,
        &session,
        &headers,
        Action::UploadWrite,
        "upload.chunk",
        creator,
        file_id,
    )
    .await
    {
        Ok(opened) => opened,
        Err(response) => return *response,
    };
    let upload = match tx
        .upload_get(&scope, resolved.id, Action::UploadWrite)
        .await
    {
        Ok(Some(upload)) => upload,
        Ok(None) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "upload read failed"),
    };
    if upload.state != "created" {
        return conflict(
            "UPLOAD_CLOSED",
            "This upload cannot receive blocks.",
            &headers,
        );
    }
    if upload.expires_at <= now {
        let _ = tx.expire_due(now).await;
        let _ = tx.commit().await;
        return expired(&headers);
    }
    let manifest_rows = match tx
        .upload_manifest_rows(&scope, resolved.id, Action::UploadWrite)
        .await
    {
        Ok(Some(rows)) => rows,
        Ok(None) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "upload manifest read failed"),
    };
    let Some(entry) = manifest_rows
        .iter()
        .find(|entry| entry.ordinal == ordinal_i32)
    else {
        return not_found(&headers);
    };
    if !matches!(
        tx.session_valid(
            session.session_id,
            session.user_id,
            now,
            session.credential_kind == crate::session::CredentialKind::Device,
        )
        .await,
        Ok(true)
    ) {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
            "Authentication is required.",
            &headers,
        );
    }
    let size = match u64::try_from(entry.size_bytes) {
        Ok(size) if size <= u64::from(craftcloud_storage::MAX_CHUNK_BYTES) => size,
        _ => return internal(&headers),
    };
    let expected_sha: Sha256Digest = match entry.sha256.as_slice().try_into() {
        Ok(digest) => digest,
        Err(_) => return internal(&headers),
    };
    let expected_digest: [u8; 32] = match entry.digest.as_slice().try_into() {
        Ok(digest) => digest,
        Err(_) => return internal(&headers),
    };
    if let Err(error) = tx.commit().await {
        return db_failure(&error, &headers, "upload start failed");
    }
    let Some(store) = services.storage.as_ref() else {
        return storage_unavailable(&headers);
    };
    if headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|declared| declared > size)
    {
        return storage_error(&StorageError::TooLarge, &headers);
    }
    let candidate_key = ObjectKey::generate();
    let mut temp = match store.begin_write_as(candidate_key.clone(), size).await {
        Ok(temp) => temp,
        Err(error) => return storage_error(&error, &headers),
    };
    let started = tokio::time::Instant::now();
    let ttl: Duration = (upload.expires_at - now)
        .try_into()
        .unwrap_or(Duration::ZERO);
    let deadline = started + ttl;
    let idle = services.settings.upload_idle_timeout;
    let mut body = body;
    loop {
        if services.clock.now() >= upload.expires_at {
            store.abort(temp).await;
            return expired(&headers);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            store.abort(temp).await;
            return expired(&headers);
        }
        match tokio::time::timeout(idle.min(remaining), body.frame()).await {
            Err(_) => {
                store.abort(temp).await;
                return if tokio::time::Instant::now() >= deadline {
                    expired(&headers)
                } else {
                    deadline_error(&headers)
                };
            }
            Ok(None) => break,
            Ok(Some(Err(_))) => {
                store.abort(temp).await;
                return interrupted(&headers);
            }
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    services.metrics.add_uploaded(data.len() as u64);
                    if let Err(error) = temp.write(&data).await {
                        store.abort(temp).await;
                        return storage_error(&error, &headers);
                    }
                }
            }
        }
    }
    let candidate_key = match store.finalize(temp, size, &expected_sha).await {
        Ok(key) if key == candidate_key => key,
        Ok(key) => {
            delete_object(&services, &key).await;
            return internal(&headers);
        }
        Err(error) => return storage_error(&error, &headers),
    };
    let mut guard = ChunkObjectGuard {
        services: Arc::clone(&services),
        key: Some(candidate_key.clone()),
    };

    let (mut tx, scope) = match upload_write_gate(
        &resolved,
        &session,
        &headers,
        Action::UploadWrite,
        "upload.chunk",
        creator,
        file_id,
    )
    .await
    {
        Ok(opened) => opened,
        Err(response) => return *response,
    };
    let now = services.clock.now();
    if !matches!(
        tx.session_valid(
            session.session_id,
            session.user_id,
            now,
            session.credential_kind == crate::session::CredentialKind::Device,
        )
        .await,
        Ok(true)
    ) {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
            "Authentication is required.",
            &headers,
        );
    }
    let current_upload = match tx
        .upload_get(&scope, resolved.id, Action::UploadWrite)
        .await
    {
        Ok(Some(upload)) => upload,
        Ok(None) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "upload read failed"),
    };
    if current_upload.state != "created" {
        return conflict(
            "UPLOAD_CLOSED",
            "This upload cannot receive blocks.",
            &headers,
        );
    }
    if current_upload.expires_at <= now {
        let _ = tx.expire_due(now).await;
        let _ = tx.commit().await;
        return expired(&headers);
    }
    let rows = match tx
        .upload_manifest_rows(&scope, resolved.id, Action::UploadWrite)
        .await
    {
        Ok(Some(rows)) => rows,
        Ok(None) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "upload manifest read failed"),
    };
    let Some(current) = rows.iter().find(|row| row.ordinal == ordinal_i32) else {
        return not_found(&headers);
    };
    if current.size_bytes != i64::try_from(size).unwrap_or(-1)
        || current.sha256.as_slice() != expected_sha
        || current.digest.as_slice() != expected_digest
    {
        return conflict(
            "UPLOAD_CHANGED",
            "The upload manifest changed; create a new upload session.",
            &headers,
        );
    }
    let already_good = if current.storage_tier.as_deref() == Some("local") {
        if let Some(key) = current.object_key.as_deref() {
            key_matches(store, key, size, &expected_sha).await
        } else {
            false
        }
    } else {
        false
    };
    let mut prior_key = None;
    let mut adopted = false;
    if already_good {
        let Some(chunk_id) = current.chunk_id else {
            return internal(&headers);
        };
        if !matches!(
            tx.upload_manifest_attach(
                &scope,
                resolved.id,
                ordinal_i32,
                chunk_id,
                now,
                Action::UploadWrite,
            )
            .await,
            Ok(true)
        ) {
            return internal(&headers);
        }
    } else {
        let matching = match tx
            .upload_manifest_candidate(
                &scope,
                resolved.id,
                ordinal_i32,
                &[expected_digest],
                now,
                Action::UploadWrite,
            )
            .await
        {
            Ok(candidate) => candidate,
            Err(error) => return db_failure(&error, &headers, "chunk lookup failed"),
        };
        let reusable = if let Some(candidate) = &matching {
            candidate.sha256.as_slice() == expected_sha
                && candidate.size_bytes == i64::try_from(size).unwrap_or(-1)
                && candidate.storage_tier == "local"
                && key_matches(store, &candidate.object_key, size, &expected_sha).await
        } else {
            false
        };
        if reusable {
            let Some(candidate) = matching else {
                return internal(&headers);
            };
            if !matches!(
                tx.upload_manifest_attach(
                    &scope,
                    resolved.id,
                    ordinal_i32,
                    candidate.id,
                    now,
                    Action::UploadWrite,
                )
                .await,
                Ok(true)
            ) {
                return internal(&headers);
            }
        } else {
            prior_key = match tx
                .upload_manifest_store_chunk(
                    &scope,
                    resolved.id,
                    ordinal_i32,
                    &expected_digest,
                    &expected_sha,
                    i64::try_from(size).unwrap_or(-1),
                    candidate_key.as_str(),
                    now,
                )
                .await
            {
                Ok(Some(prior)) => prior,
                Ok(None) => {
                    return conflict(
                        "UPLOAD_CLOSED",
                        "The upload was closed before the block could be stored.",
                        &headers,
                    );
                }
                Err(error) => return db_failure(&error, &headers, "chunk record failed"),
            };
            adopted = true;
        }
    }
    if let Err(error) = tx.commit().await {
        if adopted {
            guard.disarm();
        }
        return db_failure(&error, &headers, "chunk record failed");
    }
    if adopted {
        guard.disarm();
    }
    if let Some(old_key) = prior_key.and_then(|key| ObjectKey::parse(&key).ok()) {
        delete_object(&services, &old_key).await;
    }
    audit(
        &headers,
        session.user_id,
        "upload.chunk",
        Some(resolved.org_id),
        Some(resolved.id),
        "success",
    );
    StatusCode::NO_CONTENT.into_response()
}

async fn key_matches(
    store: &craftcloud_storage::LocalStore,
    key: &str,
    size: u64,
    expected: &Sha256Digest,
) -> bool {
    let Ok(key) = ObjectKey::parse(key) else {
        return false;
    };
    matches!(store.head(&key).await, Ok(head) if head.len == size)
        && store.verify(&key, expected).await.is_ok()
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
    let (physical_bytes, physical_workspaces) =
        match uploads::physical_usage(&services.pool, &scope).await {
            Ok(usage) => usage,
            Err(error) => return db_failure(&error, &headers, "physical usage read failed"),
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
                "physical_bytes": physical_workspaces.iter()
                    .find(|physical| physical.workspace_id == w.id)
                    .map_or(0, |physical| physical.physical_bytes),
            })
        })
        .collect();
    Json(json!({
        "organization": {
            "quota_bytes": usage.quota_bytes,
            "reserved_bytes": usage.reserved_bytes,
            "committed_bytes": usage.committed_bytes,
            "physical_bytes": physical_bytes,
        },
        "workspaces": items,
        "next_cursor": next,
    }))
    .into_response()
}

#[cfg(test)]
mod object_guard_tests {
    use super::*;
    use craftcloud_storage::{ObjectStore, WriteSink};

    #[tokio::test]
    async fn uncertain_commit_preserves_full_and_chunk_candidate_objects() {
        let root = std::env::temp_dir().join(format!("craftcloud-object-guard-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let store = craftcloud_storage::LocalStore::open(&root).await.unwrap();
        let full_key = ObjectKey::generate();
        let chunk_key = ObjectKey::generate();
        let bytes = b"committed candidate";
        let digest: Sha256Digest = Sha256::digest(bytes).into();
        for key in [&full_key, &chunk_key] {
            let mut writer = store
                .begin_write_as(key.clone(), bytes.len() as u64)
                .await
                .unwrap();
            writer.write(bytes).await.unwrap();
            store
                .finalize(writer, bytes.len() as u64, &digest)
                .await
                .unwrap();
        }
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .unwrap();
        let services = Arc::new(
            Services::new(pool, None, crate::AuthSettings::default()).with_storage(store.clone()),
        );
        let mut guard = ObjectGuard {
            services,
            org_id: Uuid::new_v4(),
            id: Uuid::new_v4(),
            key: Some(full_key.clone()),
            chunk_keys: vec![chunk_key.clone()],
        };

        // The COMMIT response was lost after the server may have committed.
        guard.disarm();
        drop(guard);

        assert_eq!(store.head(&full_key).await.unwrap().len, 19);
        assert_eq!(store.head(&chunk_key).await.unwrap().len, 19);
        std::fs::remove_dir_all(root).unwrap();
    }
}
