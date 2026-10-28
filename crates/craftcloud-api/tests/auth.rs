//! HTTP-level tests of authentication: login, sessions, CSRF, logout,
//! password change, password reset and rate limiting (CC-F02).
//!
//! Tests that need PostgreSQL are marked `#[ignore]` (`cargo xtask db-test`).
//! A manual clock replaces the system clock so expiry can be tested.

#[cfg(test)]
mod tests {
    use std::io;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use argon2::password_hash::{PasswordHasher, SaltString};
    use argon2::{Algorithm, Argon2, Params, Version};
    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request, StatusCode, header};
    use craftcloud_api::{ApiConfig, AuthSettings, Clock, Services, router_with_services};
    use craftcloud_auth::{BootstrapToken, hash_password, verify_password};
    use craftcloud_core::Capabilities;
    use craftcloud_db::PgPool;
    use craftcloud_db::setup::{self, NewInstance};
    use serde_json::{Value, json};
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use sqlx::{Connection, PgConnection};
    use time::OffsetDateTime;
    use tower::ServiceExt;
    use tracing_subscriber::fmt::MakeWriter;
    use uuid::Uuid;

    const ENV: &str = "CC_TEST_DATABASE_URL";
    const ORIGIN: &str = "https://cloud.test";
    const COOKIE_NAME: &str = "__Host-craftcloud_session";
    const PASSWORD: &str = "correct-horse-battery-staple";

    // ---- helpers ---------------------------------------------------------

    struct ManualClock(Mutex<OffsetDateTime>);

    impl ManualClock {
        fn new() -> Arc<Self> {
            Arc::new(Self(Mutex::new(OffsetDateTime::now_utc())))
        }
        fn advance(&self, by: Duration) {
            let mut now = self.0.lock().unwrap();
            *now += by;
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> OffsetDateTime {
            *self.0.lock().unwrap()
        }
    }

    fn settings() -> AuthSettings {
        AuthSettings {
            public_url: Some(ORIGIN.to_owned()),
            public_origin: Some(ORIGIN.to_owned()),
            cookie_secure: true,
            // Generous by default; rate limit tests lower them.
            ip_limit: 10_000,
            pwchange_limit: 10_000,
            failed_login_pair_limit: 10_000,
            failed_login_account_limit: 10_000,
            ..AuthSettings::default()
        }
    }

    fn router(services: Option<Services>, config: &ApiConfig) -> Router {
        router_with_services(Capabilities::development_defaults(), config, services)
    }

    fn lazy_pool() -> PgPool {
        PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(200))
            .connect_lazy("postgres://nobody:nopass@127.0.0.1:1/none")
            .unwrap()
    }

    struct Req {
        method: &'static str,
        uri: String,
        body: Option<Value>,
        cookie: Option<String>,
        csrf: Option<String>,
        origin: Option<String>,
        sec_fetch_site: Option<String>,
        peer: Option<std::net::SocketAddr>,
    }

    impl Req {
        fn new(method: &'static str, uri: &str) -> Self {
            Self {
                method,
                uri: uri.to_owned(),
                body: None,
                cookie: None,
                csrf: None,
                origin: Some(ORIGIN.to_owned()),
                sec_fetch_site: None,
                peer: None,
            }
        }
        /// Simulates the TCP peer address (what the server gets from the socket).
        fn via_peer(mut self, ip: &str) -> Self {
            self.peer = Some(std::net::SocketAddr::new(ip.parse().unwrap(), 40_000));
            self
        }
        fn json(mut self, body: Value) -> Self {
            self.body = Some(body);
            self
        }
        fn session(mut self, s: &Session) -> Self {
            self.cookie = Some(format!("{COOKIE_NAME}={}", s.token));
            self.csrf = Some(s.csrf.clone());
            self
        }
        fn cookie_only(mut self, s: &Session) -> Self {
            self.cookie = Some(format!("{COOKIE_NAME}={}", s.token));
            self
        }
        fn csrf(mut self, value: &str) -> Self {
            self.csrf = Some(value.to_owned());
            self
        }
        fn origin(mut self, value: Option<&str>) -> Self {
            self.origin = value.map(str::to_owned);
            self
        }
        fn fetch_site(mut self, value: &str) -> Self {
            self.sec_fetch_site = Some(value.to_owned());
            self
        }
        fn build(self) -> Request<Body> {
            let mut builder = Request::builder().method(self.method).uri(&self.uri);
            if let Some(cookie) = &self.cookie {
                builder = builder.header(header::COOKIE, cookie);
            }
            if let Some(csrf) = &self.csrf {
                builder = builder.header("x-csrf-token", csrf);
            }
            if let Some(origin) = &self.origin {
                builder = builder.header(header::ORIGIN, origin);
            }
            if let Some(site) = &self.sec_fetch_site {
                builder = builder.header("sec-fetch-site", site);
            }
            let mut request = match self.body {
                Some(body) => builder
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
                None => builder.body(Body::empty()).unwrap(),
            };
            if let Some(peer) = self.peer {
                request
                    .extensions_mut()
                    .insert(axum::extract::ConnectInfo(peer));
            }
            request
        }
    }

    struct Resp {
        status: StatusCode,
        headers: HeaderMap,
        body: Value,
        text: String,
    }

    impl Resp {
        fn code(&self) -> &str {
            self.body["error"]["code"].as_str().unwrap_or("")
        }
    }

    async fn call(app: &Router, req: Req) -> Resp {
        let response = app.clone().oneshot(req.build()).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        Resp {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            text: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    #[derive(Clone)]
    struct Session {
        token: String,
        csrf: String,
    }

    fn session_from(resp: &Resp) -> Session {
        let set_cookie = resp
            .headers
            .get(header::SET_COOKIE)
            .expect("Set-Cookie missing")
            .to_str()
            .unwrap();
        let value = set_cookie
            .strip_prefix(&format!("{COOKIE_NAME}="))
            .and_then(|rest| rest.split(';').next())
            .unwrap();
        Session {
            token: value.to_owned(),
            csrf: resp.body["csrf_token"].as_str().unwrap().to_owned(),
        }
    }

    // ---- log capture -------------------------------------------------------

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

    // ---- no database needed ------------------------------------------------

    #[tokio::test]
    async fn without_database_auth_routes_answer_503() {
        let app = router(None, &ApiConfig::default());
        let login = call(
            &app,
            Req::new("POST", "/api/v1/auth/login").json(json!({"email":"a@b.test","password":"x"})),
        )
        .await;
        assert_eq!(
            (login.status, login.code()),
            (StatusCode::SERVICE_UNAVAILABLE, "AUTH_UNAVAILABLE")
        );
        let session = call(&app, Req::new("GET", "/api/v1/auth/session")).await;
        assert_eq!(session.code(), "AUTH_UNAVAILABLE");
        let logout = call(&app, Req::new("POST", "/api/v1/auth/logout")).await;
        assert_eq!(logout.code(), "AUTH_UNAVAILABLE");
    }

    #[tokio::test]
    async fn ip_rate_limit_applies_before_any_work_with_retry_after() {
        let clock = ManualClock::new();
        let mut s = settings();
        s.ip_limit = 2;
        let services = Services::new(lazy_pool(), None, s).with_clock(clock.clone());
        let app = router(Some(services), &ApiConfig::default());
        let attempt = || {
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"someone@example.test","password":"whatever-password"}))
        };
        for _ in 0..2 {
            let r = call(&app, attempt()).await;
            assert_ne!(r.status, StatusCode::TOO_MANY_REQUESTS);
        }
        let limited = call(&app, attempt()).await;
        assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(limited.code(), "RATE_LIMITED");
        let retry: u64 = limited.headers[header::RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((1..=60).contains(&retry));
        assert!(limited.body["error"]["request_id"].is_string());

        // The setup and reset routes use their own per-IP buckets.
        let setup = call(
            &app,
            Req::new("POST", "/api/v1/setup/complete").json(json!({})),
        )
        .await;
        assert_ne!(setup.status, StatusCode::TOO_MANY_REQUESTS);
        for _ in 0..2 {
            let _ = call(
                &app,
                Req::new("POST", "/api/v1/auth/password-reset")
                    .json(json!({"token":"x","new_password":"y"})),
            )
            .await;
        }
        let reset = call(
            &app,
            Req::new("POST", "/api/v1/auth/password-reset")
                .json(json!({"token":"x","new_password":"y"})),
        )
        .await;
        assert_eq!(reset.status, StatusCode::TOO_MANY_REQUESTS);

        // A later window allows requests again.
        clock.advance(Duration::from_secs(61));
        let again = call(&app, attempt()).await;
        assert_ne!(again.status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn setup_route_is_rate_limited_per_ip() {
        let mut s = settings();
        s.ip_limit = 1;
        let token = BootstrapToken::new("test-bootstrap-token-0123456789-abcdefghij").unwrap();
        let services = Services::new(lazy_pool(), Some(token), s);
        let app = router(Some(services), &ApiConfig::default());
        let body = json!({"bootstrap_token":"wrong-wrong-wrong-wrong-wrong-wrong-xx","email":"a@b.test","display_name":"A","password":"long-enough-password","organization":{"name":"O","slug":"o"}});
        let first = call(
            &app,
            Req::new("POST", "/api/v1/setup/complete").json(body.clone()),
        )
        .await;
        assert_eq!(first.status, StatusCode::FORBIDDEN);
        let second = call(&app, Req::new("POST", "/api/v1/setup/complete").json(body)).await;
        assert_eq!(second.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(second.headers.contains_key(header::RETRY_AFTER));
    }

    #[tokio::test]
    async fn session_routes_without_cookie_are_401_even_for_unsafe_methods() {
        let services = Services::new(lazy_pool(), None, settings());
        let app = router(Some(services), &ApiConfig::default());
        for (method, uri) in [
            ("GET", "/api/v1/auth/session"),
            ("POST", "/api/v1/auth/logout"),
            ("POST", "/api/v1/auth/logout-all"),
            ("POST", "/api/v1/auth/password/change"),
            (
                "POST",
                "/api/v1/admin/users/00000000-0000-0000-0000-000000000000/password-reset",
            ),
        ] {
            let r = call(&app, Req::new(method, uri)).await;
            assert_eq!(
                (r.status, r.code()),
                (StatusCode::UNAUTHORIZED, "UNAUTHENTICATED"),
                "{uri}"
            );
            // Garbage and duplicated cookies are no session either.
            let mut req = Req::new(method, uri);
            req.cookie = Some(format!("{COOKIE_NAME}=garbage"));
            assert_eq!(call(&app, req).await.status, StatusCode::UNAUTHORIZED);
        }
    }

    // ---- PostgreSQL-backed -------------------------------------------------

    struct TestEnv {
        name: String,
        admin: PgConnectOptions,
        pool: PgPool,
        clock: Arc<ManualClock>,
        app: Router,
    }

    impl TestEnv {
        async fn new() -> Self {
            Self::with(settings(), ApiConfig::default()).await
        }

        async fn with(settings: AuthSettings, config: ApiConfig) -> Self {
            let url = std::env::var(ENV).expect("CC_TEST_DATABASE_URL must be set");
            let admin = PgConnectOptions::from_str(&url).unwrap();
            let name = format!("cc_test_{}", Uuid::new_v4().simple());
            let mut conn = PgConnection::connect_with(&admin).await.unwrap();
            sqlx::query(&format!("CREATE DATABASE \"{name}\""))
                .execute(&mut conn)
                .await
                .unwrap();
            let pool = PgPoolOptions::new()
                .max_connections(20)
                .acquire_timeout(Duration::from_secs(20))
                .connect_with(admin.clone().database(&name))
                .await
                .unwrap();
            craftcloud_db::migrate::run(&pool).await.unwrap();
            let clock = ManualClock::new();
            let services = Services::new(pool.clone(), None, settings).with_clock(clock.clone());
            let app = router(Some(services), &config);
            Self {
                name,
                admin,
                pool,
                clock,
                app,
            }
        }

        async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
        }

        async fn user(&self, email: &str, password: Option<&str>) -> Uuid {
            let id = Uuid::new_v4();
            let hash = password.map(|p| hash_password(p).unwrap());
            sqlx::query(
                "INSERT INTO users (id, normalized_email, display_name, password_hash) VALUES ($1, $2, 'Test User', $3)",
            )
            .bind(id)
            .bind(email)
            .bind(hash)
            .execute(&self.pool)
            .await
            .unwrap();
            id
        }

        async fn operator(&self, email: &str, password: &str) -> Uuid {
            let created = setup::complete(
                &self.pool,
                &NewInstance {
                    operator_email: email.to_owned(),
                    operator_display_name: "Operator".to_owned(),
                    operator_password_hash: hash_password(password).unwrap(),
                    organization_name: "Org".to_owned(),
                    organization_slug: "org".to_owned(),
                },
            )
            .await
            .unwrap();
            created.user_id
        }

        async fn login(&self, email: &str, password: &str) -> Resp {
            call(
                &self.app,
                Req::new("POST", "/api/v1/auth/login")
                    .json(json!({"email": email, "password": password})),
            )
            .await
        }

        async fn login_ok(&self, email: &str, password: &str) -> Session {
            let r = self.login(email, password).await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.text);
            session_from(&r)
        }

        async fn get_session(&self, s: &Session) -> Resp {
            call(
                &self.app,
                Req::new("GET", "/api/v1/auth/session").cookie_only(s),
            )
            .await
        }
    }

    impl Drop for TestEnv {
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
    async fn login_succeeds_with_a_hardened_cookie_and_stores_only_a_hash() {
        let env = TestEnv::new().await;
        let user_id = env.user("alice@example.test", Some(PASSWORD)).await;
        // E-mail is normalized like at registration.
        let r = env.login("  Alice@Example.TEST ", PASSWORD).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        assert_eq!(r.body["user_id"], json!(user_id.to_string()));
        assert!(r.body["expires_at"].as_str().unwrap().contains('T'));
        let s = session_from(&r);
        assert_eq!(s.token.len(), 43);

        let cookie = r.headers[header::SET_COOKIE].to_str().unwrap();
        for attribute in [
            "Path=/",
            "HttpOnly",
            "SameSite=Lax",
            "Secure",
            "Max-Age=604800",
        ] {
            assert!(cookie.contains(attribute), "{attribute} missing: {cookie}");
        }
        assert!(!cookie.contains("Domain"));
        assert_eq!(r.headers[header::CACHE_CONTROL], "no-store");

        // The database holds only the SHA-256 hash, never the token.
        let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM sessions")
            .fetch_one(&env.pool)
            .await
            .unwrap();
        assert_eq!(stored.len(), 32);
        assert_ne!(stored, s.token.as_bytes());
        assert_eq!(env.count("SELECT count(*) FROM sessions").await, 1);
        let dump: String = sqlx::query_scalar("SELECT s::text FROM sessions s")
            .fetch_one(&env.pool)
            .await
            .unwrap();
        assert!(!dump.contains(&s.token));
        // The CSRF token is not stored either.
        assert!(!dump.contains(&s.csrf));

        let info = env.get_session(&s).await;
        assert_eq!(info.status, StatusCode::OK);
        assert_eq!(info.body["email"], "alice@example.test");
        assert_eq!(info.body["csrf_token"], json!(s.csrf));
        assert!(info.body["instance_role"].is_null());
        assert!(info.headers.get(header::SET_COOKIE).is_none());
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn every_login_failure_looks_identical() {
        let env = TestEnv::new().await;
        env.user("bob@example.test", Some(PASSWORD)).await;
        env.user("nopass@example.test", None).await;
        let disabled = env.user("gone@example.test", Some(PASSWORD)).await;
        sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
            .bind(disabled)
            .execute(&env.pool)
            .await
            .unwrap();

        let cases = [
            ("unknown@example.test", PASSWORD),
            ("bob@example.test", "wrong-password-entirely"),
            ("nopass@example.test", PASSWORD),
            ("gone@example.test", PASSWORD),
            ("not-an-email", PASSWORD),
            ("", ""),
        ];
        let mut seen = Vec::new();
        for (email, password) in cases {
            let r = env.login(email, password).await;
            assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{email}");
            assert_eq!(r.code(), "INVALID_CREDENTIALS", "{email}");
            assert!(r.headers.get(header::SET_COOKIE).is_none(), "{email}");
            seen.push(r.body["error"]["message"].clone());
            assert!(!r.text.contains(email) || email.is_empty());
        }
        assert!(seen.windows(2).all(|w| w[0] == w[1]));
        assert_eq!(env.count("SELECT count(*) FROM sessions").await, 0);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn login_rejects_foreign_origins_and_cross_site_fetches() {
        let env = TestEnv::new().await;
        env.user("carol@example.test", Some(PASSWORD)).await;
        let body = json!({"email":"carol@example.test","password":PASSWORD});
        for req in [
            Req::new("POST", "/api/v1/auth/login")
                .json(body.clone())
                .origin(Some("https://evil.example")),
            Req::new("POST", "/api/v1/auth/login")
                .json(body.clone())
                .origin(Some("null")),
            Req::new("POST", "/api/v1/auth/login")
                .json(body.clone())
                .origin(None)
                .fetch_site("cross-site"),
        ] {
            let r = call(&env.app, req).await;
            assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, "CSRF_FAILED"));
        }
        // Non-browser clients (no Origin, no Sec-Fetch-Site) are fine.
        let ok = call(
            &env.app,
            Req::new("POST", "/api/v1/auth/login")
                .json(body)
                .origin(None),
        )
        .await;
        assert_eq!(ok.status, StatusCode::OK);
        assert_eq!(env.count("SELECT count(*) FROM sessions").await, 1);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn invalid_sessions_are_401() {
        let env = TestEnv::new().await;
        env.user("dave@example.test", Some(PASSWORD)).await;
        let s = env.login_ok("dave@example.test", PASSWORD).await;
        assert_eq!(env.get_session(&s).await.status, StatusCode::OK);

        let unknown = Session {
            token: "A".repeat(43),
            csrf: String::new(),
        };
        for bad in [
            &unknown,
            &Session {
                token: "short".into(),
                csrf: String::new(),
            },
        ] {
            let r = env.get_session(bad).await;
            assert_eq!(
                (r.status, r.code()),
                (StatusCode::UNAUTHORIZED, "UNAUTHENTICATED")
            );
        }
        let none = call(&env.app, Req::new("GET", "/api/v1/auth/session")).await;
        assert_eq!(none.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn idle_timeout_expires_sessions_and_activity_extends_them() {
        let env = TestEnv::new().await;
        env.user("erin@example.test", Some(PASSWORD)).await;
        let s = env.login_ok("erin@example.test", PASSWORD).await;

        // Activity every 7 hours keeps the session alive (8 h idle limit).
        for _ in 0..3 {
            env.clock.advance(Duration::from_secs(7 * 3600));
            assert_eq!(env.get_session(&s).await.status, StatusCode::OK);
        }
        // Idle for just over 8 hours: expired.
        env.clock.advance(Duration::from_secs(8 * 3600 + 1));
        let r = env.get_session(&s).await;
        assert_eq!(
            (r.status, r.code()),
            (StatusCode::UNAUTHORIZED, "UNAUTHENTICATED")
        );
        // It stays expired.
        assert_eq!(env.get_session(&s).await.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn absolute_timeout_ends_sessions_despite_activity() {
        let env = TestEnv::new().await;
        env.user("frank@example.test", Some(PASSWORD)).await;
        let s = env.login_ok("frank@example.test", PASSWORD).await;
        // 27 x 6 h = 162 h < 168 h: still valid thanks to the activity.
        for _ in 0..27 {
            env.clock.advance(Duration::from_secs(6 * 3600));
            assert_eq!(env.get_session(&s).await.status, StatusCode::OK);
        }
        env.clock.advance(Duration::from_secs(6 * 3600));
        let r = env.get_session(&s).await;
        assert_eq!(
            (r.status, r.code()),
            (StatusCode::UNAUTHORIZED, "UNAUTHENTICATED")
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn csrf_header_and_origin_are_both_required_for_unsafe_methods() {
        let env = TestEnv::new().await;
        env.user("gina@example.test", Some(PASSWORD)).await;
        env.user("hank@example.test", Some(PASSWORD)).await;
        let s = env.login_ok("gina@example.test", PASSWORD).await;
        let other = env.login_ok("hank@example.test", PASSWORD).await;
        let logout = || Req::new("POST", "/api/v1/auth/logout");

        let failures = [
            logout().cookie_only(&s),                                   // header missing
            logout().cookie_only(&s).csrf("wrong"),                     // wrong header
            logout().cookie_only(&s).csrf(&other.csrf),                 // another session's token
            logout().cookie_only(&s).csrf(&s.token),                    // the session token itself
            logout().session(&s).origin(Some("https://evil.example")),  // bad Origin
            logout().session(&s).origin(Some("http://cloud.test")),     // wrong scheme
            logout().session(&s).origin(None).fetch_site("cross-site"), // cross-site, no Origin
        ];
        for req in failures {
            let r = call(&env.app, req).await;
            assert_eq!(
                (r.status, r.code()),
                (StatusCode::FORBIDDEN, "CSRF_FAILED"),
                "{}",
                r.text
            );
        }
        // Nothing was revoked by the failed attempts.
        assert_eq!(env.get_session(&s).await.status, StatusCode::OK);
        // Safe methods need no CSRF header.
        assert_eq!(env.get_session(&s).await.status, StatusCode::OK);
        // Without any cookie the answer stays 401 (not 403).
        let anon = call(&env.app, logout().csrf("anything")).await;
        assert_eq!(anon.status, StatusCode::UNAUTHORIZED);

        // Correct header + same-origin: allowed (with and without Origin).
        let ok = call(&env.app, logout().session(&other).origin(None)).await;
        assert_eq!(ok.status, StatusCode::NO_CONTENT);
        let ok = call(&env.app, logout().session(&s).fetch_site("same-origin")).await;
        assert_eq!(ok.status, StatusCode::NO_CONTENT);
        let cleared = ok.headers[header::SET_COOKIE].to_str().unwrap();
        assert!(cleared.starts_with(&format!("{COOKIE_NAME}=;")) && cleared.contains("Max-Age=0"));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn logout_revokes_the_session_and_logout_all_revokes_every_session_of_the_user() {
        let env = TestEnv::new().await;
        env.user("ivy@example.test", Some(PASSWORD)).await;
        env.user("jack@example.test", Some(PASSWORD)).await;
        let a = env.login_ok("ivy@example.test", PASSWORD).await;
        let b = env.login_ok("ivy@example.test", PASSWORD).await;
        let c = env.login_ok("ivy@example.test", PASSWORD).await;
        let jack = env.login_ok("jack@example.test", PASSWORD).await;

        let r = call(
            &env.app,
            Req::new("POST", "/api/v1/auth/logout").session(&a),
        )
        .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT);
        assert_eq!(env.get_session(&a).await.status, StatusCode::UNAUTHORIZED);
        assert_eq!(env.get_session(&b).await.status, StatusCode::OK);
        // Replaying a revoked session cannot log out others.
        let replay = call(
            &env.app,
            Req::new("POST", "/api/v1/auth/logout").session(&a),
        )
        .await;
        assert_eq!(replay.status, StatusCode::UNAUTHORIZED);

        let r = call(
            &env.app,
            Req::new("POST", "/api/v1/auth/logout-all").session(&b),
        )
        .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT);
        assert_eq!(env.get_session(&b).await.status, StatusCode::UNAUTHORIZED);
        assert_eq!(env.get_session(&c).await.status, StatusCode::UNAUTHORIZED);
        assert_eq!(env.get_session(&jack).await.status, StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn disabling_an_account_invalidates_its_sessions() {
        let env = TestEnv::new().await;
        let id = env.user("kim@example.test", Some(PASSWORD)).await;
        let s = env.login_ok("kim@example.test", PASSWORD).await;
        sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
            .bind(id)
            .execute(&env.pool)
            .await
            .unwrap();
        assert_eq!(env.get_session(&s).await.status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            env.login("kim@example.test", PASSWORD).await.status,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn password_change_requires_the_current_password_and_revokes_other_sessions() {
        let env = TestEnv::new().await;
        env.user("lena@example.test", Some(PASSWORD)).await;
        let current = env.login_ok("lena@example.test", PASSWORD).await;
        let other = env.login_ok("lena@example.test", PASSWORD).await;
        let change = |body: Value| {
            Req::new("POST", "/api/v1/auth/password/change")
                .session(&current)
                .json(body)
        };

        let wrong = call(&env.app, change(json!({"current_password":"nope-nope-nope-nope","new_password":"brand-new-password-1"}))).await;
        assert_eq!(
            (wrong.status, wrong.code()),
            (StatusCode::FORBIDDEN, "INVALID_CURRENT_PASSWORD")
        );
        assert_eq!(env.get_session(&other).await.status, StatusCode::OK);

        let weak = call(
            &env.app,
            change(json!({"current_password":PASSWORD,"new_password":"short"})),
        )
        .await;
        assert_eq!(weak.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            weak.body["error"]["details"]["fields"],
            json!(["new_password"])
        );
        assert!(!weak.text.contains("short"));

        // CSRF is enforced on the change itself.
        let no_csrf = call(
            &env.app,
            Req::new("POST", "/api/v1/auth/password/change")
                .cookie_only(&current)
                .json(json!({"current_password":PASSWORD,"new_password":"brand-new-password-1"})),
        )
        .await;
        assert_eq!(no_csrf.code(), "CSRF_FAILED");

        let ok = call(
            &env.app,
            change(json!({"current_password":PASSWORD,"new_password":"brand-new-password-1"})),
        )
        .await;
        assert_eq!(ok.status, StatusCode::NO_CONTENT, "{}", ok.text);
        assert_eq!(env.get_session(&current).await.status, StatusCode::OK);
        assert_eq!(
            env.get_session(&other).await.status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            env.login("lena@example.test", PASSWORD).await.status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            env.login("lena@example.test", "brand-new-password-1")
                .await
                .status,
            StatusCode::OK
        );
        let hash: String = sqlx::query_scalar(
            "SELECT password_hash FROM users WHERE normalized_email = 'lena@example.test'",
        )
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert!(hash.starts_with("$argon2id$") && verify_password(&hash, "brand-new-password-1"));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn login_upgrades_outdated_password_hashes() {
        let env = TestEnv::new().await;
        let id = env.user("max@example.test", None).await;
        // A valid Argon2id hash with weaker parameters than the current ones.
        let weak = Argon2::new(
            Algorithm::Argon2id,
            Version::V0x13,
            Params::new(8192, 1, 1, None).unwrap(),
        );
        let salt = SaltString::encode_b64(&[7u8; 16]).unwrap();
        let old = weak
            .hash_password(PASSWORD.as_bytes(), &salt)
            .unwrap()
            .to_string();
        assert!(old.contains("m=8192,t=1,p=1"));
        sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
            .bind(id)
            .bind(&old)
            .execute(&env.pool)
            .await
            .unwrap();

        // A wrong password does not trigger the upgrade.
        assert_eq!(
            env.login("max@example.test", "wrong-password-entirely")
                .await
                .status,
            StatusCode::UNAUTHORIZED
        );
        let unchanged: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
            .bind(id)
            .fetch_one(&env.pool)
            .await
            .unwrap();
        assert_eq!(unchanged, old);

        env.login_ok("max@example.test", PASSWORD).await;
        let upgraded: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
            .bind(id)
            .fetch_one(&env.pool)
            .await
            .unwrap();
        assert!(
            upgraded.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "{upgraded}"
        );
        assert!(verify_password(&upgraded, PASSWORD));
        env.login_ok("max@example.test", PASSWORD).await;
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn password_reset_link_is_operator_only_single_use_and_expires() {
        let env = TestEnv::new().await;
        let operator_id = env.operator("op@example.test", PASSWORD).await;
        let victim_id = env.user("vera@example.test", Some(PASSWORD)).await;
        let normal_id = env.user("walt@example.test", Some(PASSWORD)).await;
        let op = env.login_ok("op@example.test", PASSWORD).await;
        let normal = env.login_ok("walt@example.test", PASSWORD).await;
        let victim_session = env.login_ok("vera@example.test", PASSWORD).await;
        let issue = |id: &str, s: &Session| {
            Req::new("POST", &format!("/api/v1/admin/users/{id}/password-reset")).session(s)
        };

        // Only the setup operator may issue links.
        let denied = call(&env.app, issue(&victim_id.to_string(), &normal)).await;
        assert_eq!(
            (denied.status, denied.code()),
            (StatusCode::FORBIDDEN, "FORBIDDEN")
        );
        assert_eq!(
            env.count("SELECT count(*) FROM password_reset_tokens")
                .await,
            0
        );
        let _ = normal_id;
        // CSRF applies to the admin route too.
        let no_csrf = call(
            &env.app,
            Req::new(
                "POST",
                &format!("/api/v1/admin/users/{victim_id}/password-reset"),
            )
            .cookie_only(&op),
        )
        .await;
        assert_eq!(no_csrf.code(), "CSRF_FAILED");
        // Unknown target / malformed id.
        let missing = call(&env.app, issue(&Uuid::new_v4().to_string(), &op)).await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        let malformed = call(&env.app, issue("not-a-uuid", &op)).await;
        assert_eq!(malformed.status, StatusCode::NOT_FOUND);

        let issued = call(&env.app, issue(&victim_id.to_string(), &op)).await;
        assert_eq!(issued.status, StatusCode::CREATED, "{}", issued.text);
        let url = issued.body["reset_url"].as_str().unwrap();
        let token = url
            .strip_prefix(&format!("{ORIGIN}/reset-password#token="))
            .expect(url);
        assert_eq!(token.len(), 43);
        assert!(issued.body["expires_at"].as_str().is_some());
        // Only the hash is stored; created_by is the operator.
        let (hash, created_by): (Vec<u8>, Option<Uuid>) =
            sqlx::query_as("SELECT token_hash, created_by_user_id FROM password_reset_tokens")
                .fetch_one(&env.pool)
                .await
                .unwrap();
        assert_eq!(hash.len(), 32);
        assert_eq!(created_by, Some(operator_id));
        let dump: String = sqlx::query_scalar("SELECT t::text FROM password_reset_tokens t")
            .fetch_one(&env.pool)
            .await
            .unwrap();
        assert!(!dump.contains(token));

        let reset = |tok: &str, pw: &str| {
            Req::new("POST", "/api/v1/auth/password-reset")
                .json(json!({"token": tok, "new_password": pw}))
        };

        // Garbage and unknown tokens look exactly the same.
        let garbage = call(&env.app, reset("garbage", "new-password-12345")).await;
        let unknown = call(&env.app, reset(&"B".repeat(43), "new-password-12345")).await;
        assert_eq!(
            (garbage.status, garbage.code()),
            (StatusCode::BAD_REQUEST, "INVALID_RESET_TOKEN")
        );
        assert_eq!(
            garbage.body["error"]["message"],
            unknown.body["error"]["message"]
        );
        assert_eq!(unknown.status, StatusCode::BAD_REQUEST);

        // A weak password does not burn the token.
        let weak = call(&env.app, reset(token, "short")).await;
        assert_eq!(weak.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            weak.body["error"]["details"]["fields"],
            json!(["new_password"])
        );

        let done = call(&env.app, reset(token, "new-password-12345")).await;
        assert_eq!(done.status, StatusCode::NO_CONTENT, "{}", done.text);
        // All sessions of the user are gone, the old password no longer works.
        assert_eq!(
            env.get_session(&victim_session).await.status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            env.login("vera@example.test", PASSWORD).await.status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            env.login("vera@example.test", "new-password-12345")
                .await
                .status,
            StatusCode::OK
        );
        // Other users' sessions are untouched.
        assert_eq!(env.get_session(&normal).await.status, StatusCode::OK);

        // Single use.
        let again = call(&env.app, reset(token, "another-password-123")).await;
        assert_eq!(
            (again.status, again.code()),
            (StatusCode::BAD_REQUEST, "INVALID_RESET_TOKEN")
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn password_reset_links_expire_after_one_hour() {
        let env = TestEnv::new().await;
        env.operator("op@example.test", PASSWORD).await;
        let victim = env.user("xena@example.test", Some(PASSWORD)).await;
        let op = env.login_ok("op@example.test", PASSWORD).await;
        let issued = call(
            &env.app,
            Req::new(
                "POST",
                &format!("/api/v1/admin/users/{victim}/password-reset"),
            )
            .session(&op),
        )
        .await;
        let url = issued.body["reset_url"].as_str().unwrap().to_owned();
        let token = url.split("token=").nth(1).unwrap().to_owned();
        let reset = |tok: &str| {
            Req::new("POST", "/api/v1/auth/password-reset")
                .json(json!({"token": tok, "new_password": "new-password-12345"}))
        };

        env.clock.advance(Duration::from_secs(3600 - 5));
        // Still valid shortly before the hour (peek only; do not consume yet).
        let weak = call(
            &env.app,
            Req::new("POST", "/api/v1/auth/password-reset")
                .json(json!({"token": token, "new_password": "x"})),
        )
        .await;
        assert_eq!(weak.status, StatusCode::UNPROCESSABLE_ENTITY);
        env.clock.advance(Duration::from_secs(6));
        let expired = call(&env.app, reset(&token)).await;
        assert_eq!(
            (expired.status, expired.code()),
            (StatusCode::BAD_REQUEST, "INVALID_RESET_TOKEN")
        );
        assert_eq!(
            env.login("xena@example.test", "new-password-12345")
                .await
                .status,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_timed_out_login_creates_no_session_and_does_not_leak_the_permit() {
        let config = ApiConfig {
            request_timeout: Duration::from_millis(5),
            ..ApiConfig::default()
        };
        let env = TestEnv::with(settings(), config).await;
        env.user("yan@example.test", Some(PASSWORD)).await;
        let r = env.login("yan@example.test", PASSWORD).await;
        assert_eq!(
            (r.status, r.code()),
            (StatusCode::SERVICE_UNAVAILABLE, "TIMEOUT")
        );
        assert!(r.headers.get(header::SET_COOKIE).is_none());
        assert_eq!(r.headers[header::CACHE_CONTROL], "no-store");
        // Give the abandoned blocking job time to finish; nothing may be created.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(env.count("SELECT count(*) FROM sessions").await, 0);

        // A second app on the same database with a sane timeout still works.
        let services = Services::new(env.pool.clone(), None, settings());
        let app = router(Some(services), &ApiConfig::default());
        let ok = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"yan@example.test","password":PASSWORD})),
        )
        .await;
        assert_eq!(ok.status, StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn secrets_never_reach_the_logs() {
        let (logs, _guard) = capture_logs();
        let env = TestEnv::new().await;
        env.operator("op@example.test", PASSWORD).await;
        let victim = env
            .user("zoe@example.test", Some("zoe-secret-password-1"))
            .await;

        let failed = env
            .login("zoe@example.test", "wrong-secret-password-9")
            .await;
        assert_eq!(failed.status, StatusCode::UNAUTHORIZED);
        let s = env
            .login_ok("zoe@example.test", "zoe-secret-password-1")
            .await;
        let op = env.login_ok("op@example.test", PASSWORD).await;
        let changed = call(&env.app, Req::new("POST", "/api/v1/auth/password/change").session(&s).json(json!({"current_password":"zoe-secret-password-1","new_password":"zoe-changed-password-2"}))).await;
        assert_eq!(changed.status, StatusCode::NO_CONTENT);
        let issued = call(
            &env.app,
            Req::new(
                "POST",
                &format!("/api/v1/admin/users/{victim}/password-reset"),
            )
            .session(&op),
        )
        .await;
        let reset_token = issued.body["reset_url"]
            .as_str()
            .unwrap()
            .split("token=")
            .nth(1)
            .unwrap()
            .to_owned();
        let reset = call(
            &env.app,
            Req::new("POST", "/api/v1/auth/password-reset")
                .json(json!({"token": reset_token, "new_password":"zoe-final-password-3"})),
        )
        .await;
        assert_eq!(reset.status, StatusCode::NO_CONTENT);
        let _ = call(
            &env.app,
            Req::new("POST", "/api/v1/auth/logout")
                .cookie_only(&s)
                .csrf("bad-csrf-value-xyz"),
        )
        .await;

        let text = logs.text();
        // The capture works: known lines are present.
        assert!(text.contains("login succeeded"), "{text}");
        assert!(text.contains("login failed"), "{text}");
        assert!(text.contains("/api/v1/auth/login"), "{text}");
        for secret in [
            "zoe-secret-password-1",
            "wrong-secret-password-9",
            "zoe-changed-password-2",
            "zoe-final-password-3",
            PASSWORD,
            &s.token,
            &s.csrf,
            &op.token,
            &op.csrf,
            &reset_token,
            "bad-csrf-value-xyz",
            "zoe@example.test",
        ] {
            assert!(
                !text.contains(secret),
                "secret leaked into logs: {}",
                &secret[..4.min(secret.len())]
            );
        }
    }

    // ---- failed-login limits ---------------------------------------------------

    fn login_from(ip: &str, email: &str, password: &str) -> Req {
        Req::new("POST", "/api/v1/auth/login")
            .json(json!({"email": email, "password": password}))
            .via_peer(ip)
    }

    fn guard_settings(pair: u32, account: u32) -> AuthSettings {
        AuthSettings {
            failed_login_pair_limit: pair,
            failed_login_account_limit: account,
            ..settings()
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn only_failed_logins_count_and_block_per_account_and_address() {
        let env = TestEnv::with(guard_settings(3, 1000), ApiConfig::default()).await;
        env.user("nora@example.test", Some(PASSWORD)).await;
        env.user("omar@example.test", Some(PASSWORD)).await;

        // Successful logins are never counted.
        for _ in 0..8 {
            let r = call(
                &env.app,
                login_from("203.0.113.10", "omar@example.test", PASSWORD),
            )
            .await;
            assert_eq!(r.status, StatusCode::OK);
        }
        // Three failures from one address block that address for that account...
        for _ in 0..3 {
            let r = call(
                &env.app,
                login_from(
                    "198.51.100.1",
                    "nora@example.test",
                    "wrong-password-entirely",
                ),
            )
            .await;
            assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        }
        let blocked = call(
            &env.app,
            login_from("198.51.100.1", "Nora@Example.test", PASSWORD),
        )
        .await;
        assert_eq!(
            (blocked.status, blocked.code()),
            (StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED")
        );
        let retry: u64 = blocked.headers[header::RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((1..=900).contains(&retry));
        // ...but the owner with the correct password from another address still works,
        let other_ip = call(
            &env.app,
            login_from("198.51.100.2", "nora@example.test", PASSWORD),
        )
        .await;
        assert_eq!(other_ip.status, StatusCode::OK);
        // ...and the blocked address can still use another account.
        let other_account = call(
            &env.app,
            login_from("198.51.100.1", "omar@example.test", PASSWORD),
        )
        .await;
        assert_eq!(other_account.status, StatusCode::OK);
        // The block ends with the window.
        env.clock.advance(Duration::from_secs(15 * 60 + 1));
        let later = call(
            &env.app,
            login_from("198.51.100.1", "nora@example.test", PASSWORD),
        )
        .await;
        assert_eq!(later.status, StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn unknown_addresses_are_limited_exactly_like_real_accounts() {
        let env = TestEnv::with(guard_settings(3, 1000), ApiConfig::default()).await;
        env.user("real@example.test", Some(PASSWORD)).await;
        let statuses = |email: &'static str| {
            let env = &env;
            async move {
                let mut out = Vec::new();
                for _ in 0..5 {
                    out.push(
                        call(
                            &env.app,
                            login_from("198.51.100.1", email, "wrong-password-entirely"),
                        )
                        .await
                        .status,
                    );
                }
                let other_ip = call(
                    &env.app,
                    login_from("198.51.100.2", email, "wrong-password-entirely"),
                )
                .await
                .status;
                (out, other_ip)
            }
        };
        let real = statuses("real@example.test").await;
        let ghost = statuses("ghost@example.test").await;
        assert_eq!(
            real, ghost,
            "existence must not show in the 401/429 pattern"
        );
        assert_eq!(real.0[3], StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(real.1, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_success_resets_the_failure_counter_of_that_address() {
        let env = TestEnv::with(guard_settings(3, 1000), ApiConfig::default()).await;
        env.user("pia@example.test", Some(PASSWORD)).await;
        for round in 0..3 {
            for _ in 0..2 {
                let r = call(
                    &env.app,
                    login_from(
                        "198.51.100.1",
                        "pia@example.test",
                        "wrong-password-entirely",
                    ),
                )
                .await;
                assert_eq!(r.status, StatusCode::UNAUTHORIZED, "round {round}");
            }
            let ok = call(
                &env.app,
                login_from("198.51.100.1", "pia@example.test", PASSWORD),
            )
            .await;
            assert_eq!(ok.status, StatusCode::OK, "round {round}");
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_global_account_ceiling_blocks_every_address_for_that_account() {
        let env = TestEnv::with(guard_settings(100, 4), ApiConfig::default()).await;
        env.user("quin@example.test", Some(PASSWORD)).await;
        env.user("rudi@example.test", Some(PASSWORD)).await;
        for i in 0..4 {
            let r = call(
                &env.app,
                login_from(
                    &format!("198.51.100.{i}"),
                    "quin@example.test",
                    "wrong-password-entirely",
                ),
            )
            .await;
            assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        }
        let new_ip = call(
            &env.app,
            login_from("203.0.113.77", "quin@example.test", PASSWORD),
        )
        .await;
        assert_eq!(new_ip.status, StatusCode::TOO_MANY_REQUESTS);
        let other_account = call(
            &env.app,
            login_from("203.0.113.77", "rudi@example.test", PASSWORD),
        )
        .await;
        assert_eq!(other_account.status, StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn ipv6_addresses_in_one_64_prefix_share_limits() {
        let mut s = guard_settings(2, 1000);
        s.ip_limit = 6;
        let env = TestEnv::with(s, ApiConfig::default()).await;
        env.user("sara@example.test", Some(PASSWORD)).await;
        for ip in ["2001:db8:1:2::1", "2001:db8:1:2:ffff:ffff:ffff:ffff"] {
            let r = call(
                &env.app,
                login_from(ip, "sara@example.test", "wrong-password-entirely"),
            )
            .await;
            assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        }
        // Third address in the same /64 is blocked, another /64 is not.
        let same = call(
            &env.app,
            login_from("2001:db8:1:2::abcd", "sara@example.test", PASSWORD),
        )
        .await;
        assert_eq!(same.status, StatusCode::TOO_MANY_REQUESTS);
        let different = call(
            &env.app,
            login_from("2001:db8:1:3::1", "sara@example.test", PASSWORD),
        )
        .await;
        assert_eq!(different.status, StatusCode::OK);
        // The request rate limit also groups the /64 (6 requests so far from it: 3 + more).
        for i in 0..3 {
            let _ = call(
                &env.app,
                login_from(&format!("2001:db8:1:2::{i:x}"), "nobody@example.test", "x"),
            )
            .await;
        }
        let over = call(
            &env.app,
            login_from("2001:db8:1:2::ee", "nobody@example.test", "x"),
        )
        .await;
        assert_eq!(over.status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn flooding_with_fresh_addresses_cannot_evict_an_active_account_counter() {
        let mut s = guard_settings(2, 1000);
        s.rate_max_entries = 5; // tiny request tables: constant eviction there
        s.account_guard_max_entries = 20;
        let env = TestEnv::with(s, ApiConfig::default()).await;
        env.user("tess@example.test", Some(PASSWORD)).await;
        for _ in 0..2 {
            let r = call(
                &env.app,
                login_from(
                    "198.51.100.1",
                    "tess@example.test",
                    "wrong-password-entirely",
                ),
            )
            .await;
            assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        }
        // 60 fresh addresses against 60 unknown e-mail addresses: the table
        // fills up, new keys are then refused, but nothing is evicted.
        let mut refused = 0;
        for i in 0..60 {
            let r = call(
                &env.app,
                login_from(
                    &format!("203.0.113.{}", i + 1),
                    &format!("flood{i}@example.test"),
                    "x",
                ),
            )
            .await;
            assert!(matches!(
                r.status,
                StatusCode::UNAUTHORIZED | StatusCode::TOO_MANY_REQUESTS
            ));
            refused += usize::from(r.status == StatusCode::TOO_MANY_REQUESTS);
        }
        assert!(refused > 0, "a full table must refuse new keys");
        let still = call(
            &env.app,
            login_from("198.51.100.1", "tess@example.test", PASSWORD),
        )
        .await;
        assert_eq!(
            still.status,
            StatusCode::TOO_MANY_REQUESTS,
            "the counter of the real account was evicted"
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_full_table_refuses_new_addresses_identically_for_real_and_unknown_accounts() {
        let mut s = guard_settings(100, 1000);
        s.account_guard_max_entries = 3;
        let env = TestEnv::with(s, ApiConfig::default()).await;
        for name in ["real1", "real2", "real3", "real4"] {
            env.user(&format!("{name}@example.test"), Some(PASSWORD))
                .await;
        }
        // Three keys fill the table (two real accounts, one unknown address).
        for email in [
            "real1@example.test",
            "real2@example.test",
            "ghost1@example.test",
        ] {
            let r = call(
                &env.app,
                login_from("198.51.100.1", email, "wrong-password-entirely"),
            )
            .await;
            assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{email}");
        }
        // A fourth real account and a fourth unknown address: same answer.
        let real = call(
            &env.app,
            login_from("198.51.100.9", "real4@example.test", PASSWORD),
        )
        .await;
        let ghost = call(
            &env.app,
            login_from("198.51.100.9", "ghost9@example.test", PASSWORD),
        )
        .await;
        assert_eq!(real.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(ghost.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            real.body["error"]["message"],
            ghost.body["error"]["message"]
        );
        assert!(
            real.headers.contains_key(header::RETRY_AFTER)
                && ghost.headers.contains_key(header::RETRY_AFTER)
        );
        // Keys that are already tracked keep working (a success is allowed).
        let ok = call(
            &env.app,
            login_from("198.51.100.1", "real1@example.test", PASSWORD),
        )
        .await;
        assert_eq!(ok.status, StatusCode::OK);
        // After the window the table is swept and the new address can log in.
        env.clock.advance(Duration::from_secs(15 * 60 + 1));
        let later = call(
            &env.app,
            login_from("198.51.100.9", "real4@example.test", PASSWORD),
        )
        .await;
        assert_eq!(later.status, StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn concurrent_wrong_guesses_cannot_exceed_the_failure_limit() {
        let env = TestEnv::with(guard_settings(5, 1000), ApiConfig::default()).await;
        env.user("conc@example.test", Some(PASSWORD)).await;
        let mut handles = Vec::new();
        for _ in 0..24 {
            let app = env.app.clone();
            handles.push(tokio::spawn(async move {
                call(
                    &app,
                    login_from(
                        "198.51.100.1",
                        "conc@example.test",
                        "wrong-password-entirely",
                    ),
                )
                .await
                .status
            }));
        }
        let (mut checked, mut refused) = (0, 0);
        for handle in handles {
            match handle.await.unwrap() {
                StatusCode::UNAUTHORIZED => checked += 1,
                StatusCode::TOO_MANY_REQUESTS => refused += 1,
                other => panic!("unexpected {other}"),
            }
        }
        // Every 401 means a password verification happened: at most the limit.
        assert_eq!((checked, refused), (5, 19));
        // A timed-out or dropped attempt would only release its slot, never count.
        let ok = call(
            &env.app,
            login_from("198.51.100.2", "conc@example.test", PASSWORD),
        )
        .await;
        assert_eq!(ok.status, StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_session_pair_is_found_next_to_an_unreadable_pair_in_one_cookie_header() {
        let env = TestEnv::new().await;
        env.user("bytes@example.test", Some(PASSWORD)).await;
        let s = env.login_ok("bytes@example.test", PASSWORD).await;
        let mut request = Req::new("GET", "/api/v1/auth/session").build();
        let mut raw = b"a=".to_vec();
        raw.extend_from_slice(&[0xff, 0xfe, 0x80]);
        raw.extend_from_slice(format!("; {COOKIE_NAME}={}", s.token).as_bytes());
        request.headers_mut().append(
            header::COOKIE,
            axum::http::HeaderValue::from_bytes(&raw).unwrap(),
        );
        let response = env.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    // ---- sessions: replacement, expiry reporting, cookies ----------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn logging_in_with_a_valid_session_cookie_revokes_that_old_session() {
        let env = TestEnv::new().await;
        env.user("uli@example.test", Some(PASSWORD)).await;
        let old = env.login_ok("uli@example.test", PASSWORD).await;
        let other = env.login_ok("uli@example.test", PASSWORD).await;
        let mut again = login_from("198.51.100.1", "uli@example.test", PASSWORD);
        again.cookie = Some(format!("{COOKIE_NAME}={}", old.token));
        let r = call(&env.app, again).await;
        assert_eq!(r.status, StatusCode::OK);
        let new = session_from(&r);
        assert_ne!(new.token, old.token);
        assert_eq!(env.get_session(&old).await.status, StatusCode::UNAUTHORIZED);
        assert_eq!(env.get_session(&new).await.status, StatusCode::OK);
        // Unrelated sessions of the same user are not touched.
        assert_eq!(env.get_session(&other).await.status, StatusCode::OK);
        // A garbage cookie does not break the login.
        let mut junk = login_from("198.51.100.1", "uli@example.test", PASSWORD);
        junk.cookie = Some(format!("{COOKIE_NAME}=garbage"));
        assert_eq!(call(&env.app, junk).await.status, StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn reported_expiry_follows_the_recorded_activity() {
        use time::format_description::well_known::Rfc3339;
        let env = TestEnv::new().await;
        env.user("vic@example.test", Some(PASSWORD)).await;
        let login = env.login("vic@example.test", PASSWORD).await;
        let s = session_from(&login);
        let at_login =
            OffsetDateTime::parse(login.body["expires_at"].as_str().unwrap(), &Rfc3339).unwrap();

        // Within the 60 s touch interval nothing is recorded: expiry unchanged.
        env.clock.advance(Duration::from_secs(30));
        let info = env.get_session(&s).await;
        let untouched =
            OffsetDateTime::parse(info.body["expires_at"].as_str().unwrap(), &Rfc3339).unwrap();
        // The database keeps microseconds, so compare with a tolerance.
        assert!((untouched - at_login).abs() < time::Duration::milliseconds(1));

        // After the interval the activity is recorded and expiry moves.
        env.clock.advance(Duration::from_secs(61));
        let info = env.get_session(&s).await;
        let touched =
            OffsetDateTime::parse(info.body["expires_at"].as_str().unwrap(), &Rfc3339).unwrap();
        assert!(((touched - at_login).as_seconds_f64() - 91.0).abs() < 0.01);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn logout_routes_clear_the_cookie_on_401_but_not_on_csrf_failures() {
        let env = TestEnv::new().await;
        env.user("wes@example.test", Some(PASSWORD)).await;
        let s = env.login_ok("wes@example.test", PASSWORD).await;
        for uri in ["/api/v1/auth/logout", "/api/v1/auth/logout-all"] {
            let gone = Session {
                token: "A".repeat(43),
                csrf: "x".into(),
            };
            let r = call(&env.app, Req::new("POST", uri).session(&gone)).await;
            assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{uri}");
            let cleared = r.headers[header::SET_COOKIE].to_str().unwrap();
            assert!(
                cleared.starts_with(&format!("{COOKIE_NAME}=;")) && cleared.contains("Max-Age=0")
            );
            // No cookie presented: nothing to clear, no Set-Cookie.
            let none = call(&env.app, Req::new("POST", uri)).await;
            assert_eq!(none.status, StatusCode::UNAUTHORIZED);
            assert!(!none.headers.contains_key(header::SET_COOKIE));
            // A malformed cookie of the session name was presented: clear it.
            let mut junk = Req::new("POST", uri);
            junk.cookie = Some(format!("{COOKIE_NAME}=garbage"));
            let junk = call(&env.app, junk).await;
            assert_eq!(junk.status, StatusCode::UNAUTHORIZED);
            assert!(junk.headers.contains_key(header::SET_COOKIE));
            // A CSRF failure keeps the (valid) cookie.
            let bad = call(
                &env.app,
                Req::new("POST", uri).cookie_only(&s).csrf("wrong"),
            )
            .await;
            assert_eq!(bad.code(), "CSRF_FAILED");
            assert!(!bad.headers.contains_key(header::SET_COOKIE));
        }
        assert_eq!(env.get_session(&s).await.status, StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_session_cookie_is_found_next_to_unreadable_cookie_headers() {
        let env = TestEnv::new().await;
        env.user("xan@example.test", Some(PASSWORD)).await;
        let s = env.login_ok("xan@example.test", PASSWORD).await;
        let mut request = Req::new("GET", "/api/v1/auth/session").build();
        request.headers_mut().append(
            header::COOKIE,
            axum::http::HeaderValue::from_bytes(&[b'a', b'=', 0xff]).unwrap(),
        );
        request.headers_mut().append(
            header::COOKIE,
            axum::http::HeaderValue::from_str(&format!("junk; {COOKIE_NAME}={}", s.token)).unwrap(),
        );
        let response = env.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn expired_rows_are_cleaned_at_most_every_ten_minutes() {
        let env = TestEnv::new().await;
        env.user("yola@example.test", Some(PASSWORD)).await;
        let uid: Uuid = sqlx::query_scalar("SELECT id FROM users")
            .fetch_one(&env.pool)
            .await
            .unwrap();
        let stale = |n: u8| {
            let pool = env.pool.clone();
            async move {
                sqlx::query("INSERT INTO sessions (id, user_id, token_hash, created_at, last_seen_at, absolute_expires_at, revoked_at) VALUES ($1, $2, $3, now() - interval '5 days', now() - interval '5 days', now() - interval '3 days', now() - interval '4 days')")
                    .bind(Uuid::new_v4())
                    .bind(uid)
                    .bind(vec![n; 32])
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        };
        stale(1).await;
        env.login_ok("yola@example.test", PASSWORD).await; // first login: cleanup runs
        assert_eq!(
            env.count("SELECT count(*) FROM sessions WHERE revoked_at IS NOT NULL")
                .await,
            0
        );
        stale(2).await;
        env.clock.advance(Duration::from_secs(5 * 60));
        env.login_ok("yola@example.test", PASSWORD).await; // within 10 minutes: no cleanup
        assert_eq!(
            env.count("SELECT count(*) FROM sessions WHERE revoked_at IS NOT NULL")
                .await,
            1
        );
        env.clock.advance(Duration::from_secs(6 * 60));
        env.login_ok("yola@example.test", PASSWORD).await; // 11 minutes since the last run
        assert_eq!(
            env.count("SELECT count(*) FROM sessions WHERE revoked_at IS NOT NULL")
                .await,
            0
        );
    }

    // ---- device list ---------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn device_list_shows_only_own_active_sessions_and_allows_revoking_them() {
        let env = TestEnv::new().await;
        env.user("zed@example.test", Some(PASSWORD)).await;
        env.user("amy@example.test", Some(PASSWORD)).await;
        let a = env.login_ok("zed@example.test", PASSWORD).await;
        let b = env.login_ok("zed@example.test", PASSWORD).await;
        let revoked = env.login_ok("zed@example.test", PASSWORD).await;
        let amy = env.login_ok("amy@example.test", PASSWORD).await;
        let r = call(
            &env.app,
            Req::new("POST", "/api/v1/auth/logout").session(&revoked),
        )
        .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT);

        let list = call(
            &env.app,
            Req::new("GET", "/api/v1/auth/sessions").cookie_only(&a),
        )
        .await;
        assert_eq!(list.status, StatusCode::OK, "{}", list.text);
        let items = list.body["sessions"].as_array().unwrap();
        assert_eq!(
            items.len(),
            2,
            "revoked and foreign sessions must not appear"
        );
        assert_eq!(
            items.iter().filter(|i| i["current"] == json!(true)).count(),
            1
        );
        for item in items {
            for field in ["id", "created_at", "last_seen_at", "expires_at", "current"] {
                assert!(item.get(field).is_some(), "{field} missing");
            }
            // No network or device data, and never the token.
            assert_eq!(item.as_object().unwrap().len(), 5);
        }
        assert!(!list.text.contains(&a.token) && !list.text.contains(&b.token));
        let other_id = items.iter().find(|i| i["current"] == json!(false)).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();

        // Another user's session id: 404, nothing revoked; garbage/unknown ids too.
        let amy_id: String = sqlx::query_scalar("SELECT s.id::text FROM sessions s JOIN users u ON u.id = s.user_id WHERE u.normalized_email = 'amy@example.test'")
            .fetch_one(&env.pool)
            .await
            .unwrap();
        for target in [
            amy_id.as_str(),
            "not-a-uuid",
            "00000000-0000-0000-0000-000000000000",
        ] {
            let r = call(
                &env.app,
                Req::new("DELETE", &format!("/api/v1/auth/sessions/{target}")).session(&a),
            )
            .await;
            assert_eq!(
                (r.status, r.code()),
                (StatusCode::NOT_FOUND, "NOT_FOUND"),
                "{target}"
            );
        }
        assert_eq!(env.get_session(&amy).await.status, StatusCode::OK);

        // CSRF is required; then revoking another own session works.
        let no_csrf = call(
            &env.app,
            Req::new("DELETE", &format!("/api/v1/auth/sessions/{other_id}")).cookie_only(&a),
        )
        .await;
        assert_eq!(no_csrf.code(), "CSRF_FAILED");
        let ok = call(
            &env.app,
            Req::new("DELETE", &format!("/api/v1/auth/sessions/{other_id}")).session(&a),
        )
        .await;
        assert_eq!(ok.status, StatusCode::NO_CONTENT);
        assert!(
            !ok.headers.contains_key(header::SET_COOKIE),
            "not the current session"
        );
        assert_eq!(env.get_session(&b).await.status, StatusCode::UNAUTHORIZED);
        // Revoking it again is a 404.
        let again = call(
            &env.app,
            Req::new("DELETE", &format!("/api/v1/auth/sessions/{other_id}")).session(&a),
        )
        .await;
        assert_eq!(again.status, StatusCode::NOT_FOUND);

        // Revoking the current session clears the cookie.
        let current_id = call(
            &env.app,
            Req::new("GET", "/api/v1/auth/sessions").cookie_only(&a),
        )
        .await
        .body["sessions"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let own = call(
            &env.app,
            Req::new("DELETE", &format!("/api/v1/auth/sessions/{current_id}")).session(&a),
        )
        .await;
        assert_eq!(own.status, StatusCode::NO_CONTENT);
        assert!(
            own.headers[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("Max-Age=0")
        );
        assert_eq!(env.get_session(&a).await.status, StatusCode::UNAUTHORIZED);
        // Anonymous callers get 401.
        let anon = call(&env.app, Req::new("GET", "/api/v1/auth/sessions")).await;
        assert_eq!(anon.status, StatusCode::UNAUTHORIZED);
    }

    // ---- reset links: M2, L6, L7 ------------------------------------------------------

    async fn operator_env() -> (TestEnv, Session) {
        let env = TestEnv::new().await;
        env.operator("op@example.test", PASSWORD).await;
        let op = env.login_ok("op@example.test", PASSWORD).await;
        (env, op)
    }

    async fn issue_link(env: &TestEnv, op: &Session, user: Uuid) -> String {
        let r = call(
            &env.app,
            Req::new(
                "POST",
                &format!("/api/v1/admin/users/{user}/password-reset"),
            )
            .session(op),
        )
        .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
        r.body["reset_url"]
            .as_str()
            .unwrap()
            .split("token=")
            .nth(1)
            .unwrap()
            .to_owned()
    }

    async fn reset_with(env: &TestEnv, token: &str, password: &str) -> Resp {
        call(
            &env.app,
            Req::new("POST", "/api/v1/auth/password-reset")
                .json(json!({"token": token, "new_password": password})),
        )
        .await
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn issuing_a_link_revokes_the_sessions_of_that_account_immediately() {
        let (env, op) = operator_env().await;
        let victim = env.user("bea@example.test", Some(PASSWORD)).await;
        let s1 = env.login_ok("bea@example.test", PASSWORD).await;
        let s2 = env.login_ok("bea@example.test", PASSWORD).await;
        let token = issue_link(&env, &op, victim).await;
        assert_eq!(env.get_session(&s1).await.status, StatusCode::UNAUTHORIZED);
        assert_eq!(env.get_session(&s2).await.status, StatusCode::UNAUTHORIZED);
        // The operator's own session is untouched.
        assert_eq!(env.get_session(&op).await.status, StatusCode::OK);
        // The old password still works (a link does not change it), the link too.
        assert_eq!(
            env.login("bea@example.test", PASSWORD).await.status,
            StatusCode::OK
        );
        assert_eq!(
            reset_with(&env, &token, "fresh-password-9999").await.status,
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_new_link_voids_older_ones_and_the_operator_can_void_pending_links() {
        let (env, op) = operator_env().await;
        let victim = env.user("cal@example.test", Some(PASSWORD)).await;
        let first = issue_link(&env, &op, victim).await;
        let second = issue_link(&env, &op, victim).await;
        let r = reset_with(&env, &first, "fresh-password-9999").await;
        assert_eq!(
            (r.status, r.code()),
            (StatusCode::BAD_REQUEST, "INVALID_RESET_TOKEN")
        );

        // DELETE voids the pending link: operator + CSRF only.
        let url = format!("/api/v1/admin/users/{victim}/password-reset");
        let normal = {
            env.user("dan@example.test", Some(PASSWORD)).await;
            env.login_ok("dan@example.test", PASSWORD).await
        };
        assert_eq!(
            call(&env.app, Req::new("DELETE", &url).session(&normal))
                .await
                .code(),
            "FORBIDDEN"
        );
        assert_eq!(
            call(&env.app, Req::new("DELETE", &url).cookie_only(&op))
                .await
                .code(),
            "CSRF_FAILED"
        );
        assert_eq!(
            call(&env.app, Req::new("DELETE", &url)).await.status,
            StatusCode::UNAUTHORIZED
        );
        let missing = call(
            &env.app,
            Req::new(
                "DELETE",
                &format!("/api/v1/admin/users/{}/password-reset", Uuid::new_v4()),
            )
            .session(&op),
        )
        .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        let done = call(&env.app, Req::new("DELETE", &url).session(&op)).await;
        assert_eq!(done.status, StatusCode::NO_CONTENT);
        let r = reset_with(&env, &second, "fresh-password-9999").await;
        assert_eq!(r.code(), "INVALID_RESET_TOKEN");
        // Voiding with nothing pending is fine.
        assert_eq!(
            call(&env.app, Req::new("DELETE", &url).session(&op))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_password_change_voids_pending_reset_links() {
        let (env, op) = operator_env().await;
        let victim = env.user("eve2@example.test", Some(PASSWORD)).await;
        let token = issue_link(&env, &op, victim).await;
        // Issuing revoked old sessions; this login happens afterwards.
        let s = env.login_ok("eve2@example.test", PASSWORD).await;
        let change = call(
            &env.app,
            Req::new("POST", "/api/v1/auth/password/change")
                .session(&s)
                .json(json!({"current_password": PASSWORD, "new_password": "changed-by-user-123"})),
        )
        .await;
        assert_eq!(change.status, StatusCode::NO_CONTENT, "{}", change.text);
        let r = reset_with(&env, &token, "fresh-password-9999").await;
        assert_eq!(
            (r.status, r.code()),
            (StatusCode::BAD_REQUEST, "INVALID_RESET_TOKEN")
        );
        assert_eq!(
            env.login("eve2@example.test", "changed-by-user-123")
                .await
                .status,
            StatusCode::OK
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn disabled_accounts_get_no_link_and_existing_links_stop_working() {
        let (env, op) = operator_env().await;
        let victim = env.user("fay@example.test", Some(PASSWORD)).await;
        let token = issue_link(&env, &op, victim).await;
        sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
            .bind(victim)
            .execute(&env.pool)
            .await
            .unwrap();
        // The earlier link is dead, uniformly.
        let r = reset_with(&env, &token, "fresh-password-9999").await;
        assert_eq!(
            (r.status, r.code()),
            (StatusCode::BAD_REQUEST, "INVALID_RESET_TOKEN")
        );
        // No new link for a disabled account.
        let issue = call(
            &env.app,
            Req::new(
                "POST",
                &format!("/api/v1/admin/users/{victim}/password-reset"),
            )
            .session(&op),
        )
        .await;
        assert_eq!(
            (issue.status, issue.code()),
            (StatusCode::CONFLICT, "ACCOUNT_DISABLED")
        );
        assert_eq!(
            env.count("SELECT count(*) FROM password_reset_tokens WHERE used_at IS NULL")
                .await,
            1
        );
        let used: i64 = env
            .count("SELECT count(*) FROM password_reset_tokens")
            .await;
        assert_eq!(used, 1, "nothing was stored for the disabled account");
    }
}
