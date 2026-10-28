//! CraftCloud HTTP API: router, handlers and centralized middleware.
//!
//! This crate only builds an [`axum::Router`]; binding sockets, configuration
//! from the environment and process lifecycle belong to the server binary.
//!
//! Scope of the PR-001 skeleton: `GET /health/live` and `GET /api/v1/info`.
//! There is no authentication, database, storage, session or upload code.

use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use craftcloud_core::Capabilities;
use serde_json::json;
use tower_http::{
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer},
    set_header::SetResponseHeaderLayer,
    timeout::TimeoutLayer,
    trace::TraceLayer,
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
    /// Maximum time to produce a response before answering 408.
    pub request_timeout: Duration,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: 64 * 1024,
            request_timeout: Duration::from_secs(10),
        }
    }
}

/// Builds the router with development-default capabilities and default limits.
pub fn router() -> Router {
    router_with(Capabilities::development_defaults(), &ApiConfig::default())
}

/// Builds the router with explicit capabilities and limits.
pub fn router_with(capabilities: Capabilities, config: &ApiConfig) -> Router {
    let routes = Router::new()
        .route("/health/live", get(health_live))
        .route("/api/v1/info", get(info))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(capabilities);
    with_middleware(routes, config)
}

/// Applies the centralized middleware stack. The last layer added is the
/// outermost one, so the order below reads from the handler outwards.
fn with_middleware(router: Router, config: &ApiConfig) -> Router {
    router
        .layer(RequestBodyLimitLayer::new(config.max_body_bytes))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            config.request_timeout,
        ))
        // Only method, path (no query string) and request id are logged;
        // never headers, cookies or authorization data.
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request<_>| {
                let request_id = request
                    .headers()
                    .get(REQUEST_ID_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-");
                tracing::info_span!(
                    "request",
                    method = %request.method(),
                    path = %request.uri().path(),
                    request_id = %request_id,
                )
            }),
        )
        .layer(PropagateRequestIdLayer::new(HeaderName::from_static(
            REQUEST_ID_HEADER,
        )))
        .layer(SetRequestIdLayer::new(
            HeaderName::from_static(REQUEST_ID_HEADER),
            UuidRequestId,
        ))
        .layer(middleware::from_fn(discard_untrusted_request_id))
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
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

async fn info(State(capabilities): State<Capabilities>) -> Json<Capabilities> {
    Json(capabilities)
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
fn error_response(
    status: StatusCode,
    code: &str,
    message: &str,
    request_headers: &HeaderMap,
) -> Response {
    let request_id = request_headers
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok());
    let body = json!({
        "error": { "code": code, "message": message, "request_id": request_id }
    });
    (status, Json(body)).into_response()
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
        with_middleware(routes, config)
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
        let (status, headers, _) = send(test_app(&config), big).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn default_body_limit_is_64_kib() {
        assert_eq!(ApiConfig::default().max_body_bytes, 64 * 1024);
    }

    #[tokio::test]
    async fn slow_handler_times_out_with_408() {
        let config = ApiConfig {
            request_timeout: Duration::from_millis(20),
            ..ApiConfig::default()
        };
        let (status, _, _) = send(test_app(&config), get_req("/slow")).await;
        assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
    }
}
