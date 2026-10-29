//! Contract tests (CC-P01-09): `contracts/openapi.yaml`, the routes the router
//! really serves, the committed snapshot and the common error shape. These
//! tests need no database: handlers that would query PostgreSQL are only
//! probed up to the point where they answer without it.
//!
//! Regenerate the snapshot after an intentional contract change with
//! `cargo xtask openapi-snapshot --update`.

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use craftcloud_api::{
        ApiConfig, AuthSettings, ROUTES, Services, router_with, router_with_services,
    };
    use craftcloud_core::Capabilities;
    use serde_json::Value;
    use sqlx::postgres::PgPoolOptions;
    use tower::ServiceExt;

    const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];
    const ORIGIN: &str = "https://cloud.test";

    fn repo_file(relative: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(relative)
    }

    fn openapi() -> Value {
        let text = std::fs::read_to_string(repo_file("contracts/openapi.yaml"))
            .expect("contracts/openapi.yaml must be readable");
        serde_yaml_ng::from_str(&text).expect("contracts/openapi.yaml must be valid YAML")
    }

    /// `(METHOD, full path)` of every documented operation. Paths are relative to
    /// the server URL `/api/v1`, except operations that override `servers`
    /// (the health endpoint lives at the server root).
    fn documented_operations(doc: &Value) -> BTreeSet<(String, String)> {
        let mut found = BTreeSet::new();
        let paths = doc["paths"].as_object().expect("paths object");
        for (path, item) in paths {
            for method in METHODS {
                let Some(op) = item.get(method) else { continue };
                let full = if op.get("servers").is_some() {
                    path.clone()
                } else {
                    format!("/api/v1{path}")
                };
                found.insert((method.to_uppercase(), full));
            }
        }
        found
    }

    fn served_operations() -> BTreeSet<(String, String)> {
        ROUTES
            .iter()
            .flat_map(|(path, methods)| {
                methods
                    .iter()
                    .map(move |m| ((*m).to_owned(), (*path).to_owned()))
            })
            .collect()
    }

    fn lazy_services() -> Services {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
            .expect("lazy pool");
        let settings = AuthSettings {
            public_url: Some(ORIGIN.to_owned()),
            public_origin: Some(ORIGIN.to_owned()),
            cookie_secure: true,
            ..AuthSettings::default()
        };
        Services::new(pool, None, settings)
    }

    struct Reply {
        status: StatusCode,
        headers: axum::http::HeaderMap,
        body: Value,
        raw: String,
    }

    async fn send(app: &Router, request: Request<Body>) -> Reply {
        let response = app.clone().oneshot(request).await.expect("infallible");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        Reply {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            raw: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    fn request(method: &str, uri: &str) -> axum::http::request::Builder {
        Request::builder().method(method).uri(uri)
    }

    // ---- OpenAPI <-> router --------------------------------------------------------

    #[test]
    fn openapi_and_route_table_describe_the_same_operations() {
        let documented = documented_operations(&openapi());
        let served = served_operations();
        let undocumented: Vec<_> = served.difference(&documented).collect();
        let unserved: Vec<_> = documented.difference(&served).collect();
        assert!(
            undocumented.is_empty() && unserved.is_empty(),
            "contract drift.\nserved but not documented: {undocumented:?}\ndocumented but not served: {unserved:?}"
        );
    }

    #[test]
    fn route_table_lists_every_route_registration_of_the_router_source() {
        // Guards the table itself: a new `.route(` in the router must add a row.
        let source = std::fs::read_to_string(repo_file("crates/craftcloud-api/src/lib.rs"))
            .expect("lib.rs readable");
        let start = source
            .find("let routes = Router::new()")
            .expect("router construction");
        let end = source[start..].find(".fallback(").expect("fallback") + start;
        let registrations = source[start..end]
            .lines()
            .filter(|line| line.trim_start().starts_with(".route("))
            .count();
        assert_eq!(
            registrations,
            ROUTES.len(),
            "ROUTES in lib.rs and the `.route(` registrations differ"
        );
        let distinct: BTreeSet<_> = ROUTES.iter().map(|(path, _)| *path).collect();
        assert_eq!(distinct.len(), ROUTES.len(), "duplicate path in ROUTES");
    }

    #[tokio::test]
    async fn the_router_serves_exactly_the_listed_methods_and_nothing_else() {
        let app = router_with(Capabilities::development_defaults(), &ApiConfig::default());
        let sample = "00000000-0000-4000-8000-000000000001";
        for (template, methods) in ROUTES {
            let mut path = (*template).to_owned();
            while let (Some(open), Some(close)) = (path.find('{'), path.find('}')) {
                path.replace_range(open..=close, sample);
            }
            for method in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
                let reply = send(
                    &app,
                    request(method, &path).body(Body::empty()).expect("request"),
                )
                .await;
                if methods.contains(&method) {
                    assert!(
                        reply.status != StatusCode::NOT_FOUND
                            && reply.status != StatusCode::METHOD_NOT_ALLOWED,
                        "{method} {template} is listed but answered {}",
                        reply.status
                    );
                } else {
                    assert_eq!(
                        (reply.status, reply.body["error"]["code"].as_str()),
                        (StatusCode::METHOD_NOT_ALLOWED, Some("METHOD_NOT_ALLOWED")),
                        "{method} {template} is not listed but is served"
                    );
                }
            }
        }
        // Undocumented paths do not exist.
        for path in [
            "/health/ready",
            "/metrics",
            "/api/v1/nope",
            "/api/v1/workspaces",
            "/api/v1/files/00000000-0000-4000-8000-000000000001",
            "/api/v2/info",
        ] {
            let reply = send(
                &app,
                request("GET", path).body(Body::empty()).expect("request"),
            )
            .await;
            assert_eq!(
                (reply.status, reply.body["error"]["code"].as_str()),
                (StatusCode::NOT_FOUND, Some("NOT_FOUND")),
                "{path}"
            );
        }
    }

    // ---- snapshot --------------------------------------------------------------------

    fn schema_name(node: &Value) -> Option<String> {
        node.get("$ref")?
            .as_str()?
            .rsplit('/')
            .next()
            .map(str::to_owned)
    }

    fn body_schema(content: &Value) -> Option<String> {
        schema_name(content.get("application/json")?.get("schema")?)
    }

    /// One line per operation: method, path, authentication, request and success
    /// body schema, response codes. Sorted, so contract changes show up as small
    /// diffs of `contracts/openapi.snapshot`.
    fn snapshot(doc: &Value) -> String {
        let mut lines = BTreeMap::new();
        let paths = doc["paths"].as_object().expect("paths");
        for (path, item) in paths {
            for method in METHODS {
                let Some(op) = item.get(method) else { continue };
                let full = if op.get("servers").is_some() {
                    path.clone()
                } else {
                    format!("/api/v1{path}")
                };
                let auth = match op.get("security").and_then(Value::as_array) {
                    None => "cookie",
                    Some(list) if list.is_empty() => "none",
                    Some(_) => "cookie+csrf",
                };
                let request_schema = op
                    .get("requestBody")
                    .and_then(|b| b.get("content"))
                    .and_then(body_schema)
                    .unwrap_or_else(|| "-".to_owned());
                let responses = op["responses"].as_object().expect("responses");
                let mut codes: Vec<&str> = responses.keys().map(String::as_str).collect();
                codes.sort_unstable();
                let ok_schema = responses
                    .iter()
                    .filter(|(code, _)| code.starts_with('2'))
                    .find_map(|(_, r)| r.get("content").and_then(body_schema))
                    .unwrap_or_else(|| "-".to_owned());
                lines.insert(
                    (full.clone(), method.to_uppercase()),
                    format!(
                        "{} {full} auth={auth} req={request_schema} ok={ok_schema} codes={}",
                        method.to_uppercase(),
                        codes.join(",")
                    ),
                );
            }
        }
        let mut out = String::from(
            "# Normalized view of contracts/openapi.yaml. Regenerate with `cargo xtask openapi-snapshot --update`.\n",
        );
        for line in lines.values() {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("# component schemas\n");
        let schemas = doc["components"]["schemas"].as_object().expect("schemas");
        for name in schemas.keys().collect::<BTreeSet<_>>() {
            out.push_str(name);
            out.push('\n');
        }
        out
    }

    #[test]
    fn openapi_matches_the_committed_snapshot() {
        let actual = snapshot(&openapi());
        let path = repo_file("contracts/openapi.snapshot");
        if std::env::var_os("UPDATE_OPENAPI_SNAPSHOT").is_some() {
            std::fs::write(&path, &actual).expect("snapshot writable");
            return;
        }
        let committed = std::fs::read_to_string(&path)
            .expect("contracts/openapi.snapshot is missing; run `cargo xtask openapi-snapshot --update`")
            .replace("\r\n", "\n");
        assert!(
            committed == actual,
            "contracts/openapi.yaml changed without the snapshot. Review the change, then run \
             `cargo xtask openapi-snapshot --update` and commit contracts/openapi.snapshot.\n\
             --- committed\n{committed}\n--- actual\n{actual}"
        );
    }

    // ---- error schema ----------------------------------------------------------------

    fn is_error_schema(content: &Value) -> bool {
        body_schema(content).as_deref() == Some("Error")
    }

    #[test]
    fn every_error_response_in_openapi_uses_the_common_error_schema() {
        let doc = openapi();
        let components = doc["components"]["responses"]
            .as_object()
            .expect("component responses");
        for (name, response) in components {
            let describes_error = response.get("content").is_some_and(is_error_schema);
            // Component responses are either errors (schema Error) or declared
            // here on purpose; none is allowed to have another error body.
            assert!(
                describes_error || response.get("content").is_none(),
                "components.responses.{name} must use schemas/Error"
            );
            assert!(
                response.get("content").is_some(),
                "components.responses.{name} has no body schema (all current component responses are errors)"
            );
        }
        let mut checked = 0;
        let paths = doc["paths"].as_object().expect("paths");
        for (path, item) in paths {
            for method in METHODS {
                let Some(op) = item.get(method) else { continue };
                for (code, response) in op["responses"].as_object().expect("responses") {
                    let is_error = code
                        .parse::<u16>()
                        .map(|c| c >= 400)
                        .unwrap_or(code == "default");
                    if !is_error {
                        continue;
                    }
                    checked += 1;
                    let ok = if let Some(reference) = response.get("$ref").and_then(Value::as_str) {
                        let name = reference.rsplit('/').next().unwrap_or_default();
                        components
                            .get(name)
                            .and_then(|r| r.get("content"))
                            .is_some_and(is_error_schema)
                    } else {
                        response.get("content").is_some_and(is_error_schema)
                    };
                    assert!(ok, "{method} {path} {code} does not use schemas/Error");
                }
            }
        }
        assert!(checked > 100, "only {checked} error responses were checked");
    }

    #[test]
    fn the_error_schema_requires_code_message_and_request_id() {
        let doc = openapi();
        let error = &doc["components"]["schemas"]["Error"];
        assert_eq!(error["required"], serde_json::json!(["error"]));
        assert_eq!(
            error["properties"]["error"]["required"],
            serde_json::json!(["code", "message", "request_id"])
        );
    }

    fn assert_error_reply(label: &str, reply: &Reply, status: StatusCode, code: &str) {
        assert_eq!(
            (reply.status, reply.body["error"]["code"].as_str()),
            (status, Some(code)),
            "{label}: {}",
            reply.raw
        );
        let error = &reply.body["error"];
        assert!(
            error["message"].as_str().is_some_and(|m| !m.is_empty()),
            "{label}: message"
        );
        let request_id = error["request_id"].as_str().unwrap_or_default();
        assert!(!request_id.is_empty(), "{label}: request_id");
        assert_eq!(
            reply
                .headers
                .get("x-request-id")
                .and_then(|v| v.to_str().ok()),
            Some(request_id),
            "{label}: x-request-id header equals request_id"
        );
        assert_eq!(
            reply
                .headers
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "{label}: Cache-Control"
        );
        assert!(
            reply
                .headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("application/json")),
            "{label}: content type"
        );
        let keys: BTreeSet<_> = error.as_object().expect("error object").keys().collect();
        assert!(
            keys.iter()
                .all(|k| ["code", "message", "request_id", "details"].contains(&k.as_str())),
            "{label}: unexpected keys {keys:?}"
        );
    }

    #[tokio::test]
    async fn real_error_responses_carry_the_common_shape_and_request_id() {
        let tiny = ApiConfig {
            max_body_bytes: 32,
            ..ApiConfig::default()
        };
        let with_db = router_with_services(
            Capabilities::development_defaults(),
            &tiny,
            Some(lazy_services()),
        );
        let without_db = router_with(Capabilities::development_defaults(), &ApiConfig::default());

        let login = |content_type: &str, body: &str| {
            request("POST", "/api/v1/auth/login")
                .header(header::CONTENT_TYPE, content_type)
                .header(header::ORIGIN, ORIGIN)
                .body(Body::from(body.to_owned()))
                .expect("request")
        };

        assert_error_reply(
            "404",
            &send(
                &without_db,
                request("GET", "/api/v1/nope")
                    .body(Body::empty())
                    .expect("r"),
            )
            .await,
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
        );
        assert_error_reply(
            "405",
            &send(
                &without_db,
                request("DELETE", "/health/live")
                    .body(Body::empty())
                    .expect("r"),
            )
            .await,
            StatusCode::METHOD_NOT_ALLOWED,
            "METHOD_NOT_ALLOWED",
        );
        assert_error_reply(
            "413",
            &send(
                &with_db,
                login(
                    "application/json",
                    &format!("{{\"email\":\"{}\"}}", "x".repeat(200)),
                ),
            )
            .await,
            StatusCode::PAYLOAD_TOO_LARGE,
            "PAYLOAD_TOO_LARGE",
        );
        assert_error_reply(
            "415",
            &send(&with_db, login("text/plain", "{}")).await,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "UNSUPPORTED_MEDIA_TYPE",
        );
        assert_error_reply(
            "422",
            &send(&with_db, login("application/json", "{\"email\":1}")).await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "VALIDATION_ERROR",
        );
        assert_error_reply(
            "401",
            &send(
                &with_db,
                request("GET", "/api/v1/auth/session")
                    .body(Body::empty())
                    .expect("r"),
            )
            .await,
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
        );
        assert_error_reply(
            "503",
            &send(
                &without_db,
                request("GET", "/api/v1/setup/status")
                    .body(Body::empty())
                    .expect("r"),
            )
            .await,
            StatusCode::SERVICE_UNAVAILABLE,
            "SETUP_UNAVAILABLE",
        );
    }

    #[tokio::test]
    async fn request_ids_are_generated_echoed_when_sane_and_replaced_otherwise() {
        let app = router_with(Capabilities::development_defaults(), &ApiConfig::default());
        let get = |id: Option<&str>| {
            let mut builder = request("GET", "/api/v1/nope");
            if let Some(id) = id {
                builder = builder.header("x-request-id", id);
            }
            builder.body(Body::empty()).expect("request")
        };
        let generated = send(&app, get(None)).await;
        assert_error_reply("generated", &generated, StatusCode::NOT_FOUND, "NOT_FOUND");
        let echoed = send(&app, get(Some("client-id_1.2"))).await;
        assert_eq!(echoed.body["error"]["request_id"], "client-id_1.2");
        assert_error_reply("echoed", &echoed, StatusCode::NOT_FOUND, "NOT_FOUND");
        let replaced = send(&app, get(Some("bad id with spaces"))).await;
        assert_ne!(replaced.body["error"]["request_id"], "bad id with spaces");
        assert_error_reply("replaced", &replaced, StatusCode::NOT_FOUND, "NOT_FOUND");
    }

    // ---- health and info -------------------------------------------------------------

    #[tokio::test]
    async fn health_and_info_follow_their_documented_shape() {
        let app = router_with(Capabilities::development_defaults(), &ApiConfig::default());
        let live = send(
            &app,
            request("GET", "/health/live")
                .body(Body::empty())
                .expect("r"),
        )
        .await;
        assert_eq!(live.status, StatusCode::OK);
        assert_eq!(live.body, serde_json::json!({"status": "live"}));
        assert!(live.headers.contains_key("x-request-id"));
        assert_eq!(live.headers[header::CACHE_CONTROL], "no-store");

        let info = send(
            &app,
            request("GET", "/api/v1/info")
                .body(Body::empty())
                .expect("r"),
        )
        .await;
        assert_eq!(info.status, StatusCode::OK);
        let keys: BTreeSet<_> = info
            .body
            .as_object()
            .expect("object")
            .keys()
            .cloned()
            .collect();
        let documented: BTreeSet<_> = ["api_version", "features", "server_mode"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(keys, documented);
        let doc = openapi();
        let required = doc["paths"]["/info"]["get"]["responses"]["200"]["content"]["application/json"]
            ["schema"]["required"]
            .as_array()
            .expect("info schema required")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect::<BTreeSet<_>>();
        assert_eq!(required, documented);
        assert_eq!(info.body["server_mode"], "development-prototype");

        // There is no readiness endpoint yet (CC-F15); the contract must not claim one.
        let ready = send(
            &app,
            request("GET", "/health/ready")
                .body(Body::empty())
                .expect("r"),
        )
        .await;
        assert_eq!(ready.status, StatusCode::NOT_FOUND);
        assert!(doc["paths"].get("/health/ready").is_none());
    }
}
