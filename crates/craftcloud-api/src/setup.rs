//! Instance setup routes (CC-F01): `GET /api/v1/setup/status` and
//! `POST /api/v1/setup/complete`.
//!
//! Decision order of `complete` (no state is revealed to a caller without a
//! valid bootstrap token):
//!
//! 1. no database configured: 503 `SETUP_UNAVAILABLE`
//! 2. unparsable body, unknown field or missing field: 422 `VALIDATION_ERROR`
//!    without `details` (415/413 for wrong media type / oversized body); this
//!    happens before the token check and does not depend on any state
//! 3. wrong token **or no bootstrap secret configured**: 403
//!    `INVALID_BOOTSTRAP_TOKEN`, identical in both cases (the real reason is
//!    only logged), regardless of setup state
//! 4. setup already completed: 409 `SETUP_ALREADY_COMPLETED` (valid token only)
//! 5. semantic field validation: 422 `VALIDATION_ERROR` with `details.fields`
//!    (field names only)
//! 6. password hashing (blocking pool, bounded concurrency), then the single
//!    setup transaction: 201, or 409 if a concurrent attempt won.
//!
//! Neither the token, the password nor any request value is logged or
//! returned in errors. The parsed request struct and the hashing copy of the
//! password are zeroized; the raw request body buffer and serde internals are
//! not. There is deliberately no rate limiting here: tokens
//! have at least 32 characters, so guessing is infeasible; a generic limiter
//! arrives with the session work (PR-005).

use axum::{
    Extension, Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use craftcloud_auth::{hash_password, validate_password};
use craftcloud_core::{DisplayName, NormalizedEmail, Slug};
use craftcloud_db::{DbError, setup as db_setup};
use serde::Deserialize;
use serde_json::json;
use zeroize::Zeroize;

use crate::responses::{internal, rejection_response, unavailable, validation_failed};
use crate::services::IpClass;
use crate::{ApiState, ClientIp, error_response};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetupRequest {
    bootstrap_token: String,
    email: String,
    display_name: String,
    password: String,
    organization: OrganizationRequest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OrganizationRequest {
    name: String,
    slug: String,
}

impl Drop for SetupRequest {
    fn drop(&mut self) {
        self.bootstrap_token.zeroize();
        self.password.zeroize();
    }
}

fn db_failure(error: &DbError, headers: &HeaderMap) -> Response {
    // DbError text is generic and free of request data.
    tracing::error!(%error, "setup database operation failed");
    match error {
        DbError::Connection(_) => unavailable(headers, "SETUP_UNAVAILABLE"),
        _ => internal(headers),
    }
}

pub(crate) async fn status(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    let Some(service) = &state.services else {
        return unavailable(&headers, "SETUP_UNAVAILABLE");
    };
    match db_setup::is_completed(&service.pool).await {
        Ok(completed) => Json(json!({ "setup_required": !completed })).into_response(),
        Err(error) => db_failure(&error, &headers),
    }
}

pub(crate) async fn complete(
    State(state): State<ApiState>,
    client_ip: Option<Extension<ClientIp>>,
    headers: HeaderMap,
    body: Result<Json<SetupRequest>, JsonRejection>,
) -> Response {
    let Some(service) = state.services.clone() else {
        return unavailable(&headers, "SETUP_UNAVAILABLE");
    };
    // Rate limit before any body handling or hashing.
    if let Err(response) = service.rate_limit_ip(
        IpClass::Setup,
        client_ip.map(|Extension(ip)| ip.0),
        &headers,
    ) {
        return *response;
    }
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => return rejection_response(&rejection, &headers),
    };
    let token_valid = match &service.bootstrap {
        Some(expected) => expected.verify(&request.bootstrap_token),
        None => {
            // Anonymous callers must not learn that no secret is configured;
            // the operator can read the real reason in the log.
            tracing::warn!("setup requested but no bootstrap secret is configured");
            false
        }
    };
    if !token_valid {
        return error_response(
            StatusCode::FORBIDDEN,
            "INVALID_BOOTSTRAP_TOKEN",
            "The bootstrap token is invalid.",
            &headers,
        );
    }

    match db_setup::is_completed(&service.pool).await {
        Ok(true) => return already_completed(&headers),
        Ok(false) => {}
        Err(error) => return db_failure(&error, &headers),
    }

    let fields = match validate(&request) {
        Ok(valid) => valid,
        Err(fields) => return validation_failed(&fields, &headers),
    };

    let mut password_copy = request.password.clone();
    let hashed = service
        .run_blocking(move || {
            let hash = hash_password(&password_copy);
            password_copy.zeroize();
            hash
        })
        .await
        .and_then(Result::ok);
    let Some(password_hash) = hashed else {
        tracing::error!("password hashing failed");
        return internal(&headers);
    };

    let new = db_setup::NewInstance {
        operator_email: fields.email,
        operator_display_name: fields.display_name,
        operator_password_hash: password_hash,
        organization_name: fields.organization_name,
        organization_slug: fields.organization_slug,
    };
    match db_setup::complete(&service.pool, &new).await {
        Ok(created) => {
            tracing::info!("instance setup completed");
            (
                StatusCode::CREATED,
                Json(json!({
                    "user_id": created.user_id.to_string(),
                    "organization_id": created.organization_id.to_string(),
                })),
            )
                .into_response()
        }
        Err(db_setup::SetupError::AlreadyCompleted) => already_completed(&headers),
        Err(db_setup::SetupError::Db(error)) => match unique_field(&error) {
            Some(field) => validation_failed(&[field], &headers),
            None => db_failure(&error, &headers),
        },
    }
}

fn already_completed(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::CONFLICT,
        "SETUP_ALREADY_COMPLETED",
        "Instance setup has already been completed.",
        headers,
    )
}

/// Field of a unique violation that the caller can fix.
fn unique_field(error: &DbError) -> Option<&'static str> {
    if !matches!(error, DbError::UniqueViolation { .. }) {
        return None;
    }
    match error.constraint() {
        Some("organizations_slug_key") => Some("organization.slug"),
        Some("users_normalized_email_key") => Some("email"),
        _ => None,
    }
}

struct ValidFields {
    email: String,
    display_name: String,
    organization_name: String,
    organization_slug: String,
}

/// Validates every field and returns all invalid field names.
fn validate(request: &SetupRequest) -> Result<ValidFields, Vec<&'static str>> {
    let mut invalid = Vec::new();
    let email = NormalizedEmail::parse(&request.email);
    if email.is_err() {
        invalid.push("email");
    }
    let display_name = DisplayName::parse(&request.display_name);
    if display_name.is_err() {
        invalid.push("display_name");
    }
    let local = email.as_ref().map_or("", NormalizedEmail::local_part);
    if validate_password(&request.password, local).is_err() {
        invalid.push("password");
    }
    let organization_name = DisplayName::parse(&request.organization.name);
    if organization_name.is_err() {
        invalid.push("organization.name");
    }
    let organization_slug = Slug::parse(&request.organization.slug);
    if organization_slug.is_err() {
        invalid.push("organization.slug");
    }
    match (email, display_name, organization_name, organization_slug) {
        (Ok(email), Ok(display_name), Ok(organization_name), Ok(slug)) if invalid.is_empty() => {
            Ok(ValidFields {
                email: email.as_str().to_owned(),
                display_name: display_name.as_str().to_owned(),
                organization_name: organization_name.as_str().to_owned(),
                organization_slug: slug.as_str().to_owned(),
            })
        }
        _ => Err(invalid),
    }
}
