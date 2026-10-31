//! Small PostgreSQL HTTP harness for the internal AC-11 tests.

use std::{
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode, header},
};
use craftcloud_auth::hash_password;
use craftcloud_core::Capabilities;
use craftcloud_db::{
    PgPool, migrate,
    setup::{self, NewInstance},
};
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection};
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

use crate::{ApiConfig, AuthSettings, Clock, Services, router_with_shared};

pub const PASSWORD: &str = "correct-horse-battery-staple";
const COOKIE_NAME: &str = "__Host-craftcloud_session";

pub struct ManualClock(Mutex<OffsetDateTime>);
impl ManualClock {
    fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(OffsetDateTime::now_utc())))
    }
    pub fn advance(&self, duration: Duration) {
        *self.0.lock().unwrap() += duration;
    }
}
impl Clock for ManualClock {
    fn now(&self) -> OffsetDateTime {
        *self.0.lock().unwrap()
    }
}

pub struct TestEnv {
    name: String,
    admin: PgConnectOptions,
    pub pool: PgPool,
    pub clock: Arc<ManualClock>,
    pub app: Router,
}

impl TestEnv {
    pub async fn new() -> Self {
        let url = std::env::var("CC_TEST_DATABASE_URL").expect("CC_TEST_DATABASE_URL must be set");
        let admin = PgConnectOptions::from_str(&url).unwrap();
        let name = format!("cc_test_{}", Uuid::new_v4().simple());
        let mut connection = PgConnection::connect_with(&admin).await.unwrap();
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&mut connection)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(20)
            .acquire_timeout(Duration::from_secs(20))
            .connect_with(admin.clone().database(&name))
            .await
            .unwrap();
        migrate::run(&pool).await.unwrap();
        setup::complete(
            &pool,
            &NewInstance {
                operator_email: "op@example.test".to_owned(),
                operator_display_name: "Operator".to_owned(),
                operator_password_hash: hash_password(PASSWORD).unwrap(),
                organization_name: "Operator Org".to_owned(),
                organization_slug: "operator-org".to_owned(),
            },
        )
        .await
        .unwrap();
        let clock = ManualClock::new();
        let services =
            Arc::new(Services::new(pool.clone(), None, settings()).with_clock(clock.clone()));
        let app = router_with_shared(
            Capabilities::development_defaults(),
            &ApiConfig {
                trusted_proxies: vec!["127.0.0.0/8".parse().unwrap()],
                ..ApiConfig::default()
            },
            Some(services),
        );
        Self {
            name,
            admin,
            pool,
            clock,
            app,
        }
    }

    pub async fn user(&self, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id,normalized_email,display_name,password_hash) VALUES ($1,$2,$3,$4)")
            .bind(id).bind(format!("{name}@example.test")).bind(name).bind(hash_password(PASSWORD).unwrap())
            .execute(&self.pool).await.unwrap();
        id
    }

    pub async fn login_ok(&self, name: &str) -> Session {
        let response = call(
            &self.app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":format!("{name}@example.test"),"password":PASSWORD})),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.text);
        session_from(&response)
    }

    pub async fn get_session(&self, session: &Session) -> Resp {
        call(
            &self.app,
            Req::new("GET", "/api/v1/auth/session").cookie_only(session),
        )
        .await
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let admin = self.admin.clone();
        let name = self.name.clone();
        let pool = self.pool.clone();
        let _ = std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async {
                pool.close().await;
                if let Ok(mut connection) = PgConnection::connect_with(&admin).await {
                    let _ =
                        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                            .execute(&mut connection)
                            .await;
                }
            });
        })
        .join();
    }
}

fn settings() -> AuthSettings {
    AuthSettings {
        public_url: Some("https://cloud.test".to_owned()),
        public_origin: Some("https://cloud.test".to_owned()),
        ip_limit: 100_000,
        pwchange_limit: 100_000,
        failed_login_pair_limit: 100_000,
        failed_login_account_limit: 100_000,
        ..AuthSettings::default()
    }
}

#[derive(Clone)]
pub struct Session {
    token: String,
    csrf: String,
}

fn session_from(response: &Resp) -> Session {
    let cookie = response
        .headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    let token = cookie
        .strip_prefix(&format!("{COOKIE_NAME}="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    Session {
        token,
        csrf: response.body["csrf_token"].as_str().unwrap().to_owned(),
    }
}

pub struct Req {
    method: &'static str,
    uri: String,
    body: Option<Value>,
    session: Option<Session>,
    origin: bool,
}
impl Req {
    pub fn new(method: &'static str, uri: &str) -> Self {
        Self {
            method,
            uri: uri.to_owned(),
            body: None,
            session: None,
            origin: true,
        }
    }
    pub fn json(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }
    pub fn session(mut self, session: &Session) -> Self {
        self.session = Some(session.clone());
        self
    }
    pub fn cookie_only(mut self, session: &Session) -> Self {
        self.session = Some(Session {
            token: session.token.clone(),
            csrf: String::new(),
        });
        self.origin = false;
        self
    }
    pub fn build(self) -> Request<Body> {
        let mut request = Request::builder().method(self.method).uri(self.uri);
        if let Some(session) = &self.session {
            request = request.header(header::COOKIE, format!("{COOKIE_NAME}={}", session.token));
            if !session.csrf.is_empty() {
                request = request.header("x-csrf-token", &session.csrf);
            }
        }
        if self.origin {
            request = request.header(header::ORIGIN, "https://cloud.test");
        }
        let body = if let Some(body) = self.body {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(body.to_string())
        } else {
            Body::empty()
        };
        request.body(body).unwrap()
    }
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Value,
    pub text: String,
}
impl Resp {
    pub fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }
}

pub async fn call(app: &Router, request: Req) -> Resp {
    let response = app.clone().oneshot(request.build()).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Resp {
        status,
        headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        text,
    }
}
