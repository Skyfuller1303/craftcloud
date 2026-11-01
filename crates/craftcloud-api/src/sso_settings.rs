//! Owner-controlled provider configuration and operator emergency switch.
use crate::responses::{internal, rejection_response, unavailable, validation_failed};
use crate::session::AuthenticatedSession;
use crate::{ApiState, Services, error_response};
use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use craftcloud_core::policy::{Action, Actor, OrgMembership, OrgRole, Resource, can};
use craftcloud_db::{DbError, sso};
use serde::Deserialize;
use serde_json::json;
use sqlx::{Postgres, Transaction};
use std::sync::Arc;
use uuid::Uuid;
use zeroize::Zeroizing;

pub(crate) async fn owner_tx(
    services: &Services,
    session: &AuthenticatedSession,
    org: Uuid,
    action: Action,
    headers: &HeaderMap,
) -> Result<Transaction<'static, Postgres>, Box<Response>> {
    let mut tx = services
        .pool
        .begin()
        .await
        .map_err(|_| Box::new(internal(headers)))?;
    let found: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
            .bind(org)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| Box::new(internal(headers)))?;
    if found.is_none() {
        return Err(Box::new(crate::orgs::not_found(headers)));
    }
    let role:Option<String>=sqlx::query_scalar("SELECT m.role FROM organization_members m JOIN users u ON u.id=m.user_id WHERE m.org_id=$1 AND m.user_id=$2 AND m.status='active' AND u.disabled_at IS NULL FOR SHARE OF u")
        .bind(org).bind(session.user_id).fetch_optional(&mut *tx).await.map_err(|_|internal(headers))?;
    let actor = Actor {
        user_id: session.user_id,
        instance_operator: false,
        membership: role
            .as_deref()
            .and_then(OrgRole::parse)
            .map(|role| OrgMembership { org_id: org, role }),
        workspace: None,
    };
    match crate::orgs::gate(
        can(&actor, action, &Resource::Organization { org_id: org }),
        headers,
        session.user_id,
        "sso.manage",
        Some(org),
        None,
    ) {
        crate::orgs::Gate::Open(_) => Ok(tx),
        crate::orgs::Gate::Closed(response) => Err(Box::new(response)),
    }
}
fn dto(c: &sso::Configuration, claims: &[sso::DomainClaim]) -> serde_json::Value {
    let domain_claims: Vec<_> = claims
        .iter()
        .map(|k| json!({"domain":k.domain,"status":k.status}))
        .collect();
    json!({"configured":true,"discovery_url":c.discovery_url,"client_id":c.client_id,"allowed_domains":c.allowed_domains,"domain_claims":domain_claims,"enabled":c.enabled,"jit_enabled":c.jit_enabled,"sso_required":c.sso_required,"trust_mfa":c.trust_mfa,"version":c.version,"client_secret_configured":true})
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SaveRequest {
    discovery_url: String,
    client_id: String,
    client_secret: Option<String>,
    allowed_domains: Vec<String>,
    enabled: bool,
    jit_enabled: bool,
    sso_required: bool,
    trust_mfa: bool,
    version: Option<i64>,
}
fn valid_domain(domain: &str) -> bool {
    domain.len() <= 253
        && domain.contains('.')
        && domain.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}
pub(crate) async fn get(
    State(state): State<ApiState>,
    session: AuthenticatedSession,
    Path(org): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let Some(s) = state.services else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let mut tx = match owner_tx(&s, &session, org, Action::SsoManage, &headers).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    match sso::configuration_in(&mut tx, org).await {
        Ok(Some(c)) => match sso::claims_for_org(&mut tx, org).await {
            Ok(claims) => Json(dto(&c, &claims)).into_response(),
            Err(_) => internal(&headers),
        },
        Ok(None) => Json(json!({"configured":false})).into_response(),
        Err(_) => internal(&headers),
    }
}
pub(crate) async fn put(
    State(state): State<ApiState>,
    session: AuthenticatedSession,
    Path(org): Path<Uuid>,
    headers: HeaderMap,
    body: Result<Json<SaveRequest>, JsonRejection>,
) -> Response {
    let Some(s) = state.services else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let mut tx = match owner_tx(&s, &session, org, Action::SsoManage, &headers).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let mut input = match body {
        Ok(Json(v)) => v,
        Err(e) => return rejection_response(&e, &headers),
    };
    let secret = Zeroizing::new(input.client_secret.take().unwrap_or_default());
    if input.client_id.is_empty()
        || input.client_id.len() > 256
        || input.client_id.chars().any(char::is_control)
        || input.discovery_url.len() > 2048
        || input.allowed_domains.is_empty()
        || input.allowed_domains.len() > 100
        || input.allowed_domains.iter().any(|d| !valid_domain(d))
        || secret.len() > 4096
    {
        return validation_failed(&["provider"], &headers);
    }
    input.allowed_domains.sort();
    input.allowed_domains.dedup();
    let prior = match sso::configuration_in(&mut tx, org).await {
        Ok(c) => c,
        Err(_) => return internal(&headers),
    };
    if prior.as_ref().map(|c| c.version) != input.version {
        return error_response(
            StatusCode::CONFLICT,
            "SSO_CONFIGURATION_CHANGED",
            "Reload the provider settings before saving.",
            &headers,
        );
    }
    let ciphertext = if secret.is_empty() {
        let Some(prior) = prior
            .as_ref()
            .filter(|c| c.client_id == input.client_id && c.discovery_url == input.discovery_url)
        else {
            return validation_failed(&["client_secret"], &headers);
        };
        prior.client_secret_ciphertext.clone()
    } else {
        let Some(key) = s.two_factor_keys.current.as_ref() else {
            return unavailable(&headers, "SSO_UNAVAILABLE");
        };
        match craftcloud_auth::encrypt_secret_for(key, org, b"oidc-client", secret.as_bytes()) {
            Ok(v) => v,
            Err(_) => return internal(&headers),
        }
    };
    let c = sso::Configuration {
        org_id: org,
        discovery_url: input.discovery_url,
        client_id: input.client_id,
        client_secret_ciphertext: ciphertext,
        allowed_domains: input.allowed_domains,
        enabled: input.enabled,
        jit_enabled: input.jit_enabled,
        sso_required: input.sso_required,
        trust_mfa: input.trust_mfa,
        version: prior.as_ref().map_or(1, |c| c.version.saturating_add(1)),
    };
    // Validate the configured address even when the provider is disabled; credentials
    // are never reused for a changed destination without an explicit replacement.
    if !crate::oidc_provider::valid_discovery_url(&c.discovery_url) {
        return validation_failed(&["discovery_url"], &headers);
    }
    if sso::save_configuration(
        &mut tx,
        &c,
        &crate::audit::ctx(&headers, Some(session.user_id)),
    )
    .await
    .is_err()
    {
        return internal(&headers);
    }
    let Ok(claims) = sso::claims_for_org(&mut tx, org).await else {
        return internal(&headers);
    };
    if tx.commit().await.is_err() {
        return internal(&headers);
    }
    Json(dto(&c, &claims)).into_response()
}
pub(crate) async fn delete(
    State(state): State<ApiState>,
    session: AuthenticatedSession,
    Path(org): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let Some(s) = state.services else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let mut tx = match owner_tx(&s, &session, org, Action::SsoManage, &headers).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    if sso::delete_configuration(
        &mut tx,
        org,
        &crate::audit::ctx(&headers, Some(session.user_id)),
    )
    .await
    .is_err()
        || tx.commit().await.is_err()
    {
        return internal(&headers);
    }
    StatusCode::NO_CONTENT.into_response()
}
pub(crate) fn provider_settings(
    s: &Services,
    c: &sso::Configuration,
) -> Result<crate::oidc_provider::ProviderSettings, DbError> {
    let redirect_uri = format!(
        "{}/api/v1/auth/oidc/callback",
        s.settings
            .public_url
            .as_deref()
            .ok_or(DbError::InvalidUrl)?
            .trim_end_matches('/')
    );
    let decrypted = s
        .two_factor_keys
        .current
        .as_ref()
        .and_then(|k| {
            craftcloud_auth::decrypt_secret_for(
                k,
                &c.client_secret_ciphertext,
                c.org_id,
                b"oidc-client",
            )
            .ok()
        })
        .or_else(|| {
            s.two_factor_keys.old.as_ref().and_then(|k| {
                craftcloud_auth::decrypt_secret_for(
                    k,
                    &c.client_secret_ciphertext,
                    c.org_id,
                    b"oidc-client",
                )
                .ok()
            })
        })
        .ok_or(DbError::Internal)?;
    let decrypted = Zeroizing::new(decrypted);
    let secret = std::str::from_utf8(&decrypted)
        .map_err(|_| DbError::Internal)?
        .to_owned();
    Ok(crate::oidc_provider::ProviderSettings {
        discovery_url: c.discovery_url.clone(),
        client_id: c.client_id.clone(),
        client_secret: secret,
        redirect_uri,
    })
}
pub(crate) async fn test(
    State(state): State<ApiState>,
    session: AuthenticatedSession,
    Path(org): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let Some(s) = state.services else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let mut tx = match owner_tx(&s, &session, org, Action::SsoManage, &headers).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let c = match sso::configuration_in(&mut tx, org).await {
        Ok(Some(c)) => c,
        Ok(None) => return crate::orgs::not_found(&headers),
        Err(_) => return internal(&headers),
    };
    drop(tx);
    let settings = match provider_settings(&s, &c) {
        Ok(v) => v,
        Err(_) => return unavailable(&headers, "SSO_UNAVAILABLE"),
    };
    match crate::oidc_provider::authorize(&settings).await {
        Ok(v) => Json(json!({"success":true,"issuer":v.issuer})).into_response(),
        Err(_) => error_response(
            StatusCode::BAD_GATEWAY,
            "SSO_PROVIDER_UNAVAILABLE",
            "The provider could not be verified.",
            &headers,
        ),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstanceRequest {
    enabled: bool,
}
async fn operator(
    s: &Arc<Services>,
    session: &AuthenticatedSession,
    headers: &HeaderMap,
) -> Result<(), Box<Response>> {
    let actor = crate::orgs::load_actor(s, session.user_id, None)
        .await
        .map_err(|_| Box::new(internal(headers)))?;
    match crate::orgs::gate(
        can(&actor, Action::InstanceSsoManage, &Resource::Instance),
        headers,
        session.user_id,
        "sso.instance_manage",
        None,
        None,
    ) {
        crate::orgs::Gate::Open(_) => Ok(()),
        crate::orgs::Gate::Closed(r) => Err(Box::new(r)),
    }
}
pub(crate) async fn instance_get(
    State(state): State<ApiState>,
    session: AuthenticatedSession,
    headers: HeaderMap,
) -> Response {
    let Some(s) = state.services else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if let Err(r) = operator(&s, &session, &headers).await {
        return *r;
    }
    match sso::instance_enabled(&s.pool).await {
        Ok(enabled) => Json(json!({"enabled":enabled})).into_response(),
        Err(_) => internal(&headers),
    }
}
pub(crate) async fn instance_put(
    State(state): State<ApiState>,
    session: AuthenticatedSession,
    headers: HeaderMap,
    body: Result<Json<InstanceRequest>, JsonRejection>,
) -> Response {
    let Some(s) = state.services else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if let Err(r) = operator(&s, &session, &headers).await {
        return *r;
    }
    let input = match body {
        Ok(Json(v)) => v,
        Err(e) => return rejection_response(&e, &headers),
    };
    match sso::set_instance_enabled(
        &s.pool,
        input.enabled,
        &crate::audit::ctx(&headers, Some(session.user_id)),
    )
    .await
    {
        Ok(()) => Json(json!({"enabled":input.enabled})).into_response(),
        Err(_) => internal(&headers),
    }
}

fn claim_dto(k: &sso::DomainClaim) -> serde_json::Value {
    json!({"organization_id":k.org_id,"organization_name":k.org_name,"domain":k.domain,"status":k.status,"requested_at":crate::orgs::rfc3339(k.requested_at)})
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClaimsQuery {
    status: Option<String>,
}
/// Operator list of domain claims; pending ones by default.
pub(crate) async fn claims_list(
    State(state): State<ApiState>,
    session: AuthenticatedSession,
    headers: HeaderMap,
    q: Result<axum::extract::Query<ClaimsQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Some(s) = state.services else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if let Err(r) = operator(&s, &session, &headers).await {
        return *r;
    }
    let status = match q {
        Ok(axum::extract::Query(q)) => q.status.unwrap_or_else(|| "pending".to_owned()),
        Err(_) => return validation_failed(&["status"], &headers),
    };
    if !matches!(status.as_str(), "pending" | "approved" | "rejected") {
        return validation_failed(&["status"], &headers);
    }
    match sso::claims_by_status(&s.pool, &status, 200).await {
        Ok(items) => {
            Json(json!({"items":items.iter().map(claim_dto).collect::<Vec<_>>()})).into_response()
        }
        Err(_) => internal(&headers),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClaimDecisionRequest {
    decision: String,
}
/// Operator decision: `approved` makes the organization's domain effective for
/// sign-in, provisioning and discovery; `rejected` withdraws or refuses it.
pub(crate) async fn claim_decide(
    State(state): State<ApiState>,
    session: AuthenticatedSession,
    Path((org, domain)): Path<(Uuid, String)>,
    headers: HeaderMap,
    body: Result<Json<ClaimDecisionRequest>, JsonRejection>,
) -> Response {
    let Some(s) = state.services else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if let Err(r) = operator(&s, &session, &headers).await {
        return *r;
    }
    let input = match body {
        Ok(Json(v)) => v,
        Err(e) => return rejection_response(&e, &headers),
    };
    let approve = match input.decision.as_str() {
        "approved" => true,
        "rejected" => false,
        _ => return validation_failed(&["decision"], &headers),
    };
    if !valid_domain(&domain) {
        return validation_failed(&["domain"], &headers);
    }
    match sso::decide_claim(
        &s.pool,
        org,
        &domain,
        approve,
        &crate::audit::ctx(&headers, Some(session.user_id)),
    )
    .await
    {
        Ok(sso::ClaimDecision::Applied) => {
            Json(json!({"organization_id":org,"domain":domain,"status":input.decision}))
                .into_response()
        }
        Ok(sso::ClaimDecision::NotFound) => crate::orgs::not_found(&headers),
        Ok(sso::ClaimDecision::DomainTaken) => error_response(
            StatusCode::CONFLICT,
            "SSO_DOMAIN_ALREADY_APPROVED",
            "Another organization already holds an approved claim on this domain.",
            &headers,
        ),
        Err(_) => internal(&headers),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn domain_allowlist_is_exact_ascii_dns() {
        for d in ["example.org", "team.example.org", "xn--bcher-kva.example"] {
            assert!(valid_domain(d));
        }
        for d in [
            "*.example.org",
            "EXAMPLE.org",
            "example.org.",
            "localhost",
            "a..org",
            "-a.org",
            "a-.org",
            "a.org:443",
            "a.org/path",
        ] {
            assert!(!valid_domain(d));
        }
    }
}
