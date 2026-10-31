//! Invitation routes (CC-F03, ADR-0013 items 5, 7 and 9).
//!
//! Without SMTP, possession of the invitation link is the proof of control of
//! the invited address: the link is shown once to the inviter (token in the URL
//! fragment, 256 bits, only a SHA-256 hash stored), valid for 72 hours, single
//! use and revocable. Acceptance is bound to the invited address and re-checks
//! the inviter's *current* rights, so stale invitations cannot escalate.
//!
//! Tokens and e-mail addresses are never logged; audit events contain ids only.

use axum::{
    Extension, Json,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use craftcloud_auth::{OpaqueToken, hash_password, validate_password};
use craftcloud_core::policy::{
    Action, Actor, InvitationSpec, OrgMembership, OrgRole, Resource, WorkspaceMembership,
    WorkspaceRole, can,
};
use craftcloud_core::{DisplayName, NormalizedEmail};
use craftcloud_db::audit::Detail;
use craftcloud_db::invitations::{
    self, AcceptBy, AcceptOutcome, CreateInvitationOutcome, Invitation, NewInvitation,
};
use craftcloud_db::members::OrgTx;
use craftcloud_db::{DbError, OrgScope, accounts, sessions};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;
use zeroize::Zeroize;

use crate::audit::{Change, commit_audited};
use crate::orgs::{
    Gate, audit, conflict, db_failure, gate, not_found, page_error, parse_id, rfc3339,
};
use crate::pagination::{PageQuery, decode_time_cursor, encode_time_cursor};
use crate::responses::{internal, rejection_response, unavailable, validation_failed};
use crate::services::IpClass;
use crate::session::{
    AuthRejection, AuthenticatedSession, CredentialKind, origin_ok, session_cookie,
};
use crate::{ApiState, ClientIp, error_response};

/// Lifetime of an invitation link (ADR-0013 item 9).
pub(crate) const INVITATION_TTL: Duration = Duration::from_secs(72 * 3600);

fn invitation_json(invitation: &Invitation, now: OffsetDateTime) -> Value {
    json!({
        "id": invitation.id.to_string(),
        "email": invitation.email,
        "org_role": invitation.org_role,
        "workspace_id": invitation.workspace_id.map(|id| id.to_string()),
        "workspace_role": invitation.workspace_role,
        "status": invitation.status(now),
        "created_by": invitation.created_by.to_string(),
        "created_at": rfc3339(invitation.created_at),
        "expires_at": rfc3339(invitation.expires_at),
    })
}

fn invalid_invitation(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "INVALID_INVITATION",
        "The invitation is invalid, expired, revoked or already used.",
        headers,
    )
}

// ---- POST /organizations/{org_id}/invitations ----------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateInvitationRequest {
    email: String,
    #[serde(default)]
    language: Option<String>,
    org_role: String,
    workspace_id: Option<String>,
    workspace_role: Option<String>,
}

pub(crate) async fn create_invitation(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(org_raw): Path<String>,
    body: Result<Json<CreateInvitationRequest>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    if let Err(response) = services.rate_limit_invite(session.user_id, &headers) {
        return *response;
    }
    if !services.email_delivery_enabled() && services.settings.public_url.is_none() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "AUTH_UNAVAILABLE",
            "CRAFTCLOUD_PUBLIC_URL must be configured to build invitation links.",
            &headers,
        );
    }

    // Structural validation (no state involved).
    let email = NormalizedEmail::parse(&request.email);
    let org_role = OrgRole::parse(&request.org_role);
    let (workspace, guest_workspace) = if org_role == Some(OrgRole::Guest) {
        let guest_workspace = match (&request.workspace_id, &request.workspace_role) {
            (None, None) => Ok(None),
            (Some(id), None) => parse_id(id).map(Some).ok_or(()),
            _ => Err(()),
        };
        (Ok(None), guest_workspace)
    } else {
        let workspace = match (&request.workspace_id, &request.workspace_role) {
            (None, None) => Ok(None),
            (Some(id), Some(role)) => match (parse_id(id), WorkspaceRole::parse(role)) {
                (Some(id), Some(role)) => Ok(Some((id, role))),
                _ => Err(()),
            },
            _ => Err(()),
        };
        (workspace, Ok(None))
    };
    let mut invalid = Vec::new();
    if email.is_err() {
        invalid.push("email");
    }
    let language = request.language.as_deref().unwrap_or("en");
    if !matches!(language, "en" | "de") {
        invalid.push("language");
    }
    if org_role.is_none() {
        invalid.push("org_role");
    }
    if workspace.is_err() || guest_workspace.is_err() {
        invalid.push("workspace_id");
        invalid.push("workspace_role");
    }
    let (Ok(email), Some(org_role), Ok(workspace), Ok(guest_workspace)) =
        (email, org_role, workspace, guest_workspace)
    else {
        return validation_failed(&invalid, &headers);
    };
    let Some(public_url) = services.settings.public_url.clone() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "AUTH_UNAVAILABLE",
            "CRAFTCLOUD_PUBLIC_URL must be configured to build invitation links.",
            &headers,
        );
    };
    let Some(org_id) = parse_id(&org_raw) else {
        return not_found(&headers);
    };

    // Policy decision under the organization lock with fresh roles.
    let mut tx = match OrgTx::begin(&services.pool, org_id).await {
        Ok(Some(tx)) => tx,
        Ok(None) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "invitation transaction failed"),
    };
    let invited_workspace = workspace.map(|(id, _)| id).or(guest_workspace);
    let loaded = async {
        let role = tx.role_of(session.user_id).await?;
        let ws_role = match invited_workspace {
            Some(workspace_id) => tx.workspace_role_of(workspace_id, session.user_id).await?,
            None => None,
        };
        let enabled = tx.workspace_admin_invites_enabled().await?;
        Ok::<_, DbError>((role, ws_role, enabled))
    }
    .await;
    let (role, ws_role, enabled) = match loaded {
        Ok(loaded) => loaded,
        Err(error) => return db_failure(&error, &headers, "actor lookup failed"),
    };
    let actor = Actor {
        user_id: session.user_id,
        instance_operator: false,
        membership: role.map(|role| OrgMembership { org_id, role }),
        workspace: invited_workspace
            .zip(ws_role)
            .map(|(workspace_id, role)| WorkspaceMembership { workspace_id, role }),
    };
    let spec = InvitationSpec {
        org_role,
        workspace,
        guest_workspace,
        workspace_admin_invites_enabled: enabled,
        created_by: Some(session.user_id),
    };
    let decision = can(
        &actor,
        Action::InvitationCreate,
        &Resource::Invitation {
            org_id,
            spec: Some(spec),
        },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "invitation.create",
        Some(org_id),
        None,
    ) {
        Gate::Open(permit) => permit,
        Gate::Closed(response) => return response,
    };
    let Some(scope) = OrgScope::from_permit(&permit) else {
        return internal(&headers);
    };

    let Ok(token) = OpaqueToken::generate() else {
        tracing::error!("invitation token generation failed");
        return internal(&headers);
    };
    let now = services.clock.now();
    let created = tx
        .invitation_create(
            &scope,
            &NewInvitation {
                email: email.as_str().to_owned(),
                language: language.to_owned(),
                email_delivery: services.email_delivery_enabled(),
                org_role,
                workspace,
                guest_workspace,
                token_hash: token.hash(),
                created_by: session.user_id,
                now,
                expires_at: now + INVITATION_TTL,
            },
        )
        .await;
    match created {
        Ok(CreateInvitationOutcome::Created(invitation)) => {
            let invitation = *invitation;
            if services.email_delivery_enabled() {
                let outbox_id = Uuid::new_v4();
                let encrypted_token =
                    match services.encrypt_email_token(outbox_id, token.expose().as_bytes()) {
                        Ok(value) => value,
                        Err(_) => return unavailable(&headers, "TWO_FACTOR_UNAVAILABLE"),
                    };
                let organization_name = match tx.current_name().await {
                    Ok(name) => name,
                    Err(error) => {
                        return db_failure(&error, &headers, "organization lookup failed");
                    }
                };
                let inviter_name = match tx.user_display_name(session.user_id).await {
                    Ok(Some(name)) => name,
                    Ok(None) => return not_found(&headers),
                    Err(error) => return db_failure(&error, &headers, "inviter lookup failed"),
                };
                let draft = craftcloud_db::email_outbox::EmailDraft {
                    id: outbox_id,
                    recipient: email.as_str().to_owned(),
                    template: "invitation".to_owned(),
                    language: language.to_owned(),
                    params: json!({
                        "recipient_name": email.as_str(),
                        "inviter_name": inviter_name,
                        "organization_name": organization_name,
                        "org_role": org_role.as_str(),
                        "invitation_id": invitation.id.to_string(),
                    }),
                    encrypted_token: Some(encrypted_token),
                    expires_at: invitation.expires_at,
                    actor_id: Some(session.user_id),
                    is_resend: false,
                    now,
                };
                match tx.enqueue_email(&draft).await {
                    Ok(craftcloud_db::email_outbox::EnqueueOutcome::Queued) => {}
                    Ok(_) => {
                        return error_response(
                            StatusCode::TOO_MANY_REQUESTS,
                            "RATE_LIMITED",
                            "Too many email messages were requested. Try again later.",
                            &headers,
                        );
                    }
                    Err(error) => {
                        return db_failure(&error, &headers, "invitation email enqueue failed");
                    }
                }
            }
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "invitation.create",
                    org_id,
                    Some(invitation.id),
                )
                .detail("role", Detail::Code(org_role.as_str())),
            )
            .await
            {
                return db_failure(&error, &headers, "invitation creation failed");
            }
            if services.email_delivery_enabled() {
                return (
                    StatusCode::ACCEPTED,
                    Json(json!({
                        "invitation_id": invitation.id.to_string(),
                        "sent_to": invitation.email,
                        "expires_at": rfc3339(invitation.expires_at),
                    })),
                )
                    .into_response();
            }
            // The token travels in the URL fragment: not sent to servers or Referer.
            let invite_url = format!("{public_url}/invite#token={}", token.expose());
            (
                StatusCode::CREATED,
                Json(json!({
                    "invitation_id": invitation.id.to_string(),
                    "invite_url": invite_url,
                    "expires_at": rfc3339(invitation.expires_at),
                })),
            )
                .into_response()
        }
        Ok(CreateInvitationOutcome::Pending) => conflict(
            "INVITATION_PENDING",
            "An open invitation for this address exists; revoke it first.",
            &headers,
        ),
        Ok(CreateInvitationOutcome::LimitReached) => conflict(
            "INVITATION_LIMIT",
            "The organization has too many open invitations.",
            &headers,
        ),
        Ok(CreateInvitationOutcome::WorkspaceMissing) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "invitation creation failed"),
    }
}

// ---- GET /organizations/{org_id}/invitations -----------------------------------

pub(crate) async fn list_invitations(
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
        Action::InvitationList,
        &Resource::Invitation { org_id, spec: None },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "invitation.list",
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
        .map(|c| decode_time_cursor(c, "invitations", &scope_text))
    {
        Some(Ok(key)) => Some(key),
        Some(Err(error)) => return page_error(error, &headers),
        None => None,
    };
    let mut rows = match invitations::list(&services.pool, &scope, limit + 1, after).await {
        Ok(rows) => rows,
        Err(error) => return db_failure(&error, &headers, "invitation list failed"),
    };
    let next = if rows.len() > limit as usize {
        rows.truncate(limit as usize);
        rows.last()
            .map(|last| encode_time_cursor("invitations", &scope_text, last.created_at, last.id))
    } else {
        None
    };
    let now = services.clock.now();
    let items: Vec<_> = rows.iter().map(|i| invitation_json(i, now)).collect();
    Json(json!({ "items": items, "next_cursor": next })).into_response()
}

// ---- DELETE /organizations/{org_id}/invitations/{id} -----------------------------

pub(crate) async fn revoke_invitation(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path((org_raw, id_raw)): Path<(String, String)>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    let Some(org_id) = parse_id(&org_raw) else {
        return not_found(&headers);
    };
    let invitation_id = parse_id(&id_raw);
    let mut tx = match OrgTx::begin(&services.pool, org_id).await {
        Ok(Some(tx)) => tx,
        Ok(None) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "invitation transaction failed"),
    };
    let loaded = async {
        let role = tx.role_of(session.user_id).await?;
        let invitation = match invitation_id {
            Some(id) => tx.invitation_get(id).await?,
            None => None,
        };
        let spec = match &invitation {
            Some(invitation) => tx.invitation_spec(invitation).await?,
            None => None,
        };
        let invited_workspace = spec
            .as_ref()
            .and_then(|s| s.workspace.map(|(id, _)| id).or(s.guest_workspace));
        let ws_role = match invited_workspace {
            Some(workspace_id) => tx.workspace_role_of(workspace_id, session.user_id).await?,
            None => None,
        };
        Ok::<_, DbError>((role, spec, ws_role))
    }
    .await;
    let (role, spec, ws_role) = match loaded {
        Ok(loaded) => loaded,
        Err(error) => return db_failure(&error, &headers, "invitation lookup failed"),
    };
    let actor = Actor {
        user_id: session.user_id,
        instance_operator: false,
        membership: role.map(|role| OrgMembership { org_id, role }),
        workspace: spec
            .and_then(|s| s.workspace.map(|(id, _)| id).or(s.guest_workspace))
            .zip(ws_role)
            .map(|(workspace_id, role)| WorkspaceMembership { workspace_id, role }),
    };
    let decision = can(
        &actor,
        Action::InvitationRevoke,
        &Resource::Invitation { org_id, spec },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "invitation.revoke",
        Some(org_id),
        invitation_id,
    ) {
        Gate::Open(permit) => permit,
        Gate::Closed(response) => return response,
    };
    let (Some(scope), Some(invitation_id)) = (OrgScope::from_permit(&permit), invitation_id) else {
        return internal(&headers);
    };
    match tx
        .invitation_revoke(&scope, invitation_id, services.clock.now())
        .await
    {
        Ok(true) => {
            if let Err(error) = commit_audited(
                tx,
                Change::new(
                    &headers,
                    session.user_id,
                    "invitation.revoke",
                    org_id,
                    Some(invitation_id),
                ),
            )
            .await
            {
                return db_failure(&error, &headers, "invitation revoke failed");
            }
            StatusCode::NO_CONTENT.into_response()
        }
        // Already accepted, revoked or invalid.
        Ok(false) => not_found(&headers),
        Err(error) => db_failure(&error, &headers, "invitation revoke failed"),
    }
}

// ---- POST /organizations/{org_id}/invitations/{id}/resend ------------------

pub(crate) async fn resend_invitation(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path((org_raw, id_raw)): Path<(String, String)>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if let Err(response) = services.rate_limit_invite(session.user_id, &headers) {
        return *response;
    }
    if !services.email_delivery_enabled() && services.settings.public_url.is_none() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "AUTH_UNAVAILABLE",
            "CRAFTCLOUD_PUBLIC_URL must be configured to build invitation links.",
            &headers,
        );
    }
    let Some(org_id) = parse_id(&org_raw) else {
        return not_found(&headers);
    };
    let invitation_id = parse_id(&id_raw);
    let mut tx = match OrgTx::begin(&services.pool, org_id).await {
        Ok(Some(tx)) => tx,
        Ok(None) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "invitation transaction failed"),
    };
    let loaded = async {
        let role = tx.role_of(session.user_id).await?;
        let invitation = match invitation_id {
            Some(id) => tx.invitation_get(id).await?,
            None => None,
        };
        let spec = match &invitation {
            Some(invitation) => tx.invitation_spec(invitation).await?,
            None => None,
        };
        let invited_workspace = spec
            .as_ref()
            .and_then(|s| s.workspace.map(|(id, _)| id).or(s.guest_workspace));
        let ws_role = match invited_workspace {
            Some(workspace_id) => tx.workspace_role_of(workspace_id, session.user_id).await?,
            None => None,
        };
        Ok::<_, DbError>((role, invitation, spec, invited_workspace, ws_role))
    }
    .await;
    let (role, invitation, spec, invited_workspace, ws_role) = match loaded {
        Ok(value) => value,
        Err(error) => return db_failure(&error, &headers, "invitation lookup failed"),
    };
    let Some(_invitation) = invitation else {
        return not_found(&headers);
    };
    let actor = Actor {
        user_id: session.user_id,
        instance_operator: false,
        membership: role.map(|role| OrgMembership { org_id, role }),
        workspace: invited_workspace
            .zip(ws_role)
            .map(|(workspace_id, role)| WorkspaceMembership { workspace_id, role }),
    };
    let decision = can(
        &actor,
        Action::InvitationCreate,
        &Resource::Invitation { org_id, spec },
    );
    let permit = match gate(
        decision,
        &headers,
        session.user_id,
        "invitation.resend",
        Some(org_id),
        invitation_id,
    ) {
        Gate::Open(permit) => permit,
        Gate::Closed(response) => return response,
    };
    let (Some(scope), Some(invitation_id)) = (OrgScope::from_permit(&permit), invitation_id) else {
        return internal(&headers);
    };
    let Ok(token) = OpaqueToken::generate() else {
        return internal(&headers);
    };
    let now = services.clock.now();
    let expires_at = now + INVITATION_TTL;
    let rotated = match tx
        .invitation_resend(
            &scope,
            invitation_id,
            &token.hash(),
            session.user_id,
            now,
            expires_at,
            services.email_delivery_enabled(),
        )
        .await
    {
        Ok(Some(invitation)) => invitation,
        Ok(None) => return not_found(&headers),
        Err(error) => return db_failure(&error, &headers, "invitation rotation failed"),
    };
    if services.email_delivery_enabled() {
        let outbox_id = Uuid::new_v4();
        let encrypted_token =
            match services.encrypt_email_token(outbox_id, token.expose().as_bytes()) {
                Ok(value) => value,
                Err(_) => return unavailable(&headers, "TWO_FACTOR_UNAVAILABLE"),
            };
        let organization_name = match tx.current_name().await {
            Ok(name) => name,
            Err(error) => return db_failure(&error, &headers, "organization lookup failed"),
        };
        let inviter_name = match tx.user_display_name(session.user_id).await {
            Ok(Some(name)) => name,
            Ok(None) => return not_found(&headers),
            Err(error) => return db_failure(&error, &headers, "inviter lookup failed"),
        };
        let draft = craftcloud_db::email_outbox::EmailDraft {
            id: outbox_id,
            recipient: rotated.email.clone(),
            template: "invitation".to_owned(),
            language: rotated.language.clone(),
            params: json!({
                "recipient_name": rotated.email.clone(),
                "inviter_name": inviter_name,
                "organization_name": organization_name,
                "org_role": rotated.org_role.clone(),
                "invitation_id": rotated.id.to_string(),
            }),
            encrypted_token: Some(encrypted_token),
            expires_at,
            actor_id: Some(session.user_id),
            is_resend: true,
            now,
        };
        match tx.enqueue_email(&draft).await {
            Ok(craftcloud_db::email_outbox::EnqueueOutcome::Queued) => {}
            Ok(_) => {
                return error_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    "RATE_LIMITED",
                    "Too many email messages were requested. Try again later.",
                    &headers,
                );
            }
            Err(error) => return db_failure(&error, &headers, "invitation email enqueue failed"),
        }
    }
    if let Err(error) = tx
        .audit(
            &crate::audit::ctx(&headers, Some(session.user_id)),
            craftcloud_db::audit::NewEvent::new("invitation.resend")
                .org(org_id)
                .target("invitation", invitation_id),
        )
        .await
    {
        return db_failure(&error, &headers, "invitation resend audit failed");
    }
    if let Err(error) = tx.commit().await {
        return db_failure(&error, &headers, "invitation resend failed");
    }
    if services.email_delivery_enabled() {
        return (
            StatusCode::ACCEPTED,
            Json(json!({"invitation_id": rotated.id.to_string(), "sent_to": rotated.email, "expires_at": rfc3339(rotated.expires_at)})),
        )
            .into_response();
    }
    let Some(public_url) = services.settings.public_url.as_deref() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "AUTH_UNAVAILABLE",
            "CRAFTCLOUD_PUBLIC_URL must be configured to build invitation links.",
            &headers,
        );
    };
    (
        StatusCode::CREATED,
        Json(json!({"invitation_id": rotated.id.to_string(), "invite_url": format!("{public_url}/invite#token={}", token.expose()), "expires_at": rfc3339(rotated.expires_at)})),
    )
        .into_response()
}

// ---- POST /invitations/accept -----------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptRequest {
    token: String,
    display_name: Option<String>,
    password: Option<String>,
}

impl Drop for AcceptRequest {
    fn drop(&mut self) {
        self.token.zeroize();
        if let Some(password) = &mut self.password {
            password.zeroize();
        }
    }
}

pub(crate) async fn accept_invitation(
    State(state): State<ApiState>,
    client_ip: Option<Extension<ClientIp>>,
    headers: HeaderMap,
    session: Result<AuthenticatedSession, AuthRejection>,
    body: Result<Json<AcceptRequest>, JsonRejection>,
) -> Response {
    let Some(services) = state.services.clone() else {
        return unavailable(&headers, "AUTH_UNAVAILABLE");
    };
    if let Err(response) = services.rate_limit_ip(
        IpClass::Accept,
        client_ip.map(|Extension(ip)| ip.0),
        &headers,
    ) {
        return *response;
    }
    // A cookie that fails the CSRF check (or any other non-401 problem) stops here;
    // no cookie or an unknown/expired one means "anonymous".
    let session = match session {
        Ok(session) => Some(session),
        Err(rejection) if rejection.is_unauthenticated() => None,
        Err(rejection) => return rejection.into_response(),
    };
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    // Origin check like login: the anonymous path creates an account and a session.
    let bearer_actor = session
        .as_ref()
        .is_some_and(|actor| actor.credential_kind == CredentialKind::Device);
    if !bearer_actor && !origin_ok(&headers, &services.settings) {
        return error_response(
            StatusCode::FORBIDDEN,
            "CSRF_FAILED",
            "The request failed the cross-site request forgery check.",
            &headers,
        );
    }
    let Some(token) = OpaqueToken::parse(request.token.trim()) else {
        return invalid_invitation(&headers);
    };
    let token_hash = token.hash();
    let now = services.clock.now();
    let peek = match invitations::peek(&services.pool, &token_hash, now).await {
        Ok(Some(peek)) => peek,
        Ok(None) => return invalid_invitation(&headers),
        Err(error) => return db_failure(&error, &headers, "invitation lookup failed"),
    };
    let account = match accounts::find_by_email(&services.pool, &peek.email).await {
        Ok(account) => account,
        Err(error) => return db_failure(&error, &headers, "account lookup failed"),
    };
    if services.email_delivery_enabled() && !peek.email_delivery {
        return invalid_invitation(&headers);
    }
    if peek.email_delivery
        && account
            .as_ref()
            .is_some_and(|account| account.email_verified_at.is_none())
    {
        return invalid_invitation(&headers);
    }

    let mut issued_hash: Option<String> = None;
    let by = match (&account, &session) {
        (Some(account), _) if account.disabled_at.is_some() => return invalid_invitation(&headers),
        (Some(_), None) => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "LOGIN_REQUIRED",
                "An account exists for the invited address; log in first.",
                &headers,
            );
        }
        (Some(account), Some(session)) if session.user_id != account.id => {
            return mismatch(&headers);
        }
        (Some(_), Some(session)) => AcceptBy::Existing {
            session_user: session.user_id,
        },
        // Logged in as somebody else while no account exists for the address.
        (None, Some(_)) => return mismatch(&headers),
        (None, None) => {
            let (Some(display_name), Some(password)) = (&request.display_name, &request.password)
            else {
                return validation_failed(&["display_name", "password"], &headers);
            };
            let name = DisplayName::parse(display_name);
            let local = peek.email.split_once('@').map_or("", |(local, _)| local);
            let password_ok = validate_password(password, local).is_ok();
            let mut invalid = Vec::new();
            if name.is_err() {
                invalid.push("display_name");
            }
            if !password_ok {
                invalid.push("password");
            }
            let Ok(name) = name else {
                return validation_failed(&invalid, &headers);
            };
            if !password_ok {
                return validation_failed(&invalid, &headers);
            }
            let mut copy = password.clone();
            let hashed = services
                .run_blocking(move || {
                    let hash = hash_password(&copy);
                    copy.zeroize();
                    hash
                })
                .await
                .and_then(Result::ok);
            let Some(password_hash) = hashed else {
                tracing::error!("password hashing failed");
                return internal(&headers);
            };
            issued_hash = Some(password_hash.clone());
            AcceptBy::NewAccount {
                display_name: name.as_str().to_owned(),
                password_hash,
            }
        }
    };

    let outcome = invitations::accept_audited(
        &services.pool,
        &token_hash,
        &by,
        now,
        &crate::audit::ctx(&headers, None),
    )
    .await;
    let (user_id, org_id, workspace_id, account_created, applied) = match outcome {
        Ok(AcceptOutcome::Accepted {
            user_id,
            org_id,
            workspace_id,
            account_created,
            invitation_id,
            inviter_id,
            workspace_role_applied,
        }) => {
            crate::observe::note_actor(user_id);
            crate::observe::note_org(org_id);
            if account_created {
                // Provenance of an account that no e-mail verification backs yet.
                tracing::info!(
                    target: "craftcloud::audit",
                    action = "account.created_via_invitation",
                    account = %user_id,
                    invitation = %invitation_id,
                    org = %org_id,
                    inviter = %inviter_id,
                    "audit"
                );
            }
            (
                user_id,
                org_id,
                workspace_id,
                account_created,
                workspace_role_applied,
            )
        }
        Ok(AcceptOutcome::Invalid) => return invalid_invitation(&headers),
        Ok(AcceptOutcome::LoginRequired) => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "LOGIN_REQUIRED",
                "An account exists for the invited address; log in first.",
                &headers,
            );
        }
        Ok(AcceptOutcome::EmailMismatch) => return mismatch(&headers),
        Ok(AcceptOutcome::AlreadyMember) => {
            return conflict(
                "ALREADY_MEMBER",
                "You are already a member of this organization.",
                &headers,
            );
        }
        Err(error) => return db_failure(&error, &headers, "invitation acceptance failed"),
    };
    audit(
        &headers,
        user_id,
        "invitation.accept",
        Some(org_id),
        workspace_id,
        "success",
    );

    let mut body = json!({
        "user_id": user_id.to_string(),
        "organization_id": org_id.to_string(),
        "workspace_id": workspace_id.map(|id| id.to_string()),
        "account_created": account_created,
        "workspace_role_applied": applied,
    });
    // A newly created account is logged in right away, like after a login.
    let mut cookie = None;
    if let (true, Some(hash)) = (account_created, issued_hash) {
        let Ok(session_token) = OpaqueToken::generate() else {
            tracing::error!("session token generation failed");
            return internal(&headers);
        };
        let absolute = now + services.settings.absolute_timeout;
        let created = sessions::create_for_verified_login_audited(
            &services.pool,
            &sessions::NewSession {
                user_id,
                token_hash: session_token.hash(),
                now,
                absolute_expires_at: absolute,
            },
            &hash,
            &crate::audit::ctx(&headers, Some(user_id)),
        )
        .await;
        match created {
            Ok(Some(_)) => {
                let expires_at = (now + services.settings.idle_timeout).min(absolute);
                if let Some(object) = body.as_object_mut() {
                    object.insert("csrf_token".to_owned(), json!(session_token.csrf_token()));
                    object.insert("expires_at".to_owned(), json!(rfc3339(expires_at)));
                }
                cookie = session_cookie(
                    &services.settings,
                    &session_token,
                    (absolute - now).whole_seconds(),
                );
            }
            // The account exists; the user can simply log in.
            Ok(None) => tracing::warn!("session for the new account could not be created"),
            Err(error) => return db_failure(&error, &headers, "session creation failed"),
        }
    }
    let mut response = Json(body).into_response();
    if let Some(cookie) = cookie {
        response
            .headers_mut()
            .append(axum::http::header::SET_COOKIE, cookie);
    }
    response
}

fn mismatch(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::FORBIDDEN,
        "INVITATION_EMAIL_MISMATCH",
        "The invitation was issued for a different e-mail address.",
        headers,
    )
}
