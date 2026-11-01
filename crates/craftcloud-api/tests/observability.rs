//! Tests of the observability surface (CC-F15, CC-P03-06): the request log and
//! its redaction, `GET /health/ready`, the operator health view and the
//! Prometheus exposition of the internal metrics listener.
//!
//! The tests marked `#[ignore]` need PostgreSQL (`cargo xtask db-test`); the
//! log format and redaction tests of the database-free router run everywhere.

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use craftcloud_api::maintenance::{
        Maintenance, MaintenanceSettings, RunOptions, Trigger, check_store_binding,
    };
    use craftcloud_api::{ApiConfig, Clock, metrics_router};
    use craftcloud_auth::BootstrapToken;
    use craftcloud_storage::LocalStore;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use tokio::sync::watch;
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::common::{
        COOKIE_NAME, PASSWORD, Req, Resp, Session, TestEnv, call, capture_logs, session_from,
        settings,
    };

    const METRICS_TOKEN: &str = "metrics-token-0123456789abcdef-0123456789";

    fn sha_hex(data: &[u8]) -> String {
        Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn id_of(r: &Resp) -> Uuid {
        Uuid::parse_str(r.body["id"].as_str().unwrap()).unwrap()
    }

    async fn get(env: &TestEnv, path: &str) -> Resp {
        call(&env.app, Req::new("GET", path)).await
    }

    async fn scrape(router: &Router, token: Option<&str>) -> (StatusCode, String) {
        let mut builder = Request::builder().method("GET").uri("/metrics");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = router
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    fn metrics_app(env: &TestEnv, token: Option<&str>) -> Router {
        metrics_router(
            Arc::clone(&env.metrics),
            Some(Arc::clone(&env.services)),
            token.map(|t| BootstrapToken::new(t).unwrap()),
        )
    }

    /// Value of the series whose line starts with `series` (name and labels).
    fn value_of(text: &str, series: &str) -> Option<f64> {
        text.lines()
            .find_map(|line| line.strip_prefix(series))
            .and_then(|rest| rest.trim().parse::<f64>().ok())
    }

    // ---- request log: format and redaction (no database) ------------------------

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buffer {
        type Writer = Buffer;
        fn make_writer(&'a self) -> Buffer {
            self.clone()
        }
    }

    impl Buffer {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    #[tokio::test]
    async fn json_request_lines_carry_route_template_status_latency_and_ids_only() {
        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        // Thread-local: the current-thread test runtime stays on this thread.
        let _guard = tracing::subscriber::set_default(subscriber);

        let app = craftcloud_api::router();
        let secret = "Qw9-secret-query-value";
        for (uri, id) in [
            ("/api/v1/info?api_key=Qw9-secret-query-value", "json-req-1"),
            ("/nope/Qw9-secret-path-segment?x=1", "json-req-2"),
            ("/health/live", "json-req-3"),
        ] {
            let request = Request::builder()
                .uri(uri)
                .header("x-request-id", id)
                .header(header::COOKIE, "__Host-craftcloud_session=COOKIESECRET")
                .header(header::AUTHORIZATION, "Bearer AUTHSECRET")
                .header("x-csrf-token", "CSRFSECRET")
                .body(Body::empty())
                .unwrap();
            let _ = app.clone().oneshot(request).await.unwrap();
        }
        let text = buffer.text();
        for hidden in [
            secret,
            "Qw9-secret-path-segment",
            "COOKIESECRET",
            "AUTHSECRET",
            "CSRFSECRET",
            "api_key",
        ] {
            assert!(!text.contains(hidden), "log leaks {hidden}:\n{text}");
        }
        let lines: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("every log line is one JSON object"))
            .collect();
        let line_for = |id: &str| -> &Value {
            lines
                .iter()
                .find(|l| {
                    l["target"] == "craftcloud::request"
                        && l["span"]["request_id"].as_str() == Some(id)
                })
                .unwrap_or_else(|| panic!("no request line for {id}:\n{text}"))
        };
        let info = line_for("json-req-1");
        // AC-01 rejects the intentionally mixed credentials before serving the route.
        assert_eq!(info["fields"]["status"], 400);
        assert_eq!(info["span"]["route"], "/api/v1/info");
        assert_eq!(info["span"]["method"], "GET");
        assert!(info["fields"]["latency_ms"].is_u64());
        assert_eq!(info["fields"]["actor_id"], "-");
        assert_eq!(info["fields"]["org_id"], "-");
        assert_eq!(info["level"], "INFO");
        let missing = line_for("json-req-2");
        assert_eq!(missing["fields"]["status"], 400);
        assert_eq!(missing["span"]["route"], "unmatched");
        // Health probes stay out of the default (info) log.
        assert!(
            !lines.iter().any(|l| {
                l["target"] == "craftcloud::request"
                    && l["span"]["request_id"].as_str() == Some("json-req-3")
                    && l["level"] != "DEBUG"
            }),
            "probe logged above debug"
        );
    }

    #[tokio::test]
    async fn bodies_of_rejected_requests_are_not_logged() {
        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let app = craftcloud_api::router();
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/login?next=/secret")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({"email": "body.person@example.test", "password": "body-password-9"})
                    .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let text = buffer.text();
        assert!(text.contains("/api/v1/auth/login"), "{text}");
        for hidden in ["body.person", "body-password-9", "next=", "/secret"] {
            assert!(!text.contains(hidden), "log leaks {hidden}:\n{text}");
        }
    }

    // ---- request log redaction across representative flows ---------------------------

    struct Flow {
        env: TestEnv,
        op: Session,
    }

    impl Flow {
        async fn send(&self, req: Req) -> Resp {
            call(&self.env.app, req).await
        }
        async fn json(&self, method: &'static str, path: &str, who: &Session, body: Value) -> Resp {
            self.send(Req::new(method, path).session(who).json(body))
                .await
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn logs_hold_no_secrets_or_personal_data_across_login_invite_reset_upload_download_and_errors()
     {
        let logs = capture_logs();
        let env = TestEnv::new().await;
        let op = env.login_ok("op").await;
        let w = Flow { env, op };
        let owner_id = w.env.user("owner.person").await;
        let owner = w.env.login_ok("owner.person").await;

        // setup: org, workspace, project, file (names are personal data)
        let org = w
            .json(
                "POST",
                "/api/v1/organizations",
                &w.op,
                json!({"name": "Org Zeta-Name-77", "slug": "zeta-77", "owner_user_id": owner_id.to_string()}),
            )
            .await;
        assert_eq!(org.status, StatusCode::CREATED, "{}", org.text);
        let org_id = id_of(&org);
        let ws = w
            .json(
                "POST",
                &format!("/api/v1/organizations/{org_id}/workspaces"),
                &owner,
                json!({"name": "Workspace-Name-88", "slug": "ws-88"}),
            )
            .await;
        let ws_id = id_of(&ws);
        let project = w
            .json(
                "POST",
                &format!("/api/v1/workspaces/{ws_id}/projects"),
                &owner,
                json!({"name": "Project-Name-99"}),
            )
            .await;
        let file = w
            .json(
                "POST",
                &format!("/api/v1/projects/{}/files", id_of(&project)),
                &owner,
                json!({"name": "secret-file-name-123.fig", "app": "figcraft", "format": "fig"}),
            )
            .await;
        let file_id = id_of(&file);

        // login (good and bad)
        let bad_login = w
            .send(
                Req::new("POST", "/api/v1/auth/login")
                    .header("x-request-id", "flow-login-bad")
                    .json(json!({"email": "ghost.person@example.test", "password": "ghost-password-1234"})),
            )
            .await;
        assert_eq!(bad_login.status, StatusCode::UNAUTHORIZED);
        let good_login = w
            .send(
                Req::new("POST", "/api/v1/auth/login")
                    .header("x-request-id", "flow-login-ok")
                    .json(json!({"email": "owner.person@example.test", "password": PASSWORD})),
            )
            .await;
        assert_eq!(good_login.status, StatusCode::OK);
        let owner = session_from(&good_login);

        // invitation create + accept (token and new password)
        let invite = w
            .json(
                "POST",
                &format!("/api/v1/organizations/{org_id}/invitations"),
                &owner,
                json!({"email": "invitee.person@example.test", "org_role": "member"}),
            )
            .await;
        assert_eq!(invite.status, StatusCode::CREATED, "{}", invite.text);
        let invite_token = invite.body["invite_url"]
            .as_str()
            .unwrap()
            .split("#token=")
            .nth(1)
            .unwrap()
            .to_owned();
        let accepted = w
            .send(
                Req::new("POST", "/api/v1/invitations/accept")
                    .header("x-request-id", "flow-accept")
                    .json(json!({"token": invite_token, "display_name": "Invitee Display", "password": "invitee-password-5678"})),
            )
            .await;
        assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.text);
        let invitee = session_from(&accepted);

        // password reset (operator issues, user redeems)
        let reset = w
            .send(
                Req::new(
                    "POST",
                    &format!("/api/v1/admin/users/{owner_id}/password-reset"),
                )
                .session(&w.op),
            )
            .await;
        assert_eq!(reset.status, StatusCode::CREATED, "{}", reset.text);
        let reset_token = reset.body["reset_url"]
            .as_str()
            .unwrap()
            .split("#token=")
            .nth(1)
            .unwrap()
            .to_owned();
        let redeemed = w
            .send(
                Req::new("POST", "/api/v1/auth/password-reset")
                    .header("x-request-id", "flow-reset")
                    .json(json!({"token": reset_token, "new_password": "reset-new-password-9012"})),
            )
            .await;
        assert_eq!(redeemed.status, StatusCode::NO_CONTENT, "{}", redeemed.text);
        let owner = w
            .env
            .login_ok_with("owner.person", "reset-new-password-9012")
            .await;

        // upload + commit + download (content and hash must never be logged)
        let data = b"file-bytes-that-must-not-appear-in-logs".to_vec();
        let sha = sha_hex(&data);
        let upload = w
            .send(
                Req::new("POST", &format!("/api/v1/files/{file_id}/uploads"))
                    .session(&owner)
                    .header("x-request-id", "flow-upload-create")
                    .json(json!({"size_bytes": data.len(), "sha256": sha})),
            )
            .await;
        assert_eq!(upload.status, StatusCode::CREATED, "{}", upload.text);
        let upload_id = upload.body["upload_id"].as_str().unwrap().to_owned();
        let open_now = scrape(&metrics_app(&w.env, None), None).await.1;
        assert_eq!(
            value_of(&open_now, "craftcloud_upload_sessions_open "),
            Some(1.0),
            "{open_now}"
        );
        let put = w
            .send(
                Req::new("PUT", &format!("/api/v1/uploads/{upload_id}/content"))
                    .session(&owner)
                    .header("x-request-id", "flow-upload-put")
                    .body(Body::from(data.clone())),
            )
            .await;
        assert_eq!(put.status, StatusCode::OK, "{}", put.text);
        let commit = w
            .send(
                Req::new("POST", &format!("/api/v1/files/{file_id}/versions"))
                    .session(&owner)
                    .header("If-None-Match", "*")
                    .header("x-request-id", "flow-commit")
                    .json(json!({"upload_id": upload_id})),
            )
            .await;
        assert_eq!(commit.status, StatusCode::CREATED, "{}", commit.text);
        let download = w
            .send(
                Req::new("GET", &format!("/api/v1/files/{file_id}/content"))
                    .cookie_only(&owner)
                    .header("x-request-id", "flow-download"),
            )
            .await;
        assert_eq!(download.status, StatusCode::OK);
        assert_eq!(download.bytes, data);

        // error paths: 401, 403, 404 with a secret query, 405, 413, 422
        let unauth = w
            .send(
                Req::new("GET", "/api/v1/auth/session")
                    .header("x-request-id", "flow-401")
                    .header("authorization", "Bearer AUTHHEADERSECRET-4455"),
            )
            .await;
        assert_eq!(unauth.status, StatusCode::UNAUTHORIZED);
        // Mixed credentials add an AC-01 error path without losing the 401 coverage.
        let mixed = w
            .send(
                Req::new("GET", "/api/v1/auth/session")
                    .header("x-request-id", "flow-400")
                    .header("authorization", "Bearer AUTHHEADERSECRET-4455")
                    .header("cookie", "__Host-craftcloud_session=BOGUSCOOKIEVALUE-6677"),
            )
            .await;
        assert_eq!(mixed.status, StatusCode::BAD_REQUEST);
        assert_eq!(mixed.code(), "AMBIGUOUS_CREDENTIALS");
        let forbidden = w
            .send(
                Req::new("GET", "/api/v1/admin/health")
                    .cookie_only(&invitee)
                    .header("x-request-id", "flow-403"),
            )
            .await;
        assert_eq!(forbidden.status, StatusCode::FORBIDDEN);
        let missing = w
            .send(
                Req::new(
                    "GET",
                    "/api/v1/unknown/path-secret-8899?access_token=QUERYTOKEN-1122#frag",
                )
                .header("x-request-id", "flow-404"),
            )
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        let wrong_method = w
            .send(Req::new("DELETE", "/health/live").header("x-request-id", "flow-405"))
            .await;
        assert_eq!(wrong_method.status, StatusCode::METHOD_NOT_ALLOWED);
        let too_big = w
            .send(
                Req::new("POST", "/api/v1/auth/login")
                    .header("x-request-id", "flow-413")
                    .json(
                        json!({"email": "x".repeat(100_000), "password": "toobig-password-3344"}),
                    ),
            )
            .await;
        assert_eq!(too_big.status, StatusCode::PAYLOAD_TOO_LARGE);
        let malformed = w
            .send(
                Req::new("POST", "/api/v1/auth/login")
                    .header("x-request-id", "flow-422")
                    .json(json!({"email": 5, "password": "malformed-password-5566"})),
            )
            .await;
        assert_eq!(malformed.status, StatusCode::UNPROCESSABLE_ENTITY);

        let text = logs.text();

        // Positive control: the request lines exist, with the template, the
        // status and the pseudonymous ids.
        let line = |id: &str| -> String {
            text.lines()
                .find(|l| l.contains("craftcloud::request") && l.contains(id))
                .unwrap_or_else(|| panic!("no request line for {id}"))
                .to_owned()
        };
        let create = line("flow-upload-create");
        assert!(
            create.contains("route=/api/v1/files/{file_id}/uploads"),
            "{create}"
        );
        assert!(create.contains("status=201"), "{create}");
        assert!(create.contains("latency_ms="), "{create}");
        assert!(create.contains(&format!("org_id={org_id}")), "{create}");
        assert!(
            create.contains(&format!("actor_id={owner_id}")),
            "actor is the pseudonymous account id: {create}"
        );
        assert!(line("flow-upload-put").contains("route=/api/v1/uploads/{upload_id}/content"));
        assert!(line("flow-download").contains("route=/api/v1/files/{file_id}/content"));
        assert!(line("flow-login-ok").contains(&format!("actor_id={owner_id}")));
        assert!(line("flow-login-bad").contains("status=401"));
        assert!(line("flow-accept").contains("route=/api/v1/invitations/accept"));
        assert!(line("flow-reset").contains("route=/api/v1/auth/password-reset"));
        assert!(line("flow-404").contains("route=unmatched"));
        assert!(line("flow-404").contains("status=404"));
        assert!(line("flow-405").contains("status=405"));
        assert!(line("flow-413").contains("status=413"));
        assert!(line("flow-422").contains("status=422"));
        assert!(line("flow-403").contains("status=403"));
        assert!(line("flow-401").contains("status=401"));
        assert!(line("flow-400").contains("status=400"));

        // No concrete path with an id, no query string, no fragment.
        for concrete in [
            format!("/api/v1/files/{file_id}"),
            format!("/api/v1/uploads/{upload_id}"),
            format!("/api/v1/organizations/{org_id}/"),
            format!("/api/v1/workspaces/{ws_id}"),
            "path-secret-8899".to_owned(),
            "access_token".to_owned(),
            "QUERYTOKEN-1122".to_owned(),
        ] {
            assert!(!text.contains(&concrete), "log holds path data {concrete}");
        }

        // Secrets and personal data.
        let secrets = [
            owner.token.as_str(),
            owner.csrf.as_str(),
            w.op.token.as_str(),
            w.op.csrf.as_str(),
            invitee.token.as_str(),
            invitee.csrf.as_str(),
            invite_token.as_str(),
            reset_token.as_str(),
            PASSWORD,
            "ghost-password-1234",
            "invitee-password-5678",
            "reset-new-password-9012",
            "toobig-password-3344",
            "malformed-password-5566",
            "AUTHHEADERSECRET-4455",
            "BOGUSCOOKIEVALUE-6677",
            "owner.person@example.test",
            "ghost.person@example.test",
            "invitee.person@example.test",
            "Invitee Display",
            "op@example.test",
            "secret-file-name-123",
            "Project-Name-99",
            "Workspace-Name-88",
            "Zeta-Name-77",
            "file-bytes-that-must-not-appear",
        ];
        for secret in secrets {
            assert!(!text.contains(secret), "log leaks {}", &secret[..6]);
        }
        assert!(!text.contains(&sha), "log leaks the content hash");
        assert!(
            !text.contains(&format!("{COOKIE_NAME}=")),
            "log leaks a cookie header"
        );
        assert!(
            !text.to_ascii_lowercase().contains("authorization"),
            "log mentions an authorization header"
        );
    }

    // ---- readiness ----------------------------------------------------------------

    fn assert_ready(r: &Resp, status: &str) {
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        assert_eq!(r.body, json!({"status": status}), "{}", r.text);
        assert_eq!(r.headers[header::CACHE_CONTROL], "no-store");
    }

    fn assert_not_ready(r: &Resp) {
        assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{}", r.text);
        assert_eq!(r.code(), "NOT_READY", "{}", r.text);
        // Nothing about components, errors or versions leaks to anonymous callers.
        let lower = r.text.to_ascii_lowercase();
        for word in [
            "postgres",
            "sqlx",
            "database",
            "storage",
            "migration",
            "pool",
        ] {
            assert!(!lower.contains(word), "ready body names {word}: {}", r.text);
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn ready_answers_ready_with_a_body_that_has_no_details() {
        let env = TestEnv::new().await;
        assert_ready(&get(&env, "/health/ready").await, "ready");
        let live = get(&env, "/health/live").await;
        assert_eq!(live.body, json!({"status": "live"}));
        // Without a store the store is simply not part of the verdict.
        let bare = TestEnv::without_storage().await;
        assert_ready(&get(&bare, "/health/ready").await, "ready");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn ready_is_503_when_the_database_is_unreachable_and_live_stays_200() {
        let env = TestEnv::new().await;
        env.pool.close().await;
        assert_not_ready(&get(&env, "/health/ready").await);
        // Liveness never depends on the database (no restart loops).
        assert_eq!(get(&env, "/health/live").await.status, StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn ready_is_503_when_migrations_are_missing() {
        let env = TestEnv::new().await;
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = (SELECT max(version) FROM _sqlx_migrations)")
            .execute(&env.pool)
            .await
            .unwrap();
        assert_not_ready(&get(&env, "/health/ready").await);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn ready_is_503_when_the_store_cannot_be_written() {
        let env = TestEnv::new().await;
        std::fs::remove_dir_all(env.storage_dir.join("tmp")).unwrap();
        assert_not_ready(&get(&env, "/health/ready").await);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_store_that_belongs_to_another_database_is_degraded_not_unready() {
        // Refused at startup: everything but uploads and downloads works.
        let env = TestEnv::with_refused_storage().await;
        assert_ready(&get(&env, "/health/ready").await, "degraded");
        let op = env.login_ok("op").await;
        let health = call(
            &env.app,
            Req::new("GET", "/api/v1/admin/health").cookie_only(&op),
        )
        .await;
        assert_eq!(health.body["status"], "degraded", "{}", health.text);
        assert_eq!(health.body["components"]["storage"]["binding"], "mismatch");
        assert_eq!(health.body["components"]["storage"]["status"], "degraded");

        // The mismatch found later by the live check is treated the same way.
        let env = TestEnv::new().await;
        let store = LocalStore::open(&env.storage_dir).await.unwrap();
        check_store_binding(&env.pool, &store, env.clock.now(), true)
            .await
            .unwrap();
        sqlx::query("UPDATE storage_binding SET store_id = $1")
            .bind(Uuid::new_v4())
            .execute(&env.pool)
            .await
            .unwrap();
        assert_ready(&get(&env, "/health/ready").await, "degraded");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn cancelled_callers_cannot_multiply_readiness_checks() {
        let env = TestEnv::new().await;
        // Many probes that give up immediately: one check runs, completes and
        // is cached although nobody waits for it.
        let mut tasks = Vec::new();
        for _ in 0..50 {
            let app = env.app.clone();
            tasks.push(tokio::spawn(async move {
                let request = Req::new("GET", "/health/ready").build();
                let _ = tokio::time::timeout(Duration::from_millis(1), app.oneshot(request)).await;
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        for _ in 0..200 {
            if env.services.readiness_checks_run() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(env.services.readiness_checks_run(), 1);
        // Within the window every further probe is answered from the cache.
        for _ in 0..20 {
            assert_ready(&get(&env, "/health/ready").await, "ready");
        }
        assert_eq!(env.services.readiness_checks_run(), 1);
        // The metric exposes the count.
        let (_, metrics) = scrape(&metrics_app(&env, None), None).await;
        assert_eq!(
            value_of(&metrics, "craftcloud_readiness_checks_total "),
            Some(1.0)
        );
        // No probe file is left in the store.
        assert_eq!(
            std::fs::read_dir(env.storage_dir.join("tmp"))
                .unwrap()
                .count(),
            0
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn readiness_turns_503_while_the_server_drains_and_liveness_stays_200() {
        let env = TestEnv::new().await;
        assert_ready(&get(&env, "/health/ready").await, "ready");
        env.services.begin_draining();
        assert_not_ready(&get(&env, "/health/ready").await);
        assert_eq!(get(&env, "/health/live").await.status, StatusCode::OK);
        let (_, metrics) = scrape(&metrics_app(&env, None), None).await;
        assert_eq!(value_of(&metrics, "craftcloud_ready "), Some(0.0));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn an_epoch_conflict_is_degraded_but_still_ready() {
        let env = TestEnv::new().await;
        let store = LocalStore::open(&env.storage_dir).await.unwrap();
        check_store_binding(&env.pool, &store, env.clock.now(), true)
            .await
            .unwrap();
        // A restored marker or database: the epochs differ by more than one.
        sqlx::query("UPDATE storage_binding SET epoch = epoch + 5")
            .execute(&env.pool)
            .await
            .unwrap();
        assert_ready(&get(&env, "/health/ready").await, "degraded");
        let op = env.login_ok("op").await;
        let health = call(
            &env.app,
            Req::new("GET", "/api/v1/admin/health").cookie_only(&op),
        )
        .await;
        assert_eq!(health.status, StatusCode::OK, "{}", health.text);
        assert_eq!(health.body["status"], "degraded");
        assert_eq!(
            health.body["components"]["storage"]["binding"],
            "epoch_conflict"
        );
        assert_eq!(health.body["maintenance"]["epoch_conflict"], true);
        let (_, metrics) = scrape(&metrics_app(&env, None), None).await;
        assert_eq!(
            value_of(&metrics, "craftcloud_storage_epoch_conflict "),
            Some(1.0)
        );
        assert_eq!(value_of(&metrics, "craftcloud_ready "), Some(1.0));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_readiness_verdict_is_cached_for_a_moment() {
        let env = TestEnv::new().await;
        assert_ready(&get(&env, "/health/ready").await, "ready");
        env.pool.close().await;
        // Within the cache window the verdict does not change (health probes
        // cannot become load) ...
        assert_ready(&get(&env, "/health/ready").await, "ready");
        // ... and afterwards it does.
        tokio::time::sleep(Duration::from_millis(3200)).await;
        assert_not_ready(&get(&env, "/health/ready").await);
    }

    // ---- operator health ------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_operator_sees_components_pool_uploads_maintenance_audit_and_version() {
        let env = TestEnv::new().await;
        let op = env.login_ok("op").await;
        let url = "/api/v1/admin/health";
        let first = call(&env.app, Req::new("GET", url).cookie_only(&op)).await;
        assert_eq!(first.status, StatusCode::OK, "{}", first.text);
        let h = &first.body;
        assert_eq!(h["status"], "ok", "{}", first.text);
        assert!(h["checked_at"].as_str().is_some());
        assert_eq!(h["components"]["database"]["status"], "ok");
        assert_eq!(h["components"]["database"]["pool"]["max"], 20);
        assert!(
            h["components"]["database"]["pool"]["size"]
                .as_u64()
                .is_some()
        );
        assert!(
            h["components"]["database"]["pool"]["idle"]
                .as_u64()
                .is_some()
        );
        assert!(
            h["components"]["database"]["pool"]["in_use"]
                .as_u64()
                .is_some()
        );
        assert_eq!(h["components"]["migrations"]["status"], "ok");
        assert_eq!(h["components"]["storage"]["status"], "ok");
        assert_eq!(h["components"]["storage"]["binding"], "unbound");
        if cfg!(unix) {
            assert!(
                h["components"]["storage"]["free_bytes"].as_u64().is_some(),
                "{}",
                first.text
            );
        }
        assert_eq!(h["uploads"]["open_sessions"], 0);
        assert_eq!(h["uploads"]["overdue_sessions"], 0);
        assert_eq!(h["maintenance"]["last_run"], Value::Null);
        assert_eq!(h["maintenance"]["last_run_ok"], Value::Null);
        assert_eq!(h["maintenance"]["epoch_conflict"], false);
        assert_eq!(h["audit"]["dropped_events_total"], 0);
        assert!(h["server"]["version"].as_str().is_some());
        assert!(h["server"]["uptime_seconds"].as_u64().is_some());
        // No backup recorded yet (see tests/backup.rs for the age after one).
        assert_eq!(h["backup"]["last_success_age_seconds"], Value::Null);
        assert_eq!(h["archive"]["configured"], false);
        assert_eq!(h["archive"]["provider_type"], Value::Null);
        assert_eq!(h["archive"]["endpoint"], Value::Null);
        assert_eq!(h["archive"]["local_bytes"], Value::Null);
        assert_eq!(h["archive"]["last_error_code"], Value::Null);
        assert_eq!(first.headers[header::CACHE_CONTROL], "no-store");

        // After a maintenance run the view names it, and the run sweeps probe
        // files that a crash left behind (older than an hour).
        let stale = env
            .storage_dir
            .join("tmp")
            .join(format!("{}.health", Uuid::new_v4().simple()));
        {
            let file = std::fs::File::create(&stale).unwrap();
            file.set_modified(std::time::SystemTime::now() - Duration::from_secs(2 * 3600))
                .unwrap();
        }
        let store = LocalStore::open(&env.storage_dir).await.unwrap();
        let day = Duration::from_secs(24 * 3600);
        let job = Maintenance::new(
            env.pool.clone(),
            Some(store),
            env.clock.clone(),
            MaintenanceSettings::new(day, day, day, 1024 * 1024),
        )
        .with_metrics(Arc::clone(&env.metrics));
        let (_keep, stop) = watch::channel(false);
        job.run_once(
            RunOptions {
                dry_run: false,
                trigger: Trigger::Scheduled,
            },
            &stop,
        )
        .await
        .unwrap();
        assert!(!stale.exists(), "stale health probe was not swept");
        let after = call(&env.app, Req::new("GET", url).cookie_only(&op)).await;
        assert_eq!(
            after.body["maintenance"]["last_run"]["trigger"],
            "scheduled"
        );
        assert_eq!(after.body["maintenance"]["last_run_ok"], true);
        assert_eq!(after.body["components"]["storage"]["binding"], "matches");
        let (_, metrics) = scrape(&metrics_app(&env, None), None).await;
        assert_eq!(
            value_of(
                &metrics,
                "craftcloud_maintenance_runs_total{result=\"ok\"} "
            ),
            Some(1.0)
        );
        assert!(
            value_of(
                &metrics,
                "craftcloud_maintenance_last_success_timestamp_seconds "
            )
            .is_some_and(|t| t > 1.0e9)
        );
        assert!(
            value_of(
                &metrics,
                "craftcloud_maintenance_last_run_timestamp_seconds "
            )
            .is_some()
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_operator_view_counts_dropped_audit_events() {
        let env = TestEnv::new().await;
        let op = env.login_ok("op").await;
        let member = env.user("member").await;
        let _ = member;
        let member = env.login_ok("member").await;
        // Use up the lossy audit budget: denials are dropped and counted.
        let held = env
            .audit_lossy
            .clone()
            .acquire_many_owned(u32::try_from(env.audit_lossy.available_permits()).unwrap())
            .await
            .unwrap();
        for _ in 0..3 {
            let denied = call(
                &env.app,
                Req::new("GET", "/api/v1/admin/health").cookie_only(&member),
            )
            .await;
            assert_eq!(denied.status, StatusCode::FORBIDDEN);
        }
        let health = call(
            &env.app,
            Req::new("GET", "/api/v1/admin/health").cookie_only(&op),
        )
        .await;
        assert_eq!(
            health.body["audit"]["dropped_events_lossy"], 3,
            "{}",
            health.text
        );
        assert_eq!(health.body["audit"]["dropped_events_total"], 3);
        let (_, metrics) = scrape(&metrics_app(&env, None), None).await;
        assert_eq!(
            value_of(
                &metrics,
                "craftcloud_audit_events_dropped_total{class=\"lossy\"} "
            ),
            Some(3.0)
        );
        drop(held);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_operator_health_view_is_operator_only() {
        let env = TestEnv::new().await;
        let url = "/api/v1/admin/health";
        // Anonymous: 401, nothing else.
        let anonymous = get(&env, url).await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        assert_eq!(anonymous.code(), "UNAUTHENTICATED");
        // A plain account and an organization owner are refused.
        let owner_id = env.user("owner").await;
        let owner = env.login_ok("owner").await;
        let op = env.login_ok("op").await;
        let org = call(
            &env.app,
            Req::new("POST", "/api/v1/organizations").session(&op).json(
                json!({"name": "Acme", "slug": "acme", "owner_user_id": owner_id.to_string()}),
            ),
        )
        .await;
        assert_eq!(org.status, StatusCode::CREATED, "{}", org.text);
        let member_id = env.user("member").await;
        let _ = member_id;
        let member = env.login_ok("member").await;
        for who in [&owner, &member] {
            let denied = call(&env.app, Req::new("GET", url).cookie_only(who)).await;
            assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.text);
            assert_eq!(denied.code(), "FORBIDDEN");
            assert!(denied.body.get("components").is_none());
        }
        // The denial is audited like every other policy denial.
        let mut denied_rows = 0;
        for _ in 0..100 {
            denied_rows = env
                .count("SELECT count(*) FROM audit_events WHERE action = 'health.read' AND result = 'denied'")
                .await;
            if denied_rows >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(denied_rows >= 1, "denial was not audited");
        // Other methods are not allowed.
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            let r = call(&env.app, Req::new(method, url).session(&op)).await;
            assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        }
        assert_eq!(
            call(&env.app, Req::new("GET", url).cookie_only(&op))
                .await
                .status,
            StatusCode::OK
        );
    }

    // ---- metrics --------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn metrics_are_not_served_on_the_public_listener() {
        let env = TestEnv::new().await;
        let op = env.login_ok("op").await;
        for path in ["/metrics", "/api/v1/metrics", "/health/metrics"] {
            let anonymous = get(&env, path).await;
            assert_eq!(anonymous.status, StatusCode::NOT_FOUND, "{path}");
            let operator = call(&env.app, Req::new("GET", path).cookie_only(&op)).await;
            assert_eq!(operator.status, StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_metrics_listener_needs_the_bearer_token_when_one_is_configured() {
        let env = TestEnv::new().await;
        let guarded = metrics_app(&env, Some(METRICS_TOKEN));
        for token in [
            None,
            Some("wrong-token-0123456789abcdef-0123456789"),
            Some(""),
            Some(&METRICS_TOKEN[..METRICS_TOKEN.len() - 1]),
        ] {
            let (status, body) = scrape(&guarded, token).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{token:?}");
            assert!(body.is_empty(), "no body for a rejected scrape");
        }
        // Basic auth and a token in the query string do not count.
        let basic = guarded
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/metrics?token={METRICS_TOKEN}"))
                    .header(header::AUTHORIZATION, format!("Basic {METRICS_TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(basic.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(basic.headers()[header::WWW_AUTHENTICATE], "Bearer");

        let (status, body) = scrape(&guarded, Some(METRICS_TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("# TYPE craftcloud_http_requests_total counter"));
        // Only GET /metrics exists on that listener.
        for (method, path) in [("POST", "/metrics"), ("GET", "/health/live"), ("GET", "/")] {
            let response = guarded
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header(header::AUTHORIZATION, format!("Bearer {METRICS_TOKEN}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(
                matches!(
                    response.status(),
                    StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                ),
                "{method} {path}"
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn metrics_count_requests_by_template_and_never_by_id_or_path() {
        let mut s = settings();
        s.ip_limit = 2;
        let env = TestEnv::with(s, ApiConfig::default()).await;
        let app = metrics_app(&env, None);

        let stray = Uuid::new_v4();
        let _ = get(&env, &format!("/api/v1/files/{stray}")).await;
        let _ = get(&env, "/definitely/not/a/route-9f3a").await;
        let _ = get(&env, "/health/live").await;
        // Rate limit: the third login from one address is rejected.
        for _ in 0..3 {
            let _ = call(
                &env.app,
                Req::new("POST", "/api/v1/auth/login")
                    .json(json!({"email": "nobody@example.test", "password": "wrong-password-12"})),
            )
            .await;
        }

        let (status, text) = scrape(&app, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            value_of(
                &text,
                "craftcloud_http_requests_total{route=\"/api/v1/files/{file_id}\",method=\"GET\",status_class=\"4xx\"} "
            ),
            Some(1.0),
            "{text}"
        );
        assert_eq!(
            value_of(
                &text,
                "craftcloud_http_requests_total{route=\"unmatched\",method=\"GET\",status_class=\"4xx\"} "
            ),
            Some(1.0)
        );
        assert_eq!(
            value_of(
                &text,
                "craftcloud_http_requests_total{route=\"/health/live\",method=\"GET\",status_class=\"2xx\"} "
            ),
            Some(1.0)
        );
        assert_eq!(
            value_of(
                &text,
                "craftcloud_http_requests_total{route=\"/api/v1/auth/login\",method=\"POST\",status_class=\"4xx\"} "
            ),
            Some(3.0)
        );
        assert_eq!(
            value_of(
                &text,
                "craftcloud_rate_limit_rejections_total{class=\"login\"} "
            ),
            Some(1.0)
        );
        assert!(text.contains(
            "craftcloud_http_request_duration_seconds_bucket{route=\"/health/live\",le=\"+Inf\"} 1"
        ));
        assert_eq!(
            value_of(&text, "craftcloud_http_requests_in_flight "),
            Some(0.0)
        );
        // Pool, readiness and build information.
        for series in [
            "craftcloud_db_pool_connections ",
            "craftcloud_db_pool_idle_connections ",
            "craftcloud_db_pool_max_connections ",
            "craftcloud_upload_sessions_open ",
            "craftcloud_uptime_seconds ",
        ] {
            assert!(
                value_of(&text, series).is_some(),
                "missing {series}\n{text}"
            );
        }
        assert_eq!(
            value_of(&text, "craftcloud_db_pool_max_connections "),
            Some(20.0)
        );
        assert_eq!(value_of(&text, "craftcloud_ready "), Some(1.0));
        assert!(text.contains("craftcloud_build_info{version=\""));
        if cfg!(unix) {
            assert!(
                value_of(&text, "craftcloud_storage_free_bytes ").is_some_and(|v| v > 0.0),
                "{text}"
            );
        }
        // Cardinality and privacy: no identifier of any kind appears.
        for forbidden in [
            stray.to_string(),
            "route-9f3a".to_owned(),
            env.operator_id.to_string(),
            "nobody@example.test".to_owned(),
            "user_id".to_owned(),
            "org_id".to_owned(),
        ] {
            assert!(!text.contains(&forbidden), "metrics expose {forbidden}");
        }
        // Prometheus text format: every sample line is `name{labels} number`.
        for line in text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
        {
            let (series, value) = line.rsplit_once(' ').expect(line);
            assert!(value.parse::<f64>().is_ok(), "bad sample {line}");
            assert!(series.starts_with("craftcloud_"), "{line}");
            if let Some((_, labels)) = series.split_once('{') {
                for label in labels.trim_end_matches('}').split(',') {
                    let (key, _) = label.split_once('=').expect("label assignment");
                    // Check label keys: the fixed class "two_factor_management"
                    // contains "actor" but carries no actor identifier.
                    assert!(!key.contains("actor"), "metrics expose actor label {key}");
                }
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn metrics_count_upload_and_download_bytes_and_open_sessions() {
        let env = TestEnv::new().await;
        let op = env.login_ok("op").await;
        let owner_id = env.user("owner").await;
        let owner = env.login_ok("owner").await;
        let org = call(
            &env.app,
            Req::new("POST", "/api/v1/organizations").session(&op).json(
                json!({"name": "Acme", "slug": "acme", "owner_user_id": owner_id.to_string()}),
            ),
        )
        .await;
        let org_id = id_of(&org);
        let ws = call(
            &env.app,
            Req::new(
                "POST",
                &format!("/api/v1/organizations/{org_id}/workspaces"),
            )
            .session(&owner)
            .json(json!({"name": "W", "slug": "w"})),
        )
        .await;
        let project = call(
            &env.app,
            Req::new(
                "POST",
                &format!("/api/v1/workspaces/{}/projects", id_of(&ws)),
            )
            .session(&owner)
            .json(json!({"name": "P"})),
        )
        .await;
        let file = call(
            &env.app,
            Req::new(
                "POST",
                &format!("/api/v1/projects/{}/files", id_of(&project)),
            )
            .session(&owner)
            .json(json!({"name": "a.fig", "app": "figcraft", "format": "fig"})),
        )
        .await;
        let file_id = id_of(&file);
        let data = vec![7_u8; 50_000];
        let upload = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/files/{file_id}/uploads"))
                .session(&owner)
                .json(json!({"size_bytes": data.len(), "sha256": sha_hex(&data)})),
        )
        .await;
        let upload_id = upload.body["upload_id"].as_str().unwrap().to_owned();
        let app = metrics_app(&env, None);
        let (_, before) = scrape(&app, None).await;
        assert_eq!(
            value_of(&before, "craftcloud_upload_sessions_open "),
            Some(1.0)
        );
        assert_eq!(
            value_of(&before, "craftcloud_upload_bytes_received_total "),
            Some(0.0)
        );
        let put = call(
            &env.app,
            Req::new("PUT", &format!("/api/v1/uploads/{upload_id}/content"))
                .session(&owner)
                .body(Body::from(data.clone())),
        )
        .await;
        assert_eq!(put.status, StatusCode::OK, "{}", put.text);
        let commit = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/files/{file_id}/versions"))
                .session(&owner)
                .header("If-None-Match", "*")
                .json(json!({"upload_id": upload_id})),
        )
        .await;
        assert_eq!(commit.status, StatusCode::CREATED, "{}", commit.text);
        let download = call(
            &env.app,
            Req::new("GET", &format!("/api/v1/files/{file_id}/content")).cookie_only(&owner),
        )
        .await;
        assert_eq!(download.bytes.len(), data.len());
        // The streaming task counts after the last chunk was taken.
        let mut text = String::new();
        for _ in 0..100 {
            text = scrape(&app, None).await.1;
            if value_of(&text, "craftcloud_download_bytes_sent_total ") == Some(50_000.0) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            value_of(&text, "craftcloud_upload_bytes_received_total "),
            Some(50_000.0),
            "{text}"
        );
        assert_eq!(
            value_of(&text, "craftcloud_download_bytes_sent_total "),
            Some(50_000.0),
            "{text}"
        );
        assert_eq!(
            value_of(&text, "craftcloud_upload_sessions_open "),
            Some(0.0)
        );
    }
}
