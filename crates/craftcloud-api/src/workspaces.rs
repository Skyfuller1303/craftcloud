//! Workspace and workspace member routes (CC-F04).
//!
//! The organization of every workspace request is resolved from the workspace
//! row, never from a client supplied organization id. Reads decide with the
//! caller's freshly loaded roles; every change runs in an `OrgTx` (organization
//! row lock, fresh roles, policy decision, change), so concurrent changes are
//! serialized. Callers who cannot see a workspace (other organization, no
//! membership, plain members without a workspace role, unknown ids) all get the
//! same 404.

use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use craftcloud_core::policy::{
    Action, Actor, AppActor, OrgMembership, OrgRole, Resource, WorkspaceMemberTarget,
    WorkspaceMembership, WorkspaceRole, can,
};
use craftcloud_core::{DisplayName, Slug};
use craftcloud_db::audit::Detail;
use craftcloud_db::members::OrgTx;
use craftcloud_db::workspaces::{
    self, NewWorkspace, Workspace, WorkspaceAddOutcome, WorkspaceMember, WorkspaceRoleOutcome,
    WorkspaceUpdateOutcome,
};
use craftcloud_db::{DbError, OrgScope};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit::{Change, commit_audited};
use crate::orgs::{Gate, conflict, db_failure, gate, not_found, page_error, parse_id, rfc3339};
use crate::pagination::{PageQuery, decode_time_cursor, encode_time_cursor};
use crate::responses::{internal, rejection_response, unavailable, validation_failed};
use crate::session::AuthenticatedSession;
use crate::{ApiState, error_response};

fn workspace_json(ws: &Workspace, my_role: Option<WorkspaceRole>) -> Value {
    json!({
        "id": ws.id.to_string(),
        "organization_id": ws.org_id.to_string(),
        "slug": ws.slug,
        "name": ws.name,
        "workspace_role": my_role.map(WorkspaceRole::as_str),
        "version": ws.version,
        "created_at": rfc3339(ws.created_at),
    })
}

fn member_json(member: &WorkspaceMember) -> Value {
    json!({
        "user_id": member.user_id.to_string(),
        "display_name": member.display_name,
        "email": member.email,
        "role": member.role,
        "joined_at": rfc3339(member.joined_at),
    })
}

fn etag_value(version: i64) -> Option<axum::http::HeaderValue> {
    axum::http::HeaderValue::from_str(&format!("\"ws-v{version}\"")).ok()
}

fn with_etag(mut response: Response, version: i64) -> Response {
    if let Some(value) = etag_value(version) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

fn parse_etag(raw: &str) -> Option<i64> {
    raw.trim()
        .strip_prefix("\"ws-v")?
        .strip_suffix('"')?
        .parse::<i64>()
        .ok()
}

/// Read-side actor for a workspace request: org role and workspace role from
/// the database for this request.
pub(crate) async fn read_actor(
    services: &crate::Services,
    user_id: Uuid,
    org_id: Uuid,
    workspace_id: Uuid,
    app_id: Option<&str>,
) -> Result<AppActor, DbError> {
    let (org_role, ws_role) =
        workspaces::roles(&services.pool, org_id, workspace_id, user_id).await?;
    let app_access = match app_id {
        Some(app) => {
            let ids: Vec<String> = services
                .settings
                .app_catalog
                .iter()
                .map(|entry| entry.id.clone())
                .collect();
            Some((
                app.to_owned(),
                craftcloud_db::app_access::is_allowed(&services.pool, org_id, user_id, app, &ids)
                    .await?,
            ))
        }
        None => None,
    };
    Ok(AppActor {
        actor: Actor {
            user_id,
            instance_operator: false,
            membership: org_role.map(|role| OrgMembership { org_id, role }),
            workspace: ws_role.map(|role| WorkspaceMembership { workspace_id, role }),
        },
        app_access,
        tool_access: None,
    })
}

/// Restriction of launcher and CLI device sessions in the organization:
/// `Some(allowed)` for those sessions, `None` for browser and app sessions.
pub(crate) async fn tool_access(
    services: &crate::Services,
    user_id: Uuid,
    org_id: Uuid,
    client_kind: &str,
) -> Result<Option<bool>, DbError> {
    if !matches!(client_kind, "launcher" | "cli") {
        return Ok(None);
    }
    Ok(Some(
        craftcloud_db::app_access::tool_allowed(&services.pool, org_id, user_id).await?,
    ))
}

/// Opens a locked transaction for a workspace request and loads the actor
/// inside it. `None` responses are the uniform 404.
pub(crate) struct Locked {
    pub(crate) tx: OrgTx,
    pub(crate) actor: AppActor,
}

impl Locked {
    /// Applies the launcher/CLI restriction of the session's client kind
    /// under the same organization lock.
    pub(crate) async fn restrict_tool_session(
        &mut self,
        user_id: Uuid,
        client_kind: &str,
    ) -> Result<(), DbError> {
        if matches!(client_kind, "launcher" | "cli") {
            self.actor.tool_access = Some(self.tx.tool_allowed(user_id).await?);
        }
        Ok(())
    }
}

pub(crate) async fn lock(
    services: &crate::Services,
    user_id: Uuid,
    org_id: Uuid,
    workspace_id: Uuid,
    headers: &HeaderMap,
    app_id: Option<&str>,
) -> Result<Locked, Box<Response>> {
    let mut tx = match OrgTx::begin(&services.pool, org_id).await {
        Ok(Some(tx)) => tx,
        Ok(None) => return Err(Box::new(not_found(headers))),
        Err(error) => {
            return Err(Box::new(db_failure(
                &error,
                headers,
                "workspace transaction failed",
            )));
        }
    };
    match tx.workspace_exists(workspace_id).await {
        Ok(true) => {}
        Ok(false) => return Err(Box::new(not_found(headers))),
        Err(error) => {
            return Err(Box::new(db_failure(
                &error,
                headers,
                "workspace lookup failed",
            )));
        }
    }
    let roles = async {
        let org_role = tx.role_of(user_id).await?;
        let ws_role = tx.workspace_role_of(workspace_id, user_id).await?;
        Ok::<_, DbError>((org_role, ws_role))
    }
    .await;
    let (org_role, ws_role) = match roles {
        Ok(roles) => roles,
        Err(error) => return Err(Box::new(db_failure(&error, headers, "role lookup failed"))),
    };
    let app_access = if let Some(app) = app_id {
        let ids: Vec<String> = services
            .settings
            .app_catalog
            .iter()
            .map(|entry| entry.id.clone())
            .collect();
        match tx.app_allowed(user_id, app, &ids).await {
            Ok(allowed) => Some((app.to_owned(), allowed)),
            Err(error) => {
                return Err(Box::new(db_failure(
                    &error,
                    headers,
                    "app assignment lookup failed",
                )));
            }
        }
    } else {
        None
    };
    let actor = AppActor {
        actor: Actor {
            user_id,
            instance_operator: false,
            membership: org_role.map(|role| OrgMembership { org_id, role }),
            workspace: ws_role.map(|role| WorkspaceMembership { workspace_id, role }),
        },
        app_access,
        tool_access: None,
    };
    Ok(Locked { tx, actor })
}

/// Resolves the organization from the workspace row; unknown ids are 404.
async fn resolve(
    state: &ApiState,
    headers: &HeaderMap,
    raw: &str,
) -> Result<(std::sync::Arc<crate::Services>, Uuid, Uuid), Box<Response>> {
    let Some(services) = state.services.clone() else {
        return Err(Box::new(unavailable(headers, "AUTH_UNAVAILABLE")));
    };
    let Some(workspace_id) = parse_id(raw) else {
        return Err(Box::new(not_found(headers)));
    };
    match workspaces::org_of(&services.pool, workspace_id).await {
        Ok(Some(org_id)) => Ok((services, workspace_id, org_id)),
        Ok(None) => Err(Box::new(not_found(headers))),
        Err(error) => Err(Box::new(db_failure(
            &error,
            headers,
            "workspace lookup failed",
        ))),
    }
}

// ---- GET /organizations/{org_id}/workspaces ----------------------------------

pub(crate) async fn list_workspaces(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(org_raw): Path<String>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let Some(org_id) = parse_id(&org_raw) else {
        return not_found(&headers);
    };
    let role =
        match craftcloud_db::organizations::active_role(&services.pool, org_id, session.user_id)
            .await
        {
            Ok(role) => role,
            Err(error) => return db_failure(&error, &headers, "actor lookup failed"),
        };
    let actor = Actor {
        user_id: session.user_id,
        instance_operator: false,
        membership: role.map(|role| OrgMembership { org_id, role }),
        workspace: None,
    };
    let decision = can(
        &actor,
        Action::WorkspaceList,
        &Resource::Organization { org_id },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "workspace.list",
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
        .map(|c| decode_time_cursor(c, "workspaces", &scope_text))
    {
        Some(Ok(key)) => Some(key),
        Some(Err(error)) => return page_error(error, &headers),
        None => None,
    };
    // Owners and admins see every workspace, others only those they belong to.
    let only_member_of = match role {
        Some(OrgRole::Owner | OrgRole::Admin) => None,
        _ => Some(session.user_id),
    };
    let mut rows =
        match workspaces::list_visible(&services.pool, &scope, only_member_of, limit + 1, after)
            .await
        {
            Ok(rows) => rows,
            Err(error) => return db_failure(&error, &headers, "workspace list failed"),
        };
    let next = if rows.len() > limit as usize {
        rows.truncate(limit as usize);
        rows.last()
            .map(|last| encode_time_cursor("workspaces", &scope_text, last.created_at, last.id))
    } else {
        None
    };
    let items: Vec<_> = rows.iter().map(|w| workspace_json(w, None)).collect();
    Json(json!({ "items": items, "next_cursor": next })).into_response()
}

// ---- POST /organizations/{org_id}/workspaces ---------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateWorkspaceRequest {
    name: String,
    slug: String,
}

pub(crate) async fn create_workspace(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(org_raw): Path<String>,
    body: Result<Json<CreateWorkspaceRequest>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    let Some(org_id) = parse_id(&org_raw) else {
        return not_found(&headers);
    };
    let mut tx = match OrgTx::begin(&services.pool, org_id).await {
        Ok(Some(tx)) => tx,
        Ok(None) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "organization transaction failed"),
    };
    let role = match tx.role_of(session.user_id).await {
        Ok(role) => role,
        Err(error) => return db_failure(&error, &headers, "actor lookup failed"),
    };
    let actor = Actor {
        user_id: session.user_id,
        instance_operator: false,
        membership: role.map(|role| OrgMembership { org_id, role }),
        workspace: None,
    };
    let decision = can(
        &actor,
        Action::WorkspaceCreate,
        &Resource::Organization { org_id },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "workspace.create",
        Some(org_id),
        None,
    ) {
        Gate::Open(permit) => permit,
        Gate::Closed(response) => return response,
    };
    let Some(scope) = OrgScope::from_permit(&permit) else {
        return internal(&headers);
    };
    let (name, slug) = (
        DisplayName::parse(&request.name),
        Slug::parse(&request.slug),
    );
    let mut invalid = Vec::new();
    if name.is_err() {
        invalid.push("name");
    }
    if slug.is_err() {
        invalid.push("slug");
    }
    let (Ok(name), Ok(slug)) = (name, slug) else {
        return validation_failed(&invalid, &headers);
    };
    let created = tx
        .workspace_create(
            &scope,
            &NewWorkspace {
                slug: slug.as_str().to_owned(),
                name: name.as_str().to_owned(),
                quota_bytes: None,
            },
        )
        .await;
    match created {
        Ok(workspace) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "workspace.create",
                    org_id,
                    Some(workspace.id),
                )
                .workspace(workspace.id),
            )
            .await
            {
                return db_failure(&error, &headers, "workspace creation failed");
            }
            let version = workspace.version;
            with_etag(
                (StatusCode::CREATED, Json(workspace_json(&workspace, None))).into_response(),
                version,
            )
        }
        Err(error) if error.constraint() == Some("workspaces_org_slug_unique") => {
            validation_failed(&["slug"], &headers)
        }
        Err(error) => db_failure(&error, &headers, "workspace creation failed"),
    }
}

// ---- GET /workspaces/{id} ------------------------------------------------------

pub(crate) async fn get_workspace(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(ws_raw): Path<String>,
) -> Response {
    let (services, workspace_id, org_id) = match resolve(&state, &headers, &ws_raw).await {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let actor = match read_actor(
        &services,
        session.user_id,
        org_id,
        workspace_id,
        session.app_id.as_deref(),
    )
    .await
    {
        Ok(actor) => actor,
        Err(error) => return db_failure(&error, &headers, "actor lookup failed"),
    };
    let decision = can(
        &actor,
        Action::WorkspaceRead,
        &Resource::Workspace {
            org_id,
            workspace_id,
        },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "workspace.read",
        Some(org_id),
        Some(workspace_id),
    ) {
        Gate::Open(permit) => permit,
        Gate::Closed(response) => return response,
    };
    let Some(scope) = OrgScope::from_permit(&permit) else {
        return internal(&headers);
    };
    match workspaces::get(&services.pool, &scope, workspace_id).await {
        Ok(Some(workspace)) => {
            let version = workspace.version;
            let role = actor.workspace.map(|w| w.role);
            with_etag(
                Json(workspace_json(&workspace, role)).into_response(),
                version,
            )
        }
        Ok(None) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "workspace read failed"),
    }
}

// ---- PATCH /workspaces/{id} ------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateWorkspaceRequest {
    name: String,
}

pub(crate) async fn update_workspace(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(ws_raw): Path<String>,
    body: Result<Json<UpdateWorkspaceRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    let (services, workspace_id, org_id) = match resolve(&state, &headers, &ws_raw).await {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let Locked { mut tx, actor } = match lock(
        &services,
        session.user_id,
        org_id,
        workspace_id,
        &headers,
        session.app_id.as_deref(),
    )
    .await
    {
        Ok(locked) => locked,
        Err(response) => return *response,
    };
    let decision = can(
        &actor,
        Action::WorkspaceManage,
        &Resource::Workspace {
            org_id,
            workspace_id,
        },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "workspace.update",
        Some(org_id),
        Some(workspace_id),
    ) {
        Gate::Open(permit) => permit,
        Gate::Closed(response) => return response,
    };
    let Some(scope) = OrgScope::from_permit(&permit) else {
        return internal(&headers);
    };
    let Ok(name) = DisplayName::parse(&request.name) else {
        return validation_failed(&["name"], &headers);
    };
    let Some(if_match) = headers.get(header::IF_MATCH) else {
        return error_response(
            StatusCode::PRECONDITION_REQUIRED,
            "PRECONDITION_REQUIRED",
            "An If-Match header with the ETag from the last read is required.",
            &headers,
        );
    };
    let expected = if_match.to_str().ok().and_then(parse_etag).unwrap_or(-1);
    match tx
        .workspace_update_name(&scope, workspace_id, name.as_str(), expected)
        .await
    {
        Ok(WorkspaceUpdateOutcome::Updated(workspace)) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "workspace.update",
                    org_id,
                    Some(workspace_id),
                )
                .workspace(workspace_id),
            )
            .await
            {
                return db_failure(&error, &headers, "workspace update failed");
            }
            let version = workspace.version;
            let role = actor.workspace.map(|w| w.role);
            with_etag(
                Json(workspace_json(&workspace, role)).into_response(),
                version,
            )
        }
        Ok(WorkspaceUpdateOutcome::VersionMismatch { current_version }) => with_etag(
            error_response(
                StatusCode::PRECONDITION_FAILED,
                "VERSION_CONFLICT",
                "The workspace changed since it was read.",
                &headers,
            ),
            current_version,
        ),
        Ok(WorkspaceUpdateOutcome::Gone) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "workspace update failed"),
    }
}

// ---- members ---------------------------------------------------------------------

pub(crate) async fn list_members(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(ws_raw): Path<String>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let (services, workspace_id, org_id) = match resolve(&state, &headers, &ws_raw).await {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let actor = match read_actor(
        &services,
        session.user_id,
        org_id,
        workspace_id,
        session.app_id.as_deref(),
    )
    .await
    {
        Ok(actor) => actor,
        Err(error) => return db_failure(&error, &headers, "actor lookup failed"),
    };
    let decision = can(
        &actor,
        Action::WorkspaceMemberList,
        &Resource::WorkspaceMember {
            org_id,
            workspace_id,
            target: None,
            role: None,
        },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "workspace.member.list",
        Some(org_id),
        Some(workspace_id),
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
    let scope_text = workspace_id.to_string();
    let after = match page
        .cursor()
        .map(|c| decode_time_cursor(c, "ws-members", &scope_text))
    {
        Some(Ok(key)) => Some(key),
        Some(Err(error)) => return page_error(error, &headers),
        None => None,
    };
    let mut rows = match workspaces::list_members(
        &services.pool,
        &scope,
        workspace_id,
        limit + 1,
        after,
    )
    .await
    {
        Ok(rows) => rows,
        Err(error) => return db_failure(&error, &headers, "workspace member list failed"),
    };
    let next = if rows.len() > limit as usize {
        rows.truncate(limit as usize);
        rows.last()
            .map(|last| encode_time_cursor("ws-members", &scope_text, last.joined_at, last.user_id))
    } else {
        None
    };
    let items: Vec<_> = rows.iter().map(member_json).collect();
    Json(json!({ "items": items, "next_cursor": next })).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AddWorkspaceMemberRequest {
    user_id: String,
    role: String,
}

pub(crate) async fn add_member(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(ws_raw): Path<String>,
    body: Result<Json<AddWorkspaceMemberRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    let (user_id, role) = (
        parse_id(&request.user_id),
        WorkspaceRole::parse(&request.role),
    );
    let mut invalid = Vec::new();
    if user_id.is_none() {
        invalid.push("user_id");
    }
    if role.is_none() {
        invalid.push("role");
    }
    let (Some(user_id), Some(role)) = (user_id, role) else {
        return validation_failed(&invalid, &headers);
    };
    let (services, workspace_id, org_id) = match resolve(&state, &headers, &ws_raw).await {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let Locked { mut tx, actor } = match lock(
        &services,
        session.user_id,
        org_id,
        workspace_id,
        &headers,
        session.app_id.as_deref(),
    )
    .await
    {
        Ok(locked) => locked,
        Err(response) => return *response,
    };
    let decision = can(
        &actor,
        Action::WorkspaceMemberAdd,
        &Resource::WorkspaceMember {
            org_id,
            workspace_id,
            target: None,
            role: Some(role),
        },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "workspace.member.add",
        Some(org_id),
        Some(user_id),
    ) {
        Gate::Open(permit) => permit,
        Gate::Closed(response) => return response,
    };
    let Some(scope) = OrgScope::from_permit(&permit) else {
        return internal(&headers);
    };
    match tx
        .workspace_member_add(&scope, workspace_id, user_id, role)
        .await
    {
        Ok(WorkspaceAddOutcome::Added(member)) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "workspace.member.add",
                    org_id,
                    Some(user_id),
                )
                .workspace(workspace_id)
                .detail("role", Detail::Code(role.as_str())),
            )
            .await
            {
                return db_failure(&error, &headers, "workspace member add failed");
            }
            (StatusCode::CREATED, Json(member_json(&member))).into_response()
        }
        Ok(WorkspaceAddOutcome::AlreadyMember) => conflict(
            "ALREADY_MEMBER",
            "The user is already a member of the workspace.",
            &headers,
        ),
        // Unknown users and users outside the organization look the same.
        Ok(WorkspaceAddOutcome::NotOrgMember | WorkspaceAddOutcome::WorkspaceGone) => {
            not_found(&headers)
        }
        Err(error) => db_failure(&error, &headers, "workspace member add failed"),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChangeWorkspaceRoleRequest {
    role: String,
}

pub(crate) async fn change_member_role(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path((ws_raw, user_raw)): Path<(String, String)>,
    body: Result<Json<ChangeWorkspaceRoleRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    let Some(new_role) = WorkspaceRole::parse(&request.role) else {
        return validation_failed(&["role"], &headers);
    };
    let Some(target_id) = parse_id(&user_raw) else {
        return not_found(&headers);
    };
    let (services, workspace_id, org_id) = match resolve(&state, &headers, &ws_raw).await {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let Locked { mut tx, actor } = match lock(
        &services,
        session.user_id,
        org_id,
        workspace_id,
        &headers,
        session.app_id.as_deref(),
    )
    .await
    {
        Ok(locked) => locked,
        Err(response) => return *response,
    };
    let target_role = match tx.workspace_role_of(workspace_id, target_id).await {
        Ok(role) => role,
        Err(error) => return db_failure(&error, &headers, "role lookup failed"),
    };
    let target = target_role.map(|role| WorkspaceMemberTarget {
        user_id: target_id,
        role,
    });
    let decision = can(
        &actor,
        Action::WorkspaceMemberChangeRole,
        &Resource::WorkspaceMember {
            org_id,
            workspace_id,
            target,
            role: Some(new_role),
        },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "workspace.member.change_role",
        Some(org_id),
        Some(target_id),
    ) {
        Gate::Open(permit) => permit,
        Gate::Closed(response) => return response,
    };
    let Some(scope) = OrgScope::from_permit(&permit) else {
        return internal(&headers);
    };
    match tx
        .workspace_member_set_role(&scope, workspace_id, target_id, new_role)
        .await
    {
        Ok(WorkspaceRoleOutcome::Changed(member)) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "workspace.member.change_role",
                    org_id,
                    Some(target_id),
                )
                .workspace(workspace_id)
                .detail("role", Detail::Code(new_role.as_str())),
            )
            .await
            {
                return db_failure(&error, &headers, "role change failed");
            }
            Json(member_json(&member)).into_response()
        }
        Ok(WorkspaceRoleOutcome::Unchanged(member)) => Json(member_json(&member)).into_response(),
        Ok(WorkspaceRoleOutcome::NotMember) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "role change failed"),
    }
}

pub(crate) async fn remove_member(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path((ws_raw, user_raw)): Path<(String, String)>,
) -> Response {
    let Some(target_id) = parse_id(&user_raw) else {
        return not_found(&headers);
    };
    let (services, workspace_id, org_id) = match resolve(&state, &headers, &ws_raw).await {
        Ok(found) => found,
        Err(response) => return *response,
    };
    let Locked { mut tx, actor } = match lock(
        &services,
        session.user_id,
        org_id,
        workspace_id,
        &headers,
        session.app_id.as_deref(),
    )
    .await
    {
        Ok(locked) => locked,
        Err(response) => return *response,
    };
    let target_role = match tx.workspace_role_of(workspace_id, target_id).await {
        Ok(role) => role,
        Err(error) => return db_failure(&error, &headers, "role lookup failed"),
    };
    let target = target_role.map(|role| WorkspaceMemberTarget {
        user_id: target_id,
        role,
    });
    let decision = can(
        &actor,
        Action::WorkspaceMemberRemove,
        &Resource::WorkspaceMember {
            org_id,
            workspace_id,
            target,
            role: None,
        },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "workspace.member.remove",
        Some(org_id),
        Some(target_id),
    ) {
        Gate::Open(permit) => permit,
        Gate::Closed(response) => return response,
    };
    let Some(scope) = OrgScope::from_permit(&permit) else {
        return internal(&headers);
    };
    match tx
        .workspace_member_remove(&scope, workspace_id, target_id)
        .await
    {
        Ok(true) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "workspace.member.remove",
                    org_id,
                    Some(target_id),
                )
                .workspace(workspace_id)
                .detail("self_removal", Detail::Flag(target_id == session.user_id)),
            )
            .await
            {
                return db_failure(&error, &headers, "member removal failed");
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "member removal failed"),
    }
}
