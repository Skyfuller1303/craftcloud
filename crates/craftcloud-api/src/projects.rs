//! Project routes and the helpers shared with the file routes (CC-F06).
//!
//! The organization and workspace of every request are resolved from the
//! workspace, project or file row, never from client supplied ids. Reads
//! decide with the caller's freshly loaded roles; every change runs in an
//! `OrgTx` (organization row lock, fresh roles, policy decision, change).
//! Callers who cannot see a workspace, project or file all get the same 404 as
//! for unknown ids. Audit lines carry ids only, never project or file names
//! (names may be personal data).

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use craftcloud_core::ProjectName;
use craftcloud_core::policy::{Action, Resource, can};
use craftcloud_db::members::OrgTx;
use craftcloud_db::projects::{
    self, Project, ProjectCreateOutcome, ProjectDeleteOutcome, ProjectRenameOutcome,
    ProjectRestoreOutcome,
};
use craftcloud_db::{OrgScope, workspaces};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit::{Change, commit_audited};
use crate::orgs::{Gate, conflict, db_failure, gate, not_found, page_error, parse_id, rfc3339};
use crate::pagination::{PageQuery, TrashListQuery, decode_time_cursor, encode_time_cursor};
use crate::responses::{internal, rejection_response, unavailable, validation_failed};
use crate::session::AuthenticatedSession;
use crate::workspaces::{Locked, lock, read_actor};
use crate::{ApiState, Services, error_response};

// ---- shared helpers ------------------------------------------------------------

pub(crate) fn opt_time(time: Option<time::OffsetDateTime>) -> Option<String> {
    time.map(rfc3339)
}

pub(crate) fn etag_header(prefix: &str, version: i64) -> Option<HeaderValue> {
    HeaderValue::from_str(&format!("\"{prefix}-v{version}\"")).ok()
}

pub(crate) fn with_etag(mut response: Response, prefix: &str, version: i64) -> Response {
    if let Some(value) = etag_header(prefix, version) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

/// The version in an `If-Match` header of the form `"<prefix>-v<n>"`; -1 when
/// absent form is malformed (never matches a stored version).
pub(crate) fn expected_version(headers: &HeaderMap, prefix: &str) -> Result<i64, Box<Response>> {
    let Some(raw) = headers.get(header::IF_MATCH) else {
        return Err(Box::new(error_response(
            StatusCode::PRECONDITION_REQUIRED,
            "PRECONDITION_REQUIRED",
            "An If-Match header with the ETag from the last read is required.",
            headers,
        )));
    };
    let parsed = raw.to_str().ok().and_then(|text| {
        text.trim()
            .strip_prefix(&format!("\"{prefix}-v"))?
            .strip_suffix('"')?
            .parse::<i64>()
            .ok()
    });
    Ok(parsed.unwrap_or(-1))
}

pub(crate) fn version_conflict(
    what: &str,
    prefix: &str,
    current_version: i64,
    headers: &HeaderMap,
) -> Response {
    with_etag(
        error_response(
            StatusCode::PRECONDITION_FAILED,
            "VERSION_CONFLICT",
            &format!("The {what} changed since it was read."),
            headers,
        ),
        prefix,
        current_version,
    )
}

pub(crate) fn retention_expired(what: &str, headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::GONE,
        "RETENTION_EXPIRED",
        &format!("The {what} was deleted too long ago and can no longer be restored."),
        headers,
    )
}

/// Where a request lives, resolved from a row.
pub(crate) struct Resolved {
    pub(crate) services: Arc<Services>,
    pub(crate) org_id: Uuid,
    pub(crate) workspace_id: Uuid,
    pub(crate) id: Uuid,
}

fn services_of(state: &ApiState, headers: &HeaderMap) -> Result<Arc<Services>, Box<Response>> {
    state
        .services
        .clone()
        .ok_or_else(|| Box::new(unavailable(headers, "AUTH_UNAVAILABLE")))
}

async fn resolve_workspace(
    state: &ApiState,
    headers: &HeaderMap,
    raw: &str,
) -> Result<Resolved, Box<Response>> {
    let services = services_of(state, headers)?;
    let Some(id) = parse_id(raw) else {
        return Err(Box::new(not_found(headers)));
    };
    match workspaces::org_of(&services.pool, id).await {
        Ok(Some(org_id)) => Ok(Resolved {
            services,
            org_id,
            workspace_id: id,
            id,
        }),
        Ok(None) => Err(Box::new(not_found(headers))),
        Err(error) => Err(Box::new(db_failure(
            &error,
            headers,
            "workspace lookup failed",
        ))),
    }
}

pub(crate) async fn resolve_project(
    state: &ApiState,
    headers: &HeaderMap,
    raw: &str,
) -> Result<Resolved, Box<Response>> {
    let services = services_of(state, headers)?;
    let Some(id) = parse_id(raw) else {
        return Err(Box::new(not_found(headers)));
    };
    match projects::locate(&services.pool, id).await {
        Ok(Some(location)) => Ok(Resolved {
            services,
            org_id: location.org_id,
            workspace_id: location.workspace_id,
            id,
        }),
        Ok(None) => Err(Box::new(not_found(headers))),
        Err(error) => Err(Box::new(db_failure(
            &error,
            headers,
            "project lookup failed",
        ))),
    }
}

/// Policy decision for a read of the resolved workspace; the scope of the
/// permitted action.
pub(crate) async fn read_gate(
    resolved: &Resolved,
    session: &AuthenticatedSession,
    headers: &HeaderMap,
    action: Action,
    name: &str,
    target: Uuid,
) -> Result<OrgScope, Box<Response>> {
    let actor = read_actor(
        &resolved.services.pool,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
    )
    .await
    .map_err(|error| Box::new(db_failure(&error, headers, "actor lookup failed")))?;
    let decision = can(
        &actor,
        action,
        &Resource::Workspace {
            org_id: resolved.org_id,
            workspace_id: resolved.workspace_id,
        },
    );
    match gate(
        decision,
        headers,
        session.user_id,
        name,
        Some(resolved.org_id),
        Some(target),
    ) {
        Gate::Open(permit) => {
            OrgScope::from_permit(&permit).ok_or_else(|| Box::new(internal(headers)))
        }
        Gate::Closed(response) => Err(Box::new(response)),
    }
}

/// Locks the organization, loads fresh roles and decides; returns the open
/// transaction and the scope of the permitted action.
pub(crate) async fn write_gate(
    resolved: &Resolved,
    session: &AuthenticatedSession,
    headers: &HeaderMap,
    action: Action,
    name: &str,
    target: Uuid,
) -> Result<(OrgTx, OrgScope), Box<Response>> {
    let Locked { tx, actor } = lock(
        &resolved.services.pool,
        session.user_id,
        resolved.org_id,
        resolved.workspace_id,
        headers,
    )
    .await?;
    let decision = can(
        &actor,
        action,
        &Resource::Workspace {
            org_id: resolved.org_id,
            workspace_id: resolved.workspace_id,
        },
    );
    match gate(
        decision,
        headers,
        session.user_id,
        name,
        Some(resolved.org_id),
        Some(target),
    ) {
        Gate::Open(permit) => OrgScope::from_permit(&permit)
            .map(|scope| (tx, scope))
            .ok_or_else(|| Box::new(internal(headers))),
        Gate::Closed(response) => Err(Box::new(response)),
    }
}

/// Paging parameters of a listing with the `deleted` (trash) switch.
pub(crate) fn trash_query(
    query: Result<Query<TrashListQuery>, QueryRejection>,
    headers: &HeaderMap,
) -> Result<(PageQuery, bool), Box<Response>> {
    let Ok(Query(query)) = query else {
        return Err(Box::new(validation_failed(&["query"], headers)));
    };
    query
        .split()
        .map_err(|_| Box::new(validation_failed(&["deleted"], headers)))
}

// ---- JSON ------------------------------------------------------------------------

fn project_json(p: &Project) -> Value {
    json!({
        "id": p.id.to_string(),
        "organization_id": p.org_id.to_string(),
        "workspace_id": p.workspace_id.to_string(),
        "name": p.name,
        "version": p.version,
        "created_by": p.created_by_user_id.to_string(),
        "created_at": rfc3339(p.created_at),
        "updated_by": p.updated_by_user_id.to_string(),
        "updated_at": rfc3339(p.updated_at),
        "deleted_at": opt_time(p.deleted_at),
    })
}

const ETAG: &str = "prj";

fn project_response(status: StatusCode, p: &Project) -> Response {
    with_etag(
        (status, Json(project_json(p))).into_response(),
        ETAG,
        p.version,
    )
}

// ---- POST /workspaces/{id}/projects ------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateProjectRequest {
    name: String,
}

pub(crate) async fn create_project(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(ws_raw): Path<String>,
    body: Result<Json<CreateProjectRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    let resolved = match resolve_workspace(&state, &headers, &ws_raw).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let (mut tx, scope) = match write_gate(
        &resolved,
        &session,
        &headers,
        Action::ProjectCreate,
        "project.create",
        resolved.workspace_id,
    )
    .await
    {
        Ok(opened) => opened,
        Err(response) => return *response,
    };
    let Ok(name) = ProjectName::parse(&request.name) else {
        return validation_failed(&["name"], &headers);
    };
    let now = resolved.services.clock.now();
    match tx
        .project_create(&scope, resolved.workspace_id, &name, session.user_id, now)
        .await
    {
        Ok(ProjectCreateOutcome::Created(project)) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "project.create",
                    resolved.org_id,
                    Some(project.id),
                )
                .workspace(resolved.workspace_id),
            )
            .await
            {
                return db_failure(&error, &headers, "project creation failed");
            }
            project_response(StatusCode::CREATED, &project)
        }
        Ok(ProjectCreateOutcome::NameTaken) => conflict(
            "PROJECT_NAME_TAKEN",
            "A project with this name exists in the workspace.",
            &headers,
        ),
        Ok(ProjectCreateOutcome::WorkspaceGone) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "project creation failed"),
    }
}

// ---- GET /workspaces/{id}/projects ---------------------------------------------------

pub(crate) async fn list_projects(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(ws_raw): Path<String>,
    query: Result<Query<TrashListQuery>, QueryRejection>,
) -> Response {
    let resolved = match resolve_workspace(&state, &headers, &ws_raw).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let scope = match read_gate(
        &resolved,
        &session,
        &headers,
        Action::ProjectList,
        "project.list",
        resolved.workspace_id,
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
    // The cursor is bound to the workspace and to the live/trash switch.
    let scope_text = format!("{}:{deleted}", resolved.workspace_id);
    let after = match page
        .cursor()
        .map(|c| decode_time_cursor(c, "projects", &scope_text))
    {
        Some(Ok(key)) => Some(key),
        Some(Err(error)) => return page_error(error, &headers),
        None => None,
    };
    let mut rows = match projects::list(
        &resolved.services.pool,
        &scope,
        resolved.workspace_id,
        deleted,
        limit + 1,
        after,
    )
    .await
    {
        Ok(rows) => rows,
        Err(error) => return db_failure(&error, &headers, "project list failed"),
    };
    let next = if rows.len() > limit as usize {
        rows.truncate(limit as usize);
        rows.last()
            .map(|last| encode_time_cursor("projects", &scope_text, last.created_at, last.id))
    } else {
        None
    };
    let items: Vec<_> = rows.iter().map(project_json).collect();
    Json(json!({ "items": items, "next_cursor": next })).into_response()
}

// ---- GET /projects/{id} -----------------------------------------------------------------

pub(crate) async fn get_project(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(project_raw): Path<String>,
) -> Response {
    let resolved = match resolve_project(&state, &headers, &project_raw).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let scope = match read_gate(
        &resolved,
        &session,
        &headers,
        Action::ProjectRead,
        "project.read",
        resolved.id,
    )
    .await
    {
        Ok(scope) => scope,
        Err(response) => return *response,
    };
    match projects::get(&resolved.services.pool, &scope, resolved.id).await {
        Ok(Some(project)) => project_response(StatusCode::OK, &project),
        Ok(None) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "project read failed"),
    }
}

// ---- PATCH /projects/{id} ----------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateProjectRequest {
    name: String,
}

pub(crate) async fn update_project(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(project_raw): Path<String>,
    body: Result<Json<UpdateProjectRequest>, JsonRejection>,
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
        Action::ProjectRename,
        "project.update",
        resolved.id,
    )
    .await
    {
        Ok(opened) => opened,
        Err(response) => return *response,
    };
    let Ok(name) = ProjectName::parse(&request.name) else {
        return validation_failed(&["name"], &headers);
    };
    let expected = match expected_version(&headers, ETAG) {
        Ok(version) => version,
        Err(response) => return *response,
    };
    let now = resolved.services.clock.now();
    match tx
        .project_rename(&scope, resolved.id, &name, expected, session.user_id, now)
        .await
    {
        Ok(ProjectRenameOutcome::Updated(project)) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "project.update",
                    resolved.org_id,
                    Some(resolved.id),
                )
                .workspace(resolved.workspace_id),
            )
            .await
            {
                return db_failure(&error, &headers, "project update failed");
            }
            project_response(StatusCode::OK, &project)
        }
        Ok(ProjectRenameOutcome::VersionMismatch { current_version }) => {
            version_conflict("project", ETAG, current_version, &headers)
        }
        Ok(ProjectRenameOutcome::NameTaken) => conflict(
            "PROJECT_NAME_TAKEN",
            "A project with this name exists in the workspace.",
            &headers,
        ),
        Ok(ProjectRenameOutcome::Deleted) => conflict(
            "PROJECT_DELETED",
            "The project is in the trash; restore it first.",
            &headers,
        ),
        Ok(ProjectRenameOutcome::Gone) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "project update failed"),
    }
}

// ---- DELETE /projects/{id} ------------------------------------------------------------------

pub(crate) async fn delete_project(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(project_raw): Path<String>,
) -> Response {
    let resolved = match resolve_project(&state, &headers, &project_raw).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let (mut tx, scope) = match write_gate(
        &resolved,
        &session,
        &headers,
        Action::ProjectDelete,
        "project.delete",
        resolved.id,
    )
    .await
    {
        Ok(opened) => opened,
        Err(response) => return *response,
    };
    let now = resolved.services.clock.now();
    match tx
        .project_delete(&scope, resolved.id, session.user_id, now)
        .await
    {
        Ok(ProjectDeleteOutcome::Deleted(_)) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "project.delete",
                    resolved.org_id,
                    Some(resolved.id),
                )
                .workspace(resolved.workspace_id),
            )
            .await
            {
                return db_failure(&error, &headers, "project deletion failed");
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(ProjectDeleteOutcome::Unchanged(_)) => StatusCode::NO_CONTENT.into_response(),
        Ok(ProjectDeleteOutcome::Gone) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "project deletion failed"),
    }
}

// ---- POST /projects/{id}/restore --------------------------------------------------------------

pub(crate) async fn restore_project(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(project_raw): Path<String>,
) -> Response {
    let resolved = match resolve_project(&state, &headers, &project_raw).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let (mut tx, scope) = match write_gate(
        &resolved,
        &session,
        &headers,
        Action::ProjectRestore,
        "project.restore",
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
        .project_restore(&scope, resolved.id, session.user_id, now, retention)
        .await
    {
        Ok(ProjectRestoreOutcome::Restored(project)) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "project.restore",
                    resolved.org_id,
                    Some(resolved.id),
                )
                .workspace(resolved.workspace_id),
            )
            .await
            {
                return db_failure(&error, &headers, "project restore failed");
            }
            project_response(StatusCode::OK, &project)
        }
        Ok(ProjectRestoreOutcome::Unchanged(project)) => project_response(StatusCode::OK, &project),
        Ok(ProjectRestoreOutcome::Expired) => retention_expired("project", &headers),
        Ok(ProjectRestoreOutcome::NameTaken) => conflict(
            "PROJECT_NAME_TAKEN",
            "A project with this name exists in the workspace.",
            &headers,
        ),
        Ok(ProjectRestoreOutcome::Gone) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "project restore failed"),
    }
}
