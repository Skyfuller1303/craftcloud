//! CraftCloud HTTP API: router, handlers and centralized middleware.
//!
//! This crate only builds an [`axum::Router`]; binding sockets, configuration
//! from the environment and process lifecycle belong to the server binary.
//!
//! Scope of the PR-001 skeleton: `GET /health/live` and `GET /api/v1/info`.
//! There is no authentication, database, storage, session or upload code.
//!
//! Absolute URLs must come from the operator-configured public URL only, never
//! from `Host` or `X-Forwarded-*` headers (see [`ClientIp`] for how forwarded
//! client addresses are treated).

mod app_access;
#[cfg(test)]
pub(crate) mod app_access_tests;
pub mod archive_transport;
pub mod archive_worker;
mod audit;
mod auth;
mod client_ip;
mod clock;
mod device;
mod email;
mod figcraft;
mod figcraft_jobs;
mod files;
mod grants;
mod health;
mod info;
mod invitations;
mod locks;
pub mod maintenance;
mod observe;
mod oidc;
mod setup_wizard;
#[cfg(test)]
mod setup_wizard_tests;
#[cfg(test)]
extern crate self as craftcloud_api;
mod oidc_provider;
#[cfg(test)]
mod oidc_tests;
mod orgs;
mod overview;
mod pagination;
mod projects;
mod ratelimit;
mod responses;
mod scim;
mod services;
mod session;
mod setup;
mod sso_settings;
mod two_factor;
mod uploads;
mod versions;
mod workspaces;

pub use client_ip::ClientIp;
pub use clock::{Clock, SystemClock};
pub use device::SecondFactorLogin;
pub use figcraft::{FigcraftWorkerState, figcraft_worker_tick, run_figcraft_worker};
pub use health::metrics_router;
pub use observe::Metrics;
pub use services::{AuthSettings, EmailTransportStatus, Services};
pub use session::{AuthRejection, AuthenticatedSession, CSRF_HEADER};

use std::{sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put},
};
use craftcloud_core::Capabilities;
use ipnet::IpNet;
use serde_json::json;
use tower_http::{
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer},
    set_header::SetResponseHeaderLayer,
};
use uuid::Uuid;

/// Name of the request correlation header.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Maximum accepted length of a client-supplied request id.
const REQUEST_ID_MAX_LEN: usize = 64;

/// Tunable limits of the HTTP stack.
#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// Maximum accepted request body size in bytes.
    pub max_body_bytes: usize,
    /// Maximum time for a handler to produce a response before answering
    /// 503 `TIMEOUT`.
    pub request_timeout: Duration,
    /// Reverse proxies whose `X-Forwarded-For` is trusted. Empty trusts nobody.
    pub trusted_proxies: Vec<IpNet>,
    /// Counters and histograms of the request middleware. Give the same
    /// registry to [`Services::with_metrics`] and to the metrics listener.
    pub metrics: Arc<Metrics>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: 64 * 1024,
            request_timeout: Duration::from_secs(10),
            trusted_proxies: Vec::new(),
            metrics: Arc::new(Metrics::new()),
        }
    }
}

/// Every route the router serves: path template and the HTTP methods
/// answered there. This is the code side of the contract; the test
/// `tests/contract.rs` compares it with `contracts/openapi.yaml`, probes the
/// router for exactly these routes and fails when `router_with_services` gains
/// a `.route(` that is not listed here.
pub const ROUTES: &[(&str, &[&str])] = &[
    ("/health/live", &["GET"]),
    ("/api/v1/auth/oidc/discover", &["POST"]),
    ("/api/v1/auth/oidc/start", &["POST"]),
    ("/api/v1/auth/oidc/authorize", &["GET"]),
    ("/api/v1/auth/oidc/callback", &["GET"]),
    ("/api/v1/auth/oidc/finish", &["POST"]),
    ("/api/v1/auth/oidc/pending", &["POST"]),
    ("/api/v1/auth/oidc/device/exchange", &["POST"]),
    (
        "/api/v1/organizations/{org_id}/sso",
        &["GET", "PUT", "DELETE"],
    ),
    ("/api/v1/organizations/{org_id}/sso/test", &["POST"]),
    ("/api/v1/admin/sso", &["GET", "PUT"]),
    ("/api/v1/admin/sso/domain-claims", &["GET"]),
    (
        "/api/v1/admin/sso/domain-claims/{org_id}/{domain}",
        &["PUT"],
    ),
    ("/api/v1/organizations/{org_id}/scim", &["GET"]),
    (
        "/api/v1/organizations/{org_id}/scim/token",
        &["POST", "DELETE"],
    ),
    (
        "/api/v1/organizations/{org_id}/scim/mappings",
        &["GET", "PUT"],
    ),
    ("/api/v1/organizations/{org_id}/scim/events", &["GET"]),
    ("/scim/v2/organizations/{org_id}/Users", &["GET", "POST"]),
    (
        "/scim/v2/organizations/{org_id}/Users/{id}",
        &["GET", "PUT", "PATCH", "DELETE"],
    ),
    ("/scim/v2/organizations/{org_id}/Groups", &["GET", "POST"]),
    (
        "/scim/v2/organizations/{org_id}/Groups/{id}",
        &["GET", "PUT", "PATCH", "DELETE"],
    ),
    ("/health/ready", &["GET"]),
    ("/api/v1/info", &["GET"]),
    ("/api/v1/setup/status", &["GET"]),
    ("/api/v1/setup/complete", &["POST"]),
    ("/api/v1/organizations", &["GET", "POST"]),
    ("/api/v1/organizations/{org_id}", &["GET", "PATCH"]),
    ("/api/v1/organizations/{org_id}/members", &["GET", "POST"]),
    ("/api/v1/organizations/{org_id}/apps", &["GET"]),
    ("/api/v1/organizations/{org_id}/app-access", &["PATCH"]),
    (
        "/api/v1/organizations/{org_id}/members/{user_id}/apps",
        &["PUT", "PATCH"],
    ),
    ("/api/v1/organizations/{org_id}/grants", &["GET"]),
    ("/api/v1/organizations/{org_id}/locks", &["GET"]),
    (
        "/api/v1/organizations/{org_id}/members/{user_id}",
        &["PATCH", "DELETE"],
    ),
    (
        "/api/v1/organizations/{org_id}/workspaces",
        &["GET", "POST"],
    ),
    ("/api/v1/workspaces/{workspace_id}", &["GET", "PATCH"]),
    (
        "/api/v1/workspaces/{workspace_id}/members",
        &["GET", "POST"],
    ),
    (
        "/api/v1/workspaces/{workspace_id}/members/{user_id}",
        &["PATCH", "DELETE"],
    ),
    (
        "/api/v1/organizations/{org_id}/invitations",
        &["GET", "POST"],
    ),
    (
        "/api/v1/organizations/{org_id}/invitations/{invitation_id}",
        &["DELETE"],
    ),
    (
        "/api/v1/organizations/{org_id}/invitations/{invitation_id}/resend",
        &["POST"],
    ),
    ("/api/v1/invitations/accept", &["POST"]),
    (
        "/api/v1/workspaces/{workspace_id}/projects",
        &["GET", "POST"],
    ),
    ("/api/v1/projects/{project_id}", &["GET", "PATCH", "DELETE"]),
    ("/api/v1/projects/{project_id}/restore", &["POST"]),
    ("/api/v1/projects/{project_id}/files", &["GET", "POST"]),
    ("/api/v1/files/{file_id}", &["GET", "PATCH", "DELETE"]),
    ("/api/v1/files/{file_id}/restore", &["POST"]),
    ("/api/v1/files/{file_id}/lock", &["POST", "DELETE"]),
    ("/api/v1/files/{file_id}/live", &["GET", "POST", "DELETE"]),
    ("/api/v1/files/{file_id}/live/snapshot", &["POST"]),
    ("/api/v1/projects/{project_id}/grants", &["GET", "POST"]),
    ("/api/v1/files/{file_id}/grants", &["GET", "POST"]),
    ("/api/v1/grants/{grant_id}", &["PATCH", "DELETE"]),
    ("/api/v1/organizations/{org_id}/shared-with-me", &["GET"]),
    ("/api/v1/files/{file_id}/uploads", &["POST"]),
    ("/api/v1/files/{file_id}/versions", &["GET", "POST"]),
    ("/api/v1/files/{file_id}/versions/{version_id}", &["GET"]),
    (
        "/api/v1/files/{file_id}/versions/{version_id}/restore",
        &["POST"],
    ),
    ("/api/v1/files/{file_id}/content", &["GET"]),
    (
        "/api/v1/files/{file_id}/versions/{version_id}/content",
        &["GET"],
    ),
    ("/api/v1/uploads/{upload_id}", &["GET", "DELETE"]),
    ("/api/v1/uploads/{upload_id}/content", &["PUT"]),
    ("/api/v1/organizations/{org_id}/usage", &["GET"]),
    ("/api/v1/organizations/{org_id}/overview", &["GET"]),
    ("/api/v1/workspaces/{workspace_id}/quota", &["PUT"]),
    ("/api/v1/admin/organizations/{org_id}/quota", &["PUT"]),
    ("/api/v1/admin/organizations/{org_id}/seats", &["PATCH"]),
    ("/api/v1/admin/organizations", &["GET"]),
    ("/api/v1/admin/users", &["GET"]),
    ("/api/v1/admin/users/email-verification", &["POST"]),
    ("/api/v1/admin/email-delivery", &["GET"]),
    ("/api/v1/admin/email-delivery/test", &["POST"]),
    ("/api/v1/admin/setup", &["GET"]),
    ("/api/v1/admin/setup/steps/{step}", &["PATCH"]),
    ("/api/v1/admin/setup/system-check", &["GET"]),
    ("/api/v1/admin/instance-settings", &["GET", "PATCH"]),
    ("/api/v1/admin/instance-settings/email-test", &["POST"]),
    ("/api/v1/admin/users/{user_id}/devices", &["GET"]),
    (
        "/api/v1/admin/users/{user_id}/devices/{device_ref}",
        &["DELETE"],
    ),
    ("/api/v1/admin/maintenance", &["GET"]),
    ("/api/v1/admin/health", &["GET"]),
    ("/api/v1/organizations/{org_id}/audit", &["GET"]),
    ("/api/v1/organizations/{org_id}/audit/export", &["GET"]),
    ("/api/v1/admin/audit", &["GET"]),
    ("/api/v1/admin/audit/export", &["GET"]),
    ("/api/v1/admin/users/{user_id}/disable", &["POST"]),
    ("/api/v1/admin/users/{user_id}/enable", &["POST"]),
    ("/api/v1/auth/login", &["POST"]),
    ("/api/v1/auth/login/second-factor", &["POST"]),
    ("/api/v1/auth/2fa", &["GET"]),
    ("/api/v1/auth/2fa/setup", &["POST"]),
    ("/api/v1/auth/2fa/confirm", &["POST"]),
    ("/api/v1/auth/2fa/disable", &["POST"]),
    ("/api/v1/auth/2fa/recovery-codes", &["POST"]),
    ("/api/v1/auth/device/login", &["POST"]),
    ("/api/v1/auth/device/login/second-factor", &["POST"]),
    ("/api/v1/auth/device/refresh", &["POST"]),
    ("/api/v1/auth/device/logout", &["POST"]),
    ("/api/v1/auth/devices", &["GET"]),
    ("/api/v1/auth/devices/{id}", &["DELETE"]),
    ("/api/v1/auth/session", &["GET"]),
    ("/api/v1/auth/email-verification", &["POST"]),
    ("/api/v1/auth/email-verification/confirm", &["POST"]),
    ("/api/v1/auth/email-change", &["POST"]),
    ("/api/v1/auth/email-change/confirm", &["POST"]),
    ("/api/v1/auth/profile", &["PATCH"]),
    ("/api/v1/auth/sessions", &["GET"]),
    ("/api/v1/auth/sessions/{session_id}", &["DELETE"]),
    ("/api/v1/auth/logout", &["POST"]),
    ("/api/v1/auth/logout-all", &["POST"]),
    ("/api/v1/auth/password/change", &["POST"]),
    ("/api/v1/auth/password-reset", &["POST"]),
    (
        "/api/v1/admin/users/{user_id}/password-reset",
        &["POST", "DELETE"],
    ),
];

/// Builds the router with development-default capabilities and default limits.
pub fn router() -> Router {
    router_with(Capabilities::development_defaults(), &ApiConfig::default())
}

/// Builds the router with explicit capabilities and limits, without
/// database-backed services (setup and authentication routes answer 503).
pub fn router_with(capabilities: Capabilities, config: &ApiConfig) -> Router {
    router_with_services(capabilities, config, None)
}

/// Builds the router with the optional database-backed services (setup and
/// authentication routes).
pub fn router_with_services(
    capabilities: Capabilities,
    config: &ApiConfig,
    services: Option<Services>,
) -> Router {
    router_with_shared(
        capabilities,
        config,
        services.map(|s| Arc::new(s.with_metrics(Arc::clone(&config.metrics)))),
    )
}

/// Like [`router_with_services`] for services the caller keeps a handle to
/// (the server shares them with the internal metrics listener). Create them
/// with [`Services::with_metrics`] and the registry of `config`, otherwise
/// the counters of the services and of the request middleware are separate.
pub fn router_with_shared(
    capabilities: Capabilities,
    config: &ApiConfig,
    services: Option<Arc<Services>>,
) -> Router {
    let state = ApiState {
        capabilities,
        services,
    };
    let audit_services = state.services.clone();
    let routes = Router::new()
        .route("/health/live", get(health_live))
        .route("/api/v1/auth/oidc/discover", post(oidc::discover))
        .route("/api/v1/auth/oidc/start", post(oidc::start))
        .route("/api/v1/auth/oidc/authorize", get(oidc::authorize))
        .route("/api/v1/auth/oidc/callback", get(oidc::callback))
        .route("/api/v1/auth/oidc/finish", post(oidc::finish))
        .route("/api/v1/auth/oidc/pending", post(oidc::pending_info))
        .route("/api/v1/auth/oidc/device/exchange", post(oidc::exchange))
        .route(
            "/api/v1/organizations/{org_id}/sso",
            get(sso_settings::get)
                .put(sso_settings::put)
                .delete(sso_settings::delete),
        )
        .route(
            "/api/v1/organizations/{org_id}/sso/test",
            post(sso_settings::test),
        )
        .route(
            "/api/v1/admin/sso",
            get(sso_settings::instance_get).put(sso_settings::instance_put),
        )
        .route(
            "/api/v1/admin/sso/domain-claims",
            get(sso_settings::claims_list),
        )
        .route(
            "/api/v1/admin/sso/domain-claims/{org_id}/{domain}",
            put(sso_settings::claim_decide),
        )
        .route("/api/v1/organizations/{org_id}/scim", get(scim::status))
        .route(
            "/api/v1/organizations/{org_id}/scim/token",
            post(scim::rotate_token).delete(scim::revoke_token),
        )
        .route(
            "/api/v1/organizations/{org_id}/scim/mappings",
            get(scim::mappings).put(scim::set_mappings),
        )
        .route(
            "/api/v1/organizations/{org_id}/scim/events",
            get(scim::events),
        )
        .route(
            "/scim/v2/organizations/{org_id}/Users",
            get(scim::list_users).post(scim::create_user),
        )
        .route(
            "/scim/v2/organizations/{org_id}/Users/{id}",
            get(scim::get_user)
                .put(scim::put_user)
                .patch(scim::patch_user)
                .delete(scim::delete_user),
        )
        .route(
            "/scim/v2/organizations/{org_id}/Groups",
            get(scim::list_groups).post(scim::create_group),
        )
        .route(
            "/scim/v2/organizations/{org_id}/Groups/{id}",
            get(scim::get_group)
                .put(scim::put_group)
                .patch(scim::patch_group)
                .delete(scim::delete_group),
        )
        .route("/health/ready", get(health::health_ready))
        .route("/api/v1/info", get(info::discover))
        .route("/api/v1/setup/status", get(setup::status))
        .route("/api/v1/setup/complete", post(setup::complete))
        .route(
            "/api/v1/organizations",
            get(orgs::list_mine).post(orgs::create_org),
        )
        .route(
            "/api/v1/organizations/{org_id}",
            get(orgs::get_org).patch(orgs::update_org),
        )
        .route(
            "/api/v1/organizations/{org_id}/members",
            get(orgs::list_members).post(orgs::add_member),
        )
        .route("/api/v1/organizations/{org_id}/apps", get(app_access::get))
        .route(
            "/api/v1/organizations/{org_id}/app-access",
            axum::routing::patch(app_access::set_mode),
        )
        .route(
            "/api/v1/organizations/{org_id}/members/{user_id}/apps",
            put(app_access::assign).patch(app_access::change),
        )
        .route(
            "/api/v1/organizations/{org_id}/grants",
            get(grants::list_organization),
        )
        .route(
            "/api/v1/organizations/{org_id}/locks",
            get(locks::list_organization),
        )
        .route(
            "/api/v1/organizations/{org_id}/members/{user_id}",
            patch(orgs::change_role).delete(orgs::remove_member),
        )
        .route(
            "/api/v1/organizations/{org_id}/workspaces",
            get(workspaces::list_workspaces).post(workspaces::create_workspace),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}",
            get(workspaces::get_workspace).patch(workspaces::update_workspace),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/members",
            get(workspaces::list_members).post(workspaces::add_member),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/members/{user_id}",
            patch(workspaces::change_member_role).delete(workspaces::remove_member),
        )
        .route(
            "/api/v1/organizations/{org_id}/invitations",
            get(invitations::list_invitations).post(invitations::create_invitation),
        )
        .route(
            "/api/v1/organizations/{org_id}/invitations/{invitation_id}",
            delete(invitations::revoke_invitation),
        )
        .route(
            "/api/v1/organizations/{org_id}/invitations/{invitation_id}/resend",
            post(invitations::resend_invitation),
        )
        .route(
            "/api/v1/invitations/accept",
            post(invitations::accept_invitation),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/projects",
            get(projects::list_projects).post(projects::create_project),
        )
        .route(
            "/api/v1/projects/{project_id}",
            get(projects::get_project)
                .patch(projects::update_project)
                .delete(projects::delete_project),
        )
        .route(
            "/api/v1/projects/{project_id}/restore",
            post(projects::restore_project),
        )
        .route(
            "/api/v1/projects/{project_id}/files",
            get(files::list_files).post(files::create_file),
        )
        .route(
            "/api/v1/files/{file_id}",
            get(files::get_file)
                .patch(files::update_file)
                .delete(files::delete_file),
        )
        .route("/api/v1/files/{file_id}/restore", post(files::restore_file))
        .route(
            "/api/v1/files/{file_id}/lock",
            post(locks::acquire).delete(locks::release),
        )
        .route(
            "/api/v1/files/{file_id}/live",
            get(figcraft::status)
                .post(figcraft_jobs::open)
                .delete(figcraft_jobs::stop),
        )
        .route(
            "/api/v1/files/{file_id}/live/snapshot",
            post(figcraft_jobs::snapshot),
        )
        .route(
            "/api/v1/projects/{project_id}/grants",
            get(grants::list_project).post(grants::create_project),
        )
        .route(
            "/api/v1/files/{file_id}/grants",
            get(grants::list_file).post(grants::create_file),
        )
        .route(
            "/api/v1/grants/{grant_id}",
            patch(grants::change).delete(grants::revoke),
        )
        .route(
            "/api/v1/organizations/{org_id}/shared-with-me",
            get(grants::shared_with_me),
        )
        .route(
            "/api/v1/files/{file_id}/uploads",
            post(uploads::create_upload),
        )
        .route(
            "/api/v1/files/{file_id}/versions",
            get(versions::list_versions).post(versions::commit_version),
        )
        .route(
            "/api/v1/files/{file_id}/versions/{version_id}",
            get(versions::get_version),
        )
        .route(
            "/api/v1/files/{file_id}/versions/{version_id}/restore",
            post(versions::restore_version),
        )
        .route(
            "/api/v1/files/{file_id}/content",
            get(versions::download_head),
        )
        .route(
            "/api/v1/files/{file_id}/versions/{version_id}/content",
            get(versions::download_version),
        )
        .route(
            "/api/v1/uploads/{upload_id}",
            get(uploads::get_upload).delete(uploads::abort_upload),
        )
        .route(
            "/api/v1/organizations/{org_id}/usage",
            get(uploads::get_usage),
        )
        .route(
            "/api/v1/organizations/{org_id}/overview",
            get(overview::get_overview),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/quota",
            put(uploads::set_workspace_quota),
        )
        .route(
            "/api/v1/admin/organizations/{org_id}/quota",
            put(uploads::set_org_quota),
        )
        .route(
            "/api/v1/admin/organizations/{org_id}/seats",
            axum::routing::patch(app_access::set_seats),
        )
        .route("/api/v1/admin/organizations", get(orgs::list_all_orgs))
        .route("/api/v1/admin/users", get(orgs::list_all_users))
        .route(
            "/api/v1/admin/users/{user_id}/devices",
            get(device::admin_list),
        )
        .route(
            "/api/v1/admin/users/{user_id}/devices/{device_ref}",
            delete(device::admin_revoke),
        )
        .route("/api/v1/admin/maintenance", get(maintenance::get_status))
        .route("/api/v1/admin/health", get(health::get_health))
        .route(
            "/api/v1/organizations/{org_id}/audit",
            get(audit::list_org_audit),
        )
        .route(
            "/api/v1/organizations/{org_id}/audit/export",
            get(audit::export_org_audit),
        )
        .route("/api/v1/admin/audit", get(audit::list_instance_audit))
        .route(
            "/api/v1/admin/audit/export",
            get(audit::export_instance_audit),
        )
        .route(
            "/api/v1/admin/users/{user_id}/disable",
            post(orgs::disable_user),
        )
        .route(
            "/api/v1/admin/users/{user_id}/enable",
            post(orgs::enable_user),
        )
        .route("/api/v1/auth/login", post(auth::login))
        .route(
            "/api/v1/auth/login/second-factor",
            post(two_factor::browser_login),
        )
        .route("/api/v1/auth/2fa", get(two_factor::status))
        .route("/api/v1/auth/2fa/setup", post(two_factor::setup))
        .route("/api/v1/auth/2fa/confirm", post(two_factor::confirm))
        .route("/api/v1/auth/2fa/disable", post(two_factor::disable))
        .route(
            "/api/v1/auth/2fa/recovery-codes",
            post(two_factor::regenerate),
        )
        .route("/api/v1/auth/device/login", post(device::login))
        .route(
            "/api/v1/auth/device/login/second-factor",
            post(two_factor::device_login),
        )
        .route("/api/v1/auth/device/refresh", post(device::refresh))
        .route("/api/v1/auth/device/logout", post(device::logout))
        .route("/api/v1/auth/devices", get(device::list))
        .route("/api/v1/auth/devices/{id}", delete(device::revoke))
        .route("/api/v1/auth/session", get(auth::session))
        .route(
            "/api/v1/auth/email-verification",
            post(email::request_verification),
        )
        .route(
            "/api/v1/auth/email-verification/confirm",
            post(email::confirm_verification),
        )
        .route("/api/v1/auth/email-change", post(email::change_email))
        .route(
            "/api/v1/auth/email-change/confirm",
            post(email::confirm_change),
        )
        .route("/api/v1/auth/profile", patch(auth::update_profile))
        .route("/api/v1/auth/sessions", get(auth::list_sessions))
        .route(
            "/api/v1/auth/sessions/{session_id}",
            delete(auth::revoke_session),
        )
        .route("/api/v1/auth/logout", post(auth::logout))
        .route("/api/v1/auth/logout-all", post(auth::logout_all))
        .route("/api/v1/auth/password/change", post(auth::change_password))
        .route("/api/v1/auth/password-reset", post(auth::reset_password))
        .route(
            "/api/v1/admin/users/{user_id}/password-reset",
            post(auth::issue_password_reset).delete(auth::void_password_reset),
        )
        .route(
            "/api/v1/admin/users/email-verification",
            post(email::request_all_verification),
        )
        .route("/api/v1/admin/email-delivery", get(email::status))
        .route("/api/v1/admin/email-delivery/test", post(email::test_email))
        .route("/api/v1/admin/setup", get(setup_wizard::status))
        .route(
            "/api/v1/admin/setup/steps/{step}",
            patch(setup_wizard::update_step),
        )
        .route(
            "/api/v1/admin/setup/system-check",
            get(setup_wizard::system_check),
        )
        .route(
            "/api/v1/admin/instance-settings",
            get(setup_wizard::get_settings).patch(setup_wizard::update_settings),
        )
        .route(
            "/api/v1/admin/instance-settings/email-test",
            post(setup_wizard::test_email),
        )
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state.clone());
    // Upload content streams a large body: no global body limit and no
    // handler timeout (the handler caps the body at the declared size and
    // times out idle reads itself). Everything else is shared.
    let streaming = Router::new()
        .route(
            "/api/v1/uploads/{upload_id}/content",
            put(uploads::put_content),
        )
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state);
    with_middleware(routes, config, false, audit_services.clone()).merge(with_middleware(
        streaming,
        config,
        true,
        audit_services,
    ))
}

/// Shared handler state. Opaque: only used as the state type of
/// [`AuthenticatedSession`].
#[derive(Clone)]
pub struct ApiState {
    pub(crate) capabilities: Capabilities,
    pub(crate) services: Option<Arc<Services>>,
}

/// Applies the centralized middleware stack. The last layer added is the
/// outermost one, so the order below reads from the handler outwards.
fn with_middleware(
    router: Router,
    config: &ApiConfig,
    streaming: bool,
    audit_services: Option<Arc<Services>>,
) -> Router {
    let scim_audit_services = audit_services.clone();
    // Innermost: stores the policy denials (403) that handlers marked.
    let router = router.layer(middleware::from_fn_with_state(
        audit_services.clone(),
        audit::record_denials,
    ));
    let router = router.layer(middleware::from_fn_with_state(
        audit_services,
        reject_ambiguous_credentials,
    ));
    let router = if streaming {
        router
    } else {
        router
            .layer(RequestBodyLimitLayer::new(config.max_body_bytes))
            // Maps limit and timeout failures to the JSON error shape.
            .layer(middleware::from_fn_with_state(
                config.request_timeout,
                limits_to_json_errors,
            ))
    };
    // API answers are never cacheable; outermost so it also covers errors.
    with_request_context(
        router.layer(middleware::from_fn_with_state(
            scim_audit_services,
            scim::normalize_errors,
        )),
        config,
    )
    .layer(SetResponseHeaderLayer::overriding(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    ))
}

async fn reject_ambiguous_credentials(
    State(services): State<Option<Arc<Services>>>,
    request: Request,
    next: Next,
) -> Response {
    let settings =
        services.map_or_else(AuthSettings::default, |services| services.settings.clone());
    // SCIM has its own credential boundary and rejects every presented cookie.
    if request.uri().path().starts_with("/scim/v2/") {
        return next.run(request).await;
    }
    if session::ambiguous_credentials(request.headers(), &settings) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "AMBIGUOUS_CREDENTIALS",
            "Send either a session cookie or a bearer token, not both.",
            request.headers(),
        );
    }
    next.run(request).await
}

/// Request id, tracing and client-address resolution, without any
/// cache policy. The API wraps its routes with this plus `no-store`; the
/// server uses it directly for the static admin UI, which sets its own
/// `Cache-Control` per file.
pub fn with_request_context(router: Router, config: &ApiConfig) -> Router {
    router
        .layer(middleware::from_fn_with_state(
            Arc::new(config.trusted_proxies.clone()),
            client_ip::resolve_client_ip,
        ))
        // Request span, metrics and the request log line. Only the route
        // template, method, status, latency and identifiers are logged; never
        // the path, query string, headers, cookies or bodies (see `observe`).
        .layer(middleware::from_fn_with_state(
            Arc::clone(&config.metrics),
            observe::observe,
        ))
        .layer(PropagateRequestIdLayer::new(HeaderName::from_static(
            REQUEST_ID_HEADER,
        )))
        .layer(SetRequestIdLayer::new(
            HeaderName::from_static(REQUEST_ID_HEADER),
            UuidRequestId,
        ))
        .layer(middleware::from_fn(discard_untrusted_request_id))
}

/// Enforces the handler timeout and turns bare 413 responses (from the body
/// limit layer or from body extractors) into the JSON error shape. Runs after
/// the request id has been set, so the error carries it.
async fn limits_to_json_errors(
    State(timeout): State<Duration>,
    request: Request,
    next: Next,
) -> Response {
    let headers = request.headers().clone();
    let Ok(response) = tokio::time::timeout(timeout, next.run(request)).await else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "TIMEOUT",
            "The server did not produce a response in time.",
            &headers,
        );
    };
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"application/json"));
    if response.status() == StatusCode::PAYLOAD_TOO_LARGE && !is_json {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "PAYLOAD_TOO_LARGE",
            "The request body exceeds the maximum allowed size.",
            &headers,
        );
    }
    response
}

/// Generates a random UUID v4 request id.
#[derive(Clone, Copy)]
struct UuidRequestId;

impl MakeRequestId for UuidRequestId {
    fn make_request_id<B>(&mut self, _request: &Request<B>) -> Option<RequestId> {
        HeaderValue::from_str(&Uuid::new_v4().to_string())
            .ok()
            .map(RequestId::new)
    }
}

fn is_sane_request_id(value: &HeaderValue) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= REQUEST_ID_MAX_LEN
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// `SetRequestIdLayer` keeps any incoming id. Remove client-supplied ids that
/// are empty, too long, contain other characters, or occur more than once, so
/// that a fresh one is generated instead.
async fn discard_untrusted_request_id(mut request: Request, next: Next) -> Response {
    let mut values = request.headers().get_all(REQUEST_ID_HEADER).iter();
    let acceptable = match (values.next(), values.next()) {
        (None, _) => true,
        (Some(first), None) => is_sane_request_id(first),
        (Some(_), Some(_)) => false,
    };
    if !acceptable {
        request.headers_mut().remove(REQUEST_ID_HEADER);
    }
    next.run(request).await
}

async fn health_live() -> Json<serde_json::Value> {
    Json(json!({ "status": "live" }))
}

async fn not_found(headers: HeaderMap) -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
        "The requested resource does not exist.",
        &headers,
    )
}

async fn method_not_allowed(headers: HeaderMap) -> Response {
    error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "METHOD_NOT_ALLOWED",
        "The HTTP method is not allowed for this resource.",
        &headers,
    )
}

/// Builds the error body described in `docs/02-architecture/api-conventions.md`.
pub(crate) fn error_response(
    status: StatusCode,
    code: &str,
    message: &str,
    request_headers: &HeaderMap,
) -> Response {
    error_response_with_details(status, code, message, request_headers, None)
}

/// Like [`error_response`] with an optional `details` object. Details must
/// never contain request values.
pub(crate) fn error_response_with_details(
    status: StatusCode,
    code: &str,
    message: &str,
    request_headers: &HeaderMap,
    details: Option<serde_json::Value>,
) -> Response {
    let request_id = request_headers
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok());
    let mut error = json!({ "code": code, "message": message, "request_id": request_id });
    if let (Some(details), Some(object)) = (details, error.as_object_mut()) {
        object.insert("details".to_owned(), details);
    }
    (status, Json(json!({ "error": error }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, body::Bytes, routing::post};
    use tower::ServiceExt;

    async fn send(app: Router, request: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        (status, headers, body.to_vec())
    }

    fn get_req(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    fn json_body(bytes: &[u8]) -> serde_json::Value {
        serde_json::from_slice(bytes).unwrap()
    }

    #[tokio::test]
    async fn health_live_returns_minimal_status() {
        let (status, headers, body) = send(router(), get_req("/health/live")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json_body(&body), json!({"status": "live"}));
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    }

    #[tokio::test]
    async fn info_matches_contract_fields() {
        let (status, _, body) = send(router(), get_req("/api/v1/info")).await;
        assert_eq!(status, StatusCode::OK);
        let value = json_body(&body);
        // openapi.yaml: required [api_version, features, server_mode]
        assert_eq!(value["api_version"], "1.0");
        assert!(value["features"].is_array());
        assert_eq!(value["server_mode"], "development-prototype");
        let parsed: Capabilities = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, Capabilities::development_defaults());
    }

    #[tokio::test]
    async fn responses_are_no_store() {
        for uri in ["/health/live", "/api/v1/info", "/nope"] {
            let (_, headers, _) = send(router(), get_req(uri)).await;
            assert_eq!(headers[header::CACHE_CONTROL], "no-store", "{uri}");
        }
    }

    #[tokio::test]
    async fn generates_request_id_when_absent() {
        let (_, headers, _) = send(router(), get_req("/health/live")).await;
        let id = headers[REQUEST_ID_HEADER].to_str().unwrap();
        assert!(Uuid::parse_str(id).is_ok(), "{id}");
    }

    #[tokio::test]
    async fn keeps_sane_incoming_request_id() {
        let request = Request::builder()
            .uri("/health/live")
            .header(REQUEST_ID_HEADER, "client-id_1.2")
            .body(Body::empty())
            .unwrap();
        let (_, headers, _) = send(router(), request).await;
        assert_eq!(headers[REQUEST_ID_HEADER], "client-id_1.2");
    }

    #[tokio::test]
    async fn replaces_untrusted_incoming_request_ids() {
        let too_long = "a".repeat(REQUEST_ID_MAX_LEN + 1);
        for bad in [too_long.as_str(), "has space", "semi;colon", "<script>", ""] {
            let request = Request::builder()
                .uri("/health/live")
                .header(REQUEST_ID_HEADER, bad)
                .body(Body::empty())
                .unwrap();
            let (_, headers, _) = send(router(), request).await;
            let id = headers[REQUEST_ID_HEADER].to_str().unwrap();
            assert_ne!(id, bad);
            assert!(Uuid::parse_str(id).is_ok(), "{bad:?} -> {id}");
        }
    }

    #[tokio::test]
    async fn replaces_non_utf8_and_duplicate_request_ids() {
        let request = Request::builder()
            .uri("/health/live")
            .header(
                REQUEST_ID_HEADER,
                HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
            )
            .body(Body::empty())
            .unwrap();
        let (_, headers, _) = send(router(), request).await;
        assert!(Uuid::parse_str(headers[REQUEST_ID_HEADER].to_str().unwrap()).is_ok());

        let request = Request::builder()
            .uri("/health/live")
            .header(REQUEST_ID_HEADER, "one")
            .header(REQUEST_ID_HEADER, "two")
            .body(Body::empty())
            .unwrap();
        let (_, headers, _) = send(router(), request).await;
        assert!(Uuid::parse_str(headers[REQUEST_ID_HEADER].to_str().unwrap()).is_ok());
    }

    #[tokio::test]
    async fn unknown_route_returns_404_error_shape() {
        let (status, headers, body) = send(router(), get_req("/nope")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let value = json_body(&body);
        assert_eq!(value["error"]["code"], "NOT_FOUND");
        assert!(value["error"]["message"].is_string());
        assert_eq!(
            value["error"]["request_id"].as_str(),
            headers[REQUEST_ID_HEADER].to_str().ok()
        );
    }

    #[tokio::test]
    async fn wrong_method_returns_405_error_shape() {
        for uri in ["/health/live", "/api/v1/info"] {
            let request = Request::builder()
                .method("POST")
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            let (status, headers, body) = send(router(), request).await;
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{uri}");
            let value = json_body(&body);
            assert_eq!(value["error"]["code"], "METHOD_NOT_ALLOWED");
            assert_eq!(
                value["error"]["request_id"].as_str(),
                headers[REQUEST_ID_HEADER].to_str().ok()
            );
            assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        }
    }

    #[tokio::test]
    async fn post_with_oversized_body_to_existing_route_is_405() {
        // The skeleton has no POST route, so the body limit cannot be observed
        // on a real route; the method check answers first.
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/info")
            .body(Body::from(vec![0u8; 128 * 1024]))
            .unwrap();
        let (status, _, _) = send(router(), request).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    fn test_app(config: &ApiConfig) -> Router {
        let routes = Router::new()
            .route(
                "/echo-len",
                post(|body: Bytes| async move { body.len().to_string() }),
            )
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    "late"
                }),
            );
        with_middleware(routes, config, false, None)
    }

    #[tokio::test]
    async fn body_limit_rejects_oversized_bodies_with_413() {
        let config = ApiConfig {
            max_body_bytes: 16,
            ..ApiConfig::default()
        };
        let small = Request::builder()
            .method("POST")
            .uri("/echo-len")
            .body(Body::from(vec![0u8; 16]))
            .unwrap();
        let (status, _, body) = send(test_app(&config), small).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"16");

        let big = Request::builder()
            .method("POST")
            .uri("/echo-len")
            .body(Body::from(vec![0u8; 17]))
            .unwrap();
        let (status, headers, body) = send(test_app(&config), big).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        let value = json_body(&body);
        assert_eq!(value["error"]["code"], "PAYLOAD_TOO_LARGE");
        assert_eq!(
            value["error"]["request_id"].as_str(),
            headers[REQUEST_ID_HEADER].to_str().ok()
        );
    }

    fn ip_app(trusted: &[&str]) -> Router {
        let config = ApiConfig {
            trusted_proxies: trusted.iter().map(|s| s.parse().unwrap()).collect(),
            ..ApiConfig::default()
        };
        let routes = Router::new().route(
            "/ip",
            get(|request: Request| async move {
                request
                    .extensions()
                    .get::<ClientIp>()
                    .map_or_else(|| "none".to_owned(), |ip| ip.0.to_string())
            }),
        );
        with_middleware(routes, &config, false, None)
    }

    async fn client_ip_for(app: Router, peer: Option<&str>, xff: Option<&str>) -> String {
        let mut builder = Request::builder().uri("/ip");
        if let Some(xff) = xff {
            builder = builder.header("x-forwarded-for", xff);
        }
        let mut request = builder.body(Body::empty()).unwrap();
        if let Some(peer) = peer {
            let addr: std::net::SocketAddr = peer.parse().unwrap();
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(addr));
        }
        let (status, _, body) = send(app, request).await;
        assert_eq!(status, StatusCode::OK);
        String::from_utf8(body).unwrap()
    }

    #[tokio::test]
    async fn client_ip_ignores_spoofed_header_from_untrusted_peer() {
        let ip = client_ip_for(
            ip_app(&["10.0.0.0/8"]),
            Some("203.0.113.9:5555"),
            Some("1.2.3.4"),
        )
        .await;
        assert_eq!(ip, "203.0.113.9");
        let ip = client_ip_for(ip_app(&[]), Some("10.0.0.1:1"), Some("1.2.3.4")).await;
        assert_eq!(ip, "10.0.0.1");
    }

    #[tokio::test]
    async fn client_ip_follows_trusted_chain() {
        let app = ip_app(&["10.0.0.0/8"]);
        let ip = client_ip_for(
            app,
            Some("10.0.0.1:1"),
            Some("6.6.6.6, 198.51.100.7, 10.0.0.5"),
        )
        .await;
        assert_eq!(ip, "198.51.100.7");
        let ip = client_ip_for(
            ip_app(&["10.0.0.0/8"]),
            Some("10.0.0.1:1"),
            Some("10.0.0.9, 10.0.0.8"),
        )
        .await;
        assert_eq!(ip, "10.0.0.9");
    }

    #[tokio::test]
    async fn client_ip_falls_back_on_garbage_and_supports_ipv6() {
        let ip = client_ip_for(ip_app(&["10.0.0.0/8"]), Some("10.0.0.1:1"), Some("garbage")).await;
        assert_eq!(ip, "10.0.0.1");
        let ip = client_ip_for(
            ip_app(&["fd00::/8"]),
            Some("[fd00::1]:1"),
            Some("2001:db8::5"),
        )
        .await;
        assert_eq!(ip, "2001:db8::5");
    }

    #[tokio::test]
    async fn client_ip_absent_without_connect_info() {
        let ip = client_ip_for(ip_app(&[]), None, Some("1.2.3.4")).await;
        assert_eq!(ip, "none");
        // Public routes still work without connection info.
        let (status, _, _) = send(router(), get_req("/health/live")).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn default_body_limit_is_64_kib() {
        assert_eq!(ApiConfig::default().max_body_bytes, 64 * 1024);
    }

    #[tokio::test]
    async fn slow_handler_times_out_with_503_json() {
        let config = ApiConfig {
            request_timeout: Duration::from_millis(20),
            ..ApiConfig::default()
        };
        let (status, headers, body) = send(test_app(&config), get_req("/slow")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        let value = json_body(&body);
        assert_eq!(value["error"]["code"], "TIMEOUT");
        assert_eq!(
            value["error"]["request_id"].as_str(),
            headers[REQUEST_ID_HEADER].to_str().ok()
        );
    }
}
