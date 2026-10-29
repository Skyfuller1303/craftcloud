//! Shared helpers of the HTTP-level integration tests that need PostgreSQL
//! (`cargo xtask db-test`): manual clock, request builder, test database.

#![allow(dead_code)]

use std::io;
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use craftcloud_api::{ApiConfig, AuthSettings, Clock, Services, router_with_services};
use craftcloud_auth::hash_password;
use craftcloud_core::Capabilities;
use craftcloud_db::PgPool;
use craftcloud_db::setup::{self, NewInstance};
use craftcloud_storage::{FaultPlan, LocalStore};
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection};
use time::OffsetDateTime;
use tower::ServiceExt;
use tracing_subscriber::fmt::MakeWriter;
use uuid::Uuid;

pub const ENV: &str = "CC_TEST_DATABASE_URL";
pub const ORIGIN: &str = "https://cloud.test";
pub const COOKIE_NAME: &str = "__Host-craftcloud_session";
pub const PASSWORD: &str = "correct-horse-battery-staple";

pub struct ManualClock(Mutex<OffsetDateTime>);

impl ManualClock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(OffsetDateTime::now_utc())))
    }
    pub fn advance(&self, by: Duration) {
        let mut now = self.0.lock().unwrap();
        *now += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> OffsetDateTime {
        *self.0.lock().unwrap()
    }
}

pub fn settings() -> AuthSettings {
    AuthSettings {
        public_url: Some(ORIGIN.to_owned()),
        public_origin: Some(ORIGIN.to_owned()),
        cookie_secure: true,
        ip_limit: 100_000,
        pwchange_limit: 100_000,
        failed_login_pair_limit: 100_000,
        failed_login_account_limit: 100_000,
        ..AuthSettings::default()
    }
}

pub struct Req {
    method: &'static str,
    uri: String,
    body: Option<Value>,
    cookie: Option<String>,
    csrf: Option<String>,
    origin: Option<String>,
    if_match: Option<String>,
    headers: Vec<(String, String)>,
    raw: Option<Body>,
}

impl Req {
    pub fn new(method: &'static str, uri: &str) -> Self {
        Self {
            method,
            uri: uri.to_owned(),
            body: None,
            cookie: None,
            csrf: None,
            origin: Some(ORIGIN.to_owned()),
            if_match: None,
            headers: Vec::new(),
            raw: None,
        }
    }
    /// Adds a header.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
    /// A raw (octet-stream) body.
    pub fn body(mut self, body: Body) -> Self {
        self.raw = Some(body);
        self
    }
    pub fn json(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }
    /// Cookie and CSRF header of a session.
    pub fn session(mut self, s: &Session) -> Self {
        self.cookie = Some(format!("{COOKIE_NAME}={}", s.token));
        self.csrf = Some(s.csrf.clone());
        self
    }
    pub fn cookie_only(mut self, s: &Session) -> Self {
        self.cookie = Some(format!("{COOKIE_NAME}={}", s.token));
        self
    }
    pub fn origin(mut self, value: Option<&str>) -> Self {
        self.origin = value.map(str::to_owned);
        self
    }
    pub fn if_match(mut self, value: &str) -> Self {
        self.if_match = Some(value.to_owned());
        self
    }
    pub fn build(self) -> Request<Body> {
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
        if let Some(value) = &self.if_match {
            builder = builder.header(header::IF_MATCH, value);
        }
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        if let Some(raw) = self.raw {
            return builder
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .body(raw)
                .unwrap();
        }
        match self.body {
            Some(body) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        }
    }
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Value,
    pub text: String,
    /// The raw body bytes.
    pub bytes: Vec<u8>,
}

impl Resp {
    pub fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }
    pub fn etag(&self) -> String {
        self.headers[header::ETAG].to_str().unwrap().to_owned()
    }
}

/// Sends a request and returns status, headers and the still unread body.
pub async fn call_stream(app: &Router, req: Req) -> (StatusCode, HeaderMap, Body) {
    let response = app.clone().oneshot(req.build()).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    (status, headers, response.into_body())
}

pub async fn call(app: &Router, req: Req) -> Resp {
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
        bytes: bytes.to_vec(),
    }
}

#[derive(Clone)]
pub struct Session {
    pub token: String,
    pub csrf: String,
}

pub fn session_from(resp: &Resp) -> Session {
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

// ---- log capture -----------------------------------------------------------

#[derive(Clone, Default)]
pub struct LogBuffer(Arc<Mutex<Vec<u8>>>);

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
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// One process-wide capture buffer (a thread-local subscriber is racy when
/// tests run in parallel).
pub fn capture_logs() -> LogBuffer {
    static LOGS: OnceLock<LogBuffer> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buffer = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
        buffer
    })
    .clone()
}

// ---- test database ---------------------------------------------------------

pub struct TestEnv {
    name: String,
    admin: PgConnectOptions,
    pub pool: PgPool,
    pub clock: Arc<ManualClock>,
    pub app: Router,
    pub operator_id: Uuid,
    /// Fault plan of the object store (nothing armed by default).
    pub faults: Arc<FaultPlan>,
    pub storage_dir: std::path::PathBuf,
    /// Budgets of best-effort audit writes (denials and failed logins; downloads
    /// and exports): a test holds their permits to simulate pressure.
    pub audit_lossy: Arc<tokio::sync::Semaphore>,
    pub audit_important: Arc<tokio::sync::Semaphore>,
}

fn shared_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| hash_password(PASSWORD).unwrap())
}

impl TestEnv {
    /// Fresh migrated database, an instance operator `op@example.test` and the
    /// router (generous rate limits).
    pub async fn new() -> Self {
        Self::with(settings(), ApiConfig::default()).await
    }

    pub async fn with(settings: AuthSettings, config: ApiConfig) -> Self {
        Self::build(settings, config, true).await
    }

    /// Like `new` but without an object store (storage not configured).
    pub async fn without_storage() -> Self {
        Self::build(settings(), ApiConfig::default(), false).await
    }

    async fn build(settings: AuthSettings, config: ApiConfig, storage: bool) -> Self {
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
        let operator_id = setup::complete(
            &pool,
            &NewInstance {
                operator_email: "op@example.test".to_owned(),
                operator_display_name: "Operator".to_owned(),
                operator_password_hash: shared_hash().to_owned(),
                organization_name: "Operator Org".to_owned(),
                organization_slug: "operator-org".to_owned(),
            },
        )
        .await
        .unwrap()
        .user_id;
        let clock = ManualClock::new();
        let storage_dir =
            std::env::temp_dir().join(format!("cc-api-test-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&storage_dir).unwrap();
        let faults = Arc::new(FaultPlan::default());
        let mut services = Services::new(pool.clone(), None, settings).with_clock(clock.clone());
        if storage {
            let store = LocalStore::open_with_faults(&storage_dir, faults.clone())
                .await
                .unwrap();
            services = services.with_storage(store);
        }
        let (audit_lossy, audit_important) = services.audit_semaphores();
        let app = router_with_services(
            Capabilities::development_defaults(),
            &config,
            Some(services),
        );
        Self {
            name,
            admin,
            pool,
            clock,
            app,
            operator_id,
            faults,
            storage_dir,
            audit_lossy,
            audit_important,
        }
    }

    pub async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
    }

    /// Creates an enabled account `<name>@example.test` with the shared password.
    pub async fn user(&self, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users (id, normalized_email, display_name, password_hash) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(format!("{name}@example.test"))
        .bind(name)
        .bind(shared_hash())
        .execute(&self.pool)
        .await
        .unwrap();
        id
    }

    pub async fn login(&self, name: &str) -> Resp {
        call(
            &self.app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email": format!("{name}@example.test"), "password": PASSWORD})),
        )
        .await
    }

    pub async fn login_ok(&self, name: &str) -> Session {
        let r = self.login(name).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        session_from(&r)
    }

    pub async fn get_session(&self, s: &Session) -> Resp {
        call(
            &self.app,
            Req::new("GET", "/api/v1/auth/session").cookie_only(s),
        )
        .await
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.storage_dir);
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
                    let _ =
                        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                            .execute(&mut conn)
                            .await;
                }
            });
        })
        .join();
    }
}
