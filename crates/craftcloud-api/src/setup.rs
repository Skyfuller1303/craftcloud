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

use std::sync::Arc;

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use craftcloud_auth::{BootstrapToken, HashError, hash_password, validate_password};
use craftcloud_core::{DisplayName, NormalizedEmail, Slug};
use craftcloud_db::{DbError, PgPool, setup as db_setup};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use zeroize::Zeroize;

use crate::{ApiState, error_response, error_response_with_details};

/// Maximum number of concurrent password hashes (each uses about 19 MiB).
const MAX_CONCURRENT_HASHES: usize = 4;

/// Database-backed services for the setup routes.
pub struct SetupService {
    pool: PgPool,
    bootstrap: Option<BootstrapToken>,
    hash_permits: Arc<Semaphore>,
}

impl SetupService {
    /// Creates the service. `bootstrap` is `None` when no bootstrap secret
    /// file is configured; setup then answers 403 `INVALID_BOOTSTRAP_TOKEN`
    /// exactly like a wrong token (the reason is only logged).
    #[must_use]
    pub fn new(pool: PgPool, bootstrap: Option<BootstrapToken>) -> Self {
        Self {
            pool,
            bootstrap,
            hash_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_HASHES)),
        }
    }
}

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

fn unavailable(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "SETUP_UNAVAILABLE",
        "Instance setup is not available because no database is configured or reachable.",
        headers,
    )
}

fn internal(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL",
        "An internal error occurred.",
        headers,
    )
}

fn db_failure(error: &DbError, headers: &HeaderMap) -> Response {
    // DbError text is generic and free of request data.
    tracing::error!(%error, "setup database operation failed");
    match error {
        DbError::Connection(_) => unavailable(headers),
        _ => internal(headers),
    }
}

pub(crate) async fn status(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    let Some(service) = &state.setup else {
        return unavailable(&headers);
    };
    match db_setup::is_completed(&service.pool).await {
        Ok(completed) => Json(json!({ "setup_required": !completed })).into_response(),
        Err(error) => db_failure(&error, &headers),
    }
}

pub(crate) async fn complete(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Result<Json<SetupRequest>, JsonRejection>,
) -> Response {
    let Some(service) = state.setup.clone() else {
        return unavailable(&headers);
    };
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
    let hashed = run_hash_job(service.hash_permits.clone(), move || {
        let hash = hash_password(&password_copy);
        password_copy.zeroize();
        hash
    })
    .await;
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

fn validation_failed(fields: &[&str], headers: &HeaderMap) -> Response {
    error_response_with_details(
        StatusCode::UNPROCESSABLE_ENTITY,
        "VALIDATION_ERROR",
        "One or more fields are invalid.",
        headers,
        Some(json!({ "fields": fields })),
    )
}

/// Maps a body rejection to an error without echoing parser messages, which
/// may quote request values.
fn rejection_response(rejection: &JsonRejection, headers: &HeaderMap) -> Response {
    match rejection.status() {
        StatusCode::PAYLOAD_TOO_LARGE => error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "PAYLOAD_TOO_LARGE",
            "The request body exceeds the maximum allowed size.",
            headers,
        ),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "UNSUPPORTED_MEDIA_TYPE",
            "The request must have Content-Type application/json.",
            headers,
        ),
        _ => error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "VALIDATION_ERROR",
            "The request body is not valid.",
            headers,
        ),
    }
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

/// Runs a hashing job on the blocking pool under a concurrency permit.
///
/// The owned permit moves into the blocking closure, so it is released only
/// when the CPU- and memory-heavy job has really finished. If the request
/// future is dropped (for example by the handler timeout) the job keeps
/// running and keeps counting against the limit.
async fn run_hash_job<F>(permits: Arc<Semaphore>, job: F) -> Option<String>
where
    F: FnOnce() -> Result<String, HashError> + Send + 'static,
{
    let permit: OwnedSemaphorePermit = permits.acquire_owned().await.ok()?;
    let joined = tokio::task::spawn_blocking(move || {
        let result = job();
        drop(permit);
        result
    })
    .await;
    joined.ok()?.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn permit_is_held_until_the_blocking_job_ends_even_if_the_caller_is_dropped() {
        let permits = Arc::new(Semaphore::new(1));
        let slow = || {
            std::thread::sleep(Duration::from_millis(400));
            Ok("hash".to_owned())
        };
        // The caller gives up (like the handler timeout does) while the job runs.
        let outcome = tokio::time::timeout(
            Duration::from_millis(50),
            run_hash_job(permits.clone(), slow),
        )
        .await;
        assert!(
            outcome.is_err(),
            "the request future must have been dropped"
        );
        assert_eq!(
            permits.available_permits(),
            0,
            "permit leaked with the dropped future"
        );

        // Released only after the blocking job completed.
        let mut released = false;
        for _ in 0..100 {
            if permits.available_permits() == 1 {
                released = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(released, "permit was never released");
    }

    #[tokio::test]
    async fn permit_is_released_after_failure_and_success() {
        let permits = Arc::new(Semaphore::new(1));
        assert_eq!(run_hash_job(permits.clone(), || Err(HashError)).await, None);
        assert_eq!(permits.available_permits(), 1);
        let ok = run_hash_job(permits.clone(), || Ok("h".to_owned())).await;
        assert_eq!(ok.as_deref(), Some("h"));
        assert_eq!(permits.available_permits(), 1);
    }
}
