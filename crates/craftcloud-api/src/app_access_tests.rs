//! AC-11 HTTP checks for app access and seats. Requires PostgreSQL.

#[path = "app_access_test_support.rs"]
pub(crate) mod support;

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, sync::Arc, time::Duration};

    use crate::{ApiConfig, AuthSettings, Clock, Services, router_with_shared};
    use axum::{Router, extract::ConnectInfo, http::StatusCode};
    use craftcloud_auth::base32_decode;
    use craftcloud_core::Capabilities;
    use craftcloud_db::app_access;
    use hmac::{Hmac, Mac};
    use serde_json::json;
    use sha1::Sha1;
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::support::{PASSWORD, Req, Resp, Session, TestEnv, call};

    type HmacSha1 = Hmac<Sha1>;

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

    fn otp(secret: &str, unix_seconds: i64) -> String {
        let key = base32_decode(secret).unwrap();
        let mut mac = HmacSha1::new_from_slice(&key).unwrap();
        mac.update(&((unix_seconds / 30) as u64).to_be_bytes());
        let digest = mac.finalize().into_bytes();
        let offset = (digest[19] & 0x0f) as usize;
        let value =
            u32::from_be_bytes(digest[offset..offset + 4].try_into().unwrap()) & 0x7fff_ffff;
        format!("{:06}", value % 1_000_000)
    }

    fn with_two_factor(env: &TestEnv) -> Router {
        let services = Services::new(env.pool.clone(), None, settings())
            .with_clock(env.clock.clone())
            .with_two_factor_keys(Some([71; 32]), None);
        router_with_shared(
            Capabilities::development_defaults(),
            &ApiConfig {
                trusted_proxies: vec!["127.0.0.0/8".parse().unwrap()],
                ..ApiConfig::default()
            },
            Some(Arc::new(services)),
        )
    }

    async fn call_device(app: &Router, req: Req) -> Resp {
        send(app, req.build()).await
    }

    async fn send(app: &Router, mut request: axum::http::Request<axum::body::Body>) -> Resp {
        request
            .extensions_mut()
            .insert(ConnectInfo("127.0.0.1:443".parse::<SocketAddr>().unwrap()));
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
            body: serde_json::from_slice(&bytes).unwrap_or_default(),
            text,
        }
    }

    async fn fixture() -> (TestEnv, Session, Uuid, Uuid, Session) {
        let env = TestEnv::new().await;
        let owner = env.login_ok("op").await;
        let org: Uuid =
            sqlx::query_scalar("SELECT id FROM organizations WHERE slug='operator-org'")
                .fetch_one(&env.pool)
                .await
                .unwrap();
        let initial_mode: String =
            sqlx::query_scalar("SELECT app_access_mode FROM organizations WHERE id=$1")
                .bind(org)
                .fetch_one(&env.pool)
                .await
                .unwrap();
        assert_eq!(
            initial_mode, "all",
            "new organizations retain the default mode"
        );
        let user = env.user("ac11-app-user").await;
        let member_session = env.login_ok("ac11-app-user").await;
        let added = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/members"))
                .session(&owner)
                .json(json!({"user_id":user,"role":"member"})),
        )
        .await;
        assert_eq!(added.status, StatusCode::CREATED, "{}", added.text);
        let mode = call(
            &env.app,
            Req::new("PATCH", &format!("/api/v1/organizations/{org}/app-access"))
                .session(&owner)
                .json(json!({"mode":"assigned"})),
        )
        .await;
        assert_eq!(mode.status, StatusCode::OK, "{}", mode.text);
        (env, owner, org, user, member_session)
    }

    async fn assign(env: &TestEnv, owner: &Session, org: Uuid, user: Uuid, apps: &[&str]) -> Resp {
        call(
            &env.app,
            Req::new(
                "PUT",
                &format!("/api/v1/organizations/{org}/members/{user}/apps"),
            )
            .session(owner)
            .json(json!({"apps":apps})),
        )
        .await
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn default_all_mode_and_browser_session_survive_app_policy() {
        let (env, owner, org, _user, _session) = fixture().await;
        let assigned_all = call(
            &env.app,
            Req::new("PATCH", &format!("/api/v1/organizations/{org}/app-access"))
                .session(&owner)
                .json(json!({"mode":"all"})),
        )
        .await;
        assert_eq!(assigned_all.status, StatusCode::OK, "{}", assigned_all.text);
        let response = call(
            &env.app,
            Req::new("GET", &format!("/api/v1/organizations/{org}/apps")).session(&owner),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.text);
        assert_eq!(response.body["mode"], "all");
        assert_eq!(response.body["seat_usage"]["used"], 2);

        let login = call_device(
            &env.app,
            Req::new("POST", "/api/v1/auth/device/login").json(json!({
                "email":"op@example.test", "password":PASSWORD,
                "client_kind":"app", "client_id":"figcraft", "device_name":"AC11 owner"
            })),
        )
        .await;
        assert_eq!(login.status, StatusCode::OK, "{}", login.text);
        let member_login = call_device(
            &env.app,
            Req::new("POST", "/api/v1/auth/device/login").json(json!({
                "email":"ac11-app-user@example.test", "password":PASSWORD,
                "client_kind":"app", "client_id":"future-app", "device_name":"AC11 member"
            })),
        )
        .await;
        assert_eq!(member_login.status, StatusCode::OK, "{}", member_login.text);
        let response = call(
            &env.app,
            Req::new("GET", &format!("/api/v1/organizations/{org}/apps")).session(&owner),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.text);
        let figcraft = response.body["apps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|app| app["id"] == "figcraft")
            .unwrap();
        assert_eq!(figcraft["active_devices"], 1);

        let browser = env.get_session(&owner).await;
        assert_eq!(browser.status, StatusCode::OK, "{}", browser.text);
        let foreign = call(&env.app, Req::new("POST", "/api/v1/organizations").session(&owner)
            .json(json!({"name":"Foreign AC11","slug":format!("foreign-ac11-{}",Uuid::new_v4().simple()),"owner_user_id":_user}))).await;
        assert_eq!(foreign.status, StatusCode::CREATED, "{}", foreign.text);
        let foreign_id = foreign.body["id"].as_str().unwrap();
        let hidden = call(
            &env.app,
            Req::new("GET", &format!("/api/v1/organizations/{foreign_id}/apps")).session(&owner),
        )
        .await;
        assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{}", hidden.text);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn setting_assigned_mode_keeps_browser_login_but_blocks_unassigned_app() {
        let (env, owner, _org, user, _session) = fixture().await;

        let browser = env.get_session(&owner).await;
        assert_eq!(browser.status, StatusCode::OK, "{}", browser.text);
        let mut tx = env.pool.begin().await.unwrap();
        let apps = vec![
            "figcraft".to_owned(),
            "photocraft".to_owned(),
            "vectorcraft".to_owned(),
        ];
        let allowed = app_access::device_login_allowed(&mut tx, user, "app", "figcraft", &apps)
            .await
            .unwrap();
        assert!(!allowed);
        let launcher_allowed =
            app_access::device_login_allowed(&mut tx, user, "launcher", "launcher", &apps)
                .await
                .unwrap();
        assert!(launcher_allowed);
        tx.rollback().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn app_device_login_denied_without_assignment_and_after_totp_completion() {
        let (mut env, owner, org, user, session) = fixture().await;
        let assigned = assign(&env, &owner, org, user, &["figcraft"]).await;
        assert_eq!(assigned.status, StatusCode::OK, "{}", assigned.text);
        env.app = with_two_factor(&env);
        let setup = call(
            &env.app,
            Req::new("POST", "/api/v1/auth/2fa/setup")
                .session(&session)
                .json(json!({"password":PASSWORD})),
        )
        .await;
        assert_eq!(setup.status, StatusCode::OK, "{}", setup.text);
        let secret = setup.body["secret"].as_str().unwrap();
        let confirm = call(
            &env.app,
            Req::new("POST", "/api/v1/auth/2fa/confirm")
                .session(&session)
                .json(json!({"code":otp(secret, env.clock.now().unix_timestamp())})),
        )
        .await;
        assert_eq!(confirm.status, StatusCode::OK, "{}", confirm.text);
        env.clock.advance(Duration::from_secs(30));

        let login = call_device(
            &env.app,
            Req::new("POST", "/api/v1/auth/device/login").json(json!({
                "email":"ac11-app-user@example.test", "password":PASSWORD,
                "client_kind":"app", "client_id":"figcraft", "device_name":"AC11 test"
            })),
        )
        .await;
        assert_eq!(
            (login.status, login.code()),
            (StatusCode::UNAUTHORIZED, "SECOND_FACTOR_REQUIRED"),
            "{}",
            login.text
        );
        let challenge = login.body["error"]["details"]["challenge_id"]
            .as_str()
            .unwrap();
        let removed = assign(&env, &owner, org, user, &[]).await;
        assert_eq!(removed.status, StatusCode::OK, "{}", removed.text);
        let denied = call_device(&env.app, Req::new("POST", "/api/v1/auth/device/login/second-factor")
            .json(json!({"challenge_id":challenge,"code":otp(secret, env.clock.now().unix_timestamp())}))).await;
        assert_eq!(
            (denied.status, denied.code()),
            (StatusCode::FORBIDDEN, "APP_NOT_ASSIGNED"),
            "{}",
            denied.text
        );
        let sessions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM device_sessions WHERE user_id=$1 AND revoked_at IS NULL",
        )
        .bind(user)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(sessions, 0);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn removing_app_assignment_revokes_device_session_and_file_lock() {
        let (env, owner, org, user, _member_session) = fixture().await;
        let assigned = assign(&env, &owner, org, user, &["figcraft"]).await;
        assert_eq!(assigned.status, StatusCode::OK, "{}", assigned.text);
        let login = call_device(
            &env.app,
            Req::new("POST", "/api/v1/auth/device/login").json(json!({
                "email":"ac11-app-user@example.test", "password":PASSWORD,
                "client_kind":"app", "client_id":"figcraft", "device_name":"AC11 test"
            })),
        )
        .await;
        assert_eq!(login.status, StatusCode::OK, "{}", login.text);
        let session_id =
            Uuid::parse_str(login.body["device_session_id"].as_str().unwrap()).unwrap();
        let workspace = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/workspaces"))
                .session(&owner)
                .json(json!({"name":"AC11 workspace","slug":"ac11"})),
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
            .session(&owner)
            .json(json!({"name":"AC11 project"})),
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
            .session(&owner)
            .json(json!({"name":"AC11 file","app":"figcraft","format":"native"})),
        )
        .await;
        assert_eq!(file.status, StatusCode::CREATED, "{}", file.text);
        let file_id = Uuid::parse_str(file.body["id"].as_str().unwrap()).unwrap();
        sqlx::query("INSERT INTO file_locks(file_id,org_id,holder_user_id,holder_session_kind,holder_session_id,client_id,acquired_at,refreshed_at,expires_at) VALUES($1,$2,$3,'device',$4,'figcraft',now(),now(),now()+interval '5 minutes')")
            .bind(file_id).bind(org).bind(user).bind(session_id).execute(&env.pool).await.unwrap();

        let removed = assign(&env, &owner, org, user, &[]).await;
        assert_eq!(removed.status, StatusCode::OK, "{}", removed.text);
        let state: (bool, i64) = sqlx::query_as("SELECT revoked_at IS NOT NULL,(SELECT count(*) FROM file_locks WHERE holder_session_id=$1) FROM device_sessions WHERE id=$1")
            .bind(session_id).fetch_one(&env.pool).await.unwrap();
        assert_eq!(state, (true, 0));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn demoted_admin_cannot_change_app_mode_with_stale_membership_state() {
        let (env, owner, org, _user, _member_session) = fixture().await;
        let admin_id = env.user("ac11-demoted-admin").await;
        let admin_session = env.login_ok("ac11-demoted-admin").await;
        let added = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/members"))
                .session(&owner)
                .json(json!({"user_id":admin_id,"role":"admin"})),
        )
        .await;
        assert_eq!(added.status, StatusCode::CREATED, "{}", added.text);
        let mut blocker = env.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
            .bind(org)
            .fetch_one(&mut *blocker)
            .await
            .unwrap();
        let app = env.app.clone();
        let admin_session_mode = admin_session.clone();
        let admin_session_assign = admin_session.clone();
        let request = tokio::spawn(async move {
            call(
                &app,
                Req::new("PATCH", &format!("/api/v1/organizations/{org}/app-access"))
                    .session(&admin_session_mode)
                    .json(json!({"mode":"all"})),
            )
            .await
        });
        let mut is_waiting = false;
        for _ in 0..200 {
            let waiters: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE pid<>pg_backend_pid() AND wait_event_type='Lock' AND query ILIKE '%organizations%'")
                .fetch_one(&env.pool).await.unwrap();
            if waiters > 0 {
                is_waiting = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            is_waiting,
            "app mode request never waited on organization lock"
        );
        sqlx::query("UPDATE organization_members SET role='member' WHERE org_id=$1 AND user_id=$2")
            .bind(org)
            .bind(admin_id)
            .execute(&env.pool)
            .await
            .unwrap();
        blocker.commit().await.unwrap();
        let denied = request.await.unwrap();
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.text);

        let promoted = call(
            &env.app,
            Req::new(
                "PATCH",
                &format!("/api/v1/organizations/{org}/members/{admin_id}"),
            )
            .session(&owner)
            .json(json!({"role":"admin"})),
        )
        .await;
        assert_eq!(promoted.status, StatusCode::OK, "{}", promoted.text);
        let mut blocker = env.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
            .bind(org)
            .fetch_one(&mut *blocker)
            .await
            .unwrap();
        let app = env.app.clone();
        let request = tokio::spawn(async move {
            call(
                &app,
                Req::new(
                    "PUT",
                    &format!("/api/v1/organizations/{org}/members/{_user}/apps"),
                )
                .session(&admin_session_assign)
                .json(json!({"apps":["figcraft"]})),
            )
            .await
        });
        let mut is_waiting = false;
        for _ in 0..200 {
            let waiters: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE pid<>pg_backend_pid() AND wait_event_type='Lock' AND query ILIKE '%organizations%'")
                .fetch_one(&env.pool).await.unwrap();
            if waiters > 0 {
                is_waiting = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            is_waiting,
            "app assignment request never waited on organization lock"
        );
        sqlx::query("UPDATE organization_members SET role='member' WHERE org_id=$1 AND user_id=$2")
            .bind(org)
            .bind(admin_id)
            .execute(&env.pool)
            .await
            .unwrap();
        blocker.commit().await.unwrap();
        let denied = request.await.unwrap();
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.text);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn operator_seat_limit_update_hides_unknown_organization() {
        let env = TestEnv::new().await;
        let operator = env.login_ok("op").await;
        let missing = Uuid::new_v4();
        let response = call(
            &env.app,
            Req::new(
                "PATCH",
                &format!("/api/v1/admin/organizations/{missing}/seats"),
            )
            .session(&operator)
            .json(json!({"seat_limit":5})),
        )
        .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{}", response.text);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn admin_member_list_returns_assigned_app_ids() {
        let (env, owner, org, user, _session) = fixture().await;
        let assigned = assign(&env, &owner, org, user, &["figcraft"]).await;
        assert_eq!(assigned.status, StatusCode::OK, "{}", assigned.text);
        let response = call(
            &env.app,
            Req::new(
                "GET",
                &format!("/api/v1/organizations/{org}/members?status=all"),
            )
            .session(&owner),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.text);
        let member = response.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|member| member["user_id"] == user.to_string())
            .unwrap();
        assert_eq!(member["apps"], json!(["figcraft"]));
    }

    async fn bearer_call(app: &Router, req: Req, token: &str) -> Resp {
        let mut request = req.build();
        request.headers_mut().insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        request.headers_mut().remove(axum::http::header::ORIGIN);
        send(app, request).await
    }

    /// Creates a workspace, project and `figcraft` file as `owner`.
    async fn make_file(
        env: &TestEnv,
        owner: &Session,
        org: Uuid,
        slug: &str,
        app: &str,
    ) -> (String, Uuid) {
        let workspace = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/workspaces"))
                .session(owner)
                .json(json!({"name":slug,"slug":slug})),
        )
        .await;
        assert_eq!(workspace.status, StatusCode::CREATED, "{}", workspace.text);
        let workspace_id = workspace.body["id"].as_str().unwrap().to_owned();
        let project = call(
            &env.app,
            Req::new(
                "POST",
                &format!("/api/v1/workspaces/{workspace_id}/projects"),
            )
            .session(owner)
            .json(json!({"name":"AC11 project"})),
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
            .session(owner)
            .json(json!({"name":"AC11 file","app":app,"format":"native"})),
        )
        .await;
        assert_eq!(file.status, StatusCode::CREATED, "{}", file.text);
        (
            workspace_id,
            Uuid::parse_str(file.body["id"].as_str().unwrap()).unwrap(),
        )
    }

    async fn device_login(env: &TestEnv, email: &str, kind: &str, client: &str) -> Resp {
        let login = call_device(
            &env.app,
            Req::new("POST", "/api/v1/auth/device/login").json(json!({
                "email":email, "password":PASSWORD,
                "client_kind":kind, "client_id":client, "device_name":"AC11 test"
            })),
        )
        .await;
        assert_eq!(login.status, StatusCode::OK, "{}", login.text);
        login
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn launcher_and_cli_sessions_cannot_lock_in_assigned_mode_but_keep_browser_actions() {
        let (env, owner, org, user, _member_session) = fixture().await;
        let (workspace, file) = make_file(&env, &owner, org, "ac11-tool", "photocraft").await;
        let added = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/workspaces/{workspace}/members"))
                .session(&owner)
                .json(json!({"user_id":user.to_string(),"role":"editor"})),
        )
        .await;
        assert_eq!(added.status, StatusCode::CREATED, "{}", added.text);
        for (kind, client) in [("launcher", "launcher"), ("cli", "craftcloud")] {
            let login = device_login(&env, "ac11-app-user@example.test", kind, client).await;
            let token = login.body["access_token"].as_str().unwrap().to_owned();
            let read = bearer_call(
                &env.app,
                Req::new("GET", &format!("/api/v1/files/{file}")),
                &token,
            )
            .await;
            assert_eq!(read.status, StatusCode::OK, "{kind}: {}", read.text);
            let lock = bearer_call(
                &env.app,
                Req::new("POST", &format!("/api/v1/files/{file}/lock"))
                    .json(json!({"client_id":client})),
                &token,
            )
            .await;
            assert_eq!(
                (lock.status, lock.code()),
                (StatusCode::FORBIDDEN, "APP_NOT_ASSIGNED"),
                "{kind}: {}",
                lock.text
            );
        }
        // Owners keep every client; `all` mode lifts the restriction.
        let owner_login = device_login(&env, "op@example.test", "launcher", "launcher").await;
        let owner_lock = bearer_call(
            &env.app,
            Req::new("POST", &format!("/api/v1/files/{file}/lock"))
                .json(json!({"client_id":"launcher"})),
            owner_login.body["access_token"].as_str().unwrap(),
        )
        .await;
        assert!(owner_lock.status.is_success(), "{}", owner_lock.text);
        let released = bearer_call(
            &env.app,
            Req::new("DELETE", &format!("/api/v1/files/{file}/lock")),
            owner_login.body["access_token"].as_str().unwrap(),
        )
        .await;
        assert!(released.status.is_success(), "{}", released.text);
        let all = call(
            &env.app,
            Req::new("PATCH", &format!("/api/v1/organizations/{org}/app-access"))
                .session(&owner)
                .json(json!({"mode":"all"})),
        )
        .await;
        assert_eq!(all.status, StatusCode::OK, "{}", all.text);
        let login = device_login(&env, "ac11-app-user@example.test", "launcher", "launcher").await;
        let lock = bearer_call(
            &env.app,
            Req::new("POST", &format!("/api/v1/files/{file}/lock"))
                .json(json!({"client_id":"launcher"})),
            login.body["access_token"].as_str().unwrap(),
        )
        .await;
        assert!(lock.status.is_success(), "{}", lock.text);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn assignments_for_owners_and_admins_are_rejected_and_patch_changes_single_apps() {
        let (env, owner, org, user, _member_session) = fixture().await;
        let admin = env.user("ac11-assign-admin").await;
        let added = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/members"))
                .session(&owner)
                .json(json!({"user_id":admin,"role":"admin"})),
        )
        .await;
        assert_eq!(added.status, StatusCode::CREATED, "{}", added.text);
        let put = assign(&env, &owner, org, admin, &["figcraft"]).await;
        assert_eq!(
            (put.status, put.code()),
            (StatusCode::CONFLICT, "APP_ASSIGNMENT_NOT_APPLICABLE"),
            "{}",
            put.text
        );
        let patch = |uid: Uuid, body: serde_json::Value| {
            call(
                &env.app,
                Req::new(
                    "PATCH",
                    &format!("/api/v1/organizations/{org}/members/{uid}/apps"),
                )
                .session(&owner)
                .json(body),
            )
        };
        let rejected = patch(admin, json!({"add":["figcraft"]})).await;
        assert_eq!(
            (rejected.status, rejected.code()),
            (StatusCode::CONFLICT, "APP_ASSIGNMENT_NOT_APPLICABLE"),
            "{}",
            rejected.text
        );
        let cleared = assign(&env, &owner, org, admin, &[]).await;
        assert_eq!(cleared.status, StatusCode::OK, "{}", cleared.text);

        let first = assign(&env, &owner, org, user, &["figcraft"]).await;
        assert_eq!(first.status, StatusCode::OK, "{}", first.text);
        let added = patch(user, json!({"add":["photocraft"]})).await;
        assert_eq!(added.status, StatusCode::OK, "{}", added.text);
        assert_eq!(added.body["apps"], json!(["figcraft", "photocraft"]));
        let removed = patch(user, json!({"remove":["figcraft"],"add":["vectorcraft"]})).await;
        assert_eq!(removed.status, StatusCode::OK, "{}", removed.text);
        assert_eq!(removed.body["apps"], json!(["photocraft", "vectorcraft"]));
        let overlap = patch(user, json!({"add":["figcraft"],"remove":["figcraft"]})).await;
        assert_eq!(
            overlap.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            overlap.text
        );
        let unknown = patch(user, json!({"add":["nope"]})).await;
        assert_eq!(
            unknown.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            unknown.text
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn removing_assignment_in_all_mode_keeps_the_device_session() {
        let (env, owner, org, user, _member_session) = fixture().await;
        let all = call(
            &env.app,
            Req::new("PATCH", &format!("/api/v1/organizations/{org}/app-access"))
                .session(&owner)
                .json(json!({"mode":"all"})),
        )
        .await;
        assert_eq!(all.status, StatusCode::OK, "{}", all.text);
        let assigned = assign(&env, &owner, org, user, &["figcraft"]).await;
        assert_eq!(assigned.status, StatusCode::OK, "{}", assigned.text);
        let login = device_login(&env, "ac11-app-user@example.test", "app", "figcraft").await;
        let session_id =
            Uuid::parse_str(login.body["device_session_id"].as_str().unwrap()).unwrap();
        let removed = assign(&env, &owner, org, user, &[]).await;
        assert_eq!(removed.status, StatusCode::OK, "{}", removed.text);
        let revoked: bool =
            sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM device_sessions WHERE id=$1")
                .bind(session_id)
                .fetch_one(&env.pool)
                .await
                .unwrap();
        assert!(!revoked, "an `all` organization enforces no assignment");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn revoking_an_app_only_ends_sessions_no_other_organization_allows() {
        let (env, owner, org_a, user, _member_session) = fixture().await;
        let (_, file_a) = make_file(&env, &owner, org_a, "ac11-multi-a", "figcraft").await;
        let org_b_resp = call(
            &env.app,
            Req::new("POST", "/api/v1/organizations")
                .session(&owner)
                .json(json!({
                    "name":"AC11 second org",
                    "slug":format!("ac11-second-{}", Uuid::new_v4().simple()),
                    "owner_user_id":user
                })),
        )
        .await;
        assert_eq!(
            org_b_resp.status,
            StatusCode::CREATED,
            "{}",
            org_b_resp.text
        );
        let org_b = Uuid::parse_str(org_b_resp.body["id"].as_str().unwrap()).unwrap();
        let assigned = assign(&env, &owner, org_a, user, &["figcraft"]).await;
        assert_eq!(assigned.status, StatusCode::OK, "{}", assigned.text);
        let login = device_login(&env, "ac11-app-user@example.test", "app", "figcraft").await;
        let session_id =
            Uuid::parse_str(login.body["device_session_id"].as_str().unwrap()).unwrap();
        let lock = |file: Uuid, org: Uuid| {
            let pool = env.pool.clone();
            async move {
                sqlx::query("INSERT INTO file_locks(file_id,org_id,holder_user_id,holder_session_kind,holder_session_id,client_id,acquired_at,refreshed_at,expires_at) VALUES($1,$2,$3,'device',$4,'figcraft',now(),now(),now()+interval '5 minutes')")
                    .bind(file).bind(org).bind(user).bind(session_id).execute(&pool).await.unwrap();
            }
        };
        lock(file_a, org_a).await;
        // A file of the second organization held by the same session.
        let (_, file_b) = {
            let owner_b = env.login_ok("ac11-app-user").await;
            make_file(&env, &owner_b, org_b, "ac11-multi-b", "figcraft").await
        };
        lock(file_b, org_b).await;

        // The account owns org B (mode `all`): the session stays, only the
        // lock on org A's file is released.
        let removed = assign(&env, &owner, org_a, user, &[]).await;
        assert_eq!(removed.status, StatusCode::OK, "{}", removed.text);
        let state: (bool, i64, i64) = sqlx::query_as("SELECT revoked_at IS NOT NULL,(SELECT count(*) FROM file_locks WHERE file_id=$2),(SELECT count(*) FROM file_locks WHERE file_id=$3) FROM device_sessions WHERE id=$1")
            .bind(session_id).bind(file_a).bind(file_b).fetch_one(&env.pool).await.unwrap();
        assert_eq!(state, (false, 0, 1));

        // Once org B no longer allows the app either, the session ends and
        // its remaining locks are released.
        sqlx::query("UPDATE organizations SET app_access_mode='assigned' WHERE id=$1")
            .bind(org_b)
            .execute(&env.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE organization_members SET role='member' WHERE org_id=$1 AND user_id=$2")
            .bind(org_b)
            .bind(user)
            .execute(&env.pool)
            .await
            .unwrap();
        let again = assign(&env, &owner, org_a, user, &["figcraft"]).await;
        assert_eq!(again.status, StatusCode::OK, "{}", again.text);
        let removed = assign(&env, &owner, org_a, user, &[]).await;
        assert_eq!(removed.status, StatusCode::OK, "{}", removed.text);
        let state: (bool, i64) = sqlx::query_as("SELECT revoked_at IS NOT NULL,(SELECT count(*) FROM file_locks WHERE holder_session_id=$1) FROM device_sessions WHERE id=$1")
            .bind(session_id).fetch_one(&env.pool).await.unwrap();
        assert_eq!(state, (true, 0));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn switching_to_assigned_mode_releases_tool_locks_of_members_only() {
        let (env, owner, org, user, _member_session) = fixture().await;
        let all = call(
            &env.app,
            Req::new("PATCH", &format!("/api/v1/organizations/{org}/app-access"))
                .session(&owner)
                .json(json!({"mode":"all"})),
        )
        .await;
        assert_eq!(all.status, StatusCode::OK, "{}", all.text);
        let (_, file_member) = make_file(&env, &owner, org, "ac11-mode-a", "figcraft").await;
        let (_, file_owner) = make_file(&env, &owner, org, "ac11-mode-b", "figcraft").await;
        let owner_id: Uuid =
            sqlx::query_scalar("SELECT id FROM users WHERE normalized_email='op@example.test'")
                .fetch_one(&env.pool)
                .await
                .unwrap();
        let mut sessions = Vec::new();
        for (email, holder, file) in [
            ("ac11-app-user@example.test", user, file_member),
            ("op@example.test", owner_id, file_owner),
        ] {
            let login = device_login(&env, email, "launcher", "launcher").await;
            let session_id =
                Uuid::parse_str(login.body["device_session_id"].as_str().unwrap()).unwrap();
            sqlx::query("INSERT INTO file_locks(file_id,org_id,holder_user_id,holder_session_kind,holder_session_id,client_id,acquired_at,refreshed_at,expires_at) VALUES($1,$2,$3,'device',$4,'launcher',now(),now(),now()+interval '5 minutes')")
                .bind(file).bind(org).bind(holder).bind(session_id).execute(&env.pool).await.unwrap();
            sessions.push((session_id, file));
        }
        let assigned = call(
            &env.app,
            Req::new("PATCH", &format!("/api/v1/organizations/{org}/app-access"))
                .session(&owner)
                .json(json!({"mode":"assigned"})),
        )
        .await;
        assert_eq!(assigned.status, StatusCode::OK, "{}", assigned.text);
        let locks: Vec<i64> = {
            let mut counts = Vec::new();
            for (_, file) in &sessions {
                counts.push(
                    sqlx::query_scalar("SELECT count(*) FROM file_locks WHERE file_id=$1")
                        .bind(file)
                        .fetch_one(&env.pool)
                        .await
                        .unwrap(),
                );
            }
            counts
        };
        assert_eq!(locks, [0, 1], "member lock released, owner lock kept");
        for (session, _) in &sessions {
            let revoked: bool = sqlx::query_scalar(
                "SELECT revoked_at IS NOT NULL FROM device_sessions WHERE id=$1",
            )
            .bind(session)
            .fetch_one(&env.pool)
            .await
            .unwrap();
            assert!(!revoked, "launcher sessions stay usable like a browser");
        }
    }
}
