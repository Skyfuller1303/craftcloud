//! Organization overview for the admin console (AC-08b): aggregate counts
//! only, computed in SQL and scoped to one organization. No names, e-mail
//! addresses or identifiers of people or objects are returned.

use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use craftcloud_core::policy::{Action, Resource, can};
use craftcloud_db::{OrgScope, overview};
use serde_json::json;

use crate::ApiState;
use crate::orgs::{Gate, db_failure, gate, load_actor, not_found, parse_id};
use crate::responses::{internal, unavailable};
use crate::session::AuthenticatedSession;

// ---- GET /organizations/{org_id}/overview ----------------------------------

pub(crate) async fn get_overview(
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
    let decision = can(
        &actor,
        Action::OrganizationOverviewRead,
        &Resource::Organization { org_id },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "organization.overview",
        Some(org_id),
        None,
    ) {
        Gate::Open(permit) => permit,
        Gate::Closed(response) => return response,
    };
    let Some(scope) = OrgScope::from_permit(&permit) else {
        return internal(&headers);
    };
    let members = match overview::member_counts(&services.pool, &scope).await {
        Ok(value) => value,
        Err(error) => return db_failure(&error, &headers, "overview member counts failed"),
    };
    let totals = match overview::totals(&services.pool, &scope).await {
        Ok(Some(value)) => value,
        Ok(None) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "overview totals failed"),
    };
    let activity = match overview::activity_daily(&services.pool, &scope).await {
        Ok(value) => value,
        Err(error) => return db_failure(&error, &headers, "overview activity failed"),
    };
    let seats = match craftcloud_db::app_access::seat_usage(&services.pool, &scope).await {
        Ok(value) => value,
        Err(error) => return db_failure(&error, &headers, "overview seat usage failed"),
    };
    let total_members: i64 = members.by_status.values().sum();
    let days: Vec<_> = activity
        .iter()
        .map(|(date, count)| json!({ "date": date.to_string(), "count": count }))
        .collect();
    Json(json!({
        "members": {
            "total": total_members,
            "by_status": members.by_status,
            "by_role": members.by_role,
        },
        "two_factor": {
            "active_members": members.active_members,
            "enrolled": members.two_factor_enrolled,
        },
        "counts": {
            "workspaces": totals.workspaces,
            "projects": totals.projects,
            "files": totals.files,
            "active_locks": totals.active_locks,
            "active_grants": totals.active_grants,
            "pending_invitations": totals.pending_invitations,
        },
        "storage": {
            "used_bytes": totals.committed_bytes.saturating_add(totals.reserved_bytes),
            "committed_bytes": totals.committed_bytes,
            "reserved_bytes": totals.reserved_bytes,
            "quota_bytes": totals.quota_bytes,
        },
        "seats": {"used": seats.0, "limit": seats.1},
        "activity_daily": days,
    }))
    .into_response()
}
