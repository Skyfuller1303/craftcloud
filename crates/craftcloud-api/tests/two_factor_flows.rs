//! AC-02 end-to-end HTTP flows. Requires PostgreSQL: `cargo xtask db-test`.

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, sync::Arc, time::Duration};

    use axum::{Router, extract::ConnectInfo, http::StatusCode};
    use craftcloud_api::{ApiConfig, AuthSettings, Clock, Services, router_with_shared};
    use craftcloud_auth::base32_decode;
    use craftcloud_core::Capabilities;
    use hmac::{Hmac, Mac};
    use serde_json::{Value, json};
    use sha1::Sha1;
    use tower::ServiceExt;

    use crate::common::{
        PASSWORD, Req, Resp, Session, TestEnv, call, capture_logs, session_from, settings,
    };

    type HmacSha1 = Hmac<Sha1>;
    const KEY: [u8; 32] = [71; 32];
    const OLD_KEY: [u8; 32] = [72; 32];
    const DEVICE_IP: &str = "127.0.0.1";

    fn app(env: &TestEnv, key: Option<[u8; 32]>, old: Option<[u8; 32]>) -> Router {
        app_with_settings(env, key, old, settings())
    }

    fn app_with_settings(
        env: &TestEnv,
        key: Option<[u8; 32]>,
        old: Option<[u8; 32]>,
        settings: AuthSettings,
    ) -> Router {
        let services = Services::new(env.pool.clone(), None, settings)
            .with_clock(env.clock.clone())
            .with_two_factor_keys(key, old);
        router_with_shared(
            Capabilities::development_defaults(),
            &ApiConfig {
                trusted_proxies: vec!["127.0.0.0/8".parse().unwrap()],
                ..ApiConfig::default()
            },
            Some(Arc::new(services)),
        )
    }

    async fn call_device_from(app: &Router, req: Req, peer: &str) -> Resp {
        let mut request = req.build();
        request.extensions_mut().insert(ConnectInfo(
            format!("{peer}:443").parse::<SocketAddr>().unwrap(),
        ));
        request
            .headers_mut()
            .insert("x-forwarded-proto", "https".parse().unwrap());
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

    async fn call_device(app: &Router, req: Req) -> Resp {
        call_device_from(app, req, DEVICE_IP).await
    }

    fn otp(secret: &str, unix_seconds: i64) -> String {
        let key = base32_decode(secret).unwrap();
        let step = (unix_seconds / 30) as u64;
        let mut mac = HmacSha1::new_from_slice(&key).unwrap();
        mac.update(&step.to_be_bytes());
        let digest = mac.finalize().into_bytes();
        let offset = (digest[19] & 0x0f) as usize;
        let value =
            u32::from_be_bytes(digest[offset..offset + 4].try_into().unwrap()) & 0x7fff_ffff;
        format!("{:06}", value % 1_000_000)
    }

    async fn setup(app: &Router, env: &TestEnv, name: &str) -> (Session, String, Vec<String>) {
        let session = env.login_ok(name).await;
        let pending = call(
            app,
            Req::new("POST", "/api/v1/auth/2fa/setup")
                .session(&session)
                .json(json!({"password":PASSWORD})),
        )
        .await;
        assert_eq!(pending.status, StatusCode::OK, "{}", pending.text);
        let secret = pending.body["secret"].as_str().unwrap().to_owned();
        assert!(
            pending.body["otpauth_url"]
                .as_str()
                .unwrap()
                .contains(&secret)
        );
        let confirmation = call(
            app,
            Req::new("POST", "/api/v1/auth/2fa/confirm")
                .session(&session)
                .json(json!({"code":otp(&secret, env.clock.now().unix_timestamp())})),
        )
        .await;
        assert_eq!(confirmation.status, StatusCode::OK, "{}", confirmation.text);
        let codes: Vec<String> = confirmation.body["recovery_codes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(codes.len(), 10);
        // Confirmation consumed its timestep; subsequent sign-ins need a new one.
        env.clock.advance(Duration::from_secs(30));
        (session, secret, codes)
    }

    fn challenge(resp: &Resp) -> String {
        assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "{}", resp.text);
        assert_eq!(resp.code(), "SECOND_FACTOR_REQUIRED");
        resp.body["error"]["details"]["challenge_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn setup_encrypts_secret_and_browser_login_accepts_totp_once_and_recovery_once() {
        let env = TestEnv::new().await;
        env.user("mfa-browser").await;
        let app = app(&env, Some(KEY), None);
        let (setup_session, secret, recovery_codes) = setup(&app, &env, "mfa-browser").await;
        let status = call(
            &app,
            Req::new("GET", "/api/v1/auth/2fa").session(&setup_session),
        )
        .await;
        assert_eq!(status.status, StatusCode::OK, "{}", status.text);
        assert_eq!(status.body["enabled"], true);
        assert_eq!(status.body["recovery_codes_remaining"], 10);

        let rows: (Vec<u8>, Option<time::OffsetDateTime>) = sqlx::query_as(
            "SELECT secret_ciphertext, enabled_at FROM user_two_factor WHERE user_id=$1",
        )
        .bind(
            sqlx::query_scalar::<_, uuid::Uuid>(
                "SELECT id FROM users WHERE normalized_email='mfa-browser@example.test'",
            )
            .fetch_one(&env.pool)
            .await
            .unwrap(),
        )
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert!(rows.1.is_some());
        assert_ne!(rows.0, base32_decode(&secret).unwrap());
        assert!(
            !rows
                .0
                .windows(secret.len())
                .any(|window| window == secret.as_bytes())
        );

        let login = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"mfa-browser@example.test","password":PASSWORD})),
        )
        .await;
        let id = challenge(&login);
        assert!(login.headers.get("set-cookie").is_none());
        assert!(login.body.get("access_token").is_none());
        let code = otp(&secret, env.clock.now().unix_timestamp());
        let complete = call(
            &app,
            Req::new("POST", "/api/v1/auth/login/second-factor")
                .json(json!({"challenge_id":id,"code":code})),
        )
        .await;
        assert_eq!(complete.status, StatusCode::OK, "{}", complete.text);
        assert!(complete.headers.get("set-cookie").is_some());

        let replay = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"mfa-browser@example.test","password":PASSWORD})),
        )
        .await;
        let replay_id = challenge(&replay);
        let denied = call(&app, Req::new("POST", "/api/v1/auth/login/second-factor").json(json!({"challenge_id":replay_id,"code":otp(&secret, env.clock.now().unix_timestamp())}))).await;
        assert_eq!(
            (denied.status, denied.code()),
            (StatusCode::UNAUTHORIZED, "INVALID_CREDENTIALS")
        );

        let recovery_login = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"mfa-browser@example.test","password":PASSWORD})),
        )
        .await;
        let recovery_id = challenge(&recovery_login);
        let recovery = call(
            &app,
            Req::new("POST", "/api/v1/auth/login/second-factor")
                .json(json!({"challenge_id":recovery_id,"recovery_code":recovery_codes[0]})),
        )
        .await;
        assert_eq!(recovery.status, StatusCode::OK, "{}", recovery.text);
        let used = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"mfa-browser@example.test","password":PASSWORD})),
        )
        .await;
        let used_id = challenge(&used);
        let rejected = call(
            &app,
            Req::new("POST", "/api/v1/auth/login/second-factor")
                .json(json!({"challenge_id":used_id,"recovery_code":recovery_codes[0]})),
        )
        .await;
        assert_eq!(
            (rejected.status, rejected.code()),
            (StatusCode::UNAUTHORIZED, "INVALID_CREDENTIALS")
        );

        let regenerated = call(
            &app,
            Req::new("POST", "/api/v1/auth/2fa/recovery-codes")
                .session(&setup_session)
                .json(json!({"code":otp(&secret,env.clock.now().unix_timestamp())})),
        )
        .await;
        assert_eq!(
            regenerated.status,
            StatusCode::UNAUTHORIZED,
            "same TOTP step must not be reused"
        );
        env.clock.advance(Duration::from_secs(30));
        let regenerated = call(
            &app,
            Req::new("POST", "/api/v1/auth/2fa/recovery-codes")
                .session(&setup_session)
                .json(json!({"code":otp(&secret,env.clock.now().unix_timestamp())})),
        )
        .await;
        assert_eq!(regenerated.status, StatusCode::OK, "{}", regenerated.text);
        assert_eq!(
            regenerated.body["recovery_codes"].as_array().unwrap().len(),
            10
        );
        let new_code = regenerated.body["recovery_codes"][0].as_str().unwrap();
        let old_codes = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"mfa-browser@example.test","password":PASSWORD})),
        )
        .await;
        let old_challenge = challenge(&old_codes);
        let old_recovery = call(
            &app,
            Req::new("POST", "/api/v1/auth/login/second-factor")
                .json(json!({"challenge_id":old_challenge,"recovery_code":recovery_codes[1]})),
        )
        .await;
        assert_eq!(old_recovery.status, StatusCode::UNAUTHORIZED);
        let fresh_codes = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"mfa-browser@example.test","password":PASSWORD})),
        )
        .await;
        let fresh_challenge = challenge(&fresh_codes);
        let fresh_recovery = call(
            &app,
            Req::new("POST", "/api/v1/auth/login/second-factor")
                .json(json!({"challenge_id":fresh_challenge,"recovery_code":new_code})),
        )
        .await;
        assert_eq!(
            fresh_recovery.status,
            StatusCode::OK,
            "{}",
            fresh_recovery.text
        );
        env.clock.advance(Duration::from_secs(30));
        let disabled = call(&app, Req::new("POST", "/api/v1/auth/2fa/disable").session(&setup_session).json(json!({"password":PASSWORD,"code":otp(&secret,env.clock.now().unix_timestamp())}))).await;
        assert_eq!(disabled.status, StatusCode::NO_CONTENT, "{}", disabled.text);
        let status = call(
            &app,
            Req::new("GET", "/api/v1/auth/2fa").session(&setup_session),
        )
        .await;
        assert_eq!(status.body["enabled"], false);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn device_login_requires_challenge_then_issues_device_tokens_for_totp_and_recovery() {
        let env = TestEnv::new().await;
        env.user("mfa-device").await;
        let app = app(&env, Some(KEY), None);
        let (_, secret, recovery_codes) = setup(&app, &env, "mfa-device").await;
        let body = json!({"email":"mfa-device@example.test","password":PASSWORD,"client_kind":"app","client_id":"craftcloud-test","device_name":"Test device"});
        let pending = call_device(
            &app,
            Req::new("POST", "/api/v1/auth/device/login").json(body.clone()),
        )
        .await;
        let id = challenge(&pending);
        assert!(pending.body.get("access_token").is_none());
        assert!(pending.body.get("refresh_token").is_none());
        let complete = call_device(
            &app,
            Req::new("POST", "/api/v1/auth/device/login/second-factor").json(
                json!({"challenge_id":id,"code":otp(&secret,env.clock.now().unix_timestamp())}),
            ),
        )
        .await;
        assert_eq!(complete.status, StatusCode::OK, "{}", complete.text);
        assert!(complete.body["access_token"].as_str().is_some());
        assert!(complete.body["refresh_token"].as_str().is_some());

        let pending = call_device(
            &app,
            Req::new("POST", "/api/v1/auth/device/login").json(body),
        )
        .await;
        let id = challenge(&pending);
        let complete = call_device(
            &app,
            Req::new("POST", "/api/v1/auth/device/login/second-factor")
                .json(json!({"challenge_id":id,"recovery_code":recovery_codes[1]})),
        )
        .await;
        assert_eq!(complete.status, StatusCode::OK, "{}", complete.text);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn missing_key_fails_setup_closed_and_reports_health_unavailable() {
        let env = TestEnv::new().await;
        env.user("mfa-nokey").await;
        let app = app(&env, None, None);
        let session = env.login_ok("op").await;
        let setup = call(
            &app,
            Req::new("POST", "/api/v1/auth/2fa/setup")
                .session(&session)
                .json(json!({"password":PASSWORD})),
        )
        .await;
        assert_eq!(
            (setup.status, setup.code()),
            (StatusCode::SERVICE_UNAVAILABLE, "TWO_FACTOR_UNAVAILABLE")
        );
        let health = call(
            &app,
            Req::new("GET", "/api/v1/admin/health").session(&session),
        )
        .await;
        assert_eq!(health.status, StatusCode::OK, "{}", health.text);
        assert_eq!(
            health.body["components"]["two_factor"]["status"],
            "not_configured"
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn challenge_expires_after_five_minutes_and_is_bound_to_device_kind_and_address() {
        let env = TestEnv::new().await;
        env.user("mfa-bound").await;
        let app = app(&env, Some(KEY), None);
        let (_, secret, _) = setup(&app, &env, "mfa-bound").await;
        let body = json!({"email":"mfa-bound@example.test","password":PASSWORD,"client_kind":"app","client_id":"craftcloud-test","device_name":"Test device"});
        let pending = call_device(
            &app,
            Req::new("POST", "/api/v1/auth/device/login").json(body),
        )
        .await;
        let id = challenge(&pending);

        let wrong_kind = call(
            &app,
            Req::new("POST", "/api/v1/auth/login/second-factor").json(
                json!({"challenge_id":id,"code":otp(&secret,env.clock.now().unix_timestamp())}),
            ),
        )
        .await;
        assert_eq!(wrong_kind.status, StatusCode::UNAUTHORIZED);

        let pending = call_device(&app, Req::new("POST", "/api/v1/auth/device/login").json(json!({"email":"mfa-bound@example.test","password":PASSWORD,"client_kind":"app","client_id":"craftcloud-test","device_name":"Test device"}))).await;
        let id = challenge(&pending);
        let wrong_address = call_device_from(
            &app,
            Req::new("POST", "/api/v1/auth/device/login/second-factor").json(
                json!({"challenge_id":id,"code":otp(&secret,env.clock.now().unix_timestamp())}),
            ),
            "127.0.0.2",
        )
        .await;
        assert_eq!(wrong_address.status, StatusCode::UNAUTHORIZED);

        let pending = call_device(&app, Req::new("POST", "/api/v1/auth/device/login").json(json!({"email":"mfa-bound@example.test","password":PASSWORD,"client_kind":"app","client_id":"craftcloud-test","device_name":"Test device"}))).await;
        let id = challenge(&pending);
        for _ in 0..5 {
            let failed = call_device(
                &app,
                Req::new("POST", "/api/v1/auth/device/login/second-factor")
                    .json(json!({"challenge_id":id,"code":"nope"})),
            )
            .await;
            assert_eq!(
                (failed.status, failed.code()),
                (StatusCode::UNAUTHORIZED, "INVALID_CREDENTIALS")
            );
        }
        let sixth = call_device(
            &app,
            Req::new("POST", "/api/v1/auth/device/login/second-factor").json(
                json!({"challenge_id":id,"code":otp(&secret,env.clock.now().unix_timestamp())}),
            ),
        )
        .await;
        assert_eq!(sixth.status, StatusCode::UNAUTHORIZED);

        let pending = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"mfa-bound@example.test","password":PASSWORD})),
        )
        .await;
        let id = challenge(&pending);
        env.clock.advance(Duration::from_secs(301));
        let expired = call(
            &app,
            Req::new("POST", "/api/v1/auth/login/second-factor").json(
                json!({"challenge_id":id,"code":otp(&secret,env.clock.now().unix_timestamp())}),
            ),
        )
        .await;
        assert_eq!(expired.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn password_change_invalidates_an_outstanding_login_challenge() {
        let env = TestEnv::new().await;
        env.user("mfa-password-change").await;
        let app = app(&env, Some(KEY), None);
        let (session, secret, _) = setup(&app, &env, "mfa-password-change").await;
        let pending = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"mfa-password-change@example.test","password":PASSWORD})),
        )
        .await;
        let id = challenge(&pending);
        let changed = call(&app, Req::new("POST", "/api/v1/auth/password/change").session(&session).json(json!({"current_password":PASSWORD,"new_password":"mfa-password-changed-password-1"}))).await;
        assert_eq!(changed.status, StatusCode::NO_CONTENT, "{}", changed.text);
        let rejected = call(
            &app,
            Req::new("POST", "/api/v1/auth/login/second-factor").json(
                json!({"challenge_id":id,"code":otp(&secret,env.clock.now().unix_timestamp())}),
            ),
        )
        .await;
        assert_eq!(rejected.status, StatusCode::UNAUTHORIZED);
        assert!(rejected.headers.get("set-cookie").is_none());
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn concurrent_browser_challenges_accept_the_same_totp_step_once() {
        let env = TestEnv::new().await;
        env.user("mfa-concurrent").await;
        let app = app(&env, Some(KEY), None);
        let (_, secret, _) = setup(&app, &env, "mfa-concurrent").await;
        let login = || {
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"mfa-concurrent@example.test","password":PASSWORD}))
        };
        let first = call(&app, login()).await;
        let second = call(&app, login()).await;
        let first_id = challenge(&first);
        let second_id = challenge(&second);
        let code = otp(&secret, env.clock.now().unix_timestamp());
        let first_req = Req::new("POST", "/api/v1/auth/login/second-factor")
            .json(json!({"challenge_id":first_id,"code":code}));
        let second_req = Req::new("POST", "/api/v1/auth/login/second-factor")
            .json(json!({"challenge_id":second_id,"code":code}));
        let (first, second) = tokio::join!(call(&app, first_req), call(&app, second_req));
        assert_eq!(
            usize::from(first.status == StatusCode::OK)
                + usize::from(second.status == StatusCode::OK),
            1,
            "first={}, second={}",
            first.text,
            second.text
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn organization_owner_must_enable_own_factor_before_requiring_it() {
        let env = TestEnv::new().await;
        let operator = env.login_ok("op").await;
        let owner_id = env.user("mfa-org-owner").await;
        let owner = env.login_ok("mfa-org-owner").await;
        let app = app(&env, Some(KEY), None);
        let created = call(
            &app,
            Req::new("POST", "/api/v1/organizations")
                .session(&operator)
                .json(json!({
                    "name":"MFA Policy Org",
                    "slug":format!("mfa-policy-{}", uuid::Uuid::new_v4().simple()),
                    "owner_user_id":owner_id.to_string()
                })),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        let org_id = created.body["id"].as_str().unwrap();
        let org = call(
            &app,
            Req::new("GET", &format!("/api/v1/organizations/{org_id}")).session(&owner),
        )
        .await;
        assert_eq!(org.status, StatusCode::OK, "{}", org.text);
        let path = format!("/api/v1/organizations/{org_id}");
        let denied = call(
            &app,
            Req::new("PATCH", &path)
                .session(&owner)
                .if_match(&org.etag())
                .json(json!({"require_two_factor":true})),
        )
        .await;
        assert_eq!(
            (denied.status, denied.code()),
            (StatusCode::FORBIDDEN, "TWO_FACTOR_SETUP_REQUIRED")
        );

        let pending = call(
            &app,
            Req::new("POST", "/api/v1/auth/2fa/setup")
                .session(&owner)
                .json(json!({"password":PASSWORD})),
        )
        .await;
        assert_eq!(pending.status, StatusCode::OK, "{}", pending.text);
        let secret = pending.body["secret"].as_str().unwrap();
        let confirmed = call(
            &app,
            Req::new("POST", "/api/v1/auth/2fa/confirm")
                .session(&owner)
                .json(json!({"code":otp(secret,env.clock.now().unix_timestamp())})),
        )
        .await;
        assert_eq!(confirmed.status, StatusCode::OK, "{}", confirmed.text);
        let updated = call(
            &app,
            Req::new("PATCH", &path)
                .session(&owner)
                .if_match(&org.etag())
                .json(json!({"require_two_factor":true})),
        )
        .await;
        assert_eq!(updated.status, StatusCode::OK, "{}", updated.text);
        assert_eq!(updated.body["require_two_factor"], true);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn admin_reset_flag_is_audited_deletes_factor_and_invalidates_sessions_and_challenges() {
        let env = TestEnv::new().await;
        let victim_id = env.user("mfa-admin-reset").await;
        let app = app(&env, Some(KEY), None);
        let (current, secret, _) = setup(&app, &env, "mfa-admin-reset").await;
        let pending = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"mfa-admin-reset@example.test","password":PASSWORD})),
        )
        .await;
        let challenge_id = challenge(&pending);
        let device = call_device(
            &app,
            Req::new("POST", "/api/v1/auth/device/login").json(json!({
                "email":"mfa-admin-reset@example.test",
                "password":PASSWORD,
                "client_kind":"app",
                "client_id":"craftcloud-test",
                "device_name":"Reset victim device"
            })),
        )
        .await;
        let device_challenge = challenge(&device);
        let device_login = call_device(
            &app,
            Req::new("POST", "/api/v1/auth/device/login/second-factor")
                .json(json!({"challenge_id":device_challenge,"code":otp(&secret,env.clock.now().unix_timestamp())})),
        )
        .await;
        assert_eq!(device_login.status, StatusCode::OK, "{}", device_login.text);
        let refresh = device_login.body["refresh_token"]
            .as_str()
            .unwrap()
            .to_owned();
        let operator = env.login_ok("op").await;
        let issued = call(
            &app,
            Req::new(
                "POST",
                &format!("/api/v1/admin/users/{victim_id}/password-reset"),
            )
            .session(&operator)
            .json(json!({"reset_two_factor":true})),
        )
        .await;
        assert_eq!(issued.status, StatusCode::CREATED, "{}", issued.text);
        assert_eq!(
            env.count(&format!(
                "SELECT count(*) FROM password_reset_tokens WHERE user_id='{victim_id}'"
            ))
            .await,
            1
        );
        assert_eq!(
            env.count(&format!("SELECT count(*) FROM audit_events WHERE action='auth.two_factor_reset' AND target_id='{victim_id}'")).await,
            1
        );
        assert_eq!(
            env.count(&format!(
                "SELECT count(*) FROM user_two_factor WHERE user_id='{victim_id}'"
            ))
            .await,
            0
        );
        assert_eq!(
            env.count(&format!(
                "SELECT count(*) FROM two_factor_login_challenges WHERE user_id='{victim_id}'"
            ))
            .await,
            0
        );
        let challenge_attempt = call(
            &app,
            Req::new("POST", "/api/v1/auth/login/second-factor")
                .json(json!({"challenge_id":challenge_id,"code":otp(&secret,env.clock.now().unix_timestamp())})),
        )
        .await;
        assert_eq!(challenge_attempt.status, StatusCode::UNAUTHORIZED);
        let refresh_attempt = call_device(
            &app,
            Req::new("POST", "/api/v1/auth/device/refresh").json(json!({"refresh_token":refresh})),
        )
        .await;
        assert_eq!(refresh_attempt.status, StatusCode::UNAUTHORIZED);

        let reset_url = issued.body["reset_url"].as_str().unwrap();
        let token = reset_url.split("#token=").nth(1).unwrap();
        let completed = call(
            &app,
            Req::new("POST", "/api/v1/auth/password-reset")
                .json(json!({"token":token,"new_password":"mfa-admin-reset-password-2"})),
        )
        .await;
        assert_eq!(
            completed.status,
            StatusCode::NO_CONTENT,
            "{}",
            completed.text
        );
        let signed_in = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"mfa-admin-reset@example.test","password":"mfa-admin-reset-password-2"})),
        )
        .await;
        assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.text);
        assert!(signed_in.headers.get("set-cookie").is_some());
        assert_eq!(
            env.get_session(&current).await.status,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn current_and_old_server_keys_decrypt_existing_totp_fail_closed_without_key() {
        let env = TestEnv::new().await;
        env.user("mfa-key-rotation").await;
        let old_app = app(&env, Some(OLD_KEY), None);
        let (_, secret, _) = setup(&old_app, &env, "mfa-key-rotation").await;
        let rotating_app = app(&env, Some(KEY), Some(OLD_KEY));
        let pending = call(
            &rotating_app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"mfa-key-rotation@example.test","password":PASSWORD})),
        )
        .await;
        let id = challenge(&pending);
        let completed = call(
            &rotating_app,
            Req::new("POST", "/api/v1/auth/login/second-factor").json(
                json!({"challenge_id":id,"code":otp(&secret,env.clock.now().unix_timestamp())}),
            ),
        )
        .await;
        assert_eq!(completed.status, StatusCode::OK, "{}", completed.text);

        let pending = call(
            &rotating_app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"mfa-key-rotation@example.test","password":PASSWORD})),
        )
        .await;
        let id = challenge(&pending);
        env.clock.advance(Duration::from_secs(30));
        let completed = call(
            &rotating_app,
            Req::new("POST", "/api/v1/auth/login/second-factor").json(
                json!({"challenge_id":id,"code":otp(&secret,env.clock.now().unix_timestamp())}),
            ),
        )
        .await;
        assert_eq!(completed.status, StatusCode::OK, "{}", completed.text);

        let wrong_key = app(&env, Some([73; 32]), None);
        let pending = call(
            &wrong_key,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"mfa-key-rotation@example.test","password":PASSWORD})),
        )
        .await;
        let id = challenge(&pending);
        let unavailable = call(
            &wrong_key,
            Req::new("POST", "/api/v1/auth/login/second-factor").json(
                json!({"challenge_id":id,"code":otp(&secret,env.clock.now().unix_timestamp())}),
            ),
        )
        .await;
        assert_eq!(
            (unavailable.status, unavailable.code()),
            (StatusCode::SERVICE_UNAVAILABLE, "TWO_FACTOR_UNAVAILABLE")
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn fresh_challenges_share_failed_code_account_rate_limit() {
        let env = TestEnv::new().await;
        env.user("mfa-rate-limit").await;
        let mut limited = settings();
        limited.failed_login_account_limit = 2;
        limited.failed_login_pair_limit = 2;
        let app = app_with_settings(&env, Some(KEY), None, limited);
        let (_, _, _) = setup(&app, &env, "mfa-rate-limit").await;
        let mut challenges = Vec::new();
        for _ in 0..3 {
            let pending = call(
                &app,
                Req::new("POST", "/api/v1/auth/login")
                    .json(json!({"email":"mfa-rate-limit@example.test","password":PASSWORD})),
            )
            .await;
            challenges.push(challenge(&pending));
        }
        for id in &challenges[..2] {
            let failed = call(
                &app,
                Req::new("POST", "/api/v1/auth/login/second-factor")
                    .json(json!({"challenge_id":id,"code":"not-a-code"})),
            )
            .await;
            assert_eq!(failed.status, StatusCode::UNAUTHORIZED);
        }
        let pending = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"mfa-rate-limit@example.test","password":PASSWORD})),
        )
        .await;
        assert_eq!(pending.status, StatusCode::TOO_MANY_REQUESTS);
        let id = &challenges[2];
        let blocked = call(
            &app,
            Req::new("POST", "/api/v1/auth/login/second-factor")
                .json(json!({"challenge_id":id,"code":"not-a-code"})),
        )
        .await;
        assert_eq!(
            blocked.status,
            StatusCode::TOO_MANY_REQUESTS,
            "{}",
            blocked.text
        );
        assert_eq!(blocked.code(), "RATE_LIMITED");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn enabling_and_disabling_preserve_current_browser_session_and_revoke_other_browser_and_device_sessions()
     {
        let env = TestEnv::new().await;
        env.user("mfa-session-revoke").await;
        let app = app(&env, Some(KEY), None);
        let other_browser = env.login_ok("mfa-session-revoke").await;
        let device_before = call_device(
            &app,
            Req::new("POST", "/api/v1/auth/device/login").json(json!({
                "email":"mfa-session-revoke@example.test",
                "password":PASSWORD,
                "client_kind":"app",
                "client_id":"craftcloud-test",
                "device_name":"Before enrollment"
            })),
        )
        .await;
        assert_eq!(
            device_before.status,
            StatusCode::OK,
            "{}",
            device_before.text
        );
        let before_refresh = device_before.body["refresh_token"].as_str().unwrap();
        let (current, secret, _) = setup(&app, &env, "mfa-session-revoke").await;
        let status = call(&app, Req::new("GET", "/api/v1/auth/2fa").session(&current)).await;
        assert_eq!(
            status.status,
            StatusCode::OK,
            "current setup session revoked on enable"
        );
        let other = call(
            &app,
            Req::new("GET", "/api/v1/auth/session").cookie_only(&other_browser),
        )
        .await;
        assert_eq!(other.status, StatusCode::UNAUTHORIZED);
        let old_device = call_device(
            &app,
            Req::new("POST", "/api/v1/auth/device/refresh")
                .json(json!({"refresh_token":before_refresh})),
        )
        .await;
        assert_eq!(old_device.status, StatusCode::UNAUTHORIZED);

        let pending = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"mfa-session-revoke@example.test","password":PASSWORD})),
        )
        .await;
        let id = challenge(&pending);
        let browser = call(
            &app,
            Req::new("POST", "/api/v1/auth/login/second-factor").json(
                json!({"challenge_id":id,"code":otp(&secret,env.clock.now().unix_timestamp())}),
            ),
        )
        .await;
        assert_eq!(browser.status, StatusCode::OK, "{}", browser.text);
        let other_browser = session_from(&browser);

        env.clock.advance(Duration::from_secs(30));
        let device_pending = call_device(
            &app,
            Req::new("POST", "/api/v1/auth/device/login").json(json!({
                "email":"mfa-session-revoke@example.test",
                "password":PASSWORD,
                "client_kind":"app",
                "client_id":"craftcloud-test",
                "device_name":"After enrollment"
            })),
        )
        .await;
        let id = challenge(&device_pending);
        let device = call_device(
            &app,
            Req::new("POST", "/api/v1/auth/device/login/second-factor").json(
                json!({"challenge_id":id,"code":otp(&secret,env.clock.now().unix_timestamp())}),
            ),
        )
        .await;
        assert_eq!(device.status, StatusCode::OK, "{}", device.text);
        let refresh = device.body["refresh_token"].as_str().unwrap();

        env.clock.advance(Duration::from_secs(30));
        let disabled = call(
            &app,
            Req::new("POST", "/api/v1/auth/2fa/disable")
                .session(&current)
                .json(json!({"password":PASSWORD,"code":otp(&secret,env.clock.now().unix_timestamp())})),
        )
        .await;
        assert_eq!(disabled.status, StatusCode::NO_CONTENT, "{}", disabled.text);
        let current_status =
            call(&app, Req::new("GET", "/api/v1/auth/2fa").session(&current)).await;
        assert_eq!(
            current_status.status,
            StatusCode::OK,
            "current session revoked on disable"
        );
        assert_eq!(current_status.body["enabled"], false);
        let other = call(
            &app,
            Req::new("GET", "/api/v1/auth/session").cookie_only(&other_browser),
        )
        .await;
        assert_eq!(other.status, StatusCode::UNAUTHORIZED);
        let old_device = call_device(
            &app,
            Req::new("POST", "/api/v1/auth/device/refresh").json(json!({"refresh_token":refresh})),
        )
        .await;
        assert_eq!(old_device.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn secret_codes_challenge_and_recovery_material_never_enter_audit_or_logs() {
        let logs = capture_logs();
        let env = TestEnv::new().await;
        env.user("mfa-no-leaks").await;
        let app = app(&env, Some(KEY), None);
        let (_, secret, recovery_codes) = setup(&app, &env, "mfa-no-leaks").await;
        let code = otp(&secret, env.clock.now().unix_timestamp());
        let pending = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"mfa-no-leaks@example.test","password":PASSWORD})),
        )
        .await;
        let challenge_id = challenge(&pending);
        let failed = call(
            &app,
            Req::new("POST", "/api/v1/auth/login/second-factor")
                .json(json!({"challenge_id":challenge_id,"code":"invalid"})),
        )
        .await;
        assert_eq!(failed.status, StatusCode::UNAUTHORIZED);
        let completed = call(
            &app,
            Req::new("POST", "/api/v1/auth/login/second-factor")
                .json(json!({"challenge_id":challenge_id,"code":code})),
        )
        .await;
        assert_eq!(completed.status, StatusCode::OK);
        let pending = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"mfa-no-leaks@example.test","password":PASSWORD})),
        )
        .await;
        let recovery_challenge = challenge(&pending);
        let completed = call(
            &app,
            Req::new("POST", "/api/v1/auth/login/second-factor")
                .json(json!({"challenge_id":recovery_challenge,"recovery_code":recovery_codes[0]})),
        )
        .await;
        assert_eq!(completed.status, StatusCode::OK);
        for _ in 0..100 {
            if env
                .count("SELECT count(*) FROM audit_events WHERE action='auth.two_factor_failed'")
                .await
                > 0
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            env.count("SELECT count(*) FROM audit_events WHERE action='auth.two_factor_failed'")
                .await
                > 0
        );
        assert_eq!(
            env.count("SELECT count(*) FROM audit_events WHERE action='auth.recovery_code_used'")
                .await,
            1
        );
        let persisted: String = sqlx::query_scalar(
            "SELECT COALESCE((SELECT string_agg(row_to_json(x)::text,' ') FROM user_two_factor x),'') || COALESCE((SELECT string_agg(row_to_json(x)::text,' ') FROM user_two_factor_recovery_codes x),'') || COALESCE((SELECT string_agg(row_to_json(x)::text,' ') FROM two_factor_login_challenges x),'') || COALESCE((SELECT string_agg(row_to_json(x)::text,' ') FROM audit_events x),'')",
        )
        .fetch_one(&env.pool)
        .await
        .unwrap();
        let log_text = logs.text();
        assert!(
            !log_text.is_empty(),
            "log capture must observe real request events"
        );
        for value in [&secret, &code, &challenge_id, &recovery_challenge]
            .into_iter()
            .chain(recovery_codes.iter())
        {
            assert!(
                !persisted.contains(value),
                "plaintext persisted in 2FA data or audit"
            );
            assert!(!log_text.contains(value), "plaintext logged by 2FA flow");
        }
    }
}
