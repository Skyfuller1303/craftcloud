//! Organization seat and Craft app assignment routes.

use crate::ApiState;
use crate::orgs::{self, Gate};
use crate::responses::{rejection_response, unavailable, validation_failed};
use crate::session::AuthenticatedSession;
use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use craftcloud_core::policy::{Action, Actor, MemberTarget, OrgMembership, Resource, can};
use craftcloud_db::{OrgScope, app_access, members::OrgTx};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

fn id(value: &str) -> Option<Uuid> {
    Uuid::parse_str(value).ok()
}
fn valid_apps(apps: &[String], catalog: &[craftcloud_core::apps::AppDefinition]) -> bool {
    apps.len() <= 64
        && apps
            .iter()
            .all(|app| catalog.iter().any(|entry| entry.id == *app))
        && {
            let mut v = apps.to_vec();
            v.sort();
            v.dedup();
            v.len() == apps.len()
        }
}

async fn begin_org_tx(
    pool: &craftcloud_db::PgPool,
    org: Uuid,
    headers: &HeaderMap,
    context: &'static str,
) -> Result<OrgTx, Box<Response>> {
    match OrgTx::begin(pool, org).await {
        Ok(Some(tx)) => Ok(tx),
        Ok(None) => Err(Box::new(orgs::not_found(headers))),
        Err(error) => Err(Box::new(orgs::db_failure(&error, headers, context))),
    }
}

pub(crate) async fn get(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(raw): Path<String>,
) -> Response {
    let Some(s) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let Some(org) = id(&raw) else {
        return orgs::not_found(&headers);
    };
    let actor = match orgs::load_actor(&s, session.user_id, Some(org)).await {
        Ok(a) => a,
        Err(e) => return orgs::db_failure(&e, &headers, "app access actor lookup failed"),
    };
    let permit = match orgs::gate(
        can(
            &actor,
            Action::OrganizationAppsRead,
            &Resource::Organization { org_id: org },
        ),
        &headers,
        session.user_id,
        "organization.apps.read",
        Some(org),
        None,
    ) {
        Gate::Open(p) => p,
        Gate::Closed(r) => return r,
    };
    let Some(scope) = OrgScope::from_permit(&permit) else {
        return orgs::not_found(&headers);
    };
    let app_ids: Vec<String> = s
        .settings
        .app_catalog
        .iter()
        .map(|app| app.id.clone())
        .collect();
    match app_access::usage(&s.pool,&scope,&app_ids).await{Ok((mode,limit,used,usage))=>Json(json!({"mode":mode,"seat_usage":{"used":used,"limit":limit},"apps":usage.into_iter().map(|u|{let app=s.settings.app_catalog.iter().find(|app|app.id==u.app);json!({"id":u.app,"name":app.map(|v|v.name.as_str()).unwrap_or("Unknown"),"formats":app.map(|v|v.formats.as_slice()).unwrap_or(&[]),"assigned_members":u.assigned_members,"active_devices":u.active_devices,"storage_bytes":u.storage_bytes})}).collect::<Vec<_>>()})).into_response(),Err(e)=>orgs::db_failure(&e,&headers,"app usage query failed")}
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModeBody {
    mode: String,
}
pub(crate) async fn set_mode(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(raw): Path<String>,
    body: Result<Json<ModeBody>, JsonRejection>,
) -> Response {
    let Some(s) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let Some(org) = id(&raw) else {
        return orgs::not_found(&headers);
    };
    let Json(body) = match body {
        Ok(v) => v,
        Err(e) => return rejection_response(&e, &headers),
    };
    if !matches!(body.mode.as_str(), "all" | "assigned") {
        return validation_failed(&["mode"], &headers);
    }
    let mut org_tx = match begin_org_tx(&s.pool, org, &headers, "app mode transaction failed").await
    {
        Ok(tx) => tx,
        Err(response) => return *response,
    };
    let role = match org_tx.role_of(session.user_id).await {
        Ok(role) => role,
        Err(error) => return orgs::db_failure(&error, &headers, "app mode actor lookup failed"),
    };
    let actor = Actor {
        user_id: session.user_id,
        instance_operator: false,
        membership: role.map(|role| OrgMembership { org_id: org, role }),
        workspace: None,
    };
    let p = match orgs::gate(
        can(
            &actor,
            Action::OrganizationAppModeSet,
            &Resource::Organization { org_id: org },
        ),
        &headers,
        session.user_id,
        "organization.apps.mode",
        Some(org),
        None,
    ) {
        Gate::Open(p) => p,
        Gate::Closed(r) => return r,
    };
    let Some(scope) = OrgScope::from_permit(&p) else {
        return orgs::not_found(&headers);
    };
    match app_access::set_mode_tx(
        &mut org_tx,
        &scope,
        &body.mode,
        s.clock.now(),
        &crate::audit::ctx(&headers, Some(session.user_id)),
    )
    .await
    {
        Ok(()) => match org_tx.commit().await {
            Ok(()) => {
                crate::figcraft::process_app_access_revocations(&s).await;
                Json(json!({"mode":body.mode})).into_response()
            }
            Err(error) => orgs::db_failure(&error, &headers, "app mode commit failed"),
        },
        Err(e) => orgs::db_failure(&e, &headers, "app mode update failed"),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssignBody {
    apps: Vec<String>,
}
pub(crate) async fn assign(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path((org_raw, user_raw)): Path<(String, String)>,
    body: Result<Json<AssignBody>, JsonRejection>,
) -> Response {
    let Some(s) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let (Some(org), Some(user)) = (id(&org_raw), id(&user_raw)) else {
        return orgs::not_found(&headers);
    };
    let Json(body) = match body {
        Ok(v) => v,
        Err(e) => return rejection_response(&e, &headers),
    };
    if !valid_apps(&body.apps, &s.settings.app_catalog) {
        return validation_failed(&["apps"], &headers);
    }
    let mut org_tx =
        match begin_org_tx(&s.pool, org, &headers, "member app transaction failed").await {
            Ok(tx) => tx,
            Err(response) => return *response,
        };
    let actor_role = match org_tx.role_of(session.user_id).await {
        Ok(role) => role,
        Err(error) => return orgs::db_failure(&error, &headers, "member app actor lookup failed"),
    };
    let target_role = match org_tx.role_of(user).await {
        Ok(role) => role,
        Err(error) => return orgs::db_failure(&error, &headers, "member app lookup failed"),
    };
    let actor = Actor {
        user_id: session.user_id,
        instance_operator: false,
        membership: actor_role.map(|role| OrgMembership { org_id: org, role }),
        workspace: None,
    };
    let target = target_role.map(|role| MemberTarget {
        user_id: user,
        role,
    });
    let p = match orgs::gate(
        can(
            &actor,
            Action::MemberAppsSet,
            &Resource::Member {
                org_id: org,
                target,
                role: None,
            },
        ),
        &headers,
        session.user_id,
        "member.apps.set",
        Some(org),
        Some(user),
    ) {
        Gate::Open(p) => p,
        Gate::Closed(r) => return r,
    };
    let Some(scope) = OrgScope::from_permit(&p) else {
        return orgs::not_found(&headers);
    };
    match app_access::set_assignments_tx(
        &mut org_tx,
        &scope,
        user,
        &body.apps,
        s.clock.now(),
        &crate::audit::ctx(&headers, Some(session.user_id)),
    )
    .await
    {
        Ok(()) => match org_tx.commit().await {
            Ok(()) => {
                crate::figcraft::process_app_access_revocations(&s).await;
                (
                    StatusCode::OK,
                    Json(json!({"user_id":user.to_string(),"apps":body.apps})),
                )
                    .into_response()
            }
            Err(error) => orgs::db_failure(&error, &headers, "member app assignment commit failed"),
        },
        Err(e) => orgs::db_failure(&e, &headers, "member app assignment failed"),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SeatBody {
    seat_limit: Option<i32>,
}
pub(crate) async fn set_seats(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(raw): Path<String>,
    body: Result<Json<SeatBody>, JsonRejection>,
) -> Response {
    let Some(s) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let Some(org) = id(&raw) else {
        return orgs::not_found(&headers);
    };
    let Json(body) = match body {
        Ok(v) => v,
        Err(e) => return rejection_response(&e, &headers),
    };
    if body.seat_limit.is_some_and(|n| n < 0) {
        return validation_failed(&["seat_limit"], &headers);
    }
    if let Err(e) = orgs::operator_gate(
        &s,
        &session,
        Action::OrganizationSeatLimitSet,
        "organization.seats.set",
        Some(org),
        &headers,
    )
    .await
    {
        return *e;
    }
    match app_access::set_seat_limit(
        &s.pool,
        org,
        body.seat_limit,
        &crate::audit::ctx(&headers, Some(session.user_id)),
    )
    .await
    {
        Ok(true) => Json(json!({"seat_limit":body.seat_limit})).into_response(),
        Ok(false) => orgs::not_found(&headers),
        Err(e) => orgs::db_failure(&e, &headers, "seat limit update failed"),
    }
}
