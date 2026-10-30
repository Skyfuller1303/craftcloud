//! Tenant-scoped project and file grants.
use crate::orgs::{Gate, db_failure, gate, not_found, parse_id, rfc3339};
use crate::pagination::{PageQuery, decode_time_cursor, encode_time_cursor};
use crate::responses::{rejection_response, unavailable, validation_failed};
use crate::workspaces::lock;
use crate::{
    ApiState,
    audit::{Change, commit_audited},
    projects::{self, Resolved},
    session::AuthenticatedSession,
};
use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use craftcloud_core::policy::{Action, Resource, can};
use craftcloud_db::grants::{self, ChangeOutcome, CreateOutcome, GrantRole, Target, TargetKind};
use serde::Deserialize;
use serde_json::json;

fn grant_json(grant: &grants::Grant) -> serde_json::Value {
    json!({"id":grant.id.to_string(),"organization_id":grant.org_id.to_string(),"workspace_id":grant.workspace_id.to_string(),"target_kind":grant.target_kind,"target_id":grant.target_id.to_string(),"grantee":{"user_id":grant.grantee_user_id.to_string(),"display_name":grant.display_name,"email":grant.email},"role":grant.role,"created_by":grant.created_by_user_id.to_string(),"created_at":rfc3339(grant.created_at),"version":grant.version})
}

async fn target(
    state: &ApiState,
    headers: &HeaderMap,
    kind: TargetKind,
    raw: &str,
) -> Result<Resolved, Box<Response>> {
    let Some(services) = state.services.clone() else {
        return Err(Box::new(unavailable(headers, "AUTH_UNAVAILABLE")));
    };
    let Some(id) = parse_id(raw) else {
        return Err(Box::new(not_found(headers)));
    };
    let location = match kind {
        TargetKind::Project => craftcloud_db::projects::locate(&services.pool, id)
            .await
            .map(|v| v.map(|p| (p.org_id, p.workspace_id))),
        TargetKind::File => craftcloud_db::files::locate(&services.pool, id)
            .await
            .map(|v| v.map(|f| (f.org_id, f.workspace_id))),
    };
    match location {
        Ok(Some((org_id, workspace_id))) => Ok(Resolved {
            services,
            org_id,
            workspace_id,
            id,
        }),
        Ok(None) => Err(Box::new(not_found(headers))),
        Err(e) => Err(Box::new(db_failure(&e, headers, "target lookup failed"))),
    }
}

async fn list_for(
    state: ApiState,
    headers: HeaderMap,
    session: AuthenticatedSession,
    raw: String,
    kind: TargetKind,
) -> Response {
    let resolved = match target(&state, &headers, kind, &raw).await {
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
            Action::GrantList,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "grant lookup failed"),
    };
    match gate(
        can(&opened.actor, Action::GrantList, &resource),
        &headers,
        session.user_id,
        "grant.list",
        Some(resolved.org_id),
        Some(resolved.id),
    ) {
        Gate::Closed(r) => r,
        Gate::Open(permit) => {
            let Some(scope) = craftcloud_db::OrgScope::from_permit(&permit) else {
                return crate::responses::internal(&headers);
            };
            let rows = match opened
                .tx
                .grant_list(
                    &scope,
                    Target {
                        org_id: resolved.org_id,
                        workspace_id: resolved.workspace_id,
                        kind,
                        id: resolved.id,
                    },
                )
                .await
            {
                Ok(v) => v,
                Err(e) => return db_failure(&e, &headers, "grant list failed"),
            };
            Json(
                json!({"items":rows.iter().map(grant_json).collect::<Vec<_>>(),"next_cursor":null}),
            )
            .into_response()
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrantRequest {
    user_id: String,
    role: String,
}

async fn create_for(
    state: ApiState,
    headers: HeaderMap,
    session: AuthenticatedSession,
    raw: String,
    kind: TargetKind,
    body: Result<Json<GrantRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(v)) => v,
        Err(e) => return rejection_response(&e, &headers),
    };
    let (Some(user_id), Some(role)) = (parse_id(&request.user_id), GrantRole::parse(&request.role))
    else {
        return validation_failed(&["user_id", "role"], &headers);
    };
    let resolved = match target(&state, &headers, kind, &raw).await {
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
            Action::GrantCreate,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "grant lookup failed"),
    };
    let permit = match gate(
        can(&opened.actor, Action::GrantCreate, &resource),
        &headers,
        session.user_id,
        "grant.created",
        Some(resolved.org_id),
        Some(resolved.id),
    ) {
        Gate::Open(p) => p,
        Gate::Closed(r) => return r,
    };
    let Some(scope) = craftcloud_db::OrgScope::from_permit(&permit) else {
        return crate::responses::internal(&headers);
    };
    let target = Target {
        org_id: resolved.org_id,
        workspace_id: resolved.workspace_id,
        kind,
        id: resolved.id,
    };
    let outcome = match opened
        .tx
        .grant_create(
            &scope,
            grants::NewGrant {
                target,
                grantee_user_id: user_id,
                role,
                created_by: session.user_id,
                now: resolved.services.clock.now(),
            },
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "grant creation failed"),
    };
    match outcome {
        CreateOutcome::Created(grant) => {
            if let Err(e) = commit_audited(
                opened.tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "grant.created",
                    resolved.org_id,
                    Some(grant.id),
                )
                .workspace(resolved.workspace_id)
                .detail("role", craftcloud_db::audit::Detail::Code(role.as_str())),
            )
            .await
            {
                return db_failure(&e, &headers, "grant creation failed");
            }
            let mut response = (StatusCode::CREATED, Json(grant_json(&grant))).into_response();
            if let Ok(etag) =
                header::HeaderValue::from_str(&format!("\"grant-v{}\"", grant.version))
            {
                response.headers_mut().insert(header::ETAG, etag);
            }
            response
        }
        CreateOutcome::TargetGone => not_found(&headers),
        CreateOutcome::FileDeleted => crate::orgs::conflict(
            "FILE_DELETED",
            "The file is in the trash; restore it first.",
            &headers,
        ),
        CreateOutcome::ProjectDeleted => crate::orgs::conflict(
            "PROJECT_DELETED",
            "The project is in the trash; restore it first.",
            &headers,
        ),
        CreateOutcome::NotMember => crate::orgs::conflict(
            "GRANTEE_NOT_MEMBER",
            "The recipient must be an active organization member.",
            &headers,
        ),
        CreateOutcome::AlreadyExists => crate::orgs::conflict(
            "GRANT_EXISTS",
            "An active grant already exists for this recipient.",
            &headers,
        ),
    }
}

macro_rules! target_handlers {
    ($list:ident,$create:ident,$kind:expr) => {
        pub(crate) async fn $list(
            State(state): State<ApiState>,
            headers: HeaderMap,
            session: AuthenticatedSession,
            Path(raw): Path<String>,
        ) -> Response {
            list_for(state, headers, session, raw, $kind).await
        }
        pub(crate) async fn $create(
            State(state): State<ApiState>,
            headers: HeaderMap,
            session: AuthenticatedSession,
            Path(raw): Path<String>,
            body: Result<Json<GrantRequest>, JsonRejection>,
        ) -> Response {
            create_for(state, headers, session, raw, $kind, body).await
        }
    };
}
target_handlers!(list_project, create_project, TargetKind::Project);
target_handlers!(list_file, create_file, TargetKind::File);

pub(crate) async fn shared_with_me(
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
    let Ok(Query(page)) = query else {
        return validation_failed(&["query"], &headers);
    };
    let limit = match page.limit() {
        Ok(v) => v,
        Err(e) => return crate::orgs::page_error(e, &headers),
    };
    let scope_text = org_id.to_string();
    let after = match page
        .cursor()
        .map(|v| decode_time_cursor(v, "shared-with-me", &scope_text))
    {
        Some(Ok(v)) => Some(v),
        Some(Err(e)) => return crate::orgs::page_error(e, &headers),
        None => None,
    };
    let actor = match crate::orgs::load_actor(&services, session.user_id, Some(org_id)).await {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "actor lookup failed"),
    };
    let permit = match gate(
        can(
            &actor,
            Action::SharedWithMe,
            &Resource::Organization { org_id },
        ),
        &headers,
        session.user_id,
        "shared-with-me.list",
        Some(org_id),
        None,
    ) {
        Gate::Open(v) => v,
        Gate::Closed(r) => return r,
    };
    let Some(_scope) = craftcloud_db::OrgScope::from_permit(&permit) else {
        return crate::responses::internal(&headers);
    };
    let mut rows = match grants::shared_with_me_items(
        &services.pool,
        org_id,
        session.user_id,
        i64::from(limit) + 1,
        after,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "shared items lookup failed"),
    };
    let next = if rows.len() > limit as usize {
        rows.truncate(limit as usize);
        rows.last()
            .map(|x| encode_time_cursor("shared-with-me", &scope_text, x.created_at, x.id))
    } else {
        None
    };
    let items:Vec<_>=rows.iter().map(|x|json!({"target_kind":x.target_kind,"target_id":x.target_id.to_string(),"role":x.role,"workspace":{"id":x.workspace_id.to_string(),"name":x.workspace_name},"project":x.project_id.map(|id|json!({"id":id.to_string(),"name":x.project_name})),"file":x.file_id.map(|id|json!({"id":id.to_string(),"name":x.file_name}))})).collect();
    Json(json!({"items":items,"next_cursor":next})).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChangeRequest {
    role: String,
}

pub(crate) async fn change(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(raw): Path<String>,
    body: Result<Json<ChangeRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(v)) => v,
        Err(e) => return rejection_response(&e, &headers),
    };
    let Some(role) = GrantRole::parse(&request.role) else {
        return validation_failed(&["role"], &headers);
    };
    let expected = match projects::expected_version(&headers, "grant") {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let Some(id) = parse_id(&raw) else {
        return not_found(&headers);
    };
    let services = match state.services.clone() {
        Some(v) => v,
        None => return unavailable(&headers, "AUTH_UNAVAILABLE"),
    };
    let located = match grants::locate(&services.pool, id).await {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "grant lookup failed"),
    };
    let Some((target, _grantee, _, _)) = located else {
        return not_found(&headers);
    };
    let mut opened = match lock(
        &services.pool,
        session.user_id,
        target.org_id,
        target.workspace_id,
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
            target.workspace_id,
            target.id,
            Action::GrantChange,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "grant lookup failed"),
    };
    let permit = match gate(
        can(&opened.actor, Action::GrantChange, &resource),
        &headers,
        session.user_id,
        "grant.changed",
        Some(target.org_id),
        Some(id),
    ) {
        Gate::Open(v) => v,
        Gate::Closed(r) => return r,
    };
    let Some(scope) = craftcloud_db::OrgScope::from_permit(&permit) else {
        return crate::responses::internal(&headers);
    };
    match opened.tx.grant_change(&scope, id, role, expected).await {
        Ok(ChangeOutcome::Changed(grant)) => {
            if let Err(e) = commit_audited(
                opened.tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "grant.changed",
                    target.org_id,
                    Some(id),
                )
                .workspace(target.workspace_id)
                .detail("role", craftcloud_db::audit::Detail::Code(role.as_str())),
            )
            .await
            {
                return db_failure(&e, &headers, "grant change failed");
            }
            let mut r = (StatusCode::OK, Json(grant_json(&grant))).into_response();
            if let Ok(v) = header::HeaderValue::from_str(&format!("\"grant-v{}\"", grant.version)) {
                r.headers_mut().insert(header::ETAG, v);
            }
            r
        }
        Ok(ChangeOutcome::VersionMismatch(current)) => {
            projects::version_conflict("grant", "grant", current, &headers)
        }
        Ok(ChangeOutcome::Gone) => not_found(&headers),
        Err(e) => db_failure(&e, &headers, "grant change failed"),
    }
}

pub(crate) async fn revoke(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(raw): Path<String>,
) -> Response {
    let Some(id) = parse_id(&raw) else {
        return not_found(&headers);
    };
    let services = match state.services.clone() {
        Some(v) => v,
        None => return unavailable(&headers, "AUTH_UNAVAILABLE"),
    };
    let located = match grants::locate(&services.pool, id).await {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "grant lookup failed"),
    };
    let Some((target, _, _, _)) = located else {
        return not_found(&headers);
    };
    let mut opened = match lock(
        &services.pool,
        session.user_id,
        target.org_id,
        target.workspace_id,
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
            target.workspace_id,
            target.id,
            Action::GrantChange,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return db_failure(&e, &headers, "grant lookup failed"),
    };
    let permit = match gate(
        can(&opened.actor, Action::GrantChange, &resource),
        &headers,
        session.user_id,
        "grant.revoked",
        Some(target.org_id),
        Some(id),
    ) {
        Gate::Open(v) => v,
        Gate::Closed(r) => return r,
    };
    let Some(scope) = craftcloud_db::OrgScope::from_permit(&permit) else {
        return crate::responses::internal(&headers);
    };
    match opened
        .tx
        .grant_revoke(&scope, id, services.clock.now())
        .await
    {
        Ok(Some((_grant, true))) => {
            if let Err(e) = commit_audited(
                opened.tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "grant.revoked",
                    target.org_id,
                    Some(id),
                )
                .workspace(target.workspace_id),
            )
            .await
            {
                return db_failure(&e, &headers, "grant revoke failed");
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Some((_, false))) | Ok(None) => {
            if let Err(e) = opened.tx.commit().await {
                return db_failure(&e, &headers, "grant revoke failed");
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => db_failure(&e, &headers, "grant revoke failed"),
    }
}
