//! AC-02 HTTP coverage for the persistent second-factor failure cap, setup
//! outcomes, the provisioning URI and the path-based organization gate.

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{Router, http::StatusCode};
    use craftcloud_api::{ApiConfig, Clock, Services, router_with_shared};
    use craftcloud_auth::base32_decode;
    use craftcloud_core::Capabilities;
    use craftcloud_db::audit::AuditCtx;
    use craftcloud_storage::LocalStore;
    use hmac::{Hmac, Mac};
    use serde_json::json;
    use sha1::Sha1;
    use uuid::Uuid;

    use crate::common::{PASSWORD, Req, Session, TestEnv, call, session_from, settings};

    const KEY: [u8; 32] = [72; 32];
    type HmacSha1 = Hmac<Sha1>;

    async fn app(env: &TestEnv) -> Router {
        let storage = LocalStore::open_with_faults(&env.storage_dir, env.faults.clone())
            .await
            .unwrap();
        let services = Services::new(env.pool.clone(), None, settings())
            .with_clock(env.clock.clone())
            .with_two_factor_keys(Some(KEY), None)
            .with_storage(storage);
        router_with_shared(
            Capabilities::development_defaults(),
            &ApiConfig::default(),
            Some(Arc::new(services)),
        )
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

    struct Enrolled {
        user_id: Uuid,
        secret: String,
        recovery_codes: Vec<String>,
    }

    async fn enroll(env: &TestEnv, app: &Router, name: &str) -> Enrolled {
        let user_id = env.user(name).await;
        let session = env.login_ok(name).await;
        let setup = call(
            app,
            Req::new("POST", "/api/v1/auth/2fa/setup")
                .session(&session)
                .json(json!({"password":PASSWORD})),
        )
        .await;
        assert_eq!(setup.status, StatusCode::OK, "{}", setup.text);
        let secret = setup.body["secret"].as_str().unwrap().to_owned();
        let confirmed = call(
            app,
            Req::new("POST", "/api/v1/auth/2fa/confirm")
                .session(&session)
                .json(json!({"code":otp(&secret, env.clock.now().unix_timestamp())})),
        )
        .await;
        assert_eq!(confirmed.status, StatusCode::OK, "{}", confirmed.text);
        let recovery_codes = confirmed.body["recovery_codes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|code| code.as_str().unwrap().to_owned())
            .collect();
        // A later login must use a fresh TOTP step.
        env.clock.advance(std::time::Duration::from_secs(30));
        Enrolled {
            user_id,
            secret,
            recovery_codes,
        }
    }

    async fn challenge(app: &Router, name: &str) -> String {
        let response = call(
            app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":format!("{name}@example.test"),"password":PASSWORD})),
        )
        .await;
        assert_eq!(
            response.code(),
            "SECOND_FACTOR_REQUIRED",
            "{}",
            response.text
        );
        response.body["error"]["details"]["challenge_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn second_factor(
        app: &Router,
        challenge_id: &str,
        proof: serde_json::Value,
    ) -> crate::common::Resp {
        let mut body = json!({"challenge_id":challenge_id});
        body.as_object_mut()
            .unwrap()
            .extend(proof.as_object().unwrap().clone());
        call(
            app,
            Req::new("POST", "/api/v1/auth/login/second-factor").json(body),
        )
        .await
    }

    async fn failed_attempts(env: &TestEnv, user_id: Uuid) -> i32 {
        sqlx::query_scalar("SELECT failed_attempts FROM user_two_factor WHERE user_id=$1")
            .bind(user_id)
            .fetch_one(&env.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_101st_second_factor_attempt_fails_even_with_a_valid_code_until_an_operator_reset()
    {
        let env = TestEnv::new().await;
        let app = app(&env).await;
        let account = enroll(&env, &app, "mfa-lock").await;

        // 99 earlier failures are in the database; the 100th happens over HTTP.
        sqlx::query("UPDATE user_two_factor SET failed_attempts=99 WHERE user_id=$1")
            .bind(account.user_id)
            .execute(&env.pool)
            .await
            .unwrap();
        let id = challenge(&app, "mfa-lock").await;
        let wrong = second_factor(&app, &id, json!({"code":"abcdef"})).await;
        assert_eq!(
            (wrong.status, wrong.code()),
            (StatusCode::UNAUTHORIZED, "INVALID_CREDENTIALS"),
            "{}",
            wrong.text
        );
        assert_eq!(failed_attempts(&env, account.user_id).await, 100);
        let locked_events: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events WHERE action='auth.two_factor_locked'",
        )
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(locked_events, 1);
        let detail: String = sqlx::query_scalar(
            "SELECT COALESCE(details::text,'') FROM audit_events WHERE action='auth.two_factor_locked'",
        )
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert!(!detail.contains(&account.secret), "{detail}");

        // The 101st attempt is refused with the same answer, with a valid TOTP
        // and with a valid recovery code alike.
        let id = challenge(&app, "mfa-lock").await;
        let valid = second_factor(
            &app,
            &id,
            json!({"code":otp(&account.secret, env.clock.now().unix_timestamp())}),
        )
        .await;
        assert_eq!(
            (valid.status, valid.code()),
            (StatusCode::UNAUTHORIZED, "INVALID_CREDENTIALS"),
            "{}",
            valid.text
        );
        let id = challenge(&app, "mfa-lock").await;
        let recovery = second_factor(
            &app,
            &id,
            json!({"recovery_code":account.recovery_codes[0]}),
        )
        .await;
        assert_eq!(
            (recovery.status, recovery.code()),
            (StatusCode::UNAUTHORIZED, "INVALID_CREDENTIALS"),
            "{}",
            recovery.text
        );
        assert_eq!(failed_attempts(&env, account.user_id).await, 100);

        // An operator reset clears the lock together with the factor.
        let audit = AuditCtx {
            actor_id: None,
            request_id: None,
        };
        assert!(
            craftcloud_db::two_factor::reset_for_user(
                &env.pool,
                account.user_id,
                None,
                None,
                None,
                None,
                None,
                env.clock.now(),
                &audit,
                "auth.two_factor_reset",
            )
            .await
            .unwrap()
        );
        let login = env.login("mfa-lock").await;
        assert_eq!(login.status, StatusCode::OK, "{}", login.text);
        let session = session_from(&login);
        let setup = call(
            &app,
            Req::new("POST", "/api/v1/auth/2fa/setup")
                .session(&session)
                .json(json!({"password":PASSWORD})),
        )
        .await;
        assert_eq!(setup.status, StatusCode::OK, "{}", setup.text);
        assert_eq!(failed_attempts(&env, account.user_id).await, 0);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_successful_second_factor_resets_the_failure_counter() {
        let env = TestEnv::new().await;
        let app = app(&env).await;
        let account = enroll(&env, &app, "mfa-reset").await;
        let id = challenge(&app, "mfa-reset").await;
        for _ in 0..2 {
            let wrong = second_factor(&app, &id, json!({"code":"abcdef"})).await;
            assert_eq!(wrong.status, StatusCode::UNAUTHORIZED, "{}", wrong.text);
        }
        assert_eq!(failed_attempts(&env, account.user_id).await, 2);
        let ok = second_factor(
            &app,
            &id,
            json!({"code":otp(&account.secret, env.clock.now().unix_timestamp())}),
        )
        .await;
        assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
        assert_eq!(failed_attempts(&env, account.user_id).await, 0);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn setup_reports_pending_and_enabled_states_and_labels_the_uri() {
        let env = TestEnv::new().await;
        let app = app(&env).await;
        env.user("mfa-setup").await;
        let first = env.login_ok("mfa-setup").await;
        let second = env.login_ok("mfa-setup").await;
        let started = call(
            &app,
            Req::new("POST", "/api/v1/auth/2fa/setup")
                .session(&first)
                .json(json!({"password":PASSWORD})),
        )
        .await;
        assert_eq!(started.status, StatusCode::OK, "{}", started.text);
        let secret = started.body["secret"].as_str().unwrap();
        assert_eq!(
            started.body["otpauth_url"].as_str().unwrap(),
            format!(
                "otpauth://totp/CraftCloud%20%28cloud.test%29:mfa-setup%40example.test?secret={secret}&issuer=CraftCloud%20%28cloud.test%29&algorithm=SHA1&digits=6&period=30"
            )
        );
        let pending = call(
            &app,
            Req::new("POST", "/api/v1/auth/2fa/setup")
                .session(&second)
                .json(json!({"password":PASSWORD})),
        )
        .await;
        assert_eq!(
            (pending.status, pending.code()),
            (StatusCode::CONFLICT, "TWO_FACTOR_SETUP_PENDING"),
            "{}",
            pending.text
        );
        let confirmed = call(
            &app,
            Req::new("POST", "/api/v1/auth/2fa/confirm")
                .session(&first)
                .json(json!({"code":otp(secret, env.clock.now().unix_timestamp())})),
        )
        .await;
        assert_eq!(confirmed.status, StatusCode::OK, "{}", confirmed.text);
        let enabled = call(
            &app,
            Req::new("POST", "/api/v1/auth/2fa/setup")
                .session(&first)
                .json(json!({"password":PASSWORD})),
        )
        .await;
        assert_eq!(
            (enabled.status, enabled.code()),
            (StatusCode::CONFLICT, "TWO_FACTOR_ALREADY_ENABLED"),
            "{}",
            enabled.text
        );
    }

    async fn operator_in_required_org(env: &TestEnv) -> (Session, Uuid) {
        let operator = env.login_ok("op").await;
        let owner_id = env.user("gate-owner").await;
        let created = call(
            &env.app,
            Req::new("POST", "/api/v1/organizations")
                .session(&operator)
                .json(json!({"name":"Gate Org","slug":format!("gate-{}",Uuid::new_v4().simple()),"owner_user_id":owner_id.to_string()})),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        let org = Uuid::parse_str(created.body["id"].as_str().unwrap()).unwrap();
        sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'admin','active') ON CONFLICT DO NOTHING")
            .bind(org)
            .bind(env.operator_id)
            .execute(&env.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE organizations SET require_two_factor=true WHERE id=$1")
            .bind(org)
            .execute(&env.pool)
            .await
            .unwrap();
        (operator, org)
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn instance_admin_routes_ignore_the_organization_two_factor_gate() {
        let env = TestEnv::new().await;
        let (operator, org) = operator_in_required_org(&env).await;
        let scoped = call(
            &env.app,
            Req::new("GET", &format!("/api/v1/organizations/{org}")).session(&operator),
        )
        .await;
        assert_eq!(
            scoped.code(),
            "TWO_FACTOR_SETUP_REQUIRED",
            "{}",
            scoped.text
        );
        let admin = call(
            &env.app,
            Req::new("PUT", &format!("/api/v1/admin/organizations/{org}/quota"))
                .session(&operator)
                .json(json!({"quota_bytes": 1_000_000})),
        )
        .await;
        assert_eq!(admin.status, StatusCode::OK, "{}", admin.text);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn undecodable_path_segments_are_rejected_without_a_server_error() {
        let env = TestEnv::new().await;
        let session = env.login_ok("op").await;
        for path in [
            "/api/v1/organizations/%ZZ",
            "/api/v1/organizations/%FF/workspaces",
            "/api/v1/files/%C3",
        ] {
            let response = call(&env.app, Req::new("GET", path).session(&session)).await;
            assert!(
                response.status.is_client_error(),
                "{path}: {} {}",
                response.status,
                response.text
            );
            assert_ne!(response.code(), "INTERNAL", "{path}: {}", response.text);
        }
    }
}
