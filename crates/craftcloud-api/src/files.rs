//! File metadata routes (CC-F06): files of a project, no content yet.
//!
//! Same structure and guarantees as [`crate::projects`]. The file name is
//! untrusted display metadata: it is validated (`FileName`), compared by its
//! NFC and case-folded key and never used as a path. Audit lines carry ids
//! only.

use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use craftcloud_core::policy::Action;
use craftcloud_core::{FileApp, FileFormat, FileName};
use craftcloud_db::files::{
    self, File, FileCreateOutcome, FileDeleteOutcome, FileRenameOutcome, FileRestoreOutcome,
    NewFile,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::ApiState;
use crate::audit::{Change, commit_audited};
use crate::orgs::{conflict, db_failure, not_found, page_error, parse_id, rfc3339};
use crate::pagination::{TrashListQuery, decode_time_cursor, encode_time_cursor};
use crate::projects::{
    Resolved, expected_version, opt_time, read_gate, resolve_project, retention_expired,
    trash_query, version_conflict, with_etag, write_gate,
};
use crate::responses::{rejection_response, unavailable, validation_failed};
use crate::session::AuthenticatedSession;

const ETAG: &str = "file";

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn file_json(f: &File) -> Value {
    json!({
        "id": f.id.to_string(),
        "organization_id": f.org_id.to_string(),
        "workspace_id": f.workspace_id.to_string(),
        "project_id": f.project_id.to_string(),
        "name": f.name,
        "app": f.app,
        "format": f.format,
        "size_bytes": f.size_bytes,
        "sha256": f.sha256.as_deref().map(hex),
        "head_version_id": f.head_version_id.map(|id| id.to_string()),
        "version": f.version,
        "created_by": f.created_by_user_id.to_string(),
        "created_at": rfc3339(f.created_at),
        "updated_by": f.updated_by_user_id.to_string(),
        "updated_at": rfc3339(f.updated_at),
        "deleted_at": opt_time(f.deleted_at),
    })
}

fn file_response(status: StatusCode, f: &File) -> Response {
    with_etag(
        (status, Json(file_json(f))).into_response(),
        ETAG,
        f.version,
    )
}

fn name_taken(headers: &HeaderMap) -> Response {
    conflict(
        "FILE_NAME_TAKEN",
        "A file with this name exists in the project.",
        headers,
    )
}

fn project_deleted(headers: &HeaderMap) -> Response {
    conflict(
        "PROJECT_DELETED",
        "The project is in the trash; restore it first.",
        headers,
    )
}

/// A file resolved from the path: `resolved.id` is the file id.
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

// ---- POST /projects/{id}/files ---------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateFileRequest {
    name: String,
    app: String,
    format: String,
}

pub(crate) async fn create_file(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(project_raw): Path<String>,
    body: Result<Json<CreateFileRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    let resolved = match resolve_project(&state, &headers, &project_raw).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let (mut tx, scope) = match write_gate(
        &resolved,
        &session,
        &headers,
        Action::FileCreate,
        "file.create",
        resolved.id,
    )
    .await
    {
        Ok(opened) => opened,
        Err(response) => return *response,
    };
    let (name, app, format) = (
        FileName::parse(&request.name),
        FileApp::parse(&request.app),
        FileFormat::parse(&request.format),
    );
    let mut invalid = Vec::new();
    if name.is_err() {
        invalid.push("name");
    }
    if app.is_err() {
        invalid.push("app");
    }
    if format.is_err() {
        invalid.push("format");
    }
    let (Ok(name), Ok(app), Ok(format)) = (name, app, format) else {
        return validation_failed(&invalid, &headers);
    };
    let now = resolved.services.clock.now();
    match tx
        .file_create(
            &scope,
            resolved.id,
            &NewFile { name, app, format },
            session.user_id,
            now,
        )
        .await
    {
        Ok(FileCreateOutcome::Created(file)) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "file.create",
                    resolved.org_id,
                    Some(file.id),
                )
                .workspace(resolved.workspace_id),
            )
            .await
            {
                return db_failure(&error, &headers, "file creation failed");
            }
            file_response(StatusCode::CREATED, &file)
        }
        Ok(FileCreateOutcome::NameTaken) => name_taken(&headers),
        Ok(FileCreateOutcome::ProjectDeleted) => project_deleted(&headers),
        Ok(FileCreateOutcome::ProjectGone) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "file creation failed"),
    }
}

// ---- GET /projects/{id}/files -------------------------------------------------------

pub(crate) async fn list_files(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(project_raw): Path<String>,
    query: Result<Query<TrashListQuery>, QueryRejection>,
) -> Response {
    let resolved = match resolve_project(&state, &headers, &project_raw).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let scope = match read_gate(
        &resolved,
        &session,
        &headers,
        Action::FileList,
        "file.list",
        resolved.id,
    )
    .await
    {
        Ok(scope) => scope,
        Err(response) => return *response,
    };
    let (page, deleted) = match trash_query(query, &headers) {
        Ok(parts) => parts,
        Err(response) => return *response,
    };
    let limit = match page.limit() {
        Ok(limit) => limit,
        Err(error) => return page_error(error, &headers),
    };
    // The cursor is bound to the project and to the live/trash switch.
    let scope_text = format!("{}:{deleted}", resolved.id);
    let after = match page
        .cursor()
        .map(|c| decode_time_cursor(c, "files", &scope_text))
    {
        Some(Ok(key)) => Some(key),
        Some(Err(error)) => return page_error(error, &headers),
        None => None,
    };
    let mut rows = match files::list(
        &resolved.services.pool,
        &scope,
        resolved.id,
        deleted,
        limit + 1,
        after,
    )
    .await
    {
        Ok(rows) => rows,
        Err(error) => return db_failure(&error, &headers, "file list failed"),
    };
    let next = if rows.len() > limit as usize {
        rows.truncate(limit as usize);
        rows.last()
            .map(|last| encode_time_cursor("files", &scope_text, last.created_at, last.id))
    } else {
        None
    };
    let items: Vec<_> = rows.iter().map(file_json).collect();
    Json(json!({ "items": items, "next_cursor": next })).into_response()
}

// ---- GET /files/{id} ---------------------------------------------------------------------

pub(crate) async fn get_file(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(file_raw): Path<String>,
) -> Response {
    let resolved = match resolve_file(&state, &headers, &file_raw).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let scope = match read_gate(
        &resolved,
        &session,
        &headers,
        Action::FileRead,
        "file.read",
        resolved.id,
    )
    .await
    {
        Ok(scope) => scope,
        Err(response) => return *response,
    };
    match files::get(&resolved.services.pool, &scope, resolved.id).await {
        Ok(Some(file)) => file_response(StatusCode::OK, &file),
        Ok(None) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "file read failed"),
    }
}

// ---- PATCH /files/{id} ---------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateFileRequest {
    name: String,
}

pub(crate) async fn update_file(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(file_raw): Path<String>,
    body: Result<Json<UpdateFileRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    let resolved = match resolve_file(&state, &headers, &file_raw).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let (mut tx, scope) = match write_gate(
        &resolved,
        &session,
        &headers,
        Action::FileRename,
        "file.update",
        resolved.id,
    )
    .await
    {
        Ok(opened) => opened,
        Err(response) => return *response,
    };
    let Ok(name) = FileName::parse(&request.name) else {
        return validation_failed(&["name"], &headers);
    };
    let expected = match expected_version(&headers, ETAG) {
        Ok(version) => version,
        Err(response) => return *response,
    };
    let now = resolved.services.clock.now();
    match tx
        .file_rename(&scope, resolved.id, &name, expected, session.user_id, now)
        .await
    {
        Ok(FileRenameOutcome::Updated(file)) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "file.update",
                    resolved.org_id,
                    Some(resolved.id),
                )
                .workspace(resolved.workspace_id),
            )
            .await
            {
                return db_failure(&error, &headers, "file update failed");
            }
            file_response(StatusCode::OK, &file)
        }
        Ok(FileRenameOutcome::VersionMismatch { current_version }) => {
            version_conflict("file", ETAG, current_version, &headers)
        }
        Ok(FileRenameOutcome::NameTaken) => name_taken(&headers),
        Ok(FileRenameOutcome::Deleted) => conflict(
            "FILE_DELETED",
            "The file is in the trash; restore it first.",
            &headers,
        ),
        Ok(FileRenameOutcome::ProjectDeleted) => project_deleted(&headers),
        Ok(FileRenameOutcome::Gone) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "file update failed"),
    }
}

// ---- DELETE /files/{id} ------------------------------------------------------------------------

pub(crate) async fn delete_file(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(file_raw): Path<String>,
) -> Response {
    let resolved = match resolve_file(&state, &headers, &file_raw).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let (mut tx, scope) = match write_gate(
        &resolved,
        &session,
        &headers,
        Action::FileDelete,
        "file.delete",
        resolved.id,
    )
    .await
    {
        Ok(opened) => opened,
        Err(response) => return *response,
    };
    let now = resolved.services.clock.now();
    match tx
        .file_delete(&scope, resolved.id, session.user_id, now)
        .await
    {
        Ok(FileDeleteOutcome::Deleted(_)) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "file.delete",
                    resolved.org_id,
                    Some(resolved.id),
                )
                .workspace(resolved.workspace_id),
            )
            .await
            {
                return db_failure(&error, &headers, "file deletion failed");
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(FileDeleteOutcome::Unchanged(_)) => StatusCode::NO_CONTENT.into_response(),
        Ok(FileDeleteOutcome::ProjectDeleted) => project_deleted(&headers),
        Ok(FileDeleteOutcome::Gone) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "file deletion failed"),
    }
}

// ---- POST /files/{id}/restore -------------------------------------------------------------------

pub(crate) async fn restore_file(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(file_raw): Path<String>,
) -> Response {
    let resolved = match resolve_file(&state, &headers, &file_raw).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let (mut tx, scope) = match write_gate(
        &resolved,
        &session,
        &headers,
        Action::FileRestore,
        "file.restore",
        resolved.id,
    )
    .await
    {
        Ok(opened) => opened,
        Err(response) => return *response,
    };
    let now = resolved.services.clock.now();
    let retention = resolved.services.settings.trash_retention;
    match tx
        .file_restore(&scope, resolved.id, session.user_id, now, retention)
        .await
    {
        Ok(FileRestoreOutcome::Restored(file)) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "file.restore",
                    resolved.org_id,
                    Some(resolved.id),
                )
                .workspace(resolved.workspace_id),
            )
            .await
            {
                return db_failure(&error, &headers, "file restore failed");
            }
            file_response(StatusCode::OK, &file)
        }
        Ok(FileRestoreOutcome::Unchanged(file)) => file_response(StatusCode::OK, &file),
        Ok(FileRestoreOutcome::Expired) => retention_expired("file", &headers),
        Ok(FileRestoreOutcome::NameTaken) => name_taken(&headers),
        Ok(FileRestoreOutcome::ProjectDeleted) => project_deleted(&headers),
        Ok(FileRestoreOutcome::Gone) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "file restore failed"),
    }
}
