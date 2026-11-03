//! File versions, compare-and-set commits, restores and downloads (CC-F08).
//!
//! Two ETags exist on a file: the metadata ETag `"file-v<n>"` (name changes,
//! `PATCH /files/{id}`) and the content ETag `"version:<uuid>"` (the head
//! version, used for `If-Match` / `If-None-Match` on commits and restores and
//! returned with downloads). The organization, workspace and creator of every
//! request come from rows, never from the client; callers who cannot see the
//! workspace get the uniform 404. Audit lines carry user and version ids only.
//!
//! Downloads re-check the caller's roles at request start, always answer
//! `application/octet-stream` with `nosniff`, a sanitized attachment name, the
//! SHA-256 of the representation and `no-store`. A full download is hashed
//! while streaming and the last chunk is held back until the digest matches,
//! so a corrupted object ends as an aborted body, never as a clean one.

use std::sync::Arc;

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::Engine as _;
use craftcloud_core::policy::Action;
use craftcloud_db::members::OrgTx;
use craftcloud_db::versions::{
    self, CommitOutcome, DownloadChunk, DownloadInfo, DownloadTarget, FileVersion, Precondition,
    RestoreOutcome,
};
use craftcloud_db::{OrgScope, files, uploads};
use craftcloud_storage::{ByteRange, ObjectKey, ObjectStore, ReadSource, StorageError, WriteSink};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::audit::{Change, commit_audited, ctx as audit_ctx, record_change, record_important};
use crate::orgs::{audit, conflict, db_failure, not_found, page_error, parse_id, rfc3339};
use crate::pagination::{PageQuery, decode_number_cursor, encode_number_cursor};
use crate::projects::{Resolved, read_gate, write_gate};
use crate::responses::{internal, rejection_response, unavailable, validation_failed};
use crate::session::AuthenticatedSession;
use crate::uploads::upload_write_gate;
use crate::{ApiState, Services, error_response, error_response_with_details};
use craftcloud_db::audit::{AggScope, Detail, NewEvent};

// ---- helpers ------------------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn version_tag(id: Uuid) -> String {
    format!("\"version:{id}\"")
}

pub(crate) fn version_json(v: &FileVersion) -> Value {
    json!({
        "id": v.id.to_string(),
        "file_id": v.file_id.to_string(),
        "number": v.number,
        "size_bytes": v.size_bytes,
        "sha256": hex(&v.sha256),
        "base_version_id": v.base_version_id.map(|id| id.to_string()),
        "restored_from_version_id": v.restored_from_version_id.map(|id| id.to_string()),
        "upload_id": v.upload_session_id.map(|id| id.to_string()),
        "created_by": v.created_by_user_id.to_string(),
        "actor_kind": v.actor_kind,
        "storage_tier": v.storage_tier,
        "pinned": v.pinned,
        "created_at": rfc3339(v.created_at),
    })
}

fn version_response(status: StatusCode, v: &FileVersion) -> Response {
    let mut response = (status, Json(version_json(v))).into_response();
    if let Ok(tag) = HeaderValue::from_str(&version_tag(v.id)) {
        response.headers_mut().insert(header::ETAG, tag);
    }
    response
}

/// 412 with the current head (ADR-0009).
fn precondition_failed(current: Option<Uuid>, headers: &HeaderMap) -> Response {
    let mut response = error_response_with_details(
        StatusCode::PRECONDITION_FAILED,
        "VERSION_CONFLICT",
        "The file changed since it was read.",
        headers,
        Some(json!({ "current_version_id": current.map(|id| id.to_string()) })),
    );
    if let Some(id) = current
        && let Ok(tag) = HeaderValue::from_str(&version_tag(id))
    {
        response.headers_mut().insert(header::ETAG, tag);
    }
    response
}

fn parse_version_tag(raw: &str) -> Option<Uuid> {
    let inner = raw.trim().strip_prefix("\"version:")?.strip_suffix('"')?;
    Uuid::parse_str(inner).ok()
}

fn precondition_required(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::PRECONDITION_REQUIRED,
        "PRECONDITION_REQUIRED",
        "If-None-Match: * (first version) or If-Match with the current version tag is required.",
        headers,
    )
}

/// `If-None-Match: *` or `If-Match: "version:<id>"`; `None` when neither is sent.
fn optional_precondition(headers: &HeaderMap) -> Result<Option<Precondition>, Box<Response>> {
    let if_match = headers.get(header::IF_MATCH);
    let if_none = headers.get(header::IF_NONE_MATCH);
    match (if_match, if_none) {
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err(Box::new(validation_failed(&["If-Match"], headers))),
        (Some(value), None) => {
            // `*`, lists and weak tags would defeat the compare-and-set or are
            // ambiguous: they are not accepted.
            let text = value.to_str().unwrap_or("").trim();
            if text == "*" || text.contains(',') || text.starts_with("W/") || text.is_empty() {
                return Err(Box::new(validation_failed(&["If-Match"], headers)));
            }
            Ok(Some(Precondition::Head(parse_version_tag(text))))
        }
        (None, Some(value)) => {
            if value.to_str().is_ok_and(|v| v.trim() == "*") {
                Ok(Some(Precondition::NoVersionYet))
            } else {
                Err(Box::new(validation_failed(&["If-None-Match"], headers)))
            }
        }
    }
}

/// The precondition of a request that cannot go on without one.
fn required_precondition(headers: &HeaderMap) -> Result<Precondition, Box<Response>> {
    optional_precondition(headers)?.ok_or_else(|| Box::new(precondition_required(headers)))
}

fn same_manifest_refs(
    expected: &[craftcloud_db::uploads::ChunkManifestRow],
    current: &[craftcloud_db::uploads::ChunkManifestRow],
) -> bool {
    expected.len() == current.len()
        && expected.iter().zip(current).all(|(before, after)| {
            before.ordinal == after.ordinal
                && before.byte_offset == after.byte_offset
                && before.size_bytes == after.size_bytes
                && before.sha256 == after.sha256
                && before.digest == after.digest
                && before.chunk_id == after.chunk_id
        })
}

/// Verifies every pinned block and the assembled file digest with bounded
/// memory. Call before reacquiring the organization write lock.
async fn verify_chunked_manifest(
    store: &craftcloud_storage::LocalStore,
    rows: &[craftcloud_db::uploads::ChunkManifestRow],
    expected_size: i64,
    expected_sha256: &[u8; 32],
) -> Result<bool, StorageError> {
    let mut total = 0i64;
    let mut whole = Sha256::new();
    for (expected_ordinal, row) in rows.iter().enumerate() {
        if row.ordinal != i32::try_from(expected_ordinal).unwrap_or(-1)
            || row.byte_offset != total
            || row.chunk_id.is_none()
            || row.storage_tier.as_deref() != Some("local")
        {
            return Ok(false);
        }
        let size = u64::try_from(row.size_bytes).map_err(|_| StorageError::LengthMismatch)?;
        let Some(raw_key) = row.object_key.as_deref() else {
            return Ok(false);
        };
        let key = ObjectKey::parse(raw_key).map_err(|_| StorageError::NotFound)?;
        let expected_chunk_sha: [u8; 32] = row
            .sha256
            .as_slice()
            .try_into()
            .map_err(|_| StorageError::ChecksumMismatch)?;
        let stored_size = u64::try_from(row.stored_bytes.ok_or(StorageError::LengthMismatch)?)
            .map_err(|_| StorageError::LengthMismatch)?;
        let encoding = row
            .encoding
            .as_deref()
            .ok_or(StorageError::LengthMismatch)?;
        if stored_size == 0 || stored_size > craftcloud_storage::MAX_CHUNK_BYTES as u64 {
            return Ok(false);
        }
        let head = store.head(&key).await?;
        if head.len != stored_size {
            return Ok(false);
        }
        let mut reader = store.open_read(&key, None).await?;
        if reader.remaining() != stored_size {
            return Ok(false);
        }
        let stored_capacity = usize::try_from(stored_size).map_err(|_| StorageError::TooLarge)?;
        let mut stored = Vec::with_capacity(stored_capacity);
        while let Some(bytes) = reader.next_chunk().await? {
            if stored.len().saturating_add(bytes.len()) > stored_capacity {
                return Ok(false);
            }
            stored.extend_from_slice(&bytes);
        }
        if stored.len() != stored_capacity {
            return Ok(false);
        }
        let plaintext = craftcloud_storage::compression::decode_chunk_bounded(
            encoding.to_owned(),
            stored,
            size,
            expected_chunk_sha,
        )
        .await
        .map_err(|_| StorageError::ChecksumMismatch)?;
        whole.update(&plaintext);
        total = total
            .checked_add(row.size_bytes)
            .ok_or(StorageError::TooLarge)?;
    }
    let actual_whole_sha: [u8; 32] = whole.finalize().into();
    Ok(total == expected_size && &actual_whole_sha == expected_sha256)
}

async fn resolve_file(
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
    match files::locate(&services.pool, id).await {
        Ok(Some(location)) => Ok(Resolved {
            services,
            org_id: location.org_id,
            workspace_id: location.workspace_id,
            id,
        }),
        Ok(None) => Err(Box::new(not_found(headers))),
        Err(error) => Err(Box::new(db_failure(&error, headers, "file lookup failed"))),
    }
}

fn file_deleted(headers: &HeaderMap) -> Response {
    conflict("FILE_DELETED", "The file is in the trash.", headers)
}

fn project_deleted(headers: &HeaderMap) -> Response {
    conflict("PROJECT_DELETED", "The project is in the trash.", headers)
}

// ---- POST /files/{id}/versions ---------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommitRequest {
    upload_id: String,
}

pub(crate) async fn commit_version(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(file_raw): Path<String>,
    body: Result<Json<CommitRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    // The syntax of the body is checked before any file is looked up: a
    // malformed `upload_id` must answer the same for existing, foreign and
    // unknown files (no existence oracle).
    let Some(upload_id) = parse_id(&request.upload_id) else {
        return validation_failed(&["upload_id"], &headers);
    };
    let resolved = match resolve_file(&state, &headers, &file_raw).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let services = Arc::clone(&resolved.services);
    // The upload must live in the same workspace as the file; anything else
    // looks like an unknown upload.
    let (creator, upload_file_id) = match uploads::locate(&services.pool, upload_id).await {
        Ok(Some(location))
            if location.org_id == resolved.org_id
                && location.workspace_id == resolved.workspace_id =>
        {
            (location.creator, location.file_id)
        }
        Ok(_) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "upload lookup failed"),
    };
    if upload_file_id != resolved.id {
        return not_found(&headers);
    }
    let upload_resolved = Resolved {
        services: Arc::clone(&services),
        org_id: resolved.org_id,
        workspace_id: resolved.workspace_id,
        id: upload_id,
    };
    let (mut tx, scope) = match upload_write_gate(
        &upload_resolved,
        &session,
        &headers,
        Action::VersionCommit,
        "version.commit",
        creator,
        upload_file_id,
    )
    .await
    {
        Ok(opened) => opened,
        Err(response) => return *response,
    };
    let pre = match optional_precondition(&headers) {
        Ok(pre) => pre,
        Err(response) => return *response,
    };
    // An Idempotency-Key, if sent, must be the one the upload was created with.
    if let Some(sent) = headers.get("idempotency-key") {
        match tx
            .upload_get(&scope, upload_id, Action::VersionCommit)
            .await
        {
            Ok(Some(upload)) => {
                if sent.to_str().ok() != upload.idempotency_key.as_deref() {
                    return conflict(
                        "IDEMPOTENCY_KEY_REUSED",
                        "The Idempotency-Key does not match the one of this upload.",
                        &headers,
                    );
                }
            }
            Ok(None) => return not_found(&headers),
            Err(error) => return db_failure(&error, &headers, "upload read failed"),
        }
    }
    let now = services.clock.now();
    let chunk_manifest = match tx
        .upload_manifest_rows(&scope, upload_id, Action::VersionCommit)
        .await
    {
        Ok(manifest) => manifest,
        Err(error) => return db_failure(&error, &headers, "upload manifest read failed"),
    };
    let (mut tx, scope, commit_now) = if let Some(manifest) = chunk_manifest {
        let upload = match tx
            .upload_get(&scope, upload_id, Action::VersionCommit)
            .await
        {
            Ok(Some(upload)) => upload,
            Ok(None) => return not_found(&headers),
            Err(error) => return db_failure(&error, &headers, "upload read failed"),
        };
        if upload.state != "committed" && pre.is_none() {
            return precondition_required(&headers);
        }
        if matches!(upload.state.as_str(), "created" | "stored") {
            if manifest.iter().any(|entry| entry.chunk_id.is_none()) {
                return conflict(
                    "UPLOAD_INCOMPLETE",
                    "Upload every missing block before committing this version.",
                    &headers,
                );
            }
            if let Err(error) = tx.commit().await {
                return db_failure(&error, &headers, "upload verification start failed");
            }
            let Some(store) = services.storage.as_ref() else {
                return unavailable(&headers, "STORAGE_UNAVAILABLE");
            };
            let expected_sha: [u8; 32] = match upload.declared_sha256.as_slice().try_into() {
                Ok(digest) => digest,
                Err(_) => return internal(&headers),
            };
            match verify_chunked_manifest(store, &manifest, upload.declared_size, &expected_sha)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    return error_response(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "CHECKSUM_MISMATCH",
                        "The assembled upload does not match the declared SHA-256.",
                        &headers,
                    );
                }
                Err(error) => return local_write_error(error, &headers),
            }
            let (mut fresh_tx, fresh_scope) = match upload_write_gate(
                &upload_resolved,
                &session,
                &headers,
                Action::VersionCommit,
                "version.commit",
                creator,
                upload_file_id,
            )
            .await
            {
                Ok(opened) => opened,
                Err(response) => return *response,
            };
            let current = match fresh_tx
                .upload_manifest_rows(&fresh_scope, upload_id, Action::VersionCommit)
                .await
            {
                Ok(Some(rows)) => rows,
                Ok(None) => {
                    return conflict("UPLOAD_CHANGED", "The upload manifest changed.", &headers);
                }
                Err(error) => return db_failure(&error, &headers, "upload manifest read failed"),
            };
            if !same_manifest_refs(&manifest, &current) {
                return conflict(
                    "UPLOAD_CHANGED",
                    "The upload blocks changed during verification; retry the commit.",
                    &headers,
                );
            }
            let fresh_now = services.clock.now();
            if !matches!(
                fresh_tx
                    .session_valid(
                        session.session_id,
                        session.user_id,
                        fresh_now,
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
            if upload.state == "created" {
                let staged_key = ObjectKey::generate();
                match fresh_tx
                    .upload_store_chunk_manifest(
                        &fresh_scope,
                        upload_id,
                        staged_key.as_str(),
                        fresh_now,
                    )
                    .await
                {
                    Ok(craftcloud_db::uploads::StoreOutcome::Stored(_, _)) => {}
                    Ok(craftcloud_db::uploads::StoreOutcome::NotReceiving(state))
                        if state == "expired" =>
                    {
                        let _ = fresh_tx.commit().await;
                        return error_response(
                            StatusCode::GONE,
                            "UPLOAD_EXPIRED",
                            "The upload session has expired; create a new one.",
                            &headers,
                        );
                    }
                    Ok(craftcloud_db::uploads::StoreOutcome::NotReceiving(state))
                        if state == "created" =>
                    {
                        return conflict(
                            "UPLOAD_INCOMPLETE",
                            "Upload every missing block before committing this version.",
                            &headers,
                        );
                    }
                    Ok(craftcloud_db::uploads::StoreOutcome::NotReceiving(_)) => {
                        return conflict(
                            "UPLOAD_CLOSED",
                            "This upload cannot be committed.",
                            &headers,
                        );
                    }
                    Ok(craftcloud_db::uploads::StoreOutcome::Gone) => return not_found(&headers),
                    Ok(craftcloud_db::uploads::StoreOutcome::FileDeleted) => {
                        return file_deleted(&headers);
                    }
                    Ok(craftcloud_db::uploads::StoreOutcome::ProjectDeleted) => {
                        return project_deleted(&headers);
                    }
                    Err(error) => return db_failure(&error, &headers, "upload staging failed"),
                }
            }
            (fresh_tx, fresh_scope, fresh_now)
        } else {
            (tx, scope, now)
        }
    } else {
        (tx, scope, now)
    };
    let outcome = match tx
        .version_commit(
            &scope,
            resolved.id,
            upload_id,
            session.user_id,
            pre,
            commit_now,
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return db_failure(&error, &headers, "commit failed"),
    };
    // One event per new version; a replay or a refusal records nothing. The
    // event commits with the version (lazy expiry done on the way is
    // committed with every outcome).
    if let CommitOutcome::Committed(version) = &outcome {
        let change = Change::new(
            &headers,
            session.user_id,
            "version.commit",
            resolved.org_id,
            Some(version.id),
        )
        .workspace(resolved.workspace_id)
        .detail("file_id", Detail::Id(resolved.id))
        .detail("number", Detail::Count(version.number));
        if let Err(error) = record_change(&mut tx, &change).await {
            return db_failure(&error, &headers, "commit failed");
        }
    }
    if let Err(error) = tx.commit().await {
        return db_failure(&error, &headers, "commit failed");
    }
    match outcome {
        CommitOutcome::Committed(version) => {
            // Crash test hook: committed, response not sent yet.
            craftcloud_storage::crash_point("version-after-commit", 0);
            audit(
                &headers,
                session.user_id,
                "version.commit",
                Some(resolved.org_id),
                Some(version.id),
                "success",
            );
            version_response(StatusCode::CREATED, &version)
        }
        CommitOutcome::Replayed(version) => version_response(StatusCode::OK, &version),
        CommitOutcome::PreconditionFailed { current } => precondition_failed(current, &headers),
        CommitOutcome::PreconditionMissing => precondition_required(&headers),
        CommitOutcome::Gone => not_found(&headers),
        CommitOutcome::NotStored(_) => conflict(
            "UPLOAD_INCOMPLETE",
            "The content of this upload has not been stored yet.",
            &headers,
        ),
        CommitOutcome::Closed(_) => conflict(
            "UPLOAD_CLOSED",
            "This upload has no stored content; create a new one.",
            &headers,
        ),
        CommitOutcome::Expired => error_response(
            StatusCode::GONE,
            "UPLOAD_EXPIRED",
            "The upload session has expired; create a new one.",
            &headers,
        ),
        CommitOutcome::FileDeleted => file_deleted(&headers),
        CommitOutcome::ProjectDeleted => project_deleted(&headers),
    }
}

// ---- GET /files/{id}/versions, GET /files/{id}/versions/{vid} ----------------------------

pub(crate) async fn list_versions(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(file_raw): Path<String>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let resolved = match resolve_file(&state, &headers, &file_raw).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let scope = match read_gate(
        &resolved,
        &session,
        &headers,
        Action::VersionList,
        "version.list",
        resolved.id,
    )
    .await
    {
        Ok(scope) => scope,
        Err(response) => return *response,
    };
    let Ok(Query(page)) = query else {
        return validation_failed(&["query"], &headers);
    };
    let limit = match page.limit() {
        Ok(limit) => limit,
        Err(error) => return page_error(error, &headers),
    };
    let scope_text = resolved.id.to_string();
    let before = match page
        .cursor()
        .map(|c| decode_number_cursor(c, "versions", &scope_text))
    {
        Some(Ok(number)) => Some(number),
        Some(Err(error)) => return page_error(error, &headers),
        None => None,
    };
    let mut rows = match versions::list(
        &resolved.services.pool,
        &scope,
        resolved.id,
        limit + 1,
        before,
    )
    .await
    {
        Ok(rows) => rows,
        Err(error) => return db_failure(&error, &headers, "version list failed"),
    };
    let next = if rows.len() > limit as usize {
        rows.truncate(limit as usize);
        rows.last()
            .map(|last| encode_number_cursor("versions", &scope_text, last.number))
    } else {
        None
    };
    let items: Vec<_> = rows.iter().map(version_json).collect();
    Json(json!({ "items": items, "next_cursor": next })).into_response()
}

pub(crate) async fn get_version(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path((file_raw, version_raw)): Path<(String, String)>,
) -> Response {
    let resolved = match resolve_file(&state, &headers, &file_raw).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let scope = match read_gate(
        &resolved,
        &session,
        &headers,
        Action::VersionRead,
        "version.read",
        resolved.id,
    )
    .await
    {
        Ok(scope) => scope,
        Err(response) => return *response,
    };
    let Some(version_id) = parse_id(&version_raw) else {
        return not_found(&headers);
    };
    match versions::get(&resolved.services.pool, &scope, resolved.id, version_id).await {
        Ok(Some(version)) => version_response(StatusCode::OK, &version),
        Ok(None) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "version read failed"),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PinVersionRequest {
    pinned: bool,
}

pub(crate) async fn pin_version(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path((file_raw, version_raw)): Path<(String, String)>,
    body: Result<Json<PinVersionRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(request)) => request,
        Err(error) => return rejection_response(&error, &headers),
    };
    let resolved = match resolve_file(&state, &headers, &file_raw).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let Some(version_id) = parse_id(&version_raw) else {
        return not_found(&headers);
    };
    let (mut tx, scope) = match write_gate(
        &resolved,
        &session,
        &headers,
        Action::VersionPin,
        if request.pinned {
            "version.pin"
        } else {
            "version.unpin"
        },
        resolved.id,
    )
    .await
    {
        Ok(opened) => opened,
        Err(response) => return *response,
    };
    let outcome = match tx
        .version_pin(
            &scope,
            resolved.id,
            version_id,
            request.pinned,
            &audit_ctx(&headers, Some(session.user_id)),
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => return db_failure(&error, &headers, "version pin failed"),
    };
    if let Err(error) = tx.commit().await {
        return db_failure(&error, &headers, "version pin failed");
    }
    match outcome {
        Some(_) => {
            match versions::get(&resolved.services.pool, &scope, resolved.id, version_id).await {
                Ok(Some(version)) => version_response(StatusCode::OK, &version),
                Ok(None) => not_found(&headers),
                Err(error) => db_failure(&error, &headers, "version read failed"),
            }
        }
        None => not_found(&headers),
    }
}

// ---- POST /files/{id}/versions/{vid}/restore --------------------------------------------------

pub(crate) async fn restore_version(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path((file_raw, version_raw)): Path<(String, String)>,
) -> Response {
    let resolved = match resolve_file(&state, &headers, &file_raw).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let (mut tx, scope) = match write_gate(
        &resolved,
        &session,
        &headers,
        Action::VersionRestore,
        "version.restore",
        resolved.id,
    )
    .await
    {
        Ok(opened) => opened,
        Err(response) => return *response,
    };
    let pre = match required_precondition(&headers) {
        Ok(Precondition::NoVersionYet) => {
            return validation_failed(&["If-None-Match"], &headers);
        }
        Ok(pre) => pre,
        Err(response) => return *response,
    };
    let Some(version_id) = parse_id(&version_raw) else {
        return not_found(&headers);
    };
    let source = match tx
        .version_restore_source(&scope, resolved.id, version_id)
        .await
    {
        Ok(DownloadTarget::Ready(info)) => info,
        Ok(DownloadTarget::Gone | DownloadTarget::NoVersion) => return not_found(&headers),
        Ok(DownloadTarget::FileDeleted) => return file_deleted(&headers),
        Ok(DownloadTarget::ProjectDeleted) => return project_deleted(&headers),
        Err(error) => return db_failure(&error, &headers, "restore lookup failed"),
    };
    if source.storage_tier == "archive" {
        if source.blob_state != "committed" {
            return blob_corrupt(&headers);
        }
        if let Err(error) = tx.commit().await {
            return db_failure(&error, &headers, "restore failed");
        }
        return restore_archived_version(resolved, &headers, &session, version_id, pre, *source)
            .await;
    }
    if source.layout == "chunked" {
        let Some(local) = resolved.services.storage.as_ref() else {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "STORAGE_UNAVAILABLE",
                "File storage is not configured on this server.",
                &headers,
            );
        };
        match restore_archived_chunks_to_local(
            local,
            resolved.services.archive_store.as_deref(),
            resolved.services.archive_crypto.as_deref(),
            &resolved.services.pool,
            &mut tx,
            &scope,
            resolved.id,
            version_id,
        )
        .await
        {
            Ok(()) => {}
            Err(ChunkRestoreError::ArchiveUnavailable) => return archive_unavailable(&headers),
            Err(ChunkRestoreError::Integrity) => {
                // version_restore_source holds a shared lock on this blob and
                // OrgTx owns the organization write lock. corrupt_response
                // updates the blob through a separate pool transaction, so
                // release both locks before awaiting that update.
                drop(tx);
                return corrupt_response(&resolved.services, &source, &headers).await;
            }
            Err(ChunkRestoreError::Local(error)) => return local_write_error(error, &headers),
            Err(ChunkRestoreError::Db(error)) => {
                return db_failure(&error, &headers, "restore failed");
            }
        }
    }
    let now = resolved.services.clock.now();
    match tx
        .version_restore(&scope, resolved.id, version_id, session.user_id, pre, now)
        .await
    {
        Ok(RestoreOutcome::Restored(version)) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "version.restore",
                    resolved.org_id,
                    Some(version.id),
                )
                .workspace(resolved.workspace_id)
                .detail("file_id", Detail::Id(resolved.id))
                .detail("number", Detail::Count(version.number)),
            )
            .await
            {
                return db_failure(&error, &headers, "restore failed");
            }
            version_response(StatusCode::CREATED, &version)
        }
        Ok(RestoreOutcome::PreconditionFailed { current }) => {
            precondition_failed(current, &headers)
        }
        Ok(RestoreOutcome::Gone) => not_found(&headers),
        Ok(RestoreOutcome::FileDeleted) => file_deleted(&headers),
        Ok(RestoreOutcome::ProjectDeleted) => project_deleted(&headers),
        Ok(RestoreOutcome::BlobCorrupt) => blob_corrupt(&headers),
        Ok(RestoreOutcome::ArchiveRecallRequired) => {
            let mut response = error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "ARCHIVE_UNAVAILABLE",
                "The archived version cannot be restored right now.",
                &headers,
            );
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
            response
        }
        Err(error) => db_failure(&error, &headers, "restore failed"),
    }
}

pub(crate) fn archive_unavailable(headers: &HeaderMap) -> Response {
    let mut response = error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "ARCHIVE_UNAVAILABLE",
        "The archived version is not available right now.",
        headers,
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
    response
}

enum ChunkRestoreError {
    ArchiveUnavailable,
    Integrity,
    Local(StorageError),
    Db(craftcloud_db::DbError),
}

// Keeps the organization transaction and storage providers explicit for the
// bounded per-version restore operation, matching the whole-blob restore path.
#[allow(clippy::too_many_arguments)]
async fn restore_archived_chunks_to_local(
    local: &craftcloud_storage::LocalStore,
    archive: Option<&craftcloud_storage::ArchiveStore>,
    crypto: Option<&craftcloud_storage::ArchiveCrypto>,
    pool: &sqlx::PgPool,
    tx: &mut OrgTx,
    scope: &OrgScope,
    file_id: Uuid,
    version_id: Uuid,
) -> Result<(), ChunkRestoreError> {
    let chunks = versions::download_chunks(pool, scope, file_id, version_id)
        .await
        .map_err(ChunkRestoreError::Db)?;
    for chunk in chunks {
        let size = u64::try_from(chunk.size_bytes).map_err(|_| ChunkRestoreError::Integrity)?;
        let stored_size =
            u64::try_from(chunk.stored_bytes).map_err(|_| ChunkRestoreError::Integrity)?;
        let local_key =
            ObjectKey::parse(&chunk.object_key).map_err(|_| ChunkRestoreError::Integrity)?;
        let expected_sha: [u8; 32] = chunk
            .sha256
            .as_slice()
            .try_into()
            .map_err(|_| ChunkRestoreError::Integrity)?;
        match local.head(&local_key).await {
            Ok(head) if head.len == stored_size => {
                let capacity =
                    usize::try_from(stored_size).map_err(|_| ChunkRestoreError::Integrity)?;
                if capacity > craftcloud_storage::MAX_CHUNK_BYTES as usize {
                    return Err(ChunkRestoreError::Integrity);
                }
                let mut reader = local
                    .open_read(&local_key, None)
                    .await
                    .map_err(ChunkRestoreError::Local)?;
                if reader.remaining() != stored_size {
                    return Err(ChunkRestoreError::Integrity);
                }
                let mut stored = Vec::with_capacity(capacity);
                while let Some(bytes) = reader
                    .next_chunk()
                    .await
                    .map_err(ChunkRestoreError::Local)?
                {
                    if stored.len().saturating_add(bytes.len()) > capacity {
                        return Err(ChunkRestoreError::Integrity);
                    }
                    stored.extend_from_slice(&bytes);
                }
                if stored.len() != capacity {
                    return Err(ChunkRestoreError::Integrity);
                }
                craftcloud_storage::compression::decode_chunk_bounded(
                    chunk.encoding.clone(),
                    stored,
                    size,
                    expected_sha,
                )
                .await
                .map_err(|_| ChunkRestoreError::Integrity)?;
                if chunk.storage_tier == "archive" {
                    let _ = tx
                        .chunk_restore_local(scope, file_id, chunk.chunk_id, local_key.as_str())
                        .await
                        .map_err(ChunkRestoreError::Db)?;
                }
                continue;
            }
            Ok(_) => return Err(ChunkRestoreError::Integrity),
            Err(StorageError::NotFound) if chunk.storage_tier == "archive" => {}
            Err(error) => return Err(ChunkRestoreError::Local(error)),
        }
        let (Some(archive), Some(crypto), Some(raw_key), Some(raw_envelope)) = (
            archive,
            crypto,
            chunk.archive_object_key.as_deref(),
            chunk.archive_wrapped_key.as_deref(),
        ) else {
            return Err(ChunkRestoreError::ArchiveUnavailable);
        };
        let key = ObjectKey::parse(raw_key).map_err(|_| ChunkRestoreError::Integrity)?;
        let envelope = craftcloud_storage::WrappedDataKey::from_bytes(raw_envelope)
            .map_err(|_| ChunkRestoreError::Integrity)?;
        let mut reader = crate::archive_transport::open_archive_range(
            archive,
            crypto,
            &key,
            &envelope,
            chunk.chunk_id,
            stored_size,
            None,
        )
        .await
        .map_err(|error| match error {
            crate::archive_transport::ArchiveTransportError::Storage(_) => {
                ChunkRestoreError::ArchiveUnavailable
            }
            _ => ChunkRestoreError::Integrity,
        })?;
        if reader.remaining() != stored_size {
            return Err(ChunkRestoreError::Integrity);
        }
        let capacity = usize::try_from(stored_size).map_err(|_| ChunkRestoreError::Integrity)?;
        if capacity > craftcloud_storage::MAX_CHUNK_BYTES as usize {
            return Err(ChunkRestoreError::Integrity);
        }
        let mut content = Vec::with_capacity(capacity);
        while let Some(bytes) = match reader.next_chunk().await {
            Ok(bytes) => bytes,
            Err(crate::archive_transport::ArchiveTransportError::Storage(_)) => {
                return Err(ChunkRestoreError::ArchiveUnavailable);
            }
            Err(_) => return Err(ChunkRestoreError::Integrity),
        } {
            if content.len().saturating_add(bytes.len()) > capacity {
                return Err(ChunkRestoreError::Integrity);
            }
            content.extend_from_slice(&bytes);
        }
        if content.len() != capacity {
            return Err(ChunkRestoreError::Integrity);
        }
        craftcloud_storage::compression::decode_chunk_bounded(
            chunk.encoding.clone(),
            content.clone(),
            size,
            expected_sha,
        )
        .await
        .map_err(|_| ChunkRestoreError::Integrity)?;
        let stored_sha: [u8; 32] = Sha256::digest(&content).into();
        let mut temp = local
            .begin_write(stored_size)
            .await
            .map_err(ChunkRestoreError::Local)?;
        if let Err(error) = temp.write(&content).await {
            local.abort(temp).await;
            return Err(ChunkRestoreError::Local(error));
        }
        let local_key = local
            .finalize(temp, stored_size, &stored_sha)
            .await
            .map_err(ChunkRestoreError::Local)?;
        if !tx
            .chunk_restore_local(scope, file_id, chunk.chunk_id, local_key.as_str())
            .await
            .map_err(ChunkRestoreError::Db)?
        {
            return Err(ChunkRestoreError::Integrity);
        }
    }
    Ok(())
}

async fn restore_archived_version(
    resolved: Resolved,
    headers: &HeaderMap,
    session: &AuthenticatedSession,
    version_id: Uuid,
    precondition: Precondition,
    source: DownloadInfo,
) -> Response {
    let services = Arc::clone(&resolved.services);
    let Some(local) = services.storage.as_ref() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "STORAGE_UNAVAILABLE",
            "File storage is not configured on this server.",
            headers,
        );
    };
    let (Some(archive), Some(crypto)) = (
        services.archive_store.as_deref(),
        services.archive_crypto.as_deref(),
    ) else {
        return archive_unavailable(headers);
    };
    let Ok(_permit) = Arc::clone(&services.archive_restore_permits)
        .acquire_owned()
        .await
    else {
        return archive_unavailable(headers);
    };
    let (Ok(archive_key), Some(wrapped), Ok(size), Ok(digest)) = (
        source.archive_object_key.as_deref().ok_or(()),
        source
            .archive_wrapped_key
            .as_deref()
            .and_then(|bytes| craftcloud_storage::WrappedDataKey::from_bytes(bytes).ok()),
        u64::try_from(source.size_bytes),
        <[u8; 32]>::try_from(source.sha256.as_slice()),
    ) else {
        return corrupt_response(&services, &source, headers).await;
    };
    let Ok(archive_key) = ObjectKey::parse(archive_key) else {
        return corrupt_response(&services, &source, headers).await;
    };
    let reader = match crate::archive_transport::open_archive_range(
        archive,
        crypto,
        &archive_key,
        &wrapped,
        source.blob_id,
        size,
        None,
    )
    .await
    {
        Ok(reader) => reader,
        Err(crate::archive_transport::ArchiveTransportError::Storage(error)) => {
            tracing::warn!(error = ?error, blob = %source.blob_id, "archive restore unavailable");
            return archive_unavailable(headers);
        }
        Err(_) => return corrupt_response(&services, &source, headers).await,
    };
    let mut temp = match local.begin_write(size).await {
        Ok(temp) => temp,
        Err(error) => return local_write_error(error, headers),
    };
    let mut hasher = Sha256::new();
    let mut copied = 0u64;
    let mut reader = reader;
    loop {
        let chunk = match reader.next_chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(crate::archive_transport::ArchiveTransportError::Storage(error)) => {
                tracing::warn!(error = ?error, blob = %source.blob_id, "archive restore stream unavailable");
                local.abort(temp).await;
                return archive_unavailable(headers);
            }
            Err(_) => {
                local.abort(temp).await;
                return corrupt_response(&services, &source, headers).await;
            }
        };
        copied = match copied.checked_add(chunk.len() as u64) {
            Some(copied) if copied <= size => copied,
            _ => {
                local.abort(temp).await;
                return corrupt_response(&services, &source, headers).await;
            }
        };
        hasher.update(&chunk);
        if let Err(error) = temp.write(&chunk).await {
            local.abort(temp).await;
            return local_write_error(error, headers);
        }
    }
    let actual: [u8; 32] = hasher.finalize().into();
    if copied != size || actual != digest {
        local.abort(temp).await;
        return corrupt_response(&services, &source, headers).await;
    }
    let key = match local.finalize(temp, size, &digest).await {
        Ok(key) => key,
        Err(error) => return local_write_error(error, headers),
    };
    let (mut tx, scope) = match write_gate(
        &resolved,
        session,
        headers,
        Action::VersionRestore,
        "version.restore",
        resolved.id,
    )
    .await
    {
        Ok(opened) => opened,
        Err(response) => {
            cleanup_restore_copy(local, &key, false).await;
            return *response;
        }
    };
    let now = services.clock.now();
    let outcome = tx
        .version_restore_copy(
            &scope,
            resolved.id,
            version_id,
            session.user_id,
            precondition,
            key.as_str(),
            now,
        )
        .await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            cleanup_restore_copy(local, &key, false).await;
            return db_failure(&error, headers, "restore failed");
        }
    };
    match outcome {
        RestoreOutcome::Restored(version) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    headers,
                    session.user_id,
                    "version.restore",
                    resolved.org_id,
                    Some(version.id),
                )
                .workspace(resolved.workspace_id)
                .detail("file_id", Detail::Id(resolved.id))
                .detail("number", Detail::Count(version.number)),
            )
            .await
            {
                // COMMIT may have succeeded while its acknowledgement was lost.
                cleanup_restore_copy(local, &key, true).await;
                return db_failure(&error, headers, "restore failed");
            }
            version_response(StatusCode::CREATED, &version)
        }
        RestoreOutcome::PreconditionFailed { current } => {
            cleanup_restore_copy(local, &key, false).await;
            precondition_failed(current, headers)
        }
        RestoreOutcome::Gone => {
            cleanup_restore_copy(local, &key, false).await;
            not_found(headers)
        }
        RestoreOutcome::FileDeleted => {
            cleanup_restore_copy(local, &key, false).await;
            file_deleted(headers)
        }
        RestoreOutcome::ProjectDeleted => {
            cleanup_restore_copy(local, &key, false).await;
            project_deleted(headers)
        }
        RestoreOutcome::BlobCorrupt => {
            cleanup_restore_copy(local, &key, false).await;
            blob_corrupt(headers)
        }
        RestoreOutcome::ArchiveRecallRequired => {
            cleanup_restore_copy(local, &key, false).await;
            archive_unavailable(headers)
        }
    }
}

async fn cleanup_restore_copy(
    local: &craftcloud_storage::LocalStore,
    key: &ObjectKey,
    commit_may_have_succeeded: bool,
) {
    if !commit_may_have_succeeded {
        let _ = local.delete(key).await;
    }
}

fn local_write_error(error: StorageError, headers: &HeaderMap) -> Response {
    if error.is_full() {
        error_response(
            StatusCode::INSUFFICIENT_STORAGE,
            "INSUFFICIENT_STORAGE",
            "The server does not have enough local storage.",
            headers,
        )
    } else {
        internal(headers)
    }
}

// ---- downloads ------------------------------------------------------------------------------------

fn blob_corrupt(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "BLOB_CORRUPT",
        "The stored data of this version failed verification.",
        headers,
    )
}

/// A body fed by a task through a small channel (bounded memory).
pub(crate) struct ChannelBody {
    rx: mpsc::Receiver<Result<Bytes, std::io::Error>>,
}

enum DownloadReader {
    Local(craftcloud_storage::ObjectReader),
    Archive(Box<crate::archive_transport::ArchiveReader<craftcloud_storage::ArchiveReader>>),
    Chunked(ChunkedReader),
}

pub(crate) enum DownloadReadError {
    Local(StorageError),
    Archive(crate::archive_transport::ArchiveTransportError),
    Chunked(StorageError),
}

pub(crate) enum ChunkOpenError {
    Local(StorageError),
    ArchiveUnavailable,
    Integrity,
}

pub(crate) struct ChunkedReader {
    store: craftcloud_storage::LocalStore,
    chunks: Vec<DownloadChunk>,
    local_available: Vec<bool>,
    archive: Option<Arc<craftcloud_storage::ArchiveStore>>,
    crypto: Option<Arc<craftcloud_storage::ArchiveCrypto>>,
    index: usize,
    range_start: u64,
    range_end: u64,
    remaining: u64,
}

impl ChunkedReader {
    pub(crate) async fn new(
        store: craftcloud_storage::LocalStore,
        chunks: Vec<DownloadChunk>,
        size: u64,
        range_start: u64,
        range_end: u64,
        archive: Option<Arc<craftcloud_storage::ArchiveStore>>,
        crypto: Option<Arc<craftcloud_storage::ArchiveCrypto>>,
    ) -> Result<Self, ChunkOpenError> {
        let mut offset = 0u64;
        let mut local_available = Vec::with_capacity(chunks.len());
        for (ordinal, chunk) in chunks.iter().enumerate() {
            let (Ok(index), Ok(chunk_offset), Ok(chunk_len), Ok(stored_len)) = (
                i32::try_from(ordinal),
                u64::try_from(chunk.byte_offset),
                u64::try_from(chunk.size_bytes),
                u64::try_from(chunk.stored_bytes),
            ) else {
                return Err(ChunkOpenError::Integrity);
            };
            if chunk.ordinal != index
                || chunk_offset != offset
                || chunk_len == 0
                || chunk_len > craftcloud_storage::MAX_CHUNK_BYTES as u64
                || stored_len == 0
                || stored_len > craftcloud_storage::MAX_CHUNK_BYTES as u64
            {
                return Err(ChunkOpenError::Integrity);
            }
            let key = ObjectKey::parse(&chunk.object_key).map_err(|_| ChunkOpenError::Integrity)?;
            let local = match store.head(&key).await {
                Ok(head) => {
                    if head.len != stored_len {
                        return Err(ChunkOpenError::Integrity);
                    }
                    true
                }
                Err(StorageError::NotFound) if chunk.storage_tier == "archive" => false,
                Err(error) => return Err(ChunkOpenError::Local(error)),
            };
            if !local {
                let (Some(archive), Some(envelope_bytes), Some(crypto), Some(raw_key)) = (
                    archive.as_deref(),
                    chunk.archive_wrapped_key.as_deref(),
                    crypto.as_deref(),
                    chunk.archive_object_key.as_deref(),
                ) else {
                    return Err(ChunkOpenError::ArchiveUnavailable);
                };
                let (Ok(archive_key), Ok(envelope)) = (
                    ObjectKey::parse(raw_key),
                    craftcloud_storage::WrappedDataKey::from_bytes(envelope_bytes),
                ) else {
                    return Err(ChunkOpenError::Integrity);
                };
                let archive_head = archive
                    .head(&archive_key)
                    .await
                    .map_err(|_| ChunkOpenError::ArchiveUnavailable)?;
                if craftcloud_storage::verify_ciphertext_len(stored_len, archive_head.len).is_err()
                    || crypto
                        .unwrap_data_key(&envelope, chunk.chunk_id, stored_len)
                        .is_err()
                {
                    return Err(ChunkOpenError::Integrity);
                }
            }
            local_available.push(local);
            offset = offset
                .checked_add(chunk_len)
                .ok_or(ChunkOpenError::Integrity)?;
        }
        if offset != size || range_start > range_end || range_end > size {
            return Err(ChunkOpenError::Integrity);
        }
        Ok(Self {
            store,
            chunks,
            local_available,
            archive,
            crypto,
            index: 0,
            range_start,
            range_end,
            remaining: range_end.saturating_sub(range_start),
        })
    }

    pub(crate) async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, DownloadReadError> {
        while let Some(chunk) = self.chunks.get(self.index) {
            let current_index = self.index;
            self.index = self.index.saturating_add(1);
            let offset = u64::try_from(chunk.byte_offset)
                .map_err(|_| DownloadReadError::Chunked(StorageError::LengthMismatch))?;
            let length = u64::try_from(chunk.size_bytes)
                .map_err(|_| DownloadReadError::Chunked(StorageError::LengthMismatch))?;
            let end = offset
                .checked_add(length)
                .ok_or(DownloadReadError::Chunked(StorageError::TooLarge))?;
            let selected_start = self.range_start.max(offset);
            let selected_end = self.range_end.min(end);
            if selected_start >= selected_end {
                continue;
            }
            let expected_len = usize::try_from(chunk.size_bytes)
                .map_err(|_| DownloadReadError::Chunked(StorageError::TooLarge))?;
            let stored_len = u64::try_from(chunk.stored_bytes)
                .map_err(|_| DownloadReadError::Chunked(StorageError::TooLarge))?;
            let mut content = Vec::with_capacity(expected_len);
            if self
                .local_available
                .get(current_index)
                .copied()
                .unwrap_or(false)
            {
                let key = ObjectKey::parse(&chunk.object_key)
                    .map_err(|_| DownloadReadError::Chunked(StorageError::NotFound))?;
                let mut reader = self
                    .store
                    .open_read(&key, None)
                    .await
                    .map_err(DownloadReadError::Chunked)?;
                if reader.remaining() != stored_len {
                    return Err(DownloadReadError::Chunked(StorageError::LengthMismatch));
                }
                while let Some(bytes) = reader
                    .next_chunk()
                    .await
                    .map_err(DownloadReadError::Chunked)?
                {
                    content.extend_from_slice(&bytes);
                }
            } else {
                let (Some(archive), Some(crypto), Some(raw_key), Some(raw_envelope)) = (
                    self.archive.as_deref(),
                    self.crypto.as_deref(),
                    chunk.archive_object_key.as_deref(),
                    chunk.archive_wrapped_key.as_deref(),
                ) else {
                    return Err(DownloadReadError::Chunked(StorageError::NotFound));
                };
                let archive_key = ObjectKey::parse(raw_key)
                    .map_err(|_| DownloadReadError::Chunked(StorageError::NotFound))?;
                let envelope = craftcloud_storage::WrappedDataKey::from_bytes(raw_envelope)
                    .map_err(|_| DownloadReadError::Chunked(StorageError::ChecksumMismatch))?;
                let mut reader = crate::archive_transport::open_archive_range(
                    archive,
                    crypto,
                    &archive_key,
                    &envelope,
                    chunk.chunk_id,
                    stored_len,
                    None,
                )
                .await
                .map_err(DownloadReadError::Archive)?;
                while let Some(bytes) = reader
                    .next_chunk()
                    .await
                    .map_err(DownloadReadError::Archive)?
                {
                    content.extend_from_slice(&bytes);
                }
            }
            let digest: [u8; 32] = chunk
                .sha256
                .as_slice()
                .try_into()
                .map_err(|_| DownloadReadError::Chunked(StorageError::ChecksumMismatch))?;
            let decoded = craftcloud_storage::compression::decode_chunk_bounded(
                chunk.encoding.clone(),
                content,
                length,
                digest,
            )
            .await
            .map_err(|_| DownloadReadError::Chunked(StorageError::ChecksumMismatch))?;
            let content = decoded;
            let start = usize::try_from(selected_start - offset)
                .map_err(|_| DownloadReadError::Chunked(StorageError::TooLarge))?;
            let stop = usize::try_from(selected_end - offset)
                .map_err(|_| DownloadReadError::Chunked(StorageError::TooLarge))?;
            let result = content
                .get(start..stop)
                .ok_or(DownloadReadError::Chunked(StorageError::LengthMismatch))?
                .to_vec();
            self.remaining = self.remaining.saturating_sub(result.len() as u64);
            return Ok(Some(result));
        }
        if self.remaining == 0 {
            Ok(None)
        } else {
            Err(DownloadReadError::Chunked(StorageError::LengthMismatch))
        }
    }
}

impl DownloadReader {
    fn remaining(&self) -> u64 {
        match self {
            Self::Local(reader) => reader.remaining(),
            Self::Archive(reader) => reader.remaining(),
            Self::Chunked(reader) => reader.remaining,
        }
    }

    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, DownloadReadError> {
        match self {
            Self::Local(reader) => reader.next_chunk().await.map_err(DownloadReadError::Local),
            Self::Archive(reader) => reader
                .next_chunk()
                .await
                .map_err(DownloadReadError::Archive),
            Self::Chunked(reader) => reader.next_chunk().await,
        }
    }
}

impl ChannelBody {
    pub(crate) fn new(rx: mpsc::Receiver<Result<Bytes, std::io::Error>>) -> Self {
        Self { rx }
    }
}

impl http_body::Body for ChannelBody {
    type Data = Bytes;
    type Error = std::io::Error;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        self.rx
            .poll_recv(cx)
            .map(|item| item.map(|chunk| chunk.map(http_body::Frame::data)))
    }
}

/// Where a `Range` header points.
#[derive(Debug, PartialEq, Eq)]
enum RangeSpec {
    /// No usable range: the whole representation.
    Whole,
    /// First and last byte (inclusive).
    Part(u64, u64),
    /// Syntactically valid but outside the representation.
    Unsatisfiable,
}

/// Single byte ranges only. Several ranges, other units and malformed values
/// are ignored (the whole representation is sent), as RFC 9110 allows.
fn parse_range(raw: Option<&str>, len: u64) -> RangeSpec {
    let Some(spec) = raw.and_then(|r| r.trim().strip_prefix("bytes=")) else {
        return RangeSpec::Whole;
    };
    if spec.contains(',') {
        return RangeSpec::Whole;
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return RangeSpec::Whole;
    };
    let number = |text: &str| -> Option<u64> {
        (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse::<u64>().ok())
            .flatten()
    };
    match (number(first), number(last)) {
        (Some(start), Some(end)) if start <= end => {
            if start >= len {
                RangeSpec::Unsatisfiable
            } else {
                RangeSpec::Part(start, end.min(len - 1))
            }
        }
        (Some(start), None) if last.is_empty() => {
            if start >= len {
                RangeSpec::Unsatisfiable
            } else {
                RangeSpec::Part(start, len - 1)
            }
        }
        (None, Some(suffix)) if first.is_empty() => {
            if suffix == 0 || len == 0 {
                RangeSpec::Unsatisfiable
            } else {
                RangeSpec::Part(len.saturating_sub(suffix), len - 1)
            }
        }
        _ => RangeSpec::Whole,
    }
}

/// `attachment; filename="<ascii>"; filename*=UTF-8''<percent-encoded>`.
fn content_disposition(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && !matches!(c, '"' | '\\' | '%' | ';' | ',' | '\'') || c == ' '
            {
                c
            } else {
                '_'
            }
        })
        .collect();
    let fallback = fallback.trim();
    let fallback = if fallback.is_empty() || fallback.chars().all(|c| c == '_' || c == '.') {
        "download"
    } else {
        fallback
    };
    let mut encoded = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

fn range_not_satisfiable(len: u64, headers: &HeaderMap) -> Response {
    let mut response = error_response(
        StatusCode::RANGE_NOT_SATISFIABLE,
        "RANGE_NOT_SATISFIABLE",
        "The requested range is outside the content.",
        headers,
    );
    if let Ok(value) = HeaderValue::from_str(&format!("bytes */{len}")) {
        response.headers_mut().insert(header::CONTENT_RANGE, value);
    }
    response
}

async fn corrupt_response(
    services: &Services,
    info: &DownloadInfo,
    headers: &HeaderMap,
) -> Response {
    tracing::error!(blob = %info.blob_id, "blob failed verification");
    if let Err(error) = versions::mark_blob_corrupt(&services.pool, info.blob_id).await {
        tracing::error!(%error, "cannot mark blob corrupt");
    }
    blob_corrupt(headers)
}

pub(crate) async fn download_head(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(file_raw): Path<String>,
    method: axum::http::Method,
) -> Response {
    download(
        &state,
        &headers,
        &session,
        &file_raw,
        None,
        method == axum::http::Method::HEAD,
    )
    .await
}

pub(crate) async fn download_version(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path((file_raw, version_raw)): Path<(String, String)>,
    method: axum::http::Method,
) -> Response {
    let Some(version_id) = parse_id(&version_raw) else {
        return not_found(&headers);
    };
    let is_head = method == axum::http::Method::HEAD;
    download(
        &state,
        &headers,
        &session,
        &file_raw,
        Some(version_id),
        is_head,
    )
    .await
}

async fn download(
    state: &ApiState,
    headers: &HeaderMap,
    session: &AuthenticatedSession,
    file_raw: &str,
    version: Option<Uuid>,
    is_head: bool,
) -> Response {
    let resolved = match resolve_file(state, headers, file_raw).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let services = Arc::clone(&resolved.services);
    // The caller's roles are read fresh for this request.
    let scope: OrgScope = match read_gate(
        &resolved,
        session,
        headers,
        Action::FileDownload,
        "file.download",
        resolved.id,
    )
    .await
    {
        Ok(scope) => scope,
        Err(response) => return *response,
    };
    let Some(store) = services.storage.as_ref() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "STORAGE_UNAVAILABLE",
            "File storage is not configured on this server.",
            headers,
        );
    };
    let info = match versions::download_target(&services.pool, &scope, resolved.id, version).await {
        Ok(DownloadTarget::Ready(info)) => info,
        Ok(DownloadTarget::Gone | DownloadTarget::NoVersion) => return not_found(headers),
        Ok(DownloadTarget::FileDeleted) => return file_deleted(headers),
        Ok(DownloadTarget::ProjectDeleted) => return project_deleted(headers),
        Err(error) => return db_failure(&error, headers, "download lookup failed"),
    };
    if info.blob_state != "committed" {
        return blob_corrupt(headers);
    }
    let (Ok(size), Ok(digest)) = (
        u64::try_from(info.size_bytes),
        <[u8; 32]>::try_from(info.sha256.as_slice()),
    ) else {
        return corrupt_response(&services, &info, headers).await;
    };
    // `If-Range` (RFC 9110 13.1.5): honor Range only if it is exactly the strong
    // ETag of this version; anything else, dates included, sends the whole content.
    let range_allowed = headers.get(header::IF_RANGE).is_none_or(|value| {
        value
            .to_str()
            .is_ok_and(|text| text.trim() == version_tag(info.version_id))
    });
    let raw_range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .filter(|_| range_allowed);
    let (storage_range, status, first, last) = match parse_range(raw_range, size) {
        RangeSpec::Whole => (None, StatusCode::OK, 0, size.saturating_sub(1)),
        RangeSpec::Part(a, b) => (
            Some(ByteRange {
                start: a,
                end: Some(b + 1),
            }),
            StatusCode::PARTIAL_CONTENT,
            a,
            b,
        ),
        RangeSpec::Unsatisfiable => return range_not_satisfiable(size, headers),
    };
    let reader = if info.storage_tier == "archive" {
        let (Some(archive), Some(crypto)) = (
            services.archive_store.as_deref(),
            services.archive_crypto.as_deref(),
        ) else {
            return archive_unavailable(headers);
        };
        let (Some(raw_key), Some(raw_envelope)) = (
            info.archive_object_key.as_deref(),
            info.archive_wrapped_key.as_deref(),
        ) else {
            return corrupt_response(&services, &info, headers).await;
        };
        let (Ok(key), Ok(envelope)) = (
            ObjectKey::parse(raw_key),
            craftcloud_storage::WrappedDataKey::from_bytes(raw_envelope),
        ) else {
            return corrupt_response(&services, &info, headers).await;
        };
        match crate::archive_transport::open_archive_range(
            archive,
            crypto,
            &key,
            &envelope,
            info.blob_id,
            size,
            storage_range,
        )
        .await
        {
            Ok(reader) => DownloadReader::Archive(Box::new(reader)),
            Err(crate::archive_transport::ArchiveTransportError::Storage(error)) => {
                tracing::warn!(error = ?error, blob = %info.blob_id, "archive object unavailable");
                return archive_unavailable(headers);
            }
            Err(_) => return corrupt_response(&services, &info, headers).await,
        }
    } else if info.layout == "chunked" {
        let chunks =
            match versions::download_chunks(&services.pool, &scope, resolved.id, info.version_id)
                .await
            {
                Ok(chunks) => chunks,
                Err(error) => return db_failure(&error, headers, "chunk manifest lookup failed"),
            };
        let range_end = if status == StatusCode::OK {
            size
        } else {
            last.saturating_add(1)
        };
        match ChunkedReader::new(
            store.clone(),
            chunks,
            size,
            first,
            range_end,
            services.archive_store.clone(),
            services.archive_crypto.clone(),
        )
        .await
        {
            Ok(reader) => DownloadReader::Chunked(reader),
            Err(ChunkOpenError::Local(StorageError::NotFound)) => {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "STORAGE_UNAVAILABLE",
                    "The stored data is not available right now.",
                    headers,
                );
            }
            Err(ChunkOpenError::ArchiveUnavailable) => return archive_unavailable(headers),
            Err(ChunkOpenError::Integrity) => {
                return corrupt_response(&services, &info, headers).await;
            }
            Err(ChunkOpenError::Local(_)) => return internal(headers),
        }
    } else if info.layout == "whole" {
        let Ok(key) = ObjectKey::parse(&info.object_key) else {
            return corrupt_response(&services, &info, headers).await;
        };
        match store.open_read(&key, storage_range).await {
            Ok(reader) => DownloadReader::Local(reader),
            // A missing object may be a mount that is not there right now: report the
            // storage as unavailable and mark nothing (only verified evidence marks).
            Err(error @ (StorageError::NotFound | StorageError::RangeNotSatisfiable)) => {
                tracing::error!(error = ?error, blob = %info.blob_id, "object not readable for download");
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "STORAGE_UNAVAILABLE",
                    "The stored data is not available right now.",
                    headers,
                );
            }
            Err(error) => {
                tracing::error!(error = ?error, "cannot open object for download");
                return internal(headers);
            }
        }
    } else {
        return corrupt_response(&services, &info, headers).await;
    };
    let whole = storage_range.is_none();
    // A stored object whose length differs from the record is corrupt.
    if whole && reader.remaining() != size {
        return corrupt_response(&services, &info, headers).await;
    }
    let length = reader.remaining();
    let body = if length == 0 || is_head {
        Body::empty()
    } else {
        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(2);
        let (services, blob_id) = (Arc::clone(&services), info.blob_id);
        tokio::spawn(stream_download_reader(
            reader,
            tx,
            whole.then_some(digest),
            services,
            blob_id,
        ));
        Body::new(ChannelBody { rx })
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let out = response.headers_mut();
    let mut set = |name: HeaderName, value: String| {
        if let Ok(value) = HeaderValue::from_str(&value) {
            out.insert(name, value);
        }
    };
    set(header::CONTENT_TYPE, "application/octet-stream".to_owned());
    set(header::CONTENT_LENGTH, length.to_string());
    set(header::ACCEPT_RANGES, "bytes".to_owned());
    set(header::ETAG, version_tag(info.version_id));
    set(
        header::CONTENT_DISPOSITION,
        content_disposition(&info.file_name),
    );
    set(
        HeaderName::from_static("x-content-type-options"),
        "nosniff".to_owned(),
    );
    set(header::CONTENT_SECURITY_POLICY, "sandbox".to_owned());
    set(
        HeaderName::from_static("repr-digest"),
        format!(
            "sha-256=:{}:",
            base64::engine::general_purpose::STANDARD.encode(digest)
        ),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        set(
            header::CONTENT_RANGE,
            format!("bytes {first}-{last}/{size}"),
        );
    }
    audit(
        headers,
        session.user_id,
        "file.download",
        Some(resolved.org_id),
        Some(info.version_id),
        "success",
    );
    // Downloads are recorded per policy (`CRAFTCLOUD_AUDIT_DOWNLOADS`) whenever
    // content is handed out, whatever the range (a client can read a whole file
    // in pieces), but folded into one row per person, version and 15-minute
    // window with a counter, so range reads cannot flood the log. HEAD returns
    // no content and is not recorded. Best-effort: a read has nothing to roll back.
    if services.settings.audit_downloads && !is_head {
        record_important(
            &services,
            &audit_ctx(headers, Some(session.user_id)),
            &NewEvent::new("file.download")
                .org(resolved.org_id)
                .workspace(resolved.workspace_id)
                .target("file_version", info.version_id)
                .detail("file_id", Detail::Id(resolved.id)),
            Some(AggScope::Target),
        )
        .await;
    }
    response
}

/// Feeds the body. For a full download the digest is checked while streaming
/// and the final chunk is withheld until it matches: on a mismatch or a short
/// read the body ends with an error (the connection is cut), the blob is marked
/// and nothing looks like a clean end.
async fn stream_download_reader(
    mut reader: DownloadReader,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    verify: Option<[u8; 32]>,
    services: Arc<Services>,
    blob_id: Uuid,
) {
    let idle = services.settings.download_idle_timeout;
    let started = services.clock.now();
    let mut sent: u64 = 0;
    // A client that does not take chunks within the idle timeout, or that
    // averages less than the minimum rate after the grace period, is dropped;
    // the reader (and its file handle) goes with this task.
    let give = |tx: &mpsc::Sender<Result<Bytes, std::io::Error>>, item| {
        let tx = tx.clone();
        async move { tokio::time::timeout(idle, tx.send(item)).await }
    };
    let mut hasher = Sha256::new();
    let mut pending: Option<Vec<u8>> = None;
    loop {
        let elapsed = (services.clock.now() - started).whole_seconds();
        if elapsed >= crate::uploads::RATE_GRACE_SECS
            && sent
                < crate::uploads::MIN_RATE_BYTES_PER_SEC
                    .saturating_mul(u64::try_from(elapsed).unwrap_or(0))
        {
            tracing::warn!(blob = %blob_id, "download too slow; cancelled");
            let _ = give(&tx, Err(std::io::Error::other("download too slow"))).await;
            return;
        }
        match reader.next_chunk().await {
            Ok(Some(chunk)) => {
                if verify.is_some() {
                    hasher.update(&chunk);
                }
                // Hold one chunk back so the last one waits for verification.
                let ready = pending.replace(chunk);
                if let Some(ready) = ready {
                    let len = ready.len() as u64;
                    sent = sent.saturating_add(len);
                    if !matches!(give(&tx, Ok(Bytes::from(ready))).await, Ok(Ok(()))) {
                        tracing::warn!(blob = %blob_id, "download client stalled or gone; stopped");
                        return;
                    }
                    services.metrics.add_downloaded(len);
                }
            }
            Ok(None) => break,
            Err(error) => {
                let corrupt = match &error {
                    DownloadReadError::Local(StorageError::Io(e)) => {
                        e.kind() == std::io::ErrorKind::UnexpectedEof
                    }
                    DownloadReadError::Archive(
                        crate::archive_transport::ArchiveTransportError::Integrity,
                    )
                    | DownloadReadError::Archive(
                        crate::archive_transport::ArchiveTransportError::Crypto(_),
                    ) => true,
                    DownloadReadError::Chunked(
                        StorageError::ChecksumMismatch | StorageError::LengthMismatch,
                    ) => true,
                    DownloadReadError::Local(_)
                    | DownloadReadError::Archive(_)
                    | DownloadReadError::Chunked(_) => false,
                };
                tracing::error!(error = ?download_read_error_name(&error), blob = %blob_id, "download read failed");
                if corrupt {
                    let _ = versions::mark_blob_corrupt(&services.pool, blob_id).await;
                }
                let _ = give(&tx, Err(std::io::Error::other("read failed"))).await;
                return;
            }
        }
    }
    if let Some(expected) = verify {
        let actual: [u8; 32] = hasher.finalize().into();
        if actual != expected {
            tracing::error!(blob = %blob_id, "download digest mismatch; blob marked corrupt");
            let _ = versions::mark_blob_corrupt(&services.pool, blob_id).await;
            let _ = give(&tx, Err(std::io::Error::other("integrity check failed"))).await;
            return;
        }
    }
    if let Some(last) = pending {
        let len = last.len() as u64;
        if matches!(give(&tx, Ok(Bytes::from(last))).await, Ok(Ok(()))) {
            services.metrics.add_downloaded(len);
        }
    }
}

fn download_read_error_name(error: &DownloadReadError) -> &'static str {
    match error {
        DownloadReadError::Local(_) => "local read failure",
        DownloadReadError::Archive(_) => "archive read failure",
        DownloadReadError::Chunked(_) => "chunk read failure",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_follow_rfc_9110_for_single_byte_ranges() {
        use RangeSpec::{Part, Unsatisfiable, Whole};
        let r = |h: &str, len| parse_range(Some(h), len);
        assert_eq!(parse_range(None, 10), Whole);
        assert_eq!(r("bytes=0-4", 10), Part(0, 4));
        assert_eq!(r("bytes=5-", 10), Part(5, 9));
        assert_eq!(r("bytes=0-99", 10), Part(0, 9));
        assert_eq!(r("bytes=-3", 10), Part(7, 9));
        assert_eq!(r("bytes=-99", 10), Part(0, 9));
        assert_eq!(r("bytes=9-9", 10), Part(9, 9));
        assert_eq!(r("bytes=10-", 10), Unsatisfiable);
        assert_eq!(r("bytes=10-12", 10), Unsatisfiable);
        assert_eq!(r("bytes=-0", 10), Unsatisfiable);
        assert_eq!(r("bytes=0-0", 0), Unsatisfiable);
        assert_eq!(r("bytes=-1", 0), Unsatisfiable);
        // Ignored: several ranges, other units, wrong order, garbage, overflow.
        for ignored in [
            "bytes=0-1,3-4",
            "items=0-1",
            "bytes=5-2",
            "bytes=a-b",
            "bytes=",
            "bytes=-",
            "0-1",
            "bytes=1-2-3",
        ] {
            assert_eq!(r(ignored, 10), Whole, "{ignored}");
        }
        assert_eq!(r("bytes=99999999999999999999-", 10), Whole);
        assert_eq!(r(&format!("bytes={}-", u64::MAX), 10), Unsatisfiable);
    }

    #[test]
    fn content_disposition_is_safe_for_hostile_names() {
        let cd = content_disposition("report \"final\"; v2.fig");
        assert!(
            cd.starts_with("attachment; filename=\"report _final__ v2.fig\"; filename*=UTF-8''")
        );
        assert!(!cd[..cd.find("filename*").unwrap_or(0)].contains("\"final\""));
        for name in [
            "a\nb\r\nSet-Cookie: x=y",
            "caf\u{e9}\u{202e}.fig",
            "..",
            "\"\"",
            "%00.fig",
            "a\\b'c,d",
        ] {
            let cd = content_disposition(name);
            assert!(HeaderValue::from_str(&cd).is_ok(), "{name:?}");
            assert!(!cd.contains('\n') && !cd.contains('\r'));
            let head = cd.split("; filename*").next().unwrap_or("");
            assert_eq!(head.matches('"').count(), 2, "{cd}");
        }
        assert!(content_disposition("caf\u{e9}.fig").contains("filename*=UTF-8''caf%C3%A9.fig"));
        assert!(content_disposition("\u{65e5}\u{672c}").contains("filename=\"download\""));
        assert!(content_disposition("..").contains("filename=\"download\""));
    }

    #[test]
    fn version_tags_round_trip() {
        let id = Uuid::new_v4();
        assert_eq!(parse_version_tag(&version_tag(id)), Some(id));
        for bad in [
            "",
            "*",
            "version:x",
            &format!("\"{id}\""),
            &format!("W/\"version:{id}\""),
            "\"version:nope\"",
        ] {
            assert_eq!(parse_version_tag(bad), None, "{bad}");
        }
    }

    #[tokio::test]
    async fn restore_copy_survives_uncertain_commit_acknowledgement() {
        let root = std::env::temp_dir().join(format!("craftcloud-restore-copy-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let local = craftcloud_storage::LocalStore::open(&root).await.unwrap();
        let bytes = b"restored archive payload";
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        let mut temp = local.begin_write(bytes.len() as u64).await.unwrap();
        temp.write(bytes).await.unwrap();
        let key = local
            .finalize(temp, bytes.len() as u64, &digest)
            .await
            .unwrap();

        cleanup_restore_copy(&local, &key, true).await;
        assert_eq!(local.head(&key).await.unwrap().len, bytes.len() as u64);
        cleanup_restore_copy(&local, &key, false).await;
        assert!(matches!(
            local.head(&key).await,
            Err(StorageError::NotFound)
        ));

        drop(local);
        std::fs::remove_dir_all(root).unwrap();
    }
}
