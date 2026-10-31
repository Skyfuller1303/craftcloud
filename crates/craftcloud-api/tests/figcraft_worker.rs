//! PostgreSQL integration tests for FigCraft snapshot and revocation workers.

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        str::FromStr,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum::body::Body;
    use axum::http::StatusCode;
    use craftcloud_api::{
        Clock,
        maintenance::{Binding, check_store_binding},
    };
    use craftcloud_backup::{CreateOptions, RestoreOptions};
    use craftcloud_db::PgPool;
    use craftcloud_figcraft::{Access, Backend, Error, Export, Link};
    use craftcloud_storage::LocalStore;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use sqlx::{Connection, PgConnection};
    use time::OffsetDateTime;
    use uuid::Uuid;

    use crate::common::{Req, Session, TestEnv, call};

    #[derive(Default)]
    struct FakeFigcraft {
        next_link: AtomicUsize,
        fail_revokes: AtomicUsize,
        exports: AtomicUsize,
        export_bytes: Mutex<Vec<u8>>,
        revoked: Mutex<Vec<String>>,
        fail_export: AtomicBool,
        on_export: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    }

    impl FakeFigcraft {
        fn fail_next_revoke(&self) {
            self.fail_revokes.store(1, Ordering::SeqCst);
        }

        fn set_export(&self, bytes: Vec<u8>) {
            *self.export_bytes.lock().unwrap() = bytes;
        }
    }

    impl Backend for FakeFigcraft {
        fn publish(&self, _original: &[u8], _name: &str) -> Result<String, Error> {
            Ok("remote-live-file".to_owned())
        }

        fn create_link(&self, _file: &str, _access: Access) -> Result<Link, Error> {
            let id = self.next_link.fetch_add(1, Ordering::SeqCst);
            Ok(Link {
                id: format!("remote-link-{id}"),
                token: format!("test-token-{id}"),
            })
        }

        fn revoke_link(&self, _file: &str, link: &str) -> Result<(), Error> {
            if self
                .fail_revokes
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    (left > 0).then_some(left.saturating_sub(1))
                })
                .is_ok()
            {
                return Err(Error::RemoteUnavailable);
            }
            self.revoked.lock().unwrap().push(link.to_owned());
            Ok(())
        }

        fn export(&self, _file: &str, original: &[u8]) -> Result<Export, Error> {
            self.exports.fetch_add(1, Ordering::SeqCst);
            if let Some(hook) = self.on_export.lock().unwrap().as_ref() {
                hook();
            }
            if self.fail_export.load(Ordering::SeqCst) {
                return Err(Error::RemoteUnavailable);
            }
            let bytes = self.export_bytes.lock().unwrap();
            Ok(Export {
                bytes: if bytes.is_empty() {
                    original.to_vec()
                } else {
                    bytes.clone()
                },
            })
        }

        fn remove(&self, _file: &str) -> Result<(), Error> {
            Ok(())
        }
    }

    struct Fixture {
        env: TestEnv,
        owner: Session,
        file: Uuid,
        backend: Arc<FakeFigcraft>,
    }

    fn archive(name: &str) -> Vec<u8> {
        let doc = figcraft_doc::Document::new(name);
        figcraft_format::save_bytes_with_history(&doc, None, &[], &[]).unwrap()
    }

    fn sha_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    async fn fixture() -> Fixture {
        let mut env = TestEnv::new().await;
        let backend = Arc::new(FakeFigcraft::default());
        env.configure_figcraft(backend.clone(), "ws://figcraft.test/figcraft")
            .await;
        let owner = env.login_ok("op").await;
        let org: Uuid =
            sqlx::query_scalar("SELECT id FROM organizations WHERE slug='operator-org'")
                .fetch_one(&env.pool)
                .await
                .unwrap();
        let workspace = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/workspaces"))
                .session(&owner)
                .json(json!({"name":"FigCraft worker","slug":"figcraft-worker"})),
        )
        .await;
        assert_eq!(workspace.status, StatusCode::CREATED, "{}", workspace.text);
        let workspace_id = Uuid::parse_str(workspace.body["id"].as_str().unwrap()).unwrap();
        let project = call(
            &env.app,
            Req::new(
                "POST",
                &format!("/api/v1/workspaces/{workspace_id}/projects"),
            )
            .session(&owner)
            .json(json!({"name":"Live"})),
        )
        .await;
        assert_eq!(project.status, StatusCode::CREATED, "{}", project.text);
        let project_id = Uuid::parse_str(project.body["id"].as_str().unwrap()).unwrap();
        let file = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/projects/{project_id}/files"))
                .session(&owner)
                .json(json!({"name":"worker.figcraft","app":"figcraft","format":"fig"})),
        )
        .await;
        assert_eq!(file.status, StatusCode::CREATED, "{}", file.text);
        let file_id = Uuid::parse_str(file.body["id"].as_str().unwrap()).unwrap();
        let source = archive("Worker original");
        let upload = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/files/{file_id}/uploads"))
                .session(&owner)
                .json(json!({"size_bytes":source.len(),"sha256":sha_hex(&source)})),
        )
        .await;
        assert_eq!(upload.status, StatusCode::CREATED, "{}", upload.text);
        let upload_id = upload.body["upload_id"].as_str().unwrap();
        let stored = call(
            &env.app,
            Req::new("PUT", &format!("/api/v1/uploads/{upload_id}/content"))
                .session(&owner)
                .body(Body::from(source.clone())),
        )
        .await;
        assert_eq!(stored.status, StatusCode::OK, "{}", stored.text);
        let committed = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/files/{file_id}/versions"))
                .session(&owner)
                .header("If-None-Match", "*")
                .json(json!({"upload_id":upload_id})),
        )
        .await;
        assert_eq!(committed.status, StatusCode::CREATED, "{}", committed.text);
        Fixture {
            env,
            owner,
            file: file_id,
            backend,
        }
    }

    async fn open_live(fixture: &Fixture, session: &Session) {
        let opened = call(
            &fixture.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/live", fixture.file)).session(session),
        )
        .await;
        assert_eq!(opened.status, StatusCode::OK, "{}", opened.text);
    }

    async fn worker_tick(env: &TestEnv, state: &mut craftcloud_api::FigcraftWorkerState) {
        craftcloud_api::figcraft_worker_tick(Arc::clone(&env.services), state).await;
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn automatic_snapshot_commits_changed_head_once_then_only_advances_snapshot_time() {
        let fixture = fixture().await;
        open_live(&fixture, &fixture.owner).await;
        fixture.backend.set_export(archive("Worker changed"));
        let mut state = craftcloud_api::FigcraftWorkerState::default();
        fixture.env.clock.advance(Duration::from_secs(16 * 60));
        worker_tick(&fixture.env, &mut state).await;
        let first_versions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM file_versions WHERE file_id=$1 AND actor_kind='figcraft_live'",
        )
        .bind(fixture.file)
        .fetch_one(&fixture.env.pool)
        .await
        .unwrap();
        assert_eq!(first_versions, 1, "changed head must be snapshotted");
        assert_eq!(fixture.backend.exports.load(Ordering::SeqCst), 1);
        let first_snapshot: OffsetDateTime =
            sqlx::query_scalar("SELECT last_snapshot_at FROM figcraft_live_files WHERE file_id=$1")
                .bind(fixture.file)
                .fetch_one(&fixture.env.pool)
                .await
                .unwrap();
        fixture.env.clock.advance(Duration::from_secs(16 * 60));
        worker_tick(&fixture.env, &mut state).await;
        let second_snapshot: OffsetDateTime =
            sqlx::query_scalar("SELECT last_snapshot_at FROM figcraft_live_files WHERE file_id=$1")
                .bind(fixture.file)
                .fetch_one(&fixture.env.pool)
                .await
                .unwrap();
        assert!(second_snapshot > first_snapshot);
        let versions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM file_versions WHERE file_id=$1 AND actor_kind='figcraft_live'",
        )
        .bind(fixture.file)
        .fetch_one(&fixture.env.pool)
        .await
        .unwrap();
        assert_eq!(
            versions, 1,
            "unchanged export must not create duplicate versions"
        );
        assert_eq!(fixture.backend.exports.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn grant_revocation_outbox_retries_after_remote_failure() {
        let fixture = fixture().await;
        open_live(&fixture, &fixture.owner).await;
        let editor_id = fixture.env.user("worker-editor").await;
        let editor = fixture.env.login_ok("worker-editor").await;
        let org: Uuid = sqlx::query_scalar("SELECT org_id FROM files WHERE id=$1")
            .bind(fixture.file)
            .fetch_one(&fixture.env.pool)
            .await
            .unwrap();
        let membership = call(
            &fixture.env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/members"))
                .session(&fixture.owner)
                .json(json!({"user_id":editor_id,"role":"member"})),
        )
        .await;
        assert_eq!(
            membership.status,
            StatusCode::CREATED,
            "{}",
            membership.text
        );
        let grant = call(
            &fixture.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/grants", fixture.file))
                .session(&fixture.owner)
                .json(json!({"user_id":editor_id,"role":"editor"})),
        )
        .await;
        assert_eq!(grant.status, StatusCode::CREATED, "{}", grant.text);
        let grant_id = grant.body["id"].as_str().unwrap().to_owned();
        open_live(&fixture, &editor).await;
        let downgraded = call(
            &fixture.env.app,
            Req::new("PATCH", &format!("/api/v1/grants/{grant_id}"))
                .session(&fixture.owner)
                .if_match("\"grant-v1\"")
                .json(json!({"role":"viewer"})),
        )
        .await;
        assert_eq!(downgraded.status, StatusCode::OK, "{}", downgraded.text);
        assert_outbox(&fixture.env, fixture.file, editor_id, "role_changed").await;
        sqlx::query(
            "DELETE FROM figcraft_outbox WHERE file_id=$1 AND user_id=$2 AND reason='role_changed'",
        )
        .bind(fixture.file)
        .bind(editor_id)
        .execute(&fixture.env.pool)
        .await
        .unwrap();
        fixture.backend.fail_next_revoke();
        let deleted = call(
            &fixture.env.app,
            Req::new("DELETE", &format!("/api/v1/grants/{grant_id}")).session(&fixture.owner),
        )
        .await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.text);
        let mut state = craftcloud_api::FigcraftWorkerState::default();
        worker_tick(&fixture.env, &mut state).await;
        let attempts: i32 = sqlx::query_scalar("SELECT attempts FROM figcraft_outbox WHERE file_id=$1 AND user_id=$2 AND reason='grant_revoked'")
            .bind(fixture.file)
            .bind(editor_id)
            .fetch_one(&fixture.env.pool)
            .await
            .unwrap();
        assert_eq!(attempts, 1);
        fixture.env.clock.advance(Duration::from_secs(5));
        worker_tick(&fixture.env, &mut state).await;
        let outbox_count: i64 = sqlx::query_scalar("SELECT count(*) FROM figcraft_outbox WHERE file_id=$1 AND user_id=$2 AND reason='grant_revoked'")
            .bind(fixture.file)
            .bind(editor_id)
            .fetch_one(&fixture.env.pool)
            .await
            .unwrap();
        assert_eq!(outbox_count, 0);
        assert!(!fixture.backend.revoked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn central_entitlement_changes_enqueue_durable_figcraft_revocations() {
        let fixture = fixture().await;
        open_live(&fixture, &fixture.owner).await;
        let org: Uuid = sqlx::query_scalar("SELECT org_id FROM files WHERE id=$1")
            .bind(fixture.file)
            .fetch_one(&fixture.env.pool)
            .await
            .unwrap();
        let workspace: Uuid = sqlx::query_scalar("SELECT workspace_id FROM files WHERE id=$1")
            .bind(fixture.file)
            .fetch_one(&fixture.env.pool)
            .await
            .unwrap();
        let user = fixture.env.user("worker-entitlement").await;
        let member = call(
            &fixture.env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/members"))
                .session(&fixture.owner)
                .json(json!({"user_id":user,"role":"member"})),
        )
        .await;
        assert_eq!(member.status, StatusCode::CREATED, "{}", member.text);
        let workspace_member = call(
            &fixture.env.app,
            Req::new("POST", &format!("/api/v1/workspaces/{workspace}/members"))
                .session(&fixture.owner)
                .json(json!({"user_id":user,"role":"editor"})),
        )
        .await;
        assert_eq!(
            workspace_member.status,
            StatusCode::CREATED,
            "{}",
            workspace_member.text
        );
        let session = fixture.env.login_ok("worker-entitlement").await;
        open_live(&fixture, &session).await;

        let workspace_role = call(
            &fixture.env.app,
            Req::new(
                "PATCH",
                &format!("/api/v1/workspaces/{workspace}/members/{user}"),
            )
            .session(&fixture.owner)
            .json(json!({"role":"viewer"})),
        )
        .await;
        assert_eq!(
            workspace_role.status,
            StatusCode::OK,
            "{}",
            workspace_role.text
        );
        assert_outbox(&fixture.env, fixture.file, user, "role_changed").await;
        sqlx::query(
            "DELETE FROM figcraft_outbox WHERE file_id=$1 AND user_id=$2 AND reason='role_changed'",
        )
        .bind(fixture.file)
        .bind(user)
        .execute(&fixture.env.pool)
        .await
        .unwrap();
        let edit_link: String = sqlx::query_scalar(
            "SELECT figcraft_link_id FROM figcraft_links WHERE file_id=$1 AND user_id=$2 AND access='edit' AND revoked_at IS NULL",
        )
        .bind(fixture.file)
        .bind(user)
        .fetch_one(&fixture.env.pool)
        .await
        .unwrap();
        let mut reconcile = craftcloud_api::FigcraftWorkerState::default();
        reconcile.reconcile_now();
        worker_tick(&fixture.env, &mut reconcile).await;
        assert!(fixture.backend.revoked.lock().unwrap().contains(&edit_link));
        let revoked_at: Option<OffsetDateTime> =
            sqlx::query_scalar("SELECT revoked_at FROM figcraft_links WHERE figcraft_link_id=$1")
                .bind(&edit_link)
                .fetch_one(&fixture.env.pool)
                .await
                .unwrap();
        assert!(
            revoked_at.is_some(),
            "reconciliation must recover a lost revoke hook"
        );
        open_live(&fixture, &session).await;

        let promoted = call(
            &fixture.env.app,
            Req::new(
                "PATCH",
                &format!("/api/v1/organizations/{org}/members/{user}"),
            )
            .session(&fixture.owner)
            .json(json!({"role":"admin"})),
        )
        .await;
        assert_eq!(promoted.status, StatusCode::OK, "{}", promoted.text);
        let upgrade_rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM figcraft_outbox WHERE file_id=$1 AND user_id=$2 AND reason='role_changed'",
        )
        .bind(fixture.file)
        .bind(user)
        .fetch_one(&fixture.env.pool)
        .await
        .unwrap();
        assert_eq!(
            upgrade_rows, 0,
            "a role upgrade must not revoke capabilities"
        );
        open_live(&fixture, &session).await;
        sqlx::query(
            "DELETE FROM figcraft_outbox WHERE file_id=$1 AND user_id=$2 AND reason='role_changed'",
        )
        .bind(fixture.file)
        .bind(user)
        .execute(&fixture.env.pool)
        .await
        .unwrap();

        let org_role = call(
            &fixture.env.app,
            Req::new(
                "PATCH",
                &format!("/api/v1/organizations/{org}/members/{user}"),
            )
            .session(&fixture.owner)
            .json(json!({"role":"member"})),
        )
        .await;
        assert_eq!(org_role.status, StatusCode::OK, "{}", org_role.text);
        assert_outbox(&fixture.env, fixture.file, user, "role_changed").await;
        let removed_workspace = call(
            &fixture.env.app,
            Req::new(
                "DELETE",
                &format!("/api/v1/workspaces/{workspace}/members/{user}"),
            )
            .session(&fixture.owner),
        )
        .await;
        assert_eq!(
            removed_workspace.status,
            StatusCode::NO_CONTENT,
            "{}",
            removed_workspace.text
        );
        assert_outbox(&fixture.env, fixture.file, user, "membership_removed").await;
        sqlx::query("DELETE FROM figcraft_outbox WHERE file_id=$1 AND user_id=$2 AND reason='membership_removed'")
            .bind(fixture.file)
            .bind(user)
            .execute(&fixture.env.pool)
            .await
            .unwrap();
        let removed_org = call(
            &fixture.env.app,
            Req::new(
                "DELETE",
                &format!("/api/v1/organizations/{org}/members/{user}"),
            )
            .session(&fixture.owner),
        )
        .await;
        assert_eq!(
            removed_org.status,
            StatusCode::NO_CONTENT,
            "{}",
            removed_org.text
        );
        assert_outbox(&fixture.env, fixture.file, user, "membership_removed").await;

        let disabled = call(
            &fixture.env.app,
            Req::new("POST", &format!("/api/v1/admin/users/{user}/disable"))
                .session(&fixture.owner),
        )
        .await;
        assert_eq!(disabled.status, StatusCode::OK, "{}", disabled.text);
        assert_outbox(&fixture.env, fixture.file, user, "account_disabled").await;

        let owner_two_factor_ready: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL)",
        )
        .bind(fixture.env.operator_id)
        .fetch_one(&fixture.env.pool)
        .await
        .unwrap();
        if !owner_two_factor_ready {
            sqlx::query("INSERT INTO user_two_factor(user_id,secret_ciphertext,enabled_at,last_accepted_step,created_at) VALUES($1,$2,$3,NULL,$3)")
                .bind(fixture.env.operator_id)
                .bind(&[0x42_u8; 32][..])
                .bind(fixture.env.clock.now())
                .execute(&fixture.env.pool)
                .await
                .unwrap();
        }
        let organization = call(
            &fixture.env.app,
            Req::new("GET", &format!("/api/v1/organizations/{org}")).session(&fixture.owner),
        )
        .await;
        assert_eq!(organization.status, StatusCode::OK, "{}", organization.text);
        sqlx::query("INSERT INTO user_two_factor(user_id,secret_ciphertext,enabled_at,last_accepted_step,created_at) VALUES($1,$2,$3,NULL,$3) ON CONFLICT (user_id) DO UPDATE SET enabled_at=EXCLUDED.enabled_at")
            .bind(fixture.env.operator_id)
            .bind(&[0x42_u8; 32][..])
            .bind(fixture.env.clock.now())
            .execute(&fixture.env.pool)
            .await
            .unwrap();
        let two_factor = call(
            &fixture.env.app,
            Req::new("PATCH", &format!("/api/v1/organizations/{org}"))
                .session(&fixture.owner)
                .if_match(&organization.etag())
                .json(json!({"require_two_factor":true})),
        )
        .await;
        assert_eq!(two_factor.status, StatusCode::OK, "{}", two_factor.text);
        assert_outbox(&fixture.env, fixture.file, user, "security_changed").await;

        // Switching the requirement off tightens nothing.
        sqlx::query("DELETE FROM figcraft_outbox")
            .execute(&fixture.env.pool)
            .await
            .unwrap();
        let organization = call(
            &fixture.env.app,
            Req::new("GET", &format!("/api/v1/organizations/{org}")).session(&fixture.owner),
        )
        .await;
        assert_eq!(organization.status, StatusCode::OK, "{}", organization.text);
        let relaxed = call(
            &fixture.env.app,
            Req::new("PATCH", &format!("/api/v1/organizations/{org}"))
                .session(&fixture.owner)
                .if_match(&organization.etag())
                .json(json!({"require_two_factor":false})),
        )
        .await;
        assert_eq!(relaxed.status, StatusCode::OK, "{}", relaxed.text);
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM figcraft_outbox")
            .fetch_one(&fixture.env.pool)
            .await
            .unwrap();
        assert_eq!(rows, 0, "switching the 2FA requirement off must not revoke");
    }

    /// Counts (and clears) the security outbox rows of one capability holder.
    async fn take_security_rows(env: &TestEnv, file: Uuid, user: Uuid) -> i64 {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM figcraft_outbox WHERE file_id=$1 AND user_id=$2 AND reason='security_changed'",
        )
        .bind(file)
        .bind(user)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM figcraft_outbox")
            .execute(&env.pool)
            .await
            .unwrap();
        count
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn account_security_events_enqueue_figcraft_revocations() {
        let fixture = fixture().await;
        open_live(&fixture, &fixture.owner).await;
        let env = &fixture.env;
        let user = env.operator_id;
        let now = env.clock.now();
        let current_hash: String =
            sqlx::query_scalar("SELECT password_hash FROM users WHERE id=$1")
                .bind(user)
                .fetch_one(&env.pool)
                .await
                .unwrap();
        let session: Uuid = sqlx::query_scalar(
            "SELECT id FROM sessions WHERE user_id=$1 AND revoked_at IS NULL LIMIT 1",
        )
        .bind(user)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        let changed = craftcloud_db::accounts::change_password(
            &env.pool,
            user,
            &current_hash,
            "replacement-hash",
            session,
            now,
        )
        .await
        .unwrap();
        assert!(changed.is_some());
        assert_eq!(
            take_security_rows(env, fixture.file, user).await,
            1,
            "password change"
        );

        craftcloud_db::sessions::revoke_all_for_user(&env.pool, user, now)
            .await
            .unwrap();
        assert_eq!(
            take_security_rows(env, fixture.file, user).await,
            1,
            "logout all"
        );
        craftcloud_db::sessions::revoke_all_for_user_audited(
            &env.pool,
            user,
            now,
            &craftcloud_db::audit::AuditCtx::system(),
        )
        .await
        .unwrap();
        assert_eq!(
            take_security_rows(env, fixture.file, user).await,
            1,
            "audited logout all"
        );

        sqlx::query("INSERT INTO user_two_factor(user_id,secret_ciphertext,enabled_at,last_accepted_step,created_at) VALUES($1,$2,$3,NULL,$3) ON CONFLICT (user_id) DO NOTHING")
            .bind(user)
            .bind(&[0x42_u8; 32][..])
            .bind(now)
            .execute(&env.pool)
            .await
            .unwrap();
        let token_hash = [7_u8; 32];
        let issued = craftcloud_db::password_reset::create(
            &env.pool,
            &craftcloud_db::password_reset::NewResetToken {
                user_id: user,
                token_hash,
                created_by: None,
                reset_two_factor: true,
                now,
                expires_at: now + time::Duration::hours(1),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            issued,
            craftcloud_db::password_reset::IssueOutcome::Issued { .. }
        ));
        let two_factor_left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM user_two_factor WHERE user_id=$1")
                .bind(user)
                .fetch_one(&env.pool)
                .await
                .unwrap();
        assert_eq!(two_factor_left, 0);
        assert_eq!(
            take_security_rows(env, fixture.file, user).await,
            1,
            "reset issue"
        );

        let consumed =
            craftcloud_db::password_reset::consume(&env.pool, &token_hash, "reset-hash", now)
                .await
                .unwrap();
        assert_eq!(consumed, Some(user));
        assert_eq!(
            take_security_rows(env, fixture.file, user).await,
            1,
            "reset use"
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn busy_file_guard_defers_revocation_without_counting_an_attempt() {
        let fixture = fixture().await;
        open_live(&fixture, &fixture.owner).await;
        let env = &fixture.env;
        let user = env.operator_id;
        let now = env.clock.now();
        let mut conn = env.pool.acquire().await.unwrap();
        let locked: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1,0))")
                .bind(fixture.file.to_string())
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert!(locked);
        craftcloud_db::figcraft::enqueue_file_user_revocations_pool(
            &env.pool,
            fixture.file,
            user,
            "security_changed",
            now,
        )
        .await
        .unwrap();
        let mut state = craftcloud_api::FigcraftWorkerState::default();
        worker_tick(env, &mut state).await;
        let row: (i32, Option<OffsetDateTime>, OffsetDateTime) = sqlx::query_as(
            "SELECT attempts,claimed_at,available_at FROM figcraft_outbox WHERE file_id=$1 AND user_id=$2",
        )
        .bind(fixture.file)
        .bind(user)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(row.0, 0, "local contention is not a failed attempt");
        assert_eq!(row.1, None);
        assert!(row.2 > now && row.2 <= now + time::Duration::seconds(3));
        sqlx::query("SELECT pg_advisory_unlock(hashtextextended($1,0))")
            .bind(fixture.file.to_string())
            .execute(&mut *conn)
            .await
            .unwrap();
        drop(conn);
        env.clock.advance(Duration::from_secs(3));
        worker_tick(env, &mut state).await;
        assert_eq!(
            take_security_rows(env, fixture.file, user).await,
            0,
            "the deferred revocation completes once the guard is free"
        );
        assert!(!fixture.backend.revoked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn enqueue_keeps_attempts_and_pulls_an_existing_item_forward() {
        let fixture = fixture().await;
        open_live(&fixture, &fixture.owner).await;
        let env = &fixture.env;
        let user = env.operator_id;
        let now = env.clock.now();
        craftcloud_db::figcraft::enqueue_file_user_revocations_pool(
            &env.pool,
            fixture.file,
            user,
            "security_changed",
            now + time::Duration::minutes(5),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE figcraft_outbox SET attempts=3 WHERE file_id=$1")
            .bind(fixture.file)
            .execute(&env.pool)
            .await
            .unwrap();
        craftcloud_db::figcraft::enqueue_file_user_revocations_pool(
            &env.pool,
            fixture.file,
            user,
            "security_changed",
            now,
        )
        .await
        .unwrap();
        let row: (i32, OffsetDateTime) = sqlx::query_as(
            "SELECT attempts,available_at FROM figcraft_outbox WHERE file_id=$1 AND user_id=$2",
        )
        .bind(fixture.file)
        .bind(user)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(row.0, 3);
        assert!(row.1 <= now + time::Duration::milliseconds(1));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn failing_final_snapshot_backs_off_instead_of_exporting_every_tick() {
        let fixture = fixture().await;
        open_live(&fixture, &fixture.owner).await;
        let env = &fixture.env;
        sqlx::query("UPDATE figcraft_links SET revoked_at=$2 WHERE file_id=$1")
            .bind(fixture.file)
            .bind(env.clock.now())
            .execute(&env.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE figcraft_live_files SET state='unpublishing',final_snapshot_done=false,last_snapshot_attempt_at=NULL WHERE file_id=$1")
            .bind(fixture.file)
            .execute(&env.pool)
            .await
            .unwrap();
        fixture.backend.fail_export.store(true, Ordering::SeqCst);
        let mut state = craftcloud_api::FigcraftWorkerState::default();
        worker_tick(env, &mut state).await;
        assert_eq!(fixture.backend.exports.load(Ordering::SeqCst), 1);
        for _ in 0..5 {
            env.clock.advance(Duration::from_secs(1));
            worker_tick(env, &mut state).await;
        }
        assert_eq!(
            fixture.backend.exports.load(Ordering::SeqCst),
            1,
            "a failed final snapshot waits before the next attempt"
        );
        env.clock.advance(Duration::from_secs(61));
        worker_tick(env, &mut state).await;
        assert_eq!(fixture.backend.exports.load(Ordering::SeqCst), 2);
        fixture.backend.fail_export.store(false, Ordering::SeqCst);
        env.clock.advance(Duration::from_secs(61));
        worker_tick(env, &mut state).await;
        env.clock.advance(Duration::from_secs(1));
        worker_tick(env, &mut state).await;
        let finished: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM figcraft_live_files WHERE file_id=$1 AND state='republish'",
        )
        .bind(fixture.file)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(
            finished, 1,
            "recovery completes once the export works again"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn revocation_queued_during_an_export_is_processed_in_the_same_pass() {
        let fixture = fixture().await;
        open_live(&fixture, &fixture.owner).await;
        let env = &fixture.env;
        let (file, user, pool) = (fixture.file, env.operator_id, env.pool.clone());
        let handle = tokio::runtime::Handle::current();
        let now = env.clock.now();
        *fixture.backend.on_export.lock().unwrap() = Some(Box::new(move || {
            let pool = pool.clone();
            handle.block_on(async move {
                craftcloud_db::figcraft::enqueue_file_user_revocations_pool(
                    &pool,
                    file,
                    user,
                    "security_changed",
                    now,
                )
                .await
                .unwrap();
            });
        }));
        env.clock.advance(Duration::from_secs(16 * 60));
        let mut state = craftcloud_api::FigcraftWorkerState::default();
        worker_tick(env, &mut state).await;
        assert_eq!(fixture.backend.exports.load(Ordering::SeqCst), 1);
        assert!(
            !fixture.backend.revoked.lock().unwrap().is_empty(),
            "revocation must not wait behind the snapshot item"
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn live_operations_are_authorized_and_rate_limited_before_a_job_permit() {
        let fixture = fixture().await;
        fixture.env.user("figcraft-outsider").await;
        let outsider = fixture.env.login_ok("figcraft-outsider").await;
        let permits = fixture.env.services.figcraft_job_permits_for_tests();
        let held = permits.clone().acquire_many_owned(4).await.unwrap();
        let denied = call(
            &fixture.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/live", fixture.file)).session(&outsider),
        )
        .await;
        assert!(
            matches!(denied.status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN),
            "an unauthorized caller is refused before capacity is checked: {} {}",
            denied.status,
            denied.text
        );
        let busy = call(
            &fixture.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/live", fixture.file))
                .session(&fixture.owner),
        )
        .await;
        assert_eq!(
            busy.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            busy.text
        );
        assert_eq!(busy.code(), "FIGCRAFT_BUSY");
        drop(held);
        // Unauthorized attempts never count against any budget.
        for _ in 0..30 {
            let denied = call(
                &fixture.env.app,
                Req::new("POST", &format!("/api/v1/files/{}/live", fixture.file))
                    .session(&outsider),
            )
            .await;
            assert_ne!(denied.status, StatusCode::TOO_MANY_REQUESTS);
        }
        // The owner already used one operation above; a new window starts clean.
        fixture.env.clock.advance(Duration::from_secs(61));
        let mut limited = None;
        for attempt in 1..=25 {
            let response = call(
                &fixture.env.app,
                Req::new("POST", &format!("/api/v1/files/{}/live", fixture.file))
                    .session(&fixture.owner),
            )
            .await;
            if response.status == StatusCode::TOO_MANY_REQUESTS {
                limited = Some(attempt);
                break;
            }
            assert_eq!(response.status, StatusCode::OK, "{}", response.text);
        }
        assert_eq!(limited, Some(21), "20 operations per window, then 429");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn revoked_link_rows_are_pruned_after_a_day() {
        let fixture = fixture().await;
        open_live(&fixture, &fixture.owner).await;
        let env = &fixture.env;
        sqlx::query("UPDATE figcraft_links SET revoked_at=$2 WHERE file_id=$1")
            .bind(fixture.file)
            .bind(env.clock.now())
            .execute(&env.pool)
            .await
            .unwrap();
        let mut state = craftcloud_api::FigcraftWorkerState::default();
        worker_tick(env, &mut state).await;
        let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM figcraft_links WHERE file_id=$1")
            .bind(fixture.file)
            .fetch_one(&env.pool)
            .await
            .unwrap();
        assert!(kept > 0, "recently revoked rows stay for diagnosis");
        env.clock.advance(Duration::from_secs(25 * 3600));
        worker_tick(env, &mut state).await;
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM figcraft_links WHERE file_id=$1")
            .bind(fixture.file)
            .fetch_one(&env.pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }

    async fn assert_outbox(env: &TestEnv, file: Uuid, user: Uuid, reason: &str) {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM figcraft_outbox WHERE file_id=$1 AND user_id=$2 AND reason=$3",
        )
        .bind(file)
        .bind(user)
        .bind(reason)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(count, 1, "missing durable outbox row for {reason}");
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!("cc-{label}-{}", Uuid::new_v4().simple()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn child(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct RestoreTarget {
        name: String,
        admin: PgConnectOptions,
        pool: PgPool,
        store_dir: TempDir,
    }

    impl RestoreTarget {
        async fn new() -> Self {
            let url =
                std::env::var("CC_TEST_DATABASE_URL").expect("CC_TEST_DATABASE_URL must be set");
            let admin = PgConnectOptions::from_str(&url).unwrap();
            let name = format!("cc_test_{}", Uuid::new_v4().simple());
            let mut conn = PgConnection::connect_with(&admin).await.unwrap();
            sqlx::query(&format!("CREATE DATABASE \"{name}\""))
                .execute(&mut conn)
                .await
                .unwrap();
            let pool = PgPoolOptions::new()
                .max_connections(10)
                .acquire_timeout(Duration::from_secs(20))
                .connect_with(admin.clone().database(&name))
                .await
                .unwrap();
            Self {
                name,
                admin,
                pool,
                store_dir: TempDir::new("figcraft-restore-store"),
            }
        }

        async fn restore(&self, backup: &Path) {
            std::fs::create_dir_all(&self.store_dir.0).unwrap();
            let store = LocalStore::open(&self.store_dir.0).await.unwrap();
            craftcloud_backup::restore(&self.pool, &store, backup, &RestoreOptions::default())
                .await
                .unwrap();
        }
    }

    impl Drop for RestoreTarget {
        fn drop(&mut self) {
            let admin = self.admin.clone();
            let name = self.name.clone();
            let _ = std::thread::spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                runtime.block_on(async {
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
    async fn backup_restore_republishes_figcraft_and_drops_old_capabilities() {
        let fixture = fixture().await;
        let store = LocalStore::open(&fixture.env.storage_dir).await.unwrap();
        let binding = check_store_binding(&fixture.env.pool, &store, fixture.env.clock.now(), true)
            .await
            .unwrap();
        assert!(matches!(binding, Binding::Created(_)));
        open_live(&fixture, &fixture.owner).await;
        let source_version: Uuid =
            sqlx::query_scalar("SELECT head_version_id FROM files WHERE id=$1")
                .bind(fixture.file)
                .fetch_one(&fixture.env.pool)
                .await
                .unwrap();
        sqlx::query("UPDATE figcraft_live_files SET last_snapshot_version_id=$2,last_snapshot_at=$3,last_snapshot_attempt_at=$3,final_snapshot_done=true WHERE file_id=$1")
            .bind(fixture.file)
            .bind(source_version)
            .bind(fixture.env.clock.now())
            .execute(&fixture.env.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE figcraft_links SET figcraft_link_id='old-remote-link',token_ciphertext=$2 WHERE file_id=$1 AND revoked_at IS NULL")
            .bind(fixture.file)
            .bind(&[0x5a_u8; 48][..])
            .execute(&fixture.env.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO figcraft_outbox(file_id,user_id,reason,available_at,created_at) VALUES($1,$2,'security_changed',$3,$3)")
            .bind(fixture.file)
            .bind(fixture.env.operator_id)
            .bind(fixture.env.clock.now())
            .execute(&fixture.env.pool)
            .await
            .unwrap();

        let dirs = TempDir::new("figcraft-backup");
        let backup = dirs.child("backup");
        craftcloud_backup::create(
            &fixture.env.pool,
            &store,
            &backup,
            &CreateOptions {
                server_version: "test",
                lock_wait: Duration::from_secs(2),
                on_phase: None,
            },
        )
        .await
        .unwrap();
        let target = RestoreTarget::new().await;
        target.restore(&backup).await;
        let restored: (Option<String>, String, Option<OffsetDateTime>, bool) = sqlx::query_as(
            "SELECT figcraft_file_id,state,last_snapshot_attempt_at,final_snapshot_done FROM figcraft_live_files WHERE file_id=$1",
        )
        .bind(fixture.file)
        .fetch_one(&target.pool)
        .await
        .unwrap();
        assert_eq!(restored.0, None);
        assert_eq!(restored.1, "republish");
        assert_eq!(restored.2, None);
        assert!(!restored.3);
        let link: (Option<String>, Option<Vec<u8>>, bool) = sqlx::query_as(
            "SELECT figcraft_link_id,token_ciphertext,revoked_at IS NOT NULL FROM figcraft_links WHERE file_id=$1",
        )
        .bind(fixture.file)
        .fetch_one(&target.pool)
        .await
        .unwrap();
        assert_eq!(link.0, None);
        assert_eq!(link.1, None);
        assert!(link.2);
        let outbox: i64 = sqlx::query_scalar("SELECT count(*) FROM figcraft_outbox")
            .fetch_one(&target.pool)
            .await
            .unwrap();
        assert_eq!(outbox, 0);
    }
}
