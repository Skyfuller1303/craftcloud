//! Organization, membership and account administration routes (CC-F04,
//! CC-F05).
//!
//! Every handler follows the same order: authenticate (extractor, CSRF on
//! unsafe methods) -> load the actor fresh from the database -> ask the policy
//! engine -> only then touch data, through a scope that only an `Allow`
//! permit can produce. `Hidden` answers 404 (not a member, unknown id, the
//! operator without membership), `Forbidden` answers 403 and is only given to
//! actors who may see the resource. Decisions and outcomes of changes are
//! emitted as structured `audit` log events (actor, action, ids, result,
//! request id; never e-mail addresses, names or secrets) until the audit
//! store of CC-F11 exists.

use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use craftcloud_core::policy::{
    Action, Actor, Decision, MemberTarget, OrgMembership, OrgRole, Permit, Resource,
};
use craftcloud_core::{DisplayName, Slug};
use craftcloud_db::accounts::DisableOutcome;
use craftcloud_db::audit::Detail;
use craftcloud_db::members::{AddOutcome, Member, OrgTx, RemoveOutcome, RoleOutcome};
use craftcloud_db::organizations::{
    self, CreateOutcome, NewOrganization, Organization, UpdateOutcome,
};
use craftcloud_db::{DbError, OrgScope, accounts};
use serde::Deserialize;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::audit::{Change, Denied, commit_audited, ctx as audit_ctx};
use crate::pagination::{
    PageError, PageQuery, UserListQuery, decode_time_cursor, encode_time_cursor,
};
use crate::responses::{internal, rejection_response, unavailable, validation_failed};
use crate::services::Services;
use crate::session::AuthenticatedSession;
use crate::{ApiState, REQUEST_ID_HEADER, error_response};

// ---- shared helpers --------------------------------------------------------

pub(crate) fn rfc3339(time: OffsetDateTime) -> String {
    time.format(&Rfc3339).unwrap_or_default()
}

pub(crate) fn db_failure(error: &DbError, headers: &HeaderMap, what: &str) -> Response {
    tracing::error!(%error, "{what}");
    match error {
        DbError::Connection(_) => unavailable(headers, "AUTH_UNAVAILABLE"),
        _ => internal(headers),
    }
}

pub(crate) fn not_found(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
        "The requested resource does not exist.",
        headers,
    )
}

pub(crate) fn forbidden(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::FORBIDDEN,
        "FORBIDDEN",
        "You are not allowed to perform this action.",
        headers,
    )
}

pub(crate) fn conflict(code: &str, message: &str, headers: &HeaderMap) -> Response {
    error_response(StatusCode::CONFLICT, code, message, headers)
}

pub(crate) fn page_error(error: PageError, headers: &HeaderMap) -> Response {
    match error {
        PageError::Limit => validation_failed(&["limit"], headers),
        PageError::Cursor => error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_CURSOR",
            "The cursor is invalid for this listing.",
            headers,
        ),
    }
}

/// Structured audit event. Contains only identifiers and the outcome.
pub(crate) fn audit(
    headers: &HeaderMap,
    actor: Uuid,
    action: &str,
    org: Option<Uuid>,
    target: Option<Uuid>,
    result: &str,
) {
    let request_id = headers
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    tracing::info!(
        target: "craftcloud::audit",
        actor = %actor,
        action,
        org = %org.map(|o| o.to_string()).unwrap_or_default(),
        target_id = %target.map(|t| t.to_string()).unwrap_or_default(),
        result,
        request_id,
        "audit"
    );
}

/// Loads the policy actor from the database for this request. Nothing is
/// cached, so a changed role applies to the very next request.
pub(crate) async fn load_actor(
    services: &Services,
    user_id: Uuid,
    org_id: Option<Uuid>,
) -> Result<Actor, DbError> {
    let instance_operator = accounts::is_setup_operator(&services.pool, user_id).await?;
    let membership = match org_id {
        Some(org_id) => organizations::active_role(&services.pool, org_id, user_id)
            .await?
            .map(|role| OrgMembership { org_id, role }),
        None => None,
    };
    Ok(Actor {
        user_id,
        instance_operator,
        membership,
        workspace: None,
    })
}

/// Outcome of a policy check in a handler.
pub(crate) enum Gate {
    Open(Permit),
    Closed(Response),
}

pub(crate) fn gate(
    decision: Decision,
    headers: &HeaderMap,
    actor: Uuid,
    action_name: &str,
    org: Option<Uuid>,
    target: Option<Uuid>,
) -> Gate {
    match decision {
        Decision::Allow(permit) => {
            crate::observe::note_actor(actor);
            if let Some(org_id) = permit.org_id() {
                crate::observe::note_org(org_id);
            }
            Gate::Open(permit)
        }
        Decision::Forbidden => {
            audit(headers, actor, action_name, org, target, "forbidden");
            let mut response = forbidden(headers);
            // Stored as an aggregated audit row by `audit::record_denials`.
            response.extensions_mut().insert(Denied {
                actor,
                action: action_name.to_owned(),
                org,
                target,
            });
            Gate::Closed(response)
        }
        Decision::Hidden => {
            audit(headers, actor, action_name, org, target, "hidden");
            Gate::Closed(not_found(headers))
        }
    }
}

pub(crate) fn parse_id(raw: &str) -> Option<Uuid> {
    Uuid::parse_str(raw).ok()
}

fn etag(version: i64) -> Option<HeaderValue> {
    HeaderValue::from_str(&format!("\"org-v{version}\"")).ok()
}

fn parse_etag(raw: &str) -> Option<i64> {
    raw.trim()
        .strip_prefix("\"org-v")?
        .strip_suffix('"')?
        .parse::<i64>()
        .ok()
}

pub(crate) fn with_etag(mut response: Response, version: i64) -> Response {
    if let Some(value) = etag(version) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

pub(crate) fn org_json(org: &Organization, role: Option<OrgRole>) -> Value {
    json!({
        "id": org.id.to_string(),
        "slug": org.slug,
        "name": org.name,
        "role": role.map(OrgRole::as_str),
        "version": org.version,
        "workspace_admin_invites_enabled": org.workspace_admin_invites_enabled,
        "created_at": rfc3339(org.created_at),
    })
}

fn member_json(member: &Member) -> Value {
    json!({
        "user_id": member.user_id.to_string(),
        "display_name": member.display_name,
        "email": member.email,
        "role": member.role,
        "joined_at": rfc3339(member.joined_at),
    })
}

// ---- GET /organizations ----------------------------------------------------

pub(crate) async fn list_mine(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let Ok(Query(page)) = query else {
        return validation_failed(&["query"], &headers);
    };
    let limit = match page.limit() {
        Ok(limit) => limit,
        Err(error) => return page_error(error, &headers),
    };
    let scope = session.user_id.to_string();
    let after = match page.cursor().map(|c| decode_time_cursor(c, "orgs", &scope)) {
        Some(Ok(key)) => Some(key),
        Some(Err(error)) => return page_error(error, &headers),
        None => None,
    };
    // Listing one's own memberships needs no organization permit.
    let actor = match load_actor(&services, session.user_id, None).await {
        Ok(actor) => actor,
        Err(error) => return db_failure(&error, &headers, "actor lookup failed"),
    };
    if !craftcloud_core::policy::can(&actor, Action::OrganizationListOwn, &Resource::Instance)
        .is_allow()
    {
        return forbidden(&headers);
    }
    let mut rows =
        match organizations::list_for_user(&services.pool, session.user_id, limit + 1, after).await
        {
            Ok(rows) => rows,
            Err(error) => return db_failure(&error, &headers, "organization list failed"),
        };
    let next = if rows.len() > limit as usize {
        rows.truncate(limit as usize);
        rows.last()
            .map(|last| encode_time_cursor("orgs", &scope, last.created_at, last.id))
    } else {
        None
    };
    let items: Vec<_> = rows
        .iter()
        .map(|o| {
            json!({
                "id": o.id.to_string(), "slug": o.slug, "name": o.name, "role": o.role,
                "version": o.version, "created_at": rfc3339(o.created_at),
            })
        })
        .collect();
    Json(json!({ "items": items, "next_cursor": next })).into_response()
}

// ---- GET /organizations/{org_id} -------------------------------------------

pub(crate) async fn get_org(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(org_raw): Path<String>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let Some(org_id) = parse_id(&org_raw) else {
        return not_found(&headers);
    };
    let actor = match load_actor(&services, session.user_id, Some(org_id)).await {
        Ok(actor) => actor,
        Err(error) => return db_failure(&error, &headers, "actor lookup failed"),
    };
    let decision = craftcloud_core::policy::can(
        &actor,
        Action::OrganizationRead,
        &Resource::Organization { org_id },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "organization.read",
        Some(org_id),
        None,
    ) {
        Gate::Open(permit) => permit,
        Gate::Closed(response) => return response,
    };
    let Some(scope) = OrgScope::from_permit(&permit) else {
        return internal(&headers);
    };
    match organizations::get(&services.pool, &scope).await {
        Ok(Some(org)) => {
            let role = actor.membership.map(|m| m.role);
            let version = org.version;
            with_etag(Json(org_json(&org, role)).into_response(), version)
        }
        Ok(None) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "organization read failed"),
    }
}

// ---- PATCH /organizations/{org_id} -----------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateOrgRequest {
    name: Option<String>,
    workspace_admin_invites_enabled: Option<bool>,
}

pub(crate) async fn update_org(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(org_raw): Path<String>,
    body: Result<Json<UpdateOrgRequest>, JsonRejection>,
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
    // Rename, role check and policy decision happen under the organization
    // lock, with a freshly read role, like all membership changes.
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
    let decision = craftcloud_core::policy::can(
        &actor,
        Action::OrganizationUpdate,
        &Resource::Organization { org_id },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "organization.update",
        Some(org_id),
        None,
    ) {
        Gate::Open(permit) => permit,
        Gate::Closed(response) => return response,
    };
    let Some(scope) = OrgScope::from_permit(&permit) else {
        return internal(&headers);
    };
    let name = match request.name.as_deref().map(DisplayName::parse) {
        Some(Ok(name)) => Some(name),
        Some(Err(_)) => return validation_failed(&["name"], &headers),
        None => None,
    };
    if name.is_none() && request.workspace_admin_invites_enabled.is_none() {
        return validation_failed(&["name", "workspace_admin_invites_enabled"], &headers);
    }
    // Optimistic concurrency (ADR-0009): the version the client last saw.
    let Some(if_match) = headers.get(header::IF_MATCH) else {
        return error_response(
            StatusCode::PRECONDITION_REQUIRED,
            "PRECONDITION_REQUIRED",
            "An If-Match header with the ETag from the last read is required.",
            &headers,
        );
    };
    // An unparsable value can never match the current version.
    let expected = if_match.to_str().ok().and_then(parse_etag).unwrap_or(-1);
    // Read under the lock, so the audit event states what really changed.
    let name_before = match tx.current_name().await {
        Ok(value) => value,
        Err(error) => return db_failure(&error, &headers, "organization update failed"),
    };
    let invites_before = match tx.workspace_admin_invites_enabled().await {
        Ok(value) => value,
        Err(error) => return db_failure(&error, &headers, "organization update failed"),
    };
    match tx
        .update_settings(
            &scope,
            name.as_ref().map(DisplayName::as_str),
            request.workspace_admin_invites_enabled,
            expected,
        )
        .await
    {
        Ok(UpdateOutcome::Updated(org)) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "organization.update",
                    org_id,
                    Some(org_id),
                )
                .detail(
                    "name_changed",
                    Detail::Flag(name.as_ref().is_some_and(|n| n.as_str() != name_before)),
                )
                .detail(
                    "invites_setting_changed",
                    Detail::Flag(
                        request
                            .workspace_admin_invites_enabled
                            .is_some_and(|value| value != invites_before),
                    ),
                ),
            )
            .await
            {
                return db_failure(&error, &headers, "organization update failed");
            }
            let version = org.version;
            with_etag(Json(org_json(&org, role)).into_response(), version)
        }
        Ok(UpdateOutcome::VersionMismatch { current_version }) => {
            audit(
                &headers,
                session.user_id,
                "organization.update",
                Some(org_id),
                None,
                "version_conflict",
            );
            with_etag(
                error_response(
                    StatusCode::PRECONDITION_FAILED,
                    "VERSION_CONFLICT",
                    "The organization changed since it was read.",
                    &headers,
                ),
                current_version,
            )
        }
        Ok(UpdateOutcome::Gone) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "organization update failed"),
    }
}

// ---- GET /organizations/{org_id}/members -----------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemberPageQuery {
    limit: Option<String>,
    cursor: Option<String>,
    query: Option<String>,
}

pub(crate) async fn list_members(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(org_raw): Path<String>,
    query: Result<Query<MemberPageQuery>, QueryRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let Some(org_id) = parse_id(&org_raw) else {
        return not_found(&headers);
    };
    let actor = match load_actor(&services, session.user_id, Some(org_id)).await {
        Ok(actor) => actor,
        Err(error) => return db_failure(&error, &headers, "actor lookup failed"),
    };
    let decision = craftcloud_core::policy::can(
        &actor,
        Action::MemberList,
        &Resource::Member {
            org_id,
            target: None,
            role: None,
        },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "member.list",
        Some(org_id),
        None,
    ) {
        Gate::Open(permit) => permit,
        Gate::Closed(response) => return response,
    };
    let Some(scope) = OrgScope::from_permit(&permit) else {
        return internal(&headers);
    };
    let Ok(Query(member_page)) = query else {
        return validation_failed(&["query"], &headers);
    };
    let search = member_page
        .query
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_lowercase);
    let page = PageQuery {
        limit: member_page.limit,
        cursor: member_page.cursor,
    };
    let limit = match page.limit() {
        Ok(limit) => limit,
        Err(error) => return page_error(error, &headers),
    };
    let scope_text = search
        .as_ref()
        .map_or_else(|| org_id.to_string(), |term| format!("{org_id}:{term}"));
    let cursor_kind = if search.is_some() {
        "member-search"
    } else {
        "members"
    };
    let after = match page
        .cursor()
        .map(|c| decode_time_cursor(c, cursor_kind, &scope_text))
    {
        Some(Ok(key)) => Some(key),
        Some(Err(error)) => return page_error(error, &headers),
        None => None,
    };
    if let Some(term) = &search {
        if term.chars().count() < 2 || term.chars().count() > 100 {
            return validation_failed(&["query"], &headers);
        }
        let grant_manager: bool = match craftcloud_db::members::can_manage_grants(
            &services.pool,
            org_id,
            session.user_id,
        )
        .await
        {
            Ok(v) => v,
            Err(error) => {
                return db_failure(&error, &headers, "member search authorization failed");
            }
        };
        let decision = craftcloud_core::policy::can(
            &actor,
            Action::MemberSearch,
            &Resource::MemberSearch {
                org_id,
                grant_manager,
            },
        );
        match gate(
            decision,
            &headers,
            session.user_id,
            "member.search",
            Some(org_id),
            None,
        ) {
            Gate::Open(_) => {}
            Gate::Closed(response) => return response,
        }
    }
    let mut rows = match if let Some(term) = &search {
        craftcloud_db::members::search(&services.pool, &scope, term, limit + 1, after).await
    } else {
        craftcloud_db::members::list(&services.pool, &scope, limit + 1, after).await
    } {
        Ok(rows) => rows,
        Err(error) => return db_failure(&error, &headers, "member list failed"),
    };
    let next = if rows.len() > limit as usize {
        rows.truncate(limit as usize);
        rows.last()
            .map(|last| encode_time_cursor(cursor_kind, &scope_text, last.joined_at, last.user_id))
    } else {
        None
    };
    let items: Vec<_> = rows.iter().map(member_json).collect();
    Json(json!({ "items": items, "next_cursor": next })).into_response()
}

// ---- POST /organizations/{org_id}/members ----------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AddMemberRequest {
    user_id: String,
    role: String,
}

pub(crate) async fn add_member(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(org_raw): Path<String>,
    body: Result<Json<AddMemberRequest>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    let (user_id, role) = (parse_id(&request.user_id), OrgRole::parse(&request.role));
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
    let Some(org_id) = parse_id(&org_raw) else {
        return not_found(&headers);
    };
    let tx = match OrgTx::begin(&services.pool, org_id).await {
        Ok(Some(tx)) => tx,
        Ok(None) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "membership transaction failed"),
    };
    let mut tx = tx;
    let membership = match tx.role_of(session.user_id).await {
        Ok(role) => role.map(|role| OrgMembership { org_id, role }),
        Err(error) => return db_failure(&error, &headers, "actor lookup failed"),
    };
    let actor = Actor {
        user_id: session.user_id,
        instance_operator: false,
        membership,
        workspace: None,
    };
    let decision = craftcloud_core::policy::can(
        &actor,
        Action::MemberAdd,
        &Resource::Member {
            org_id,
            target: None,
            role: Some(role),
        },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "member.add",
        Some(org_id),
        Some(user_id),
    ) {
        Gate::Open(permit) => permit,
        Gate::Closed(response) => return response,
    };
    let Some(scope) = OrgScope::from_permit(&permit) else {
        return internal(&headers);
    };
    // Existence of the user is only revealed after the permission check.
    let outcome = match tx.add(&scope, user_id, role).await {
        Ok(outcome) => outcome,
        Err(error) => return db_failure(&error, &headers, "member add failed"),
    };
    match outcome {
        AddOutcome::Added(member) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "member.add",
                    org_id,
                    Some(user_id),
                )
                .detail("role", Detail::Code(role.as_str())),
            )
            .await
            {
                return db_failure(&error, &headers, "member add failed");
            }
            (StatusCode::CREATED, Json(member_json(&member))).into_response()
        }
        AddOutcome::AlreadyMember => {
            conflict("ALREADY_MEMBER", "The user is already a member.", &headers)
        }
        AddOutcome::UserNotFound => not_found(&headers),
        AddOutcome::UserDisabled => {
            conflict("ACCOUNT_DISABLED", "The account is disabled.", &headers)
        }
    }
}

// ---- PATCH / DELETE /organizations/{org_id}/members/{user_id} --------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChangeRoleRequest {
    role: String,
}

pub(crate) async fn change_role(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path((org_raw, user_raw)): Path<(String, String)>,
    body: Result<Json<ChangeRoleRequest>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    let Some(new_role) = OrgRole::parse(&request.role) else {
        return validation_failed(&["role"], &headers);
    };
    let (Some(org_id), Some(target_id)) = (parse_id(&org_raw), parse_id(&user_raw)) else {
        return not_found(&headers);
    };
    let mut tx = match OrgTx::begin(&services.pool, org_id).await {
        Ok(Some(tx)) => tx,
        Ok(None) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "membership transaction failed"),
    };
    let (actor_role, target_role) = match (
        tx.role_of(session.user_id).await,
        tx.role_of(target_id).await,
    ) {
        (Ok(a), Ok(t)) => (a, t),
        (Err(error), _) | (_, Err(error)) => {
            return db_failure(&error, &headers, "role lookup failed");
        }
    };
    let actor = Actor {
        user_id: session.user_id,
        instance_operator: false,
        membership: actor_role.map(|role| OrgMembership { org_id, role }),
        workspace: None,
    };
    let target = target_role.map(|role| MemberTarget {
        user_id: target_id,
        role,
    });
    let decision = craftcloud_core::policy::can(
        &actor,
        Action::MemberChangeRole,
        &Resource::Member {
            org_id,
            target,
            role: Some(new_role),
        },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "member.change_role",
        Some(org_id),
        Some(target_id),
    ) {
        Gate::Open(permit) => permit,
        Gate::Closed(response) => return response,
    };
    let Some(scope) = OrgScope::from_permit(&permit) else {
        return internal(&headers);
    };
    match tx.set_role(&scope, target_id, new_role).await {
        Ok(RoleOutcome::Changed(member)) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "member.change_role",
                    org_id,
                    Some(target_id),
                )
                .detail("role", Detail::Code(new_role.as_str()))
                .detail(
                    "previous_role",
                    Detail::Code(target_role.map_or("none", OrgRole::as_str)),
                ),
            )
            .await
            {
                return db_failure(&error, &headers, "role change failed");
            }
            Json(member_json(&member)).into_response()
        }
        Ok(RoleOutcome::Unchanged(member)) => Json(member_json(&member)).into_response(),
        Ok(RoleOutcome::NotMember) => not_found(&headers),
        Ok(RoleOutcome::LastOwner) => {
            audit(
                &headers,
                session.user_id,
                "member.change_role",
                Some(org_id),
                Some(target_id),
                "last_owner",
            );
            conflict(
                "LAST_OWNER",
                "An organization needs at least one owner.",
                &headers,
            )
        }
        Err(error) => db_failure(&error, &headers, "role change failed"),
    }
}

pub(crate) async fn remove_member(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path((org_raw, user_raw)): Path<(String, String)>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let (Some(org_id), Some(target_id)) = (parse_id(&org_raw), parse_id(&user_raw)) else {
        return not_found(&headers);
    };
    let mut tx = match OrgTx::begin(&services.pool, org_id).await {
        Ok(Some(tx)) => tx,
        Ok(None) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "membership transaction failed"),
    };
    let (actor_role, target_role) = match (
        tx.role_of(session.user_id).await,
        tx.stored_role_of(target_id).await,
    ) {
        (Ok(a), Ok(t)) => (a, t),
        (Err(error), _) | (_, Err(error)) => {
            return db_failure(&error, &headers, "role lookup failed");
        }
    };
    let actor = Actor {
        user_id: session.user_id,
        instance_operator: false,
        membership: actor_role.map(|role| OrgMembership { org_id, role }),
        workspace: None,
    };
    let target = target_role.map(|role| MemberTarget {
        user_id: target_id,
        role,
    });
    let decision = craftcloud_core::policy::can(
        &actor,
        Action::MemberRemove,
        &Resource::Member {
            org_id,
            target,
            role: None,
        },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "member.remove",
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
        .remove_audited(
            &scope,
            target_id,
            &audit_ctx(&headers, Some(session.user_id)),
        )
        .await
    {
        Ok(RemoveOutcome::Removed) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "member.remove",
                    org_id,
                    Some(target_id),
                )
                .detail(
                    "role",
                    Detail::Code(target_role.map_or("none", OrgRole::as_str)),
                )
                .detail("self_removal", Detail::Flag(target_id == session.user_id)),
            )
            .await
            {
                return db_failure(&error, &headers, "member removal failed");
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(RemoveOutcome::NotMember) => not_found(&headers),
        Ok(RemoveOutcome::LastOwner) => {
            audit(
                &headers,
                session.user_id,
                "member.remove",
                Some(org_id),
                Some(target_id),
                "last_owner",
            );
            conflict(
                "LAST_OWNER",
                "An organization needs at least one owner.",
                &headers,
            )
        }
        Err(error) => db_failure(&error, &headers, "member removal failed"),
    }
}

// ---- operator: POST /organizations, GET /admin/organizations ---------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateOrgRequest {
    name: String,
    slug: String,
    owner_user_id: String,
}

/// Policy check for instance-level (operator) actions.
pub(crate) async fn operator_gate(
    services: &Services,
    session: &AuthenticatedSession,
    action: Action,
    action_name: &str,
    target: Option<Uuid>,
    headers: &HeaderMap,
) -> Result<(), Box<Response>> {
    let actor = load_actor(services, session.user_id, None)
        .await
        .map_err(|error| Box::new(db_failure(&error, headers, "actor lookup failed")))?;
    match gate(
        craftcloud_core::policy::can(&actor, action, &Resource::Instance),
        headers,
        session.user_id,
        action_name,
        None,
        target,
    ) {
        Gate::Open(_) => Ok(()),
        Gate::Closed(response) => Err(Box::new(response)),
    }
}

pub(crate) async fn create_org(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    body: Result<Json<CreateOrgRequest>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    if let Err(response) = operator_gate(
        &services,
        &session,
        Action::OrganizationCreate,
        "organization.create",
        None,
        &headers,
    )
    .await
    {
        return *response;
    }
    let name = DisplayName::parse(&request.name);
    let slug = Slug::parse(&request.slug);
    let owner = parse_id(&request.owner_user_id);
    let mut invalid = Vec::new();
    if name.is_err() {
        invalid.push("name");
    }
    if slug.is_err() {
        invalid.push("slug");
    }
    if owner.is_none() {
        invalid.push("owner_user_id");
    }
    let (Ok(name), Ok(slug), Some(owner_user_id)) = (name, slug, owner) else {
        return validation_failed(&invalid, &headers);
    };
    let created = organizations::create_with_owner_audited(
        &services.pool,
        &NewOrganization {
            slug: slug.as_str().to_owned(),
            name: name.as_str().to_owned(),
            owner_user_id,
        },
        &crate::audit::ctx(&headers, Some(session.user_id)),
    )
    .await;
    match created {
        Ok(CreateOutcome::Created(org)) => {
            audit(
                &headers,
                session.user_id,
                "organization.create",
                Some(org.id),
                Some(owner_user_id),
                "success",
            );
            let version = org.version;
            let mut value = org_json(&org, None);
            if let Some(object) = value.as_object_mut() {
                object.remove("role");
                object.insert("owner_user_id".to_owned(), json!(owner_user_id.to_string()));
            }
            with_etag((StatusCode::CREATED, Json(value)).into_response(), version)
        }
        Ok(CreateOutcome::OwnerNotFound | CreateOutcome::OwnerDisabled) => {
            // Operator-only route: naming the field is fine.
            validation_failed(&["owner_user_id"], &headers)
        }
        Err(error) if error.constraint() == Some("organizations_slug_key") => {
            validation_failed(&["slug"], &headers)
        }
        Err(error) => db_failure(&error, &headers, "organization creation failed"),
    }
}

pub(crate) async fn list_all_orgs(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if let Err(response) = operator_gate(
        &services,
        &session,
        Action::OrganizationListAll,
        "organization.list_all",
        None,
        &headers,
    )
    .await
    {
        return *response;
    }
    let Ok(Query(page)) = query else {
        return validation_failed(&["query"], &headers);
    };
    let limit = match page.limit() {
        Ok(limit) => limit,
        Err(error) => return page_error(error, &headers),
    };
    let after = match page
        .cursor()
        .map(|c| decode_time_cursor(c, "admin-orgs", "instance"))
    {
        Some(Ok(key)) => Some(key),
        Some(Err(error)) => return page_error(error, &headers),
        None => None,
    };
    let mut rows = match organizations::list_all(&services.pool, limit + 1, after).await {
        Ok(rows) => rows,
        Err(error) => return db_failure(&error, &headers, "organization list failed"),
    };
    let next = if rows.len() > limit as usize {
        rows.truncate(limit as usize);
        rows.last()
            .map(|last| encode_time_cursor("admin-orgs", "instance", last.created_at, last.id))
    } else {
        None
    };
    // Metadata only: no member lists, no user data, no files.
    let items: Vec<_> = rows
        .iter()
        .map(|o| {
            json!({
                "id": o.id.to_string(), "slug": o.slug, "name": o.name,
                "member_count": o.member_count, "created_at": rfc3339(o.created_at),
            })
        })
        .collect();
    Json(json!({ "items": items, "next_cursor": next })).into_response()
}

/// Instance-level account metadata for the operator, including the provenance
/// of invitation-created accounts (squatting remedy: disable the account).
pub(crate) async fn list_all_users(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    query: Result<Query<UserListQuery>, QueryRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if let Err(response) = operator_gate(
        &services,
        &session,
        Action::UserListAll,
        "user.list_all",
        None,
        &headers,
    )
    .await
    {
        return *response;
    }
    let Ok(Query(query)) = query else {
        return validation_failed(&["query"], &headers);
    };
    let (page, unverified) = match query.split() {
        Ok(parts) => parts,
        Err(_) => return validation_failed(&["unverified"], &headers),
    };
    let limit = match page.limit() {
        Ok(limit) => limit,
        Err(error) => return page_error(error, &headers),
    };
    // The cursor is bound to the filter, so it cannot be replayed for another one.
    let scope = if unverified { "unverified" } else { "all" };
    let after = match page
        .cursor()
        .map(|c| decode_time_cursor(c, "admin-users", scope))
    {
        Some(Ok(key)) => Some(key),
        Some(Err(error)) => return page_error(error, &headers),
        None => None,
    };
    let mut rows = match accounts::list_all(&services.pool, limit + 1, after, unverified).await {
        Ok(rows) => rows,
        Err(error) => return db_failure(&error, &headers, "account list failed"),
    };
    let next = if rows.len() > limit as usize {
        rows.truncate(limit as usize);
        rows.last()
            .map(|last| encode_time_cursor("admin-users", scope, last.created_at, last.id))
    } else {
        None
    };
    let items: Vec<_> = rows
        .iter()
        .map(|u| {
            let origin = u.invitation_id.map(|invitation| {
                json!({
                    "organization_id": u.origin_organization_id.map(|id| id.to_string()),
                    "invited_by_user_id": u.invited_by_user_id.map(|id| id.to_string()),
                    "invitation_id": invitation.to_string(),
                })
            });
            json!({
                "id": u.id.to_string(),
                "email": u.normalized_email,
                "display_name": u.display_name,
                "created_at": rfc3339(u.created_at),
                "disabled": u.disabled_at.is_some(),
                "email_verified": u.email_verified_at.is_some(),
                "origin": origin,
            })
        })
        .collect();
    Json(json!({ "items": items, "next_cursor": next })).into_response()
}

// ---- operator: POST /admin/users/{user_id}/disable | enable -----------------

/// Disabling answers 200 with the organizations left without an enabled owner
/// (empty when none); enabling answers 204.
fn disable_response(
    disable: bool,
    orphaned: &[Uuid],
    headers: &HeaderMap,
    actor: Uuid,
) -> Response {
    if !disable {
        return StatusCode::NO_CONTENT.into_response();
    }
    if !orphaned.is_empty() {
        tracing::warn!(
            orphaned_organizations = orphaned.len(),
            "account disabled; organizations without an enabled owner remain \
             (recover with `craftcloud-server admin set-org-owner`)"
        );
        for org in orphaned {
            audit(
                headers,
                actor,
                "organization.orphaned",
                Some(*org),
                None,
                "no_enabled_owner",
            );
        }
    }
    let ids: Vec<String> = orphaned.iter().map(Uuid::to_string).collect();
    Json(json!({ "orphaned_organization_ids": ids })).into_response()
}

async fn set_user_disabled(
    state: ApiState,
    headers: HeaderMap,
    session: AuthenticatedSession,
    user_raw: String,
    disable: bool,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let (action, name) = if disable {
        (Action::UserDisable, "user.disable")
    } else {
        (Action::UserEnable, "user.enable")
    };
    if let Err(response) = operator_gate(
        &services,
        &session,
        action,
        name,
        parse_id(&user_raw),
        &headers,
    )
    .await
    {
        return *response;
    }
    let Some(user_id) = parse_id(&user_raw) else {
        return not_found(&headers);
    };
    match accounts::set_disabled_audited(
        &services.pool,
        user_id,
        disable,
        services.clock.now(),
        &crate::audit::ctx(&headers, Some(session.user_id)),
    )
    .await
    {
        Ok(DisableOutcome::Changed {
            orphaned_organizations,
        }) => {
            audit(
                &headers,
                session.user_id,
                name,
                None,
                Some(user_id),
                "success",
            );
            disable_response(disable, &orphaned_organizations, &headers, session.user_id)
        }
        Ok(DisableOutcome::Unchanged {
            orphaned_organizations,
        }) => disable_response(disable, &orphaned_organizations, &headers, session.user_id),
        Ok(DisableOutcome::NoSuchUser) => not_found(&headers),
        Ok(DisableOutcome::IsOperator) => conflict(
            "CANNOT_DISABLE_OPERATOR",
            "The instance operator cannot be disabled.",
            &headers,
        ),
        Err(error) => db_failure(&error, &headers, "account state change failed"),
    }
}

pub(crate) async fn disable_user(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(user_raw): Path<String>,
) -> Response {
    set_user_disabled(state, headers, session, user_raw, true).await
}

pub(crate) async fn enable_user(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(user_raw): Path<String>,
) -> Response {
    set_user_disabled(state, headers, session, user_raw, false).await
}

/// Policy gate shared with the password reset routes of `auth`.
pub(crate) async fn operator_only(
    services: &Services,
    session: &AuthenticatedSession,
    action: Action,
    action_name: &str,
    target: Option<Uuid>,
    headers: &HeaderMap,
) -> Result<(), Box<Response>> {
    operator_gate(services, session, action, action_name, target, headers).await
}
