//! HTTP-level tests of the instance setup routes (CC-F01, abuse case 1:
//! `/setup/complete` race creating two owners).
//!
//! Tests without database access use a lazy pool that never connects.
//! Tests marked `#[ignore]` need PostgreSQL (`cargo xtask db-test`).

#[cfg(test)]
mod tests {
    use std::io;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use craftcloud_api::{ApiConfig, AuthSettings, Services, router_with_services};
    use craftcloud_auth::{BootstrapToken, verify_password};
    use craftcloud_core::Capabilities;
    use craftcloud_db::PgPool;
    use serde_json::{Value, json};
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use sqlx::{Connection, PgConnection, Row};
    use tower::ServiceExt;
    use tracing_subscriber::fmt::MakeWriter;
    use uuid::Uuid;

    const TOKEN: &str = "test-bootstrap-token-0123456789-abcdefghij";
    const PASSWORD: &str = "correct-horse-battery-staple";
    const ENV: &str = "CC_TEST_DATABASE_URL";

    // ---- helpers ---------------------------------------------------------

    fn svc(pool: PgPool, bootstrap: Option<BootstrapToken>) -> Services {
        // Generous limits: the concurrency tests send many requests from one
        // (unknown) client address; rate limiting is tested in auth.rs.
        let settings = AuthSettings {
            ip_limit: 10_000,
            pwchange_limit: 10_000,
            ..AuthSettings::default()
        };
        Services::new(pool, bootstrap, settings)
    }

    fn app(setup: Option<Services>) -> Router {
        router_with_services(
            Capabilities::development_defaults(),
            &ApiConfig::default(),
            setup,
        )
    }

    fn lazy_pool() -> PgPool {
        PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(200))
            .connect_lazy("postgres://nobody:nopass@127.0.0.1:1/none")
            .unwrap()
    }

    fn valid_body() -> Value {
        json!({
            "bootstrap_token": TOKEN,
            "email": "Operator@Example.Test",
            "display_name": "The Operator",
            "password": PASSWORD,
            "organization": { "name": "First Org", "slug": "first-org" }
        })
    }

    fn post(body: &Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/v1/setup/complete")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Value, String) {
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            text,
        )
    }

    fn code(value: &Value) -> &str {
        value["error"]["code"].as_str().unwrap_or("")
    }

    /// Collects tracing output of the current thread.
    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl io::Write for LogBuffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for LogBuffer {
        type Writer = LogBuffer;
        fn make_writer(&'a self) -> LogBuffer {
            self.clone()
        }
    }

    impl LogBuffer {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    /// One process-wide capture buffer. A thread-local subscriber is racy when
    /// tests run in parallel (callsite interest caching), so a global
    /// subscriber is installed once; checks use values unique to each test.
    fn capture_logs() -> (LogBuffer, ()) {
        static LOGS: std::sync::OnceLock<LogBuffer> = std::sync::OnceLock::new();
        let buffer = LOGS.get_or_init(|| {
            let buffer = LogBuffer::default();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(buffer.clone())
                .with_max_level(tracing::Level::TRACE)
                .finish();
            let _ = tracing::subscriber::set_global_default(subscriber);
            buffer
        });
        (buffer.clone(), ())
    }

    // ---- no database needed ----------------------------------------------

    #[tokio::test]
    async fn without_database_setup_is_unavailable() {
        let app = app(None);
        let (status, body, _) = send(&app, get("/api/v1/setup/status")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(code(&body), "SETUP_UNAVAILABLE");
        let (status, body, _) = send(&app, post(&valid_body())).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(code(&body), "SETUP_UNAVAILABLE");
    }

    #[tokio::test]
    async fn without_bootstrap_secret_the_answer_equals_a_wrong_token() {
        let (logs, _guard) = capture_logs();
        let disabled = app(Some(svc(lazy_pool(), None)));
        let (status, disabled_body, _) = send(&disabled, post(&valid_body())).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(code(&disabled_body), "INVALID_BOOTSTRAP_TOKEN");

        let configured = app(Some(svc(
            lazy_pool(),
            Some(BootstrapToken::new("another-bootstrap-token-0123456789abcdef").unwrap()),
        )));
        let (status, wrong_body, _) = send(&configured, post(&valid_body())).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // Indistinguishable for the caller except for the request id.
        assert_eq!(disabled_body["error"]["code"], wrong_body["error"]["code"]);
        assert_eq!(
            disabled_body["error"]["message"],
            wrong_body["error"]["message"]
        );
        assert_eq!(
            disabled_body["error"].get("details"),
            wrong_body["error"].get("details")
        );

        // The operator can see the real reason in the log, without secrets.
        assert!(
            logs.text().contains("no bootstrap secret is configured"),
            "{}",
            logs.text()
        );
        assert!(!logs.text().contains(TOKEN));
    }

    #[tokio::test]
    async fn wrong_token_is_forbidden_and_never_logged() {
        let (logs, _guard) = capture_logs();
        let token = BootstrapToken::new(TOKEN).unwrap();
        let app = app(Some(svc(lazy_pool(), Some(token))));
        let mut body = valid_body();
        body["bootstrap_token"] = json!("wrong-token-wrong-token-wrong-token-xx");
        body["password"] = json!("distinctive-password-do-not-log");
        let (status, value, text) = send(&app, post(&body)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(code(&value), "INVALID_BOOTSTRAP_TOKEN");
        assert!(value["error"]["request_id"].is_string());
        // Capture must work, otherwise the absence checks below prove nothing.
        assert!(
            logs.text().contains("/api/v1/setup/complete"),
            "request span was not captured: {}",
            logs.text()
        );
        for secret in ["wrong-token-wrong", "distinctive-password", TOKEN] {
            assert!(!text.contains(secret));
            assert!(!logs.text().contains(secret), "{secret} leaked into logs");
        }
    }

    #[tokio::test]
    async fn malformed_requests_are_rejected_without_echoing_values() {
        let token = BootstrapToken::new(TOKEN).unwrap();
        let app = app(Some(svc(lazy_pool(), Some(token))));

        let not_json = Request::builder()
            .method("POST")
            .uri("/api/v1/setup/complete")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{\"password\": \"leak-me-please"))
            .unwrap();
        let (status, body, text) = send(&app, not_json).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(code(&body), "VALIDATION_ERROR");
        assert!(!text.contains("leak-me-please"));

        let mut extra = valid_body();
        extra["is_admin"] = json!("sneaky-extra-value");
        let (status, body, text) = send(&app, post(&extra)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(code(&body), "VALIDATION_ERROR");
        assert!(!text.contains("sneaky-extra-value"));

        // Unparsable bodies are answered before (and independent of) the
        // token check and carry no details.
        let unconfigured = router_with_services(
            Capabilities::development_defaults(),
            &ApiConfig::default(),
            Some(svc(lazy_pool(), None)),
        );
        let (status, body, _) = send(&unconfigured, post(&json!({ "password": "x" }))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(code(&body), "VALIDATION_ERROR");
        assert!(body["error"].get("details").is_none());

        let mut missing = valid_body();
        missing.as_object_mut().unwrap().remove("password");
        let (status, _, _) = send(&app, post(&missing)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        let wrong_type = Request::builder()
            .method("POST")
            .uri("/api/v1/setup/complete")
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from(valid_body().to_string()))
            .unwrap();
        let (status, body, _) = send(&app, wrong_type).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(code(&body), "UNSUPPORTED_MEDIA_TYPE");

        let mut big = valid_body();
        big["display_name"] = json!("x".repeat(70 * 1024));
        let (status, body, _) = send(&app, post(&big)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(code(&body), "PAYLOAD_TOO_LARGE");
    }

    #[tokio::test]
    async fn wrong_methods_on_setup_routes_are_405() {
        let app = app(None);
        let (status, _, _) = send(&app, get("/api/v1/setup/complete")).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/setup/status")
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = send(&app, request).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    // ---- PostgreSQL-backed -----------------------------------------------

    struct TestDb {
        name: String,
        admin: PgConnectOptions,
        pool: PgPool,
    }

    impl TestDb {
        async fn new() -> Self {
            let url = std::env::var(ENV).expect("CC_TEST_DATABASE_URL must be set");
            let admin = PgConnectOptions::from_str(&url).unwrap();
            let name = format!("cc_test_{}", Uuid::new_v4().simple());
            let mut conn = PgConnection::connect_with(&admin).await.unwrap();
            sqlx::query(&format!("CREATE DATABASE \"{name}\""))
                .execute(&mut conn)
                .await
                .unwrap();
            let pool = PgPoolOptions::new()
                .max_connections(30)
                .acquire_timeout(Duration::from_secs(20))
                .connect_with(admin.clone().database(&name))
                .await
                .unwrap();
            craftcloud_db::migrate::run(&pool).await.unwrap();
            Self { name, admin, pool }
        }

        fn app(&self) -> Router {
            let token = BootstrapToken::new(TOKEN).unwrap();
            app(Some(svc(self.pool.clone(), Some(token))))
        }

        async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
        }
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            let admin = self.admin.clone();
            let name = self.name.clone();
            let _ = std::thread::spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                rt.block_on(async {
                    if let Ok(mut conn) = PgConnection::connect_with(&admin).await {
                        let _ = sqlx::query(&format!(
                            "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
                        ))
                        .execute(&mut conn)
                        .await;
                    }
                });
            })
            .join();
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn setup_flow_status_wrong_token_success_and_repeat() {
        let (logs, _guard) = capture_logs();
        let db = TestDb::new().await;
        let app = db.app();

        let (status, body, _) = send(&app, get("/api/v1/setup/status")).await;
        assert_eq!(
            (status, &body["setup_required"]),
            (StatusCode::OK, &json!(true))
        );

        // Wrong token: 403 and nothing created.
        let mut wrong = valid_body();
        wrong["bootstrap_token"] = json!("wrong-token-wrong-token-wrong-token-xx");
        let (status, body, _) = send(&app, post(&wrong)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(code(&body), "INVALID_BOOTSTRAP_TOKEN");
        assert_eq!(db.count("SELECT count(*) FROM users").await, 0);

        // Invalid fields: 422 listing field names only, nothing created.
        let mut invalid = valid_body();
        invalid["email"] = json!("not-an-email");
        invalid["password"] = json!("short");
        invalid["organization"]["slug"] = json!("Bad Slug!");
        let (status, body, text) = send(&app, post(&invalid)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(code(&body), "VALIDATION_ERROR");
        assert_eq!(
            body["error"]["details"]["fields"],
            json!(["email", "password", "organization.slug"])
        );
        assert!(!text.contains("not-an-email") && !text.contains("Bad Slug"));
        assert_eq!(db.count("SELECT count(*) FROM users").await, 0);

        // Password equal to the e-mail local part is rejected.
        let mut same = valid_body();
        same["password"] = json!("LongLocalPart");
        same["email"] = json!("longlocalpart@example.test");
        let (status, body, _) = send(&app, post(&same)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"]["details"]["fields"], json!(["password"]));

        // Success.
        let (status, body, _) = send(&app, post(&valid_body())).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let user_id = Uuid::parse_str(body["user_id"].as_str().unwrap()).unwrap();
        let org_id = Uuid::parse_str(body["organization_id"].as_str().unwrap()).unwrap();

        let row = sqlx::query(
            "SELECT normalized_email, display_name, instance_role, password_hash FROM users WHERE id = $1",
        )
        .bind(user_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            row.get::<String, _>("normalized_email"),
            "operator@example.test"
        );
        assert_eq!(row.get::<String, _>("display_name"), "The Operator");
        assert_eq!(
            row.get::<Option<String>, _>("instance_role").as_deref(),
            Some("operator")
        );
        let hash = row.get::<Option<String>, _>("password_hash").unwrap();
        assert!(hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert!(verify_password(&hash, PASSWORD));
        assert!(!verify_password(&hash, "another-password-entirely"));
        assert!(!hash.contains(PASSWORD));

        assert_eq!(db.count("SELECT count(*) FROM users").await, 1);
        assert_eq!(db.count("SELECT count(*) FROM organizations").await, 1);
        assert_eq!(db.count("SELECT count(*) FROM instance_setup").await, 1);
        let membership = sqlx::query(
            "SELECT role, status FROM organization_members WHERE org_id = $1 AND user_id = $2",
        )
        .bind(org_id)
        .bind(user_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(membership.get::<String, _>("role"), "owner");
        assert_eq!(membership.get::<String, _>("status"), "active");

        // Closed afterwards: valid token -> 409, wrong token still 403.
        let (status, body, _) = send(&app, post(&valid_body())).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(code(&body), "SETUP_ALREADY_COMPLETED");
        let (status, body, _) = send(&app, post(&wrong)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(code(&body), "INVALID_BOOTSTRAP_TOKEN");
        let (_, body, _) = send(&app, get("/api/v1/setup/status")).await;
        assert_eq!(body["setup_required"], json!(false));
        assert_eq!(db.count("SELECT count(*) FROM users").await, 1);

        // Neither secret reached the logs (and the capture itself works).
        assert!(
            logs.text().contains("instance setup completed"),
            "success log line not captured: {}",
            logs.text()
        );
        for secret in [TOKEN, PASSWORD, "wrong-token-wrong"] {
            assert!(!logs.text().contains(secret), "{secret} leaked into logs");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn twenty_concurrent_requests_create_exactly_one_operator() {
        let db = TestDb::new().await;
        let app = db.app();
        let mut handles = Vec::new();
        for i in 0..20 {
            let app = app.clone();
            handles.push(tokio::spawn(async move {
                let mut body = valid_body();
                body["email"] = json!(format!("operator{i}@example.test"));
                body["organization"]["slug"] = json!(format!("org-{i}"));
                send(&app, post(&body)).await
            }));
        }
        let mut created = 0;
        let mut conflicts = 0;
        for handle in handles {
            let (status, body, _) = handle.await.unwrap();
            match status {
                StatusCode::CREATED => created += 1,
                StatusCode::CONFLICT => {
                    assert_eq!(code(&body), "SETUP_ALREADY_COMPLETED");
                    conflicts += 1;
                }
                other => panic!("unexpected status {other}: {body}"),
            }
        }
        assert_eq!((created, conflicts), (1, 19));
        assert_eq!(db.count("SELECT count(*) FROM users").await, 1);
        assert_eq!(db.count("SELECT count(*) FROM organizations").await, 1);
        assert_eq!(
            db.count("SELECT count(*) FROM organization_members").await,
            1
        );
        assert_eq!(db.count("SELECT count(*) FROM instance_setup").await, 1);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn failure_inside_the_transaction_leaves_nothing_behind() {
        let db = TestDb::new().await;
        // An unrelated organization already owns the slug, so the insert fails
        // after the singleton row and the operator were written in the tx.
        let other = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, normalized_email, display_name) VALUES ($1, 'x@example.test', 'X')")
            .bind(other)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO organizations (id, slug, name, owner_user_id) VALUES ($1, 'first-org', 'Taken', $2)")
            .bind(Uuid::new_v4())
            .bind(other)
            .execute(&db.pool)
            .await
            .unwrap();
        let app = db.app();

        let (status, body, text) = send(&app, post(&valid_body())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            body["error"]["details"]["fields"],
            json!(["organization.slug"])
        );
        assert!(!text.contains("Taken"));
        assert_eq!(
            db.count("SELECT count(*) FROM users WHERE instance_role = 'operator'")
                .await,
            0
        );
        assert_eq!(db.count("SELECT count(*) FROM users").await, 1);
        assert_eq!(db.count("SELECT count(*) FROM instance_setup").await, 0);
        let (_, body, _) = send(&app, get("/api/v1/setup/status")).await;
        assert_eq!(body["setup_required"], json!(true));

        // Retry with a free slug succeeds: the failed attempt closed nothing.
        let mut retry = valid_body();
        retry["organization"]["slug"] = json!("free-slug");
        let (status, _, _) = send(&app, post(&retry)).await;
        assert_eq!(status, StatusCode::CREATED);
    }
}
