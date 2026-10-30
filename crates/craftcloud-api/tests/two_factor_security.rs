#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{Router, http::StatusCode};
    use craftcloud_api::{ApiConfig, Clock, Services, router_with_shared};
    use craftcloud_auth::base32_decode;
    use craftcloud_core::Capabilities;
    use craftcloud_storage::LocalStore;
    use hmac::{Hmac, Mac};
    use serde_json::json;
    use sha1::Sha1;
    use uuid::Uuid;

    use crate::common::{PASSWORD, Req, TestEnv, call, session_from, settings};

    const KEY: [u8; 32] = [71; 32];
    type HmacSha1 = Hmac<Sha1>;

    async fn app(env: &TestEnv, pair_limit: u32, account_limit: u32) -> Router {
        let mut settings = settings();
        settings.failed_login_pair_limit = pair_limit;
        settings.failed_login_account_limit = account_limit;
        let storage = LocalStore::open_with_faults(&env.storage_dir, env.faults.clone())
            .await
            .unwrap();
        let services = Services::new(env.pool.clone(), None, settings)
            .with_clock(env.clock.clone())
            .with_two_factor_keys(Some(KEY), None)
            .with_storage(storage);
        router_with_shared(
            Capabilities::development_defaults(),
            &ApiConfig::default(),
            Some(Arc::new(services)),
        )
    }

    fn id(resp: &crate::common::Resp) -> Uuid {
        Uuid::parse_str(resp.body["id"].as_str().unwrap()).unwrap()
    }

    fn encode_uuid(id: Uuid) -> String {
        id.to_string()
            .bytes()
            .map(|byte| format!("%{byte:02X}"))
            .collect()
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

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn organization_gate_covers_encoded_ids_trash_and_closed_uploads() {
        let env = TestEnv::new().await;
        let app = app(&env, 100, 100).await;
        let operator = env.login_ok("op").await;
        let owner_id = env.user("mfa-scope-owner").await;
        let owner = env.login_ok("mfa-scope-owner").await;
        let created = call(
            &app,
            Req::new("POST", "/api/v1/organizations")
                .session(&operator)
                .json(json!({
                    "name":"Scope Gate",
                    "slug":format!("scope-{}", Uuid::new_v4().simple()),
                    "owner_user_id":owner_id.to_string()
                })),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        let org = id(&created);
        let workspace = call(
            &app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/workspaces"))
                .session(&owner)
                .json(json!({"name":"Scope WS","slug":"scope-ws"})),
        )
        .await;
        assert_eq!(workspace.status, StatusCode::CREATED, "{}", workspace.text);
        let workspace = id(&workspace);
        let project = call(
            &app,
            Req::new("POST", &format!("/api/v1/workspaces/{workspace}/projects"))
                .session(&owner)
                .json(json!({"name":"Scope Project"})),
        )
        .await;
        assert_eq!(project.status, StatusCode::CREATED, "{}", project.text);
        let project = id(&project);
        let file = call(
            &app,
            Req::new("POST", &format!("/api/v1/projects/{project}/files"))
                .session(&owner)
                .json(json!({"name":"scope.fig","app":"figcraft","format":"fig"})),
        )
        .await;
        assert_eq!(file.status, StatusCode::CREATED, "{}", file.text);
        let file = id(&file);
        let upload = call(
            &app,
            Req::new("POST", &format!("/api/v1/files/{file}/uploads"))
                .session(&owner)
                .json(json!({"size_bytes":1,"sha256":"0000000000000000000000000000000000000000000000000000000000000000"})),
        )
        .await;
        assert_eq!(upload.status, StatusCode::CREATED, "{}", upload.text);
        let upload_id = Uuid::parse_str(upload.body["upload_id"].as_str().unwrap()).unwrap();

        sqlx::query("UPDATE organizations SET require_two_factor=true WHERE id=$1")
            .bind(org)
            .execute(&env.pool)
            .await
            .unwrap();
        let encoded_org = encode_uuid(org);
        let org_denied = call(
            &app,
            Req::new("GET", &format!("/api/v1/organizations/{encoded_org}")).session(&owner),
        )
        .await;
        assert_eq!(
            (org_denied.status, org_denied.code()),
            (StatusCode::FORBIDDEN, "TWO_FACTOR_SETUP_REQUIRED"),
            "{}",
            org_denied.text
        );

        let encoded_file = encode_uuid(file);
        let active_denied = call(
            &app,
            Req::new("GET", &format!("/api/v1/files/{encoded_file}")).session(&owner),
        )
        .await;
        assert_eq!(
            active_denied.code(),
            "TWO_FACTOR_SETUP_REQUIRED",
            "{}",
            active_denied.text
        );

        sqlx::query("UPDATE files SET deleted_at=now(), deleted_by_user_id=$2 WHERE id=$1")
            .bind(file)
            .bind(owner_id)
            .execute(&env.pool)
            .await
            .unwrap();
        let trash_get = call(
            &app,
            Req::new("GET", &format!("/api/v1/files/{file}")).session(&owner),
        )
        .await;
        assert_eq!(
            trash_get.code(),
            "TWO_FACTOR_SETUP_REQUIRED",
            "{}",
            trash_get.text
        );
        let trash_restore = call(
            &app,
            Req::new("POST", &format!("/api/v1/files/{file}/restore")).session(&owner),
        )
        .await;
        assert_eq!(
            trash_restore.code(),
            "TWO_FACTOR_SETUP_REQUIRED",
            "{}",
            trash_restore.text
        );

        sqlx::query("UPDATE upload_sessions SET state='aborted', expires_at=now()-interval '1 hour' WHERE id=$1")
            .bind(upload_id)
            .execute(&env.pool)
            .await
            .unwrap();
        let closed_upload = call(
            &app,
            Req::new("GET", &format!("/api/v1/uploads/{upload_id}")).session(&owner),
        )
        .await;
        assert_eq!(
            closed_upload.code(),
            "TWO_FACTOR_SETUP_REQUIRED",
            "{}",
            closed_upload.text
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn management_proof_failures_share_account_and_address_budget() {
        let env = TestEnv::new().await;
        let app = app(&env, 3, 100).await;
        env.user("mfa-management-budget").await;
        let session = env.login_ok("mfa-management-budget").await;
        let pending = call(
            &app,
            Req::new("POST", "/api/v1/auth/2fa/setup")
                .session(&session)
                .json(json!({"password":PASSWORD})),
        )
        .await;
        assert_eq!(pending.status, StatusCode::OK, "{}", pending.text);
        for _ in 0..2 {
            let failed = call(
                &app,
                Req::new("POST", "/api/v1/auth/2fa/confirm")
                    .session(&session)
                    .json(json!({"code":"not-a-code"})),
            )
            .await;
            assert_eq!(failed.status, StatusCode::UNAUTHORIZED, "{}", failed.text);
        }
        let password_only = call(
            &app,
            Req::new("POST", "/api/v1/auth/2fa/setup")
                .session(&session)
                .json(json!({"password":PASSWORD})),
        )
        .await;
        assert_eq!(
            password_only.status,
            StatusCode::OK,
            "{}",
            password_only.text
        );
        let third_guess = call(
            &app,
            Req::new("POST", "/api/v1/auth/2fa/confirm")
                .session(&session)
                .json(json!({"code":"not-a-code"})),
        )
        .await;
        assert_eq!(
            third_guess.status,
            StatusCode::UNAUTHORIZED,
            "{}",
            third_guess.text
        );
        let blocked = call(
            &app,
            Req::new("POST", "/api/v1/auth/2fa/confirm")
                .session(&session)
                .json(json!({"code":"not-a-code"})),
        )
        .await;
        assert_eq!(
            (blocked.status, blocked.code()),
            (StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED"),
            "{}",
            blocked.text
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn mfa_login_replaces_old_cookie_and_runs_challenge_cleanup() {
        let env = TestEnv::new().await;
        let app = app(&env, 100, 100).await;
        env.user("mfa-cookie-replacement").await;
        let old_session = env.login_ok("mfa-cookie-replacement").await;
        let setup = call(
            &app,
            Req::new("POST", "/api/v1/auth/2fa/setup")
                .session(&old_session)
                .json(json!({"password":PASSWORD})),
        )
        .await;
        assert_eq!(setup.status, StatusCode::OK, "{}", setup.text);
        let secret = setup.body["secret"].as_str().unwrap();
        let confirmed = call(
            &app,
            Req::new("POST", "/api/v1/auth/2fa/confirm")
                .session(&old_session)
                .json(json!({"code":otp(secret, env.clock.now().unix_timestamp())})),
        )
        .await;
        assert_eq!(confirmed.status, StatusCode::OK, "{}", confirmed.text);
        env.clock.advance(std::time::Duration::from_secs(30));

        let user_id: Uuid = sqlx::query_scalar(
            "SELECT id FROM users WHERE normalized_email='mfa-cookie-replacement@example.test'",
        )
        .fetch_one(&env.pool)
        .await
        .unwrap();
        let now = env.clock.now();
        sqlx::query("INSERT INTO two_factor_login_challenges(id_hash,user_id,password_hash_digest,login_kind,client_address,created_at,expires_at) VALUES($1,$2,$3,'browser','unknown',$4,$5)")
            .bind(vec![9_u8; 32])
            .bind(user_id)
            .bind(vec![8_u8; 32])
            .bind(now - time::Duration::minutes(6))
            .bind(now - time::Duration::minutes(1))
            .execute(&env.pool)
            .await
            .unwrap();

        let challenge = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .session(&old_session)
                .json(json!({"email":"mfa-cookie-replacement@example.test","password":PASSWORD})),
        )
        .await;
        assert_eq!(
            challenge.code(),
            "SECOND_FACTOR_REQUIRED",
            "{}",
            challenge.text
        );
        let remaining: i64 =
            sqlx::query_scalar("SELECT count(*) FROM two_factor_login_challenges WHERE id_hash=$1")
                .bind(vec![9_u8; 32])
                .fetch_one(&env.pool)
                .await
                .unwrap();
        assert_eq!(
            remaining, 0,
            "expired challenge remains after MFA login challenge"
        );

        let challenge_id = challenge.body["error"]["details"]["challenge_id"]
            .as_str()
            .unwrap();
        let login = call(
            &app,
            Req::new("POST", "/api/v1/auth/login/second-factor")
                .session(&old_session)
                .json(json!({"challenge_id":challenge_id,"code":otp(secret,env.clock.now().unix_timestamp())})),
        )
        .await;
        assert_eq!(login.status, StatusCode::OK, "{}", login.text);
        let new_session = session_from(&login);
        let old_check = call(
            &app,
            Req::new("GET", "/api/v1/auth/session").cookie_only(&old_session),
        )
        .await;
        assert_eq!(
            old_check.status,
            StatusCode::UNAUTHORIZED,
            "{}",
            old_check.text
        );
        let new_check = call(
            &app,
            Req::new("GET", "/api/v1/auth/session").cookie_only(&new_session),
        )
        .await;
        assert_eq!(new_check.status, StatusCode::OK, "{}", new_check.text);
    }
}
