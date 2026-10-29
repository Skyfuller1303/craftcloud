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
use craftcloud_db::versions::{
    self, CommitOutcome, DownloadInfo, DownloadTarget, FileVersion, Precondition, RestoreOutcome,
};
use craftcloud_db::{OrgScope, files, uploads};
use craftcloud_storage::{ByteRange, ObjectKey, ObjectStore, ReadSource, StorageError};
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

fn version_json(v: &FileVersion) -> Value {
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
    let creator = match uploads::locate(&services.pool, upload_id).await {
        Ok(Some(location))
            if location.org_id == resolved.org_id
                && location.workspace_id == resolved.workspace_id =>
        {
            location.creator
        }
        Ok(_) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "upload lookup failed"),
    };
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
        match uploads::get(&services.pool, &scope, upload_id).await {
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
    let outcome = match tx
        .version_commit(&scope, resolved.id, upload_id, session.user_id, pre, now)
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
        Err(error) => db_failure(&error, &headers, "restore failed"),
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
    let (Ok(key), Ok(size), Ok(digest)) = (
        ObjectKey::parse(&info.object_key),
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
    let reader = match store.open_read(&key, storage_range).await {
        Ok(reader) => reader,
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
    };
    let whole = storage_range.is_none();
    // A stored object whose length differs from the record is corrupt.
    if whole && reader.remaining() != size {
        return corrupt_response(&services, &info, headers).await;
    }
    let length = reader.remaining();
    let body = if length == 0 {
        Body::empty()
    } else {
        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(2);
        let (services, blob_id) = (Arc::clone(&services), info.blob_id);
        tokio::spawn(stream_object(
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
async fn stream_object(
    mut reader: craftcloud_storage::ObjectReader,
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
                let truncated = matches!(
                    &error,
                    StorageError::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof
                );
                tracing::error!(%error, blob = %blob_id, "download read failed");
                if truncated && verify.is_some() {
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
}
