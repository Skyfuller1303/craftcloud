//! Error responses shared by the route modules.

use axum::{
    extract::rejection::JsonRejection,
    http::{HeaderMap, StatusCode},
    response::Response,
};
use serde_json::json;

use crate::{error_response, error_response_with_details};

pub(crate) fn unavailable(headers: &HeaderMap, code: &str) -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        code,
        "The service is not available because no database is configured or reachable.",
        headers,
    )
}

pub(crate) fn internal(headers: &HeaderMap) -> Response {
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL",
        "An internal error occurred.",
        headers,
    )
}

pub(crate) fn validation_failed(fields: &[&str], headers: &HeaderMap) -> Response {
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
pub(crate) fn rejection_response(rejection: &JsonRejection, headers: &HeaderMap) -> Response {
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
