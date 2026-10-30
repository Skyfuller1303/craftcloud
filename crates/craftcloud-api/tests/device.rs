//! HTTP-level tests of desktop device sessions (AC-01).
//!
//! These tests need PostgreSQL and run through `cargo xtask db-test`.

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::StatusCode;
    use bytes::Bytes;
    use craftcloud_api::{ApiConfig, AuthSettings};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use tokio::sync::{Barrier, mpsc};
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::common::{PASSWORD, Req, Resp, Session, TestEnv, call, capture_logs, settings};

    struct ChannelBody {
        rx: mpsc::Receiver<Bytes>,
    }

    impl http_body::Body for ChannelBody {
        type Data = Bytes;
        type Error = std::convert::Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
            self.rx
                .poll_recv(cx)
                .map(|frame| frame.map(|data| Ok(http_body::Frame::data(data))))
        }
    }

    fn assert_status(resp: &Resp, status: StatusCode, code: &str) {
        assert_eq!((resp.status, resp.code()), (status, code), "{}", resp.text);
    }

    fn test_config() -> ApiConfig {
        ApiConfig {
            trusted_proxies: vec!["127.0.0.1/32".parse().unwrap()],
            ..ApiConfig::default()
        }
    }

    async fn device_test_env() -> TestEnv {
        TestEnv::with(settings(), test_config()).await
    }

    async fn call_from_proxy(app: &Router, req: Req, peer: &str, proto: Option<&str>) -> Resp {
        let mut request = req.build();
        let addr: std::net::SocketAddr = format!("{peer}:443").parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(addr));
        if let Some(proto) = proto {
            request
                .headers_mut()
                .insert("x-forwarded-proto", proto.parse().unwrap());
        }
        let response = app.clone().oneshot(request).await.unwrap();
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
            bytes: bytes.to_vec(),
        }
    }

    fn device_body(email: &str, password: &str) -> Value {
        json!({
            "email": email,
            "password": password,
            "client_kind": "app",
            "client_id": "photocraft",
            "device_name": "PhotoCraft on LAPTOP-7"
        })
    }

    async fn device_login(env: &TestEnv, email: &str, password: &str) -> Resp {
        call_from_proxy(
            &env.app,
            Req::new("POST", "/api/v1/auth/device/login").json(device_body(email, password)),
            "127.0.0.1",
            Some("https"),
        )
        .await
    }

    async fn device_for(env: &TestEnv, email: &str) -> Resp {
        device_login(env, email, PASSWORD).await
    }

    fn access(resp: &Resp) -> String {
        resp.body["access_token"].as_str().unwrap().to_owned()
    }

    fn refresh(resp: &Resp) -> String {
        resp.body["refresh_token"].as_str().unwrap().to_owned()
    }

    fn device_id(resp: &Resp) -> Uuid {
        Uuid::parse_str(resp.body["device_session_id"].as_str().unwrap()).unwrap()
    }

    fn bearer(method: &'static str, path: &str, token: &str) -> Req {
        Req::new(method, path).header("Authorization", &format!("Bearer {token}"))
    }

    async fn refresh_token(env: &TestEnv, token: &str) -> Resp {
        call_from_proxy(
            &env.app,
            Req::new("POST", "/api/v1/auth/device/refresh").json(json!({"refresh_token": token})),
            "127.0.0.1",
            Some("https"),
        )
        .await
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn device_login_requires_trusted_https_forwarding() {
        let env = device_test_env().await;
        env.user("tls").await;
        let request = || {
            Req::new("POST", "/api/v1/auth/device/login")
                .json(device_body("tls@example.test", PASSWORD))
        };

        let missing_peer = call(&env.app, request()).await;
        assert_status(
            &missing_peer,
            StatusCode::BAD_REQUEST,
            "DEVICE_TLS_REQUIRED",
        );
        let missing_proto = call_from_proxy(&env.app, request(), "127.0.0.1", None).await;
        assert_status(
            &missing_proto,
            StatusCode::BAD_REQUEST,
            "DEVICE_TLS_REQUIRED",
        );
        let untrusted = call_from_proxy(&env.app, request(), "192.0.2.1", Some("https")).await;
        assert_status(&untrusted, StatusCode::BAD_REQUEST, "DEVICE_TLS_REQUIRED");
        let cleartext = call_from_proxy(&env.app, request(), "127.0.0.1", Some("http")).await;
        assert_status(&cleartext, StatusCode::BAD_REQUEST, "DEVICE_TLS_REQUIRED");
        let accepted = call_from_proxy(&env.app, request(), "127.0.0.1", Some("https")).await;
        assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.text);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn device_login_has_uniform_failures_and_shares_failed_login_limits() {
        let env = device_test_env().await;
        env.user("known").await;
        let wrong = device_login(&env, "known@example.test", "wrong-password-entirely").await;
        let unknown = device_login(&env, "unknown@example.test", "wrong-password-entirely").await;
        assert_status(&wrong, StatusCode::UNAUTHORIZED, "INVALID_CREDENTIALS");
        assert_status(&unknown, StatusCode::UNAUTHORIZED, "INVALID_CREDENTIALS");
        assert_eq!(
            wrong.body["error"]["message"],
            unknown.body["error"]["message"]
        );

        let mut limited = settings();
        limited.failed_login_pair_limit = 1;
        let env = TestEnv::with(limited, test_config()).await;
        env.user("shared").await;
        let browser_failure = call_from_proxy(
            &env.app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"shared@example.test","password":"wrong-password-entirely"})),
            "127.0.0.1",
            None,
        )
        .await;
        assert_status(
            &browser_failure,
            StatusCode::UNAUTHORIZED,
            "INVALID_CREDENTIALS",
        );
        let device_failure =
            device_login(&env, "shared@example.test", "wrong-password-entirely").await;
        assert_status(
            &device_failure,
            StatusCode::TOO_MANY_REQUESTS,
            "RATE_LIMITED",
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn refresh_rotates_token_and_reuse_revokes_the_family() {
        let env = device_test_env().await;
        env.user("rotate").await;
        let logged_in = device_for(&env, "rotate@example.test").await;
        assert_eq!(logged_in.status, StatusCode::OK, "{}", logged_in.text);
        assert!(logged_in.body["access_expires_at"].as_str().is_some());
        assert!(logged_in.body["refresh_expires_at"].as_str().is_some());
        let device = device_id(&logged_in);
        let old = refresh(&logged_in);

        let rotated = refresh_token(&env, &old).await;
        assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.text);
        assert_ne!(refresh(&rotated), old);
        assert_status(
            &refresh_token(&env, &old).await,
            StatusCode::UNAUTHORIZED,
            "REFRESH_TOKEN_INVALID",
        );
        assert_status(
            &refresh_token(&env, &refresh(&rotated)).await,
            StatusCode::UNAUTHORIZED,
            "REFRESH_TOKEN_INVALID",
        );
        assert_status(
            &call(
                &env.app,
                bearer("GET", "/api/v1/auth/session", &access(&rotated)),
            )
            .await,
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
        );
        assert_eq!(
            env.count(&format!("SELECT count(*) FROM device_sessions WHERE id = '{device}' AND revoked_at IS NOT NULL"))
                .await,
            1
        );
        wait_for_audit(&env, "auth.device_refresh_reuse", 1).await;
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn refresh_rejects_idle_and_absolute_expiry() {
        let mut idle_settings: AuthSettings = settings();
        idle_settings.device_idle_timeout = Duration::from_secs(10 * 24 * 3600);
        idle_settings.device_absolute_timeout = Duration::from_secs(90 * 24 * 3600);
        let env = TestEnv::with(idle_settings, test_config()).await;
        env.user("idle").await;
        let token = refresh(&device_for(&env, "idle@example.test").await);
        env.clock.advance(Duration::from_secs(11 * 24 * 3600));
        assert_status(
            &refresh_token(&env, &token).await,
            StatusCode::UNAUTHORIZED,
            "REFRESH_TOKEN_INVALID",
        );

        let mut absolute_settings: AuthSettings = settings();
        absolute_settings.device_idle_timeout = Duration::from_secs(30 * 24 * 3600);
        absolute_settings.device_absolute_timeout = Duration::from_secs(20 * 24 * 3600);
        let env = TestEnv::with(absolute_settings, test_config()).await;
        env.user("absolute").await;
        let token = refresh(&device_for(&env, "absolute@example.test").await);
        env.clock.advance(Duration::from_secs(21 * 24 * 3600));
        assert_status(
            &refresh_token(&env, &token).await,
            StatusCode::UNAUTHORIZED,
            "REFRESH_TOKEN_INVALID",
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn access_tokens_expire_and_rotated_refresh_replay_race_revokes_both_results() {
        let mut short_access: AuthSettings = settings();
        short_access.device_access_ttl = Duration::from_secs(30);
        let env = TestEnv::with(short_access, test_config()).await;
        env.user("access-expiry").await;
        let device = device_for(&env, "access-expiry@example.test").await;
        env.clock.advance(Duration::from_secs(31));
        assert_status(
            &call(
                &env.app,
                bearer("GET", "/api/v1/auth/session", &access(&device)),
            )
            .await,
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
        );

        let env = device_test_env().await;
        env.user("refresh-race").await;
        let device = device_for(&env, "refresh-race@example.test").await;
        let old = refresh(&device);
        let (first, second) = tokio::join!(refresh_token(&env, &old), refresh_token(&env, &old));
        let mut statuses = [first.status.as_u16(), second.status.as_u16()];
        statuses.sort_unstable();
        assert_eq!(statuses, [200, 401]);
        let rotated = if first.status == StatusCode::OK {
            first
        } else {
            second
        };
        assert_status(
            &call(
                &env.app,
                bearer("GET", "/api/v1/auth/session", &access(&rotated)),
            )
            .await,
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn bearer_auth_works_without_csrf_and_rejects_mixed_credentials() {
        let env = device_test_env().await;
        env.user("bearer").await;
        let web = env.login_ok("bearer").await;
        let device = device_for(&env, "bearer@example.test").await;
        let token = access(&device);
        assert!(token.starts_with("ccd_at_"));
        assert!(refresh(&device).starts_with("ccd_rt_"));
        assert_eq!(
            call(&env.app, bearer("GET", "/api/v1/auth/session", &token))
                .await
                .status,
            StatusCode::OK
        );
        let devices = call(&env.app, bearer("GET", "/api/v1/auth/devices", &token)).await;
        assert_eq!(devices.status, StatusCode::OK, "{}", devices.text);
        assert_eq!(devices.body["devices"].as_array().unwrap().len(), 1);
        assert_eq!(devices.body["devices"][0]["client_id"], "photocraft");
        assert!(!devices.text.contains(&token));
        assert_eq!(
            call(&env.app, bearer("GET", "/api/v1/organizations", &token))
                .await
                .status,
            StatusCode::OK
        );
        let mixed = call(
            &env.app,
            Req::new("GET", "/api/v1/auth/session")
                .cookie_only(&web)
                .header("Authorization", &format!("Bearer {token}")),
        )
        .await;
        assert_status(&mixed, StatusCode::BAD_REQUEST, "AMBIGUOUS_CREDENTIALS");

        let stranger = env.user("foreign").await;
        let operator = env.login_ok("op").await;
        let foreign_org = create_org(&env, &operator, stranger).await;
        let path = format!("/api/v1/organizations/{foreign_org}/workspaces");
        let via_bearer = call(&env.app, bearer("GET", &path, &token)).await;
        let via_cookie = call(&env.app, Req::new("GET", &path).cookie_only(&web)).await;
        assert_status(&via_bearer, StatusCode::NOT_FOUND, "NOT_FOUND");
        assert_eq!(
            (
                via_bearer.status,
                via_bearer.body["error"]["message"].clone()
            ),
            (
                via_cookie.status,
                via_cookie.body["error"]["message"].clone()
            )
        );

        let csrf = call(
            &env.app,
            Req::new("POST", "/api/v1/auth/logout-all").cookie_only(&web),
        )
        .await;
        assert_status(&csrf, StatusCode::FORBIDDEN, "CSRF_FAILED");
        assert_eq!(
            call_from_proxy(
                &env.app,
                Req::new("POST", "/api/v1/auth/device/logout")
                    .header("Authorization", &format!("Bearer {token}")),
                "127.0.0.1",
                Some("https"),
            )
            .await
            .status,
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn bearer_invitation_accept_uses_origin_exemption_but_cookie_stays_protected() {
        let env = device_test_env().await;
        let operator = env.login_ok("op").await;
        let owner_id = env.user("invite-owner").await;
        let owner = env.login_ok("invite-owner").await;
        let recipient_id = env.user("invite-recipient").await;
        let recipient_cookie = env.login_ok("invite-recipient").await;
        let recipient_device = device_for(&env, "invite-recipient@example.test").await;
        let org = create_org(&env, &operator, owner_id).await;
        let invited = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/invitations"))
                .session(&owner)
                .json(json!({"email":"invite-recipient@example.test","org_role":"member"})),
        )
        .await;
        assert_eq!(invited.status, StatusCode::CREATED, "{}", invited.text);
        let token = invited.body["invite_url"]
            .as_str()
            .unwrap()
            .split("#token=")
            .nth(1)
            .unwrap();
        let path = "/api/v1/invitations/accept";
        let cookie_cross_origin = call(
            &env.app,
            Req::new("POST", path)
                .session(&recipient_cookie)
                .header("Origin", "https://attacker.example")
                .json(json!({"token":token})),
        )
        .await;
        assert_status(&cookie_cross_origin, StatusCode::FORBIDDEN, "CSRF_FAILED");
        let bearer_cross_origin = call(
            &env.app,
            Req::new("POST", path)
                .header(
                    "Authorization",
                    &format!("Bearer {}", access(&recipient_device)),
                )
                .header("Origin", "https://attacker.example")
                .json(json!({"token":token})),
        )
        .await;
        assert_eq!(
            bearer_cross_origin.status,
            StatusCode::OK,
            "{}",
            bearer_cross_origin.text
        );
        assert_eq!(
            bearer_cross_origin.body["user_id"],
            recipient_id.to_string()
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn bearer_auth_reaches_workspace_project_file_version_usage_invite_audit_and_admin_routes()
     {
        let env = device_test_env().await;
        let owner_id = env.user("module-owner").await;
        let owner = env.login_ok("module-owner").await;
        let device = device_for(&env, "module-owner@example.test").await;
        let token = access(&device);
        let operator = env.login_ok("op").await;
        let org = create_org(&env, &operator, owner_id).await;
        let workspace = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/workspaces"))
                .session(&owner)
                .json(json!({"name":"Main","slug":"main"})),
        )
        .await;
        assert_eq!(workspace.status, StatusCode::CREATED, "{}", workspace.text);
        let workspace_id = workspace.body["id"].as_str().unwrap();
        let project = call(
            &env.app,
            Req::new(
                "POST",
                &format!("/api/v1/workspaces/{workspace_id}/projects"),
            )
            .session(&owner)
            .json(json!({"name":"Project"})),
        )
        .await;
        assert_eq!(project.status, StatusCode::CREATED, "{}", project.text);
        let project_id = project.body["id"].as_str().unwrap();
        let file = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/projects/{project_id}/files"))
                .session(&owner)
                .json(json!({"name":"design.fig","app":"figcraft","format":"fig"})),
        )
        .await;
        assert_eq!(file.status, StatusCode::CREATED, "{}", file.text);
        let file_id = file.body["id"].as_str().unwrap();

        let cases = [
            format!("/api/v1/organizations/{org}/workspaces"),
            format!("/api/v1/workspaces/{workspace_id}/projects"),
            format!("/api/v1/projects/{project_id}/files"),
            format!("/api/v1/files/{file_id}/versions"),
            format!("/api/v1/organizations/{org}/usage"),
            format!("/api/v1/organizations/{org}/invitations"),
            format!("/api/v1/organizations/{org}/audit"),
        ];
        for path in cases {
            let response = call(&env.app, bearer("GET", &path, &token)).await;
            assert_eq!(
                response.status,
                StatusCode::OK,
                "GET {path}: {}",
                response.text
            );
        }

        let operator_device = device_for(&env, "op@example.test").await;
        let admin = call(
            &env.app,
            bearer("GET", "/api/v1/admin/health", &access(&operator_device)),
        )
        .await;
        assert_eq!(admin.status, StatusCode::OK, "{}", admin.text);

        let query_token = call(
            &env.app,
            Req::new("GET", &format!("/api/v1/auth/session?access_token={token}")),
        )
        .await;
        assert_status(&query_token, StatusCode::UNAUTHORIZED, "UNAUTHENTICATED");
    }

    async fn create_org(env: &TestEnv, operator: &Session, owner: Uuid) -> Uuid {
        let response = call(
            &env.app,
            Req::new("POST", "/api/v1/organizations")
                .session(operator)
                .json(json!({"name":"Foreign Org","slug":format!("foreign-{}", owner.simple()),"owner_user_id":owner.to_string()})),
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED, "{}", response.text);
        Uuid::parse_str(response.body["id"].as_str().unwrap()).unwrap()
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn password_and_account_actions_revoke_device_sessions() {
        let env = device_test_env().await;
        env.user("change").await;
        let web = env.login_ok("change").await;
        let device = device_for(&env, "change@example.test").await;
        let changed = call(
            &env.app,
            Req::new("POST", "/api/v1/auth/password/change")
                .session(&web)
                .json(json!({"current_password":PASSWORD,"new_password":"different-password-123"})),
        )
        .await;
        assert_eq!(changed.status, StatusCode::NO_CONTENT, "{}", changed.text);
        assert_status(
            &refresh_token(&env, &refresh(&device)).await,
            StatusCode::UNAUTHORIZED,
            "REFRESH_TOKEN_INVALID",
        );

        let reset_id = env.user("reset").await;
        let reset_device = device_for(&env, "reset@example.test").await;
        let operator = env.login_ok("op").await;
        let issued = call(
            &env.app,
            Req::new(
                "POST",
                &format!("/api/v1/admin/users/{reset_id}/password-reset"),
            )
            .session(&operator),
        )
        .await;
        assert_eq!(issued.status, StatusCode::CREATED, "{}", issued.text);
        let reset_revoke_actor: Uuid = sqlx::query_scalar(
            "SELECT actor_id FROM audit_events WHERE action='auth.device_revoked' AND target_id=$1",
        )
        .bind(device_id(&reset_device))
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(reset_revoke_actor, env.operator_id);
        let url = issued.body["reset_url"].as_str().unwrap();
        let reset_token = url.split("#token=").nth(1).unwrap();
        let reset = call(
            &env.app,
            Req::new("POST", "/api/v1/auth/password-reset")
                .json(json!({"token":reset_token,"new_password":"new-reset-password-123"})),
        )
        .await;
        assert_eq!(reset.status, StatusCode::NO_CONTENT, "{}", reset.text);
        assert_status(
            &refresh_token(&env, &refresh(&reset_device)).await,
            StatusCode::UNAUTHORIZED,
            "REFRESH_TOKEN_INVALID",
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn logout_all_disable_and_last_org_removal_revoke_devices() {
        let env = device_test_env().await;
        env.user("logout").await;
        let logout_web = env.login_ok("logout").await;
        let logout_device = device_for(&env, "logout@example.test").await;
        let result = call(
            &env.app,
            Req::new("POST", "/api/v1/auth/logout-all").session(&logout_web),
        )
        .await;
        assert_eq!(result.status, StatusCode::NO_CONTENT);
        assert_status(
            &refresh_token(&env, &refresh(&logout_device)).await,
            StatusCode::UNAUTHORIZED,
            "REFRESH_TOKEN_INVALID",
        );

        let disabled_id = env.user("disabled").await;
        let disabled_device = device_for(&env, "disabled@example.test").await;
        let operator = env.login_ok("op").await;
        let disabled = call(
            &env.app,
            Req::new(
                "POST",
                &format!("/api/v1/admin/users/{disabled_id}/disable"),
            )
            .session(&operator),
        )
        .await;
        assert_eq!(disabled.status, StatusCode::OK, "{}", disabled.text);
        let disable_revoke_actor: Uuid = sqlx::query_scalar(
            "SELECT actor_id FROM audit_events WHERE action='auth.device_revoked' AND target_id=$1",
        )
        .bind(device_id(&disabled_device))
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(disable_revoke_actor, env.operator_id);
        assert_status(
            &refresh_token(&env, &refresh(&disabled_device)).await,
            StatusCode::UNAUTHORIZED,
            "REFRESH_TOKEN_INVALID",
        );

        let member_id = env.user("member").await;
        let owner_id = env.user("owner").await;
        let owner = env.login_ok("owner").await;
        let member_device = device_for(&env, "member@example.test").await;
        let org = create_org(&env, &operator, owner_id).await;
        let added = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/members"))
                .session(&owner)
                .json(json!({"user_id":member_id.to_string(),"role":"member"})),
        )
        .await;
        assert_eq!(added.status, StatusCode::CREATED, "{}", added.text);
        let removed = call(
            &env.app,
            Req::new(
                "DELETE",
                &format!("/api/v1/organizations/{org}/members/{member_id}"),
            )
            .session(&owner),
        )
        .await;
        assert_eq!(removed.status, StatusCode::NO_CONTENT, "{}", removed.text);
        assert_status(
            &refresh_token(&env, &refresh(&member_device)).await,
            StatusCode::UNAUTHORIZED,
            "REFRESH_TOKEN_INVALID",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn device_password_change_and_cookie_logout_all_serialize_without_deadlock() {
        let env = device_test_env().await;
        let user_id = env.user("serial-auth").await;
        let logout_session = env.login_ok("serial-auth").await;
        let device = device_for(&env, "serial-auth@example.test").await;
        let device_token = access(&device);
        let barrier = Arc::new(Barrier::new(3));

        let change_barrier = Arc::clone(&barrier);
        let change_app = env.app.clone();
        let change = tokio::spawn(async move {
            change_barrier.wait().await;
            call(
                &change_app,
                Req::new("POST", "/api/v1/auth/password/change")
                    .header("Authorization", &format!("Bearer {device_token}"))
                    .json(json!({
                        "current_password": PASSWORD,
                        "new_password": "serialized-password-123"
                    })),
            )
            .await
        });

        let logout_barrier = Arc::clone(&barrier);
        let logout_app = env.app.clone();
        let logout = tokio::spawn(async move {
            logout_barrier.wait().await;
            call(
                &logout_app,
                Req::new("POST", "/api/v1/auth/logout-all").session(&logout_session),
            )
            .await
        });

        barrier.wait().await;
        let (change, logout) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(change, logout)
        })
        .await
        .expect("password change and logout-all deadlocked");
        let (change, logout) = (change.unwrap(), logout.unwrap());
        assert!(
            [StatusCode::NO_CONTENT, StatusCode::UNAUTHORIZED].contains(&change.status),
            "password change: {} {}",
            change.status,
            change.text
        );
        assert!(
            [StatusCode::NO_CONTENT, StatusCode::UNAUTHORIZED].contains(&logout.status),
            "logout-all: {} {}",
            logout.status,
            logout.text
        );
        assert_eq!(
            env.count(&format!(
                "SELECT count(*) FROM device_sessions WHERE user_id='{user_id}' AND revoked_at IS NULL"
            ))
            .await,
            0
        );
        assert_status(
            &refresh_token(&env, &refresh(&device)).await,
            StatusCode::UNAUTHORIZED,
            "REFRESH_TOKEN_INVALID",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn upload_rechecks_a_device_revoked_while_content_is_streaming() {
        let env = device_test_env().await;
        let user_id = env.user("upload-device").await;
        let web = env.login_ok("upload-device").await;
        let device = device_for(&env, "upload-device@example.test").await;
        let token = access(&device);
        let operator = env.login_ok("op").await;
        let org = create_org(&env, &operator, user_id).await;
        let workspace = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/workspaces"))
                .session(&web)
                .json(json!({"name":"Main","slug":"main"})),
        )
        .await;
        assert_eq!(workspace.status, StatusCode::CREATED, "{}", workspace.text);
        let project = call(
            &env.app,
            Req::new(
                "POST",
                &format!(
                    "/api/v1/workspaces/{}/projects",
                    workspace.body["id"].as_str().unwrap()
                ),
            )
            .session(&web)
            .json(json!({"name":"Project"})),
        )
        .await;
        assert_eq!(project.status, StatusCode::CREATED, "{}", project.text);
        let file = call(
            &env.app,
            Req::new(
                "POST",
                &format!(
                    "/api/v1/projects/{}/files",
                    project.body["id"].as_str().unwrap()
                ),
            )
            .session(&web)
            .json(json!({"name":"design.fig","app":"figcraft","format":"fig"})),
        )
        .await;
        assert_eq!(file.status, StatusCode::CREATED, "{}", file.text);

        let data: Vec<u8> = (0..120_000).map(|i| (i as u8).wrapping_mul(31)).collect();
        let sha = Sha256::digest(&data)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let upload = call(
            &env.app,
            bearer(
                "POST",
                &format!(
                    "/api/v1/files/{}/uploads",
                    file.body["id"].as_str().unwrap()
                ),
                &token,
            )
            .json(json!({"size_bytes":data.len(),"sha256":sha})),
        )
        .await;
        assert_eq!(upload.status, StatusCode::CREATED, "{}", upload.text);
        let upload_id = upload.body["upload_id"].as_str().unwrap().to_owned();
        let (tx, rx) = mpsc::channel(2);
        let app = env.app.clone();
        let access = token.clone();
        let path = format!("/api/v1/uploads/{upload_id}/content");
        let put = tokio::spawn(async move {
            call(
                &app,
                Req::new("PUT", &path)
                    .header("Authorization", &format!("Bearer {access}"))
                    .body(Body::new(ChannelBody { rx })),
            )
            .await
        });
        tx.send(Bytes::copy_from_slice(&data[..60_000]))
            .await
            .unwrap();
        let mut receiving = false;
        for _ in 0..500 {
            let state: String =
                sqlx::query_scalar("SELECT state FROM upload_sessions WHERE id = $1")
                    .bind(Uuid::parse_str(&upload_id).unwrap())
                    .fetch_one(&env.pool)
                    .await
                    .unwrap();
            if state == "receiving" {
                receiving = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(receiving, "upload did not reach receiving state");
        let revoked = call(
            &env.app,
            Req::new(
                "DELETE",
                &format!("/api/v1/auth/devices/{}", device_id(&device)),
            )
            .session(&web),
        )
        .await;
        assert_eq!(revoked.status, StatusCode::NO_CONTENT, "{}", revoked.text);
        tx.send(Bytes::copy_from_slice(&data[60_000..]))
            .await
            .unwrap();
        drop(tx);
        assert_status(
            &put.await.unwrap(),
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
        );
        let state: String = sqlx::query_scalar("SELECT state FROM upload_sessions WHERE id = $1")
            .bind(Uuid::parse_str(&upload_id).unwrap())
            .fetch_one(&env.pool)
            .await
            .unwrap();
        assert_eq!(state, "failed");
        assert_eq!(env.count("SELECT count(*) FROM blobs").await, 0);
        assert_eq!(
            env.count("SELECT count(*) FROM device_sessions WHERE revoked_at IS NOT NULL")
                .await,
            1
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn logs_and_audit_never_contain_device_tokens_or_email() {
        let env = device_test_env().await;
        env.user("secret-user").await;
        let logs = capture_logs();
        let logged_in = device_for(&env, "secret-user@example.test").await;
        let raw_access = access(&logged_in);
        let raw_refresh = refresh(&logged_in);
        let _ = refresh_token(&env, &raw_refresh).await;
        wait_for_audit(&env, "auth.device_login", 1).await;
        let log_text = logs.text();
        assert!(!log_text.contains(&raw_access));
        assert!(!log_text.contains(&raw_refresh));
        assert!(!log_text.contains(PASSWORD));
        assert!(!log_text.contains("secret-user@example.test"));
        let audit: String = sqlx::query_scalar(
            "SELECT COALESCE(string_agg(row_to_json(a)::text, ' '), '') FROM audit_events a",
        )
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert!(!audit.contains(&raw_access));
        assert!(!audit.contains(&raw_refresh));
        assert!(!audit.contains(PASSWORD));
        assert!(!audit.contains("secret-user@example.test"));
        assert_eq!(
            env.count("SELECT count(*) FROM audit_events WHERE action = 'auth.device_login'")
                .await,
            1
        );
        assert_eq!(env.count("SELECT count(*) FROM device_sessions WHERE refresh_hash IS NOT NULL AND octet_length(refresh_hash) = 32").await, 1);
    }

    async fn wait_for_audit(env: &TestEnv, action: &str, wanted: i64) {
        for _ in 0..200 {
            let count = env
                .count(&format!(
                    "SELECT count(*) FROM audit_events WHERE action = '{action}'"
                ))
                .await;
            if count >= wanted {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("audit action {action} did not reach count {wanted}");
    }
}
