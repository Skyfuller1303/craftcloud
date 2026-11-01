//! Integration tests against a real PostgreSQL server.
//!
//! Run with `cargo xtask db-test`, which starts a throwaway container and sets
//! `CC_TEST_DATABASE_URL`. Every test creates its own database
//! `cc_test_<uuid>` and drops it afterwards.

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::time::Duration;

    use craftcloud_db::migrate::{self, migrator_up_to};
    use craftcloud_db::organizations::{self, MemberStatus, NewOrganization, OrgRole};
    use craftcloud_db::sessions::{self, NewSession};
    use craftcloud_db::setup::{self, NewInstance, SetupError};
    use craftcloud_db::users::{self, NewUser, User};
    use craftcloud_db::workspaces::{self, MAX_LIST_LIMIT, NewWorkspace, WorkspaceRole};
    use craftcloud_db::{DbError, OrgScope, PgPool, SchemaProblem};
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use sqlx::{Connection, PgConnection, Row};
    use uuid::Uuid;

    const ENV: &str = "CC_TEST_DATABASE_URL";

    /// A throwaway database, dropped (with FORCE) when the guard is dropped.
    struct TestDb {
        name: String,
        admin: PgConnectOptions,
        pool: PgPool,
    }

    impl TestDb {
        async fn new() -> Self {
            let admin_url = std::env::var(ENV).expect("CC_TEST_DATABASE_URL must be set");
            let admin = PgConnectOptions::from_str(&admin_url).unwrap();
            let name = format!("cc_test_{}", Uuid::new_v4().simple());
            let mut conn = PgConnection::connect_with(&admin).await.unwrap();
            sqlx::query(&format!("CREATE DATABASE \"{name}\""))
                .execute(&mut conn)
                .await
                .unwrap();
            conn.close().await.unwrap();
            let pool = Self::pool_for(&admin, &name, 8).await;
            Self { name, admin, pool }
        }

        async fn pool_for(admin: &PgConnectOptions, name: &str, max: u32) -> PgPool {
            PgPoolOptions::new()
                .max_connections(max)
                .acquire_timeout(Duration::from_secs(10))
                .connect_with(admin.clone().database(name))
                .await
                .unwrap()
        }

        async fn extra_pool(&self) -> PgPool {
            Self::pool_for(&self.admin, &self.name, 4).await
        }
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            let admin = self.admin.clone();
            let name = self.name.clone();
            // Drop runs inside a runtime; use a separate thread with its own.
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
                        let _ = conn.close().await;
                    }
                });
            })
            .join();
        }
    }

    async fn migrated() -> TestDb {
        let db = TestDb::new().await;
        migrate::run(&db.pool).await.unwrap();
        db
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn email_outbox_applies_recipient_actor_and_resend_limits_durably() {
        let db = migrated().await;
        let actor = user(&db.pool, "email-actor@example.test").await;
        let now = time::OffsetDateTime::now_utc();
        let draft = |recipient: String, is_resend: bool| craftcloud_db::email_outbox::EmailDraft {
            id: Uuid::new_v4(),
            recipient,
            template: "email_verification".to_owned(),
            language: "en".to_owned(),
            params: serde_json::json!({"recipient_name": "Test"}),
            encrypted_token: None,
            expires_at: now + time::Duration::hours(24),
            actor_id: Some(actor.id),
            is_resend,
            now,
        };
        let recipient = "target@example.test".to_owned();
        for _ in 0..5 {
            let mut tx = db.pool.begin().await.unwrap();
            assert_eq!(
                craftcloud_db::email_outbox::enqueue_in(&mut tx, &draft(recipient.clone(), false))
                    .await
                    .unwrap(),
                craftcloud_db::email_outbox::EnqueueOutcome::Queued
            );
            tx.commit().await.unwrap();
        }
        let mut tx = db.pool.begin().await.unwrap();
        assert_eq!(
            craftcloud_db::email_outbox::enqueue_in(&mut tx, &draft(recipient.clone(), true))
                .await
                .unwrap(),
            craftcloud_db::email_outbox::EnqueueOutcome::ResendTooSoon
        );
        tx.rollback().await.unwrap();
        let mut tx = db.pool.begin().await.unwrap();
        assert_eq!(
            craftcloud_db::email_outbox::enqueue_in(&mut tx, &draft(recipient.clone(), false))
                .await
                .unwrap(),
            craftcloud_db::email_outbox::EnqueueOutcome::RecipientLimit
        );
        tx.rollback().await.unwrap();
        for index in 0..25 {
            let mut tx = db.pool.begin().await.unwrap();
            assert_eq!(
                craftcloud_db::email_outbox::enqueue_in(
                    &mut tx,
                    &draft(format!("actor-{index}@example.test"), false)
                )
                .await
                .unwrap(),
                craftcloud_db::email_outbox::EnqueueOutcome::Queued
            );
            tx.commit().await.unwrap();
        }
        let mut tx = db.pool.begin().await.unwrap();
        assert_eq!(
            craftcloud_db::email_outbox::enqueue_in(
                &mut tx,
                &draft("actor-limit@example.test".to_owned(), false)
            )
            .await
            .unwrap(),
            craftcloud_db::email_outbox::EnqueueOutcome::ActorLimit
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn email_outbox_lifecycle_retains_then_erases_ciphertext_and_fences_leases() {
        let db = migrated().await;
        let actor = user(&db.pool, "email-lifecycle@example.test").await;
        let now = time::OffsetDateTime::now_utc();
        let make_draft = |recipient: &str, expires_at| craftcloud_db::email_outbox::EmailDraft {
            id: Uuid::new_v4(),
            recipient: recipient.to_owned(),
            template: "test".to_owned(),
            language: "en".to_owned(),
            params: serde_json::json!({"recipient_name":"Test"}),
            encrypted_token: Some(vec![0x5a; 32]),
            expires_at,
            actor_id: Some(actor.id),
            is_resend: false,
            now,
        };
        let retry = make_draft("retry@example.test", now + time::Duration::hours(24));
        let permanent = make_draft("permanent@example.test", now + time::Duration::hours(24));
        let expiring = make_draft("expiry@example.test", now + time::Duration::hours(1));
        for draft in [&retry, &permanent, &expiring] {
            let mut tx = db.pool.begin().await.unwrap();
            assert_eq!(
                craftcloud_db::email_outbox::enqueue_in(&mut tx, draft)
                    .await
                    .unwrap(),
                craftcloud_db::email_outbox::EnqueueOutcome::Queued
            );
            tx.commit().await.unwrap();
        }

        let lease_until = now + time::Duration::seconds(30);
        let claims = craftcloud_db::email_outbox::claim_batch(&db.pool, now, lease_until, 3)
            .await
            .unwrap();
        assert_eq!(claims.len(), 3);
        let retry_claim = claims.iter().find(|claim| claim.id == retry.id).unwrap();
        let permanent_claim = claims
            .iter()
            .find(|claim| claim.id == permanent.id)
            .unwrap();
        let expiry_claim = claims.iter().find(|claim| claim.id == expiring.id).unwrap();
        assert!(retry_claim.encrypted_token.is_some());
        assert!(
            craftcloud_db::email_outbox::mark_failed(
                &db.pool,
                retry.id,
                retry_claim.lease_token,
                now,
                now + time::Duration::minutes(1),
                false,
                "smtp_timeout"
            )
            .await
            .unwrap()
        );
        let retained: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT encrypted_token FROM outbound_emails WHERE id=$1")
                .bind(retry.id)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(retained, Some(vec![0x5a; 32]));

        assert!(
            craftcloud_db::email_outbox::mark_failed(
                &db.pool,
                permanent.id,
                permanent_claim.lease_token,
                now,
                now,
                true,
                "smtp_rejected"
            )
            .await
            .unwrap()
        );
        let (status, ciphertext): (String, Option<Vec<u8>>) =
            sqlx::query_as("SELECT status,encrypted_token FROM outbound_emails WHERE id=$1")
                .bind(permanent.id)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(status, "failed");
        assert!(ciphertext.is_none());

        let expired_at = now + time::Duration::hours(1);
        assert_eq!(
            craftcloud_db::email_outbox::expire_pending(&db.pool, expired_at)
                .await
                .unwrap(),
            1
        );
        let (status, ciphertext): (String, Option<Vec<u8>>) =
            sqlx::query_as("SELECT status,encrypted_token FROM outbound_emails WHERE id=$1")
                .bind(expiring.id)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(status, "expired");
        assert!(ciphertext.is_none());
        assert!(
            !craftcloud_db::email_outbox::mark_sent(
                &db.pool,
                expiring.id,
                expiry_claim.lease_token,
                expired_at
            )
            .await
            .unwrap()
        );

        let retry_at = now + time::Duration::minutes(1);
        let retried = craftcloud_db::email_outbox::claim_batch(
            &db.pool,
            retry_at,
            retry_at + time::Duration::seconds(30),
            3,
        )
        .await
        .unwrap();
        assert_eq!(retried.len(), 1);
        let retry_claim = &retried[0];
        assert_eq!(retry_claim.id, retry.id);
        assert_eq!(retry_claim.attempts, 2);
        assert!(retry_claim.encrypted_token.is_some());
        assert!(
            craftcloud_db::email_outbox::mark_sent(
                &db.pool,
                retry.id,
                retry_claim.lease_token,
                retry_at
            )
            .await
            .unwrap()
        );
        let (status, ciphertext): (String, Option<Vec<u8>>) =
            sqlx::query_as("SELECT status,encrypted_token FROM outbound_emails WHERE id=$1")
                .bind(retry.id)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(status, "sent");
        assert!(ciphertext.is_none());
        assert!(
            !craftcloud_db::email_outbox::mark_sent(
                &db.pool,
                retry.id,
                retry_claim.lease_token,
                retry_at
            )
            .await
            .unwrap()
        );
        assert!(
            !craftcloud_db::email_outbox::mark_failed(
                &db.pool,
                retry.id,
                retry_claim.lease_token,
                retry_at,
                retry_at,
                true,
                "smtp_rejected"
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn email_capabilities_reject_changed_address_and_stale_security_proof() {
        let db = migrated().await;
        let account = user(&db.pool, "email-proof@example.test").await;
        let now = time::OffsetDateTime::now_utc();
        let verify_hash = [11_u8; 32];
        let mut tx = db.pool.begin().await.unwrap();
        assert!(
            craftcloud_db::email_verification::issue_verification_in(
                &mut tx,
                account.id,
                "email-proof@example.test",
                &verify_hash,
                now,
                now + time::Duration::hours(1)
            )
            .await
            .unwrap()
        );
        tx.commit().await.unwrap();
        sqlx::query("UPDATE users SET normalized_email='changed@example.test' WHERE id=$1")
            .bind(account.id)
            .execute(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            craftcloud_db::email_verification::confirm_verification(&db.pool, &verify_hash, now)
                .await
                .unwrap(),
            None
        );
        let verified: Option<time::OffsetDateTime> =
            sqlx::query_scalar("SELECT email_verified_at FROM users WHERE id=$1")
                .bind(account.id)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(verified.is_none());

        sqlx::query("UPDATE users SET password_hash='test-hash' WHERE id=$1")
            .bind(account.id)
            .execute(&db.pool)
            .await
            .unwrap();
        let email_session = email_change_session(&db.pool, account.id, now).await;
        let change_hash = [12_u8; 32];
        let mut tx = db.pool.begin().await.unwrap();
        assert_eq!(
            craftcloud_db::email_verification::stage_email_change_verified_in(
                &mut tx,
                account.id,
                "test-hash",
                "new-address@example.test",
                &change_hash,
                now,
                now + time::Duration::hours(1),
                None,
                email_session,
            )
            .await
            .unwrap(),
            craftcloud_db::email_verification::StageEmailChangeOutcome::Staged
        );
        tx.commit().await.unwrap();
        assert!(matches!(
            craftcloud_db::password_reset::create(
                &db.pool,
                &craftcloud_db::password_reset::NewResetToken {
                    user_id: account.id,
                    token_hash: [13_u8; 32],
                    created_by: None,
                    reset_two_factor: false,
                    now,
                    expires_at: now + time::Duration::hours(1),
                },
            )
            .await
            .unwrap(),
            craftcloud_db::password_reset::IssueOutcome::Issued { .. }
        ));
        let mut tx = db.pool.begin().await.unwrap();
        assert_eq!(
            craftcloud_db::email_verification::stage_email_change_verified_in(
                &mut tx,
                account.id,
                "test-hash",
                "another-address@example.test",
                &[14_u8; 32],
                now,
                now + time::Duration::hours(1),
                None,
                email_session,
            )
            .await
            .unwrap(),
            craftcloud_db::email_verification::StageEmailChangeOutcome::ProofInvalid
        );
        tx.rollback().await.unwrap();
        assert_eq!(
            craftcloud_db::email_verification::confirm_email_change(&db.pool, &change_hash, now)
                .await
                .unwrap(),
            craftcloud_db::email_verification::ConfirmEmailChangeOutcome::Invalid
        );
        let (address, pending): (String, bool) = sqlx::query_as(
            "SELECT normalized_email,EXISTS(SELECT 1 FROM pending_email_changes WHERE user_id=$1) FROM users WHERE id=$1"
        ).bind(account.id).fetch_one(&db.pool).await.unwrap();
        assert_eq!(address, "changed@example.test");
        assert!(!pending);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn email_verification_and_outbox_roll_back_as_one_unit() {
        let db = migrated().await;
        let account = user(&db.pool, "email-atomic@example.test").await;
        let now = time::OffsetDateTime::now_utc();
        let hash = [21_u8; 32];
        let mut tx = db.pool.begin().await.unwrap();
        assert!(
            craftcloud_db::email_verification::issue_verification_in(
                &mut tx,
                account.id,
                "email-atomic@example.test",
                &hash,
                now,
                now + time::Duration::hours(1),
            )
            .await
            .unwrap()
        );
        let draft = craftcloud_db::email_outbox::EmailDraft {
            id: Uuid::new_v4(),
            recipient: "email-atomic@example.test".to_owned(),
            template: "email_verification".to_owned(),
            language: "en".to_owned(),
            params: serde_json::json!({"recipient_name":"Test"}),
            encrypted_token: Some(vec![9; 32]),
            expires_at: now + time::Duration::hours(1),
            actor_id: Some(account.id),
            is_resend: true,
            now,
        };
        assert_eq!(
            craftcloud_db::email_outbox::enqueue_in(&mut tx, &draft)
                .await
                .unwrap(),
            craftcloud_db::email_outbox::EnqueueOutcome::Queued
        );
        tx.rollback().await.unwrap();
        let (tokens, messages): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM email_verification_tokens WHERE user_id=$1), \
             (SELECT count(*) FROM outbound_emails WHERE id=$2)",
        )
        .bind(account.id)
        .bind(draft.id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!((tokens, messages), (0, 0));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn email_change_releases_an_expired_cross_user_reservation() {
        let db = migrated().await;
        let first = user(&db.pool, "email-reservation-first@example.test").await;
        let second = user(&db.pool, "email-reservation-second@example.test").await;
        sqlx::query("UPDATE users SET password_hash='reservation-hash' WHERE id IN ($1,$2)")
            .bind(first.id)
            .bind(second.id)
            .execute(&db.pool)
            .await
            .unwrap();
        let now = time::OffsetDateTime::now_utc();
        let first_session = email_change_session(&db.pool, first.id, now).await;
        let target = "reserved-target@example.test";
        let mut tx = db.pool.begin().await.unwrap();
        assert_eq!(
            craftcloud_db::email_verification::stage_email_change_verified_in(
                &mut tx,
                first.id,
                "reservation-hash",
                target,
                &[41_u8; 32],
                now,
                now + time::Duration::seconds(1),
                None,
                first_session,
            )
            .await
            .unwrap(),
            craftcloud_db::email_verification::StageEmailChangeOutcome::Staged
        );
        tx.commit().await.unwrap();

        let later = now + time::Duration::seconds(2);
        let second_session = email_change_session(&db.pool, second.id, later).await;
        let mut tx = db.pool.begin().await.unwrap();
        assert_eq!(
            craftcloud_db::email_verification::stage_email_change_verified_in(
                &mut tx,
                second.id,
                "reservation-hash",
                target,
                &[42_u8; 32],
                later,
                later + time::Duration::hours(1),
                None,
                second_session,
            )
            .await
            .unwrap(),
            craftcloud_db::email_verification::StageEmailChangeOutcome::Staged
        );
        tx.commit().await.unwrap();
        let (first_pending, second_pending): (bool, bool) = sqlx::query_as(
            "SELECT EXISTS(SELECT 1 FROM pending_email_changes WHERE user_id=$1), \
             EXISTS(SELECT 1 FROM pending_email_changes WHERE user_id=$2 AND normalized_email=$3)",
        )
        .bind(first.id)
        .bind(second.id)
        .bind(target)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(!first_pending);
        assert!(second_pending);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn email_change_consumes_current_totp_and_rejects_factor_rotation() {
        let db = migrated().await;
        let account = user(&db.pool, "email-factor@example.test").await;
        let now = time::OffsetDateTime::now_utc();
        let secret = vec![7_u8; 32];
        sqlx::query("UPDATE users SET password_hash='factor-hash' WHERE id=$1")
            .bind(account.id)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO user_two_factor(user_id,secret_ciphertext,enabled_at,created_at) VALUES($1,$2,$3,$3)")
            .bind(account.id).bind(&secret).bind(now).execute(&db.pool).await.unwrap();
        let email_session = email_change_session(&db.pool, account.id, now).await;
        let mut tx = db.pool.begin().await.unwrap();
        assert_eq!(
            craftcloud_db::email_verification::stage_email_change_verified_in(
                &mut tx,
                account.id,
                "factor-hash",
                "factor-new@example.test",
                &[31_u8; 32],
                now,
                now + time::Duration::hours(1),
                Some((
                    &secret,
                    craftcloud_db::two_factor::VerifiedFactor::TotpStep(100)
                )),
                email_session,
            )
            .await
            .unwrap(),
            craftcloud_db::email_verification::StageEmailChangeOutcome::Staged
        );
        tx.commit().await.unwrap();
        let accepted_step: Option<i64> =
            sqlx::query_scalar("SELECT last_accepted_step FROM user_two_factor WHERE user_id=$1")
                .bind(account.id)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(accepted_step, Some(100));

        let mut tx = db.pool.begin().await.unwrap();
        assert_eq!(
            craftcloud_db::email_verification::stage_email_change_verified_in(
                &mut tx,
                account.id,
                "factor-hash",
                "factor-other@example.test",
                &[32_u8; 32],
                now,
                now + time::Duration::hours(1),
                Some((
                    &secret,
                    craftcloud_db::two_factor::VerifiedFactor::TotpStep(100)
                )),
                email_session,
            )
            .await
            .unwrap(),
            craftcloud_db::email_verification::StageEmailChangeOutcome::ProofInvalid
        );
        tx.rollback().await.unwrap();

        sqlx::query("UPDATE user_two_factor SET secret_ciphertext=$2 WHERE user_id=$1")
            .bind(account.id)
            .bind(vec![8_u8; 32])
            .execute(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            craftcloud_db::email_verification::confirm_email_change(&db.pool, &[31_u8; 32], now)
                .await
                .unwrap(),
            craftcloud_db::email_verification::ConfirmEmailChangeOutcome::Invalid
        );
        let remains: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pending_email_changes WHERE user_id=$1)",
        )
        .bind(account.id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(!remains);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn email_change_confirmation_revokes_sessions_locks_and_login_challenges() {
        let db = migrated().await;
        let account = user(&db.pool, "email-revoke@example.test").await;
        let (scope, owner) = org(&db.pool, "email-revoke").await;
        let workspace = workspaces::create(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceCreate),
            &NewWorkspace {
                slug: "email-revoke".to_owned(),
                name: "Email revoke".to_owned(),
                quota_bytes: None,
            },
        )
        .await
        .unwrap();
        let project_id = Uuid::new_v4();
        sqlx::query("INSERT INTO projects(id,org_id,workspace_id,name,name_key,created_by_user_id,created_at,updated_by_user_id,updated_at) VALUES($1,$2,$3,'Email revoke','email-revoke',$4,$5,$4,$5)")
            .bind(project_id).bind(scope.org_id()).bind(workspace.id).bind(owner.id).bind(time::OffsetDateTime::now_utc()).execute(&db.pool).await.unwrap();
        let file_id = Uuid::new_v4();
        let now = time::OffsetDateTime::now_utc();
        sqlx::query("INSERT INTO files(id,org_id,workspace_id,project_id,name,name_key,app,format,created_by_user_id,created_at,updated_by_user_id,updated_at) VALUES($1,$2,$3,$4,'File','file','figcraft','figcraft',$5,$6,$5,$6)")
            .bind(file_id).bind(scope.org_id()).bind(workspace.id).bind(project_id).bind(owner.id).bind(now).execute(&db.pool).await.unwrap();
        sqlx::query("UPDATE users SET password_hash='revoke-hash' WHERE id=$1")
            .bind(account.id)
            .execute(&db.pool)
            .await
            .unwrap();
        let session = email_change_session(&db.pool, account.id, now).await;
        sqlx::query("INSERT INTO file_locks(file_id,org_id,holder_user_id,holder_session_kind,holder_session_id,client_id,acquired_at,refreshed_at,expires_at) VALUES($1,$2,$3,'browser',$4,'browser-test',$5,$5,$6)")
            .bind(file_id).bind(scope.org_id()).bind(account.id).bind(session.id).bind(now).bind(now + time::Duration::hours(1)).execute(&db.pool).await.unwrap();
        sqlx::query("INSERT INTO two_factor_login_challenges(id_hash,user_id,password_hash_digest,login_kind,client_address,created_at,expires_at) VALUES($1,$2,$3,'browser','127.0.0.1',$4,$5)")
            .bind([51_u8; 32].as_slice()).bind(account.id).bind([52_u8; 32].as_slice()).bind(now).bind(now + time::Duration::minutes(3)).execute(&db.pool).await.unwrap();

        let token_hash = [53_u8; 32];
        let mut tx = db.pool.begin().await.unwrap();
        assert_eq!(
            craftcloud_db::email_verification::stage_email_change_verified_in(
                &mut tx,
                account.id,
                "revoke-hash",
                "email-revoked@example.test",
                &token_hash,
                now,
                now + time::Duration::hours(1),
                None,
                session,
            )
            .await
            .unwrap(),
            craftcloud_db::email_verification::StageEmailChangeOutcome::Staged
        );
        tx.commit().await.unwrap();
        assert_eq!(
            craftcloud_db::email_verification::confirm_email_change(&db.pool, &token_hash, now)
                .await
                .unwrap(),
            craftcloud_db::email_verification::ConfirmEmailChangeOutcome::Confirmed
        );
        let (revoked, locks, challenges): (bool, i64, i64) = sqlx::query_as(
            "SELECT (SELECT revoked_at IS NOT NULL FROM sessions WHERE id=$1), \
             (SELECT count(*) FROM file_locks WHERE holder_user_id=$2), \
             (SELECT count(*) FROM two_factor_login_challenges WHERE user_id=$2)",
        )
        .bind(session.id)
        .bind(account.id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(revoked);
        assert_eq!((locks, challenges), (0, 0));
    }

    async fn user(pool: &PgPool, email: &str) -> User {
        users::insert(
            pool,
            &NewUser {
                normalized_email: email.to_owned(),
                display_name: "Test User".to_owned(),
            },
        )
        .await
        .unwrap()
    }

    async fn email_change_session(
        pool: &PgPool,
        user_id: Uuid,
        now: time::OffsetDateTime,
    ) -> craftcloud_db::email_verification::EmailChangeSession {
        let id = Uuid::new_v4();
        let mut token_hash = [0_u8; 32];
        token_hash[..16].copy_from_slice(id.as_bytes());
        let expires_at = now + time::Duration::hours(2);
        sqlx::query(
            "INSERT INTO sessions(id,user_id,token_hash,created_at,last_seen_at,absolute_expires_at) \
             VALUES($1,$2,$3,$4,$4,$5)",
        )
        .bind(id)
        .bind(user_id)
        .bind(token_hash.as_slice())
        .bind(now)
        .bind(expires_at)
        .execute(pool)
        .await
        .unwrap();
        craftcloud_db::email_verification::EmailChangeSession {
            id,
            device: false,
            expires_at,
        }
    }

    /// Creates an organization owned by a fresh user who is also its owner member.
    async fn org(pool: &PgPool, slug: &str) -> (OrgScope, User) {
        let owner = user(pool, &format!("owner-{slug}@example.test")).await;
        let created = organizations::create(
            pool,
            &NewOrganization {
                slug: slug.to_owned(),
                name: format!("Org {slug}"),
                owner_user_id: owner.id,
            },
        )
        .await
        .unwrap();
        let scope = scope_for(created.id);
        organizations::add_member(
            pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            owner.id,
            OrgRole::Owner,
            MemberStatus::Active,
        )
        .await
        .unwrap();
        (scope, owner)
    }

    /// A scope can only come from a policy `Allow`; tests ask the policy as an
    /// owner of the organization.
    fn scope_for(org_id: Uuid) -> OrgScope {
        use craftcloud_core::policy::{Action, Actor, OrgMembership, Resource, can};
        let actor = Actor {
            user_id: Uuid::new_v4(),
            instance_operator: false,
            membership: Some(OrgMembership {
                org_id,
                role: OrgRole::Owner,
            }),
            workspace: None,
        };
        let permit = can(
            &actor,
            Action::OrganizationUpdate,
            &Resource::Organization { org_id },
        )
        .permit()
        .unwrap();
        OrgScope::from_permit(&permit).unwrap()
    }

    /// The scope for exactly one action (every writing repository function
    /// requires the scope of its own action).
    fn sc(scope: &OrgScope, action: craftcloud_core::policy::Action) -> OrgScope {
        use craftcloud_core::policy::{Actor, OrgMembership, Resource, can};
        let org_id = scope.org_id();
        let actor = Actor {
            user_id: Uuid::new_v4(),
            instance_operator: false,
            membership: Some(OrgMembership {
                org_id,
                role: OrgRole::Owner,
            }),
            workspace: None,
        };
        let resource = match action {
            craftcloud_core::policy::Action::MemberAdd => Resource::Member {
                org_id,
                target: None,
                role: Some(OrgRole::Member),
            },
            craftcloud_core::policy::Action::MemberChangeRole
            | craftcloud_core::policy::Action::MemberRemove => Resource::Member {
                org_id,
                target: Some(craftcloud_core::policy::MemberTarget {
                    user_id: Uuid::new_v4(),
                    role: OrgRole::Member,
                }),
                role: Some(OrgRole::Admin),
            },
            craftcloud_core::policy::Action::WorkspaceMemberAdd
            | craftcloud_core::policy::Action::WorkspaceMemberChangeRole
            | craftcloud_core::policy::Action::WorkspaceMemberRemove
            | craftcloud_core::policy::Action::WorkspaceMemberList => Resource::WorkspaceMember {
                org_id,
                workspace_id: Uuid::new_v4(),
                target: Some(craftcloud_core::policy::WorkspaceMemberTarget {
                    user_id: Uuid::new_v4(),
                    role: craftcloud_core::policy::WorkspaceRole::Viewer,
                }),
                role: Some(craftcloud_core::policy::WorkspaceRole::Viewer),
            },
            craftcloud_core::policy::Action::WorkspaceManage
            | craftcloud_core::policy::Action::WorkspaceRead => Resource::Workspace {
                org_id,
                workspace_id: Uuid::new_v4(),
            },
            craftcloud_core::policy::Action::WorkspaceCreate => Resource::Organization { org_id },
            craftcloud_core::policy::Action::GrantList
            | craftcloud_core::policy::Action::GrantCreate
            | craftcloud_core::policy::Action::GrantChange => Resource::Object {
                org_id,
                workspace_id: Uuid::new_v4(),
                target: craftcloud_core::policy::ObjectTarget::File { grant_role: None },
            },
            _ => Resource::Organization { org_id },
        };
        let permit = can(&actor, action, &resource).permit().unwrap();
        OrgScope::from_permit(&permit).unwrap()
    }

    /// A scope stemming from a read-only action.
    fn read_scope_for(org_id: Uuid) -> OrgScope {
        use craftcloud_core::policy::{Action, Actor, OrgMembership, Resource, can};
        let actor = Actor {
            user_id: Uuid::new_v4(),
            instance_operator: false,
            membership: Some(OrgMembership {
                org_id,
                role: OrgRole::Owner,
            }),
            workspace: None,
        };
        let permit = can(
            &actor,
            Action::MemberList,
            &Resource::Member {
                org_id,
                target: None,
                role: None,
            },
        )
        .permit()
        .unwrap();
        OrgScope::from_permit(&permit).unwrap()
    }

    /// Like `org`, but for databases migrated only to an older schema (before
    /// migration 0005 added `organizations.version`).
    async fn legacy_org(pool: &PgPool, slug: &str) -> (OrgScope, User) {
        let owner = user(pool, &format!("owner-{slug}@example.test")).await;
        let org_id = Uuid::new_v4();
        sqlx::query("INSERT INTO organizations (id, slug, name, owner_user_id) VALUES ($1, $2, 'Legacy', $3)")
            .bind(org_id)
            .bind(slug)
            .bind(owner.id)
            .execute(pool)
            .await
            .unwrap();
        let scope = scope_for(org_id);
        organizations::add_member(
            pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            owner.id,
            OrgRole::Owner,
            MemberStatus::Active,
        )
        .await
        .unwrap();
        (scope, owner)
    }

    fn new_ws(slug: &str) -> NewWorkspace {
        NewWorkspace {
            slug: slug.to_owned(),
            name: format!("Workspace {slug}"),
            quota_bytes: None,
        }
    }

    fn db_err(error: sqlx::Error) -> DbError {
        DbError::from(error)
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn fresh_database_migrates_and_verifies() {
        let db = TestDb::new().await;
        assert_eq!(
            migrate::verify(&db.pool).await,
            Err(DbError::Schema(SchemaProblem::NotMigrated))
        );
        let applied = migrate::run(&db.pool).await.unwrap();
        assert_eq!(
            applied.iter().map(|a| a.version).collect::<Vec<_>>(),
            vec![
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21
            ]
        );
        migrate::verify(&db.pool).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn migration_0016_grant_shape_keys_and_shared_projection_are_valid() {
        let db = migrated().await;
        let (base_scope, owner) = org(&db.pool, "ac03-shared").await;
        let workspace = workspaces::create(
            &db.pool,
            &sc(
                &base_scope,
                craftcloud_core::policy::Action::WorkspaceCreate,
            ),
            &NewWorkspace {
                slug: "shared".to_owned(),
                name: "Shared workspace".to_owned(),
                quota_bytes: None,
            },
        )
        .await
        .unwrap();
        let grantee = user(&db.pool, "ac03-grantee@example.test").await;
        organizations::add_member(
            &db.pool,
            &sc(&base_scope, craftcloud_core::policy::Action::MemberAdd),
            grantee.id,
            OrgRole::Member,
            MemberStatus::Active,
        )
        .await
        .unwrap();
        let project_id = Uuid::new_v4();
        sqlx::query("INSERT INTO projects(id,org_id,workspace_id,name,name_key,created_by_user_id,created_at,updated_by_user_id,updated_at) VALUES($1,$2,$3,'Project','project',$4,CURRENT_TIMESTAMP,$4,CURRENT_TIMESTAMP)")
            .bind(project_id).bind(base_scope.org_id()).bind(workspace.id).bind(owner.id).execute(&db.pool).await.unwrap();
        let file_id = Uuid::new_v4();
        sqlx::query("INSERT INTO files(id,org_id,workspace_id,project_id,name,name_key,app,format,created_by_user_id,created_at,updated_by_user_id,updated_at) VALUES($1,$2,$3,$4,'File','file','figcraft','figcraft',$5,CURRENT_TIMESTAMP,$5,CURRENT_TIMESTAMP)")
            .bind(file_id).bind(base_scope.org_id()).bind(workspace.id).bind(project_id).bind(owner.id).execute(&db.pool).await.unwrap();

        let invalid = sqlx::query("INSERT INTO grants(id,org_id,workspace_id,target_kind,target_id,grantee_user_id,role,created_by_user_id,created_at) VALUES($1,$2,$3,'file',$4,$5,'viewer',$6,CURRENT_TIMESTAMP)")
            .bind(Uuid::new_v4()).bind(base_scope.org_id()).bind(workspace.id).bind(file_id).bind(grantee.id).bind(owner.id).execute(&db.pool).await;
        assert!(
            matches!(invalid, Err(sqlx::Error::Database(error)) if error.constraint() == Some("grants_target_shape"))
        );

        let grant_id = Uuid::new_v4();
        sqlx::query("INSERT INTO grants(id,org_id,workspace_id,target_kind,target_id,file_id,grantee_user_id,role,created_by_user_id,created_at) VALUES($1,$2,$3,'file',$4,$4,$5,'editor',$6,CURRENT_TIMESTAMP)")
            .bind(grant_id).bind(base_scope.org_id()).bind(workspace.id).bind(file_id).bind(grantee.id).bind(owner.id).execute(&db.pool).await.unwrap();
        let rows = craftcloud_db::grants::shared_with_me_items(
            &db.pool,
            base_scope.org_id(),
            grantee.id,
            10,
            None,
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].workspace_name, "Shared workspace");
        assert_eq!(rows[0].project_id, Some(project_id));
        assert_eq!(rows[0].project_name.as_deref(), Some("Project"));
        assert_eq!(rows[0].file_id, Some(file_id));
        assert_eq!(rows[0].file_name.as_deref(), Some("File"));

        let session_owner = Uuid::new_v4();
        let session_grantee = Uuid::new_v4();
        let now = time::OffsetDateTime::now_utc();
        for (session_id, user_id, token_hash) in [
            (session_owner, owner.id, [1_u8; 32]),
            (session_grantee, grantee.id, [2_u8; 32]),
        ] {
            sqlx::query("INSERT INTO sessions(id,user_id,token_hash,created_at,last_seen_at,absolute_expires_at) VALUES($1,$2,$3,$4,$4,$5)")
                .bind(session_id).bind(user_id).bind(token_hash.as_slice()).bind(now).bind(now + time::Duration::hours(1)).execute(&db.pool).await.unwrap();
        }
        let pool = db.pool.clone();
        let org_id = base_scope.org_id();
        let concurrent_acquire = |user_id, session_id| {
            let pool = pool.clone();
            async move {
                let mut tx = craftcloud_db::members::OrgTx::begin(&pool, org_id)
                    .await
                    .unwrap()
                    .unwrap();
                let outcome = tx
                    .lock_acquire(
                        file_id,
                        craftcloud_db::locks::LockSessionContext {
                            user_id,
                            session_id,
                            expires_at: now + time::Duration::hours(1),
                            idle_timeout_secs: 3600,
                            kind: craftcloud_db::locks::LockSessionKind::Browser,
                        },
                        "test-client",
                        false,
                    )
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
                outcome
            }
        };
        let (a, b) = tokio::join!(
            concurrent_acquire(owner.id, session_owner),
            concurrent_acquire(grantee.id, session_grantee)
        );
        assert_eq!(
            (matches!(&a, craftcloud_db::locks::AcquireOutcome::Acquired { .. }) as usize)
                + (matches!(&b, craftcloud_db::locks::AcquireOutcome::Acquired { .. }) as usize),
            1
        );
        assert_eq!(
            (matches!(&a, craftcloud_db::locks::AcquireOutcome::Locked { .. }) as usize)
                + (matches!(&b, craftcloud_db::locks::AcquireOutcome::Locked { .. }) as usize),
            1
        );

        // A project operation must find a foreign lease even if the current session's file sorts first.
        let second_file_id = Uuid::new_v4();
        sqlx::query("INSERT INTO files(id,org_id,workspace_id,project_id,name,name_key,app,format,created_by_user_id,created_at,updated_by_user_id,updated_at) VALUES($1,$2,$3,$4,'Second file','second-file','photocraft','native',$5,CURRENT_TIMESTAMP,$5,CURRENT_TIMESTAMP)")
            .bind(second_file_id).bind(base_scope.org_id()).bind(workspace.id).bind(project_id).bind(owner.id).execute(&db.pool).await.unwrap();
        sqlx::query("DELETE FROM file_locks WHERE org_id=$1 AND file_id IN ($2,$3)")
            .bind(base_scope.org_id())
            .bind(file_id)
            .bind(second_file_id)
            .execute(&db.pool)
            .await
            .unwrap();
        for (target_file, user_id, session_id) in [
            (file_id, owner.id, session_owner),
            (second_file_id, grantee.id, session_grantee),
        ] {
            let mut tx = craftcloud_db::members::OrgTx::begin(&db.pool, base_scope.org_id())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                tx.lock_acquire(
                    target_file,
                    craftcloud_db::locks::LockSessionContext {
                        user_id,
                        session_id,
                        expires_at: now + time::Duration::hours(1),
                        idle_timeout_secs: 3600,
                        kind: craftcloud_db::locks::LockSessionKind::Browser,
                    },
                    "project-order",
                    false,
                )
                .await
                .unwrap(),
                craftcloud_db::locks::AcquireOutcome::Acquired { .. }
            ));
            tx.commit().await.unwrap();
        }
        let mut tx = craftcloud_db::members::OrgTx::begin(&db.pool, base_scope.org_id())
            .await
            .unwrap()
            .unwrap();
        let foreign = tx
            .active_project_lock(project_id, session_owner)
            .await
            .unwrap();
        assert_eq!(foreign.map(|lock| lock.file_id), Some(second_file_id));
        tx.commit().await.unwrap();

        // Grant demotion and revoke clear a grantee's active lease in their own transaction.
        sqlx::query("DELETE FROM file_locks WHERE org_id=$1 AND file_id=$2")
            .bind(base_scope.org_id())
            .bind(file_id)
            .execute(&db.pool)
            .await
            .unwrap();
        let mut lease_tx = craftcloud_db::members::OrgTx::begin(&db.pool, base_scope.org_id())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            lease_tx
                .lock_acquire(
                    file_id,
                    craftcloud_db::locks::LockSessionContext {
                        user_id: grantee.id,
                        session_id: session_grantee,
                        expires_at: now + time::Duration::hours(1),
                        idle_timeout_secs: 3600,
                        kind: craftcloud_db::locks::LockSessionKind::Browser,
                    },
                    "demotion-test",
                    false,
                )
                .await
                .unwrap(),
            craftcloud_db::locks::AcquireOutcome::Acquired { .. }
        ));
        lease_tx.commit().await.unwrap();
        let mut demotion_tx = craftcloud_db::members::OrgTx::begin(&db.pool, base_scope.org_id())
            .await
            .unwrap()
            .unwrap();
        let outcome = demotion_tx
            .grant_change(
                &sc(&base_scope, craftcloud_core::policy::Action::GrantChange),
                grant_id,
                craftcloud_db::grants::GrantRole::Viewer,
                1,
            )
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            craftcloud_db::grants::ChangeOutcome::Changed(_)
        ));
        demotion_tx.commit().await.unwrap();
        let still_locked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM file_locks WHERE org_id=$1 AND file_id=$2 AND holder_user_id=$3)")
            .bind(base_scope.org_id()).bind(file_id).bind(grantee.id).fetch_one(&db.pool).await.unwrap();
        assert!(
            !still_locked,
            "editor-to-viewer demotion clears the grant lease atomically"
        );

        let mut lease_tx = craftcloud_db::members::OrgTx::begin(&db.pool, base_scope.org_id())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            lease_tx
                .lock_acquire(
                    file_id,
                    craftcloud_db::locks::LockSessionContext {
                        user_id: grantee.id,
                        session_id: session_grantee,
                        expires_at: now + time::Duration::hours(1),
                        idle_timeout_secs: 3600,
                        kind: craftcloud_db::locks::LockSessionKind::Browser,
                    },
                    "revoke-test",
                    false,
                )
                .await
                .unwrap(),
            craftcloud_db::locks::AcquireOutcome::Acquired { .. }
        ));
        lease_tx.commit().await.unwrap();
        let mut revoke_tx = craftcloud_db::members::OrgTx::begin(&db.pool, base_scope.org_id())
            .await
            .unwrap()
            .unwrap();
        assert!(
            revoke_tx
                .grant_revoke(
                    &sc(&base_scope, craftcloud_core::policy::Action::GrantChange),
                    grant_id,
                    now
                )
                .await
                .unwrap()
                .is_some()
        );
        revoke_tx.commit().await.unwrap();
        let still_locked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM file_locks WHERE org_id=$1 AND file_id=$2 AND holder_user_id=$3)")
            .bind(base_scope.org_id()).bind(file_id).bind(grantee.id).fetch_one(&db.pool).await.unwrap();
        assert!(
            !still_locked,
            "grant revocation clears the grant lease atomically"
        );

        let project_grant_id = Uuid::new_v4();
        sqlx::query("INSERT INTO grants(id,org_id,workspace_id,target_kind,target_id,project_id,grantee_user_id,role,created_by_user_id,created_at) VALUES($1,$2,$3,'project',$4,$4,$5,'editor',$6,CURRENT_TIMESTAMP)")
            .bind(project_grant_id).bind(base_scope.org_id()).bind(workspace.id).bind(project_id).bind(grantee.id).bind(owner.id).execute(&db.pool).await.unwrap();
        let mut lease_tx = craftcloud_db::members::OrgTx::begin(&db.pool, base_scope.org_id())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            lease_tx
                .lock_acquire(
                    file_id,
                    craftcloud_db::locks::LockSessionContext {
                        user_id: grantee.id,
                        session_id: session_grantee,
                        expires_at: now + time::Duration::hours(1),
                        idle_timeout_secs: 3600,
                        kind: craftcloud_db::locks::LockSessionKind::Browser,
                    },
                    "project-demotion",
                    false,
                )
                .await
                .unwrap(),
            craftcloud_db::locks::AcquireOutcome::Acquired { .. }
        ));
        lease_tx.commit().await.unwrap();
        let mut demotion_tx = craftcloud_db::members::OrgTx::begin(&db.pool, base_scope.org_id())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            demotion_tx
                .grant_change(
                    &sc(&base_scope, craftcloud_core::policy::Action::GrantChange),
                    project_grant_id,
                    craftcloud_db::grants::GrantRole::Viewer,
                    1
                )
                .await
                .unwrap(),
            craftcloud_db::grants::ChangeOutcome::Changed(_)
        ));
        demotion_tx.commit().await.unwrap();
        let still_locked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM file_locks WHERE org_id=$1 AND file_id=$2 AND holder_user_id=$3)").bind(base_scope.org_id()).bind(file_id).bind(grantee.id).fetch_one(&db.pool).await.unwrap();
        assert!(
            !still_locked,
            "project grant demotion clears descendant leases"
        );
    }

    /// Org with an owner, a member (grantee), a workspace, a project and a
    /// non-figcraft file.
    struct LeaseFixture {
        scope: OrgScope,
        owner: User,
        user: User,
        workspace_id: Uuid,
        project_id: Uuid,
        file_id: Uuid,
    }

    async fn lease_fixture(db: &TestDb, slug: &str) -> LeaseFixture {
        let (scope, owner) = org(&db.pool, slug).await;
        let workspace = workspaces::create(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceCreate),
            &NewWorkspace {
                slug: "ws".to_owned(),
                name: "Workspace".to_owned(),
                quota_bytes: None,
            },
        )
        .await
        .unwrap();
        let user = user(&db.pool, &format!("member-{slug}@example.test")).await;
        organizations::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            user.id,
            OrgRole::Member,
            MemberStatus::Active,
        )
        .await
        .unwrap();
        let project_id = Uuid::new_v4();
        sqlx::query("INSERT INTO projects(id,org_id,workspace_id,name,name_key,created_by_user_id,created_at,updated_by_user_id,updated_at) VALUES($1,$2,$3,'Project','project',$4,CURRENT_TIMESTAMP,$4,CURRENT_TIMESTAMP)")
            .bind(project_id).bind(scope.org_id()).bind(workspace.id).bind(owner.id).execute(&db.pool).await.unwrap();
        let file_id =
            insert_file(&db.pool, &scope, workspace.id, project_id, owner.id, "file").await;
        LeaseFixture {
            scope,
            owner,
            user,
            workspace_id: workspace.id,
            project_id,
            file_id,
        }
    }

    async fn insert_file(
        pool: &PgPool,
        scope: &OrgScope,
        workspace_id: Uuid,
        project_id: Uuid,
        user_id: Uuid,
        name: &str,
    ) -> Uuid {
        let file_id = Uuid::new_v4();
        sqlx::query("INSERT INTO files(id,org_id,workspace_id,project_id,name,name_key,app,format,created_by_user_id,created_at,updated_by_user_id,updated_at) VALUES($1,$2,$3,$4,$5,$5,'photocraft','pcf',$6,CURRENT_TIMESTAMP,$6,CURRENT_TIMESTAMP)")
            .bind(file_id).bind(scope.org_id()).bind(workspace_id).bind(project_id).bind(name).bind(user_id).execute(pool).await.unwrap();
        file_id
    }

    async fn insert_browser_session(pool: &PgPool, user_id: Uuid, marker: u8) -> Uuid {
        let id = Uuid::new_v4();
        let now = time::OffsetDateTime::now_utc();
        sqlx::query("INSERT INTO sessions(id,user_id,token_hash,created_at,last_seen_at,absolute_expires_at) VALUES($1,$2,$3,$4,$4,$5)")
            .bind(id).bind(user_id).bind([marker; 32].as_slice()).bind(now).bind(now + time::Duration::hours(1)).execute(pool).await.unwrap();
        id
    }

    /// A device session created `age` ago whose access token expires `access_ttl` from now.
    async fn insert_device_session(
        pool: &PgPool,
        user_id: Uuid,
        marker: u8,
        access_ttl: time::Duration,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let now = time::OffsetDateTime::now_utc();
        sqlx::query("INSERT INTO device_sessions(id,user_id,name,client_kind,client_id,refresh_hash,refresh_family,access_hash,access_expires_at,created_at,last_used_at,idle_expires_at,absolute_expires_at) VALUES($1,$2,'Desktop','app','app-1',$3,$4,$5,$6,$7,$7,$8,$9)")
            .bind(id).bind(user_id).bind([marker; 32].as_slice()).bind(Uuid::new_v4())
            .bind([marker.wrapping_add(100); 32].as_slice()).bind(now + access_ttl).bind(now - time::Duration::hours(3))
            .bind(now + time::Duration::days(1)).bind(now + time::Duration::days(30))
            .execute(pool).await.unwrap();
        id
    }

    fn lease_context(
        user_id: Uuid,
        session_id: Uuid,
        kind: craftcloud_db::locks::LockSessionKind,
    ) -> craftcloud_db::locks::LockSessionContext {
        craftcloud_db::locks::LockSessionContext {
            user_id,
            session_id,
            expires_at: time::OffsetDateTime::now_utc() + time::Duration::hours(1),
            idle_timeout_secs: 3600,
            kind,
        }
    }

    async fn acquire_lease(
        pool: &PgPool,
        org_id: Uuid,
        file_id: Uuid,
        context: craftcloud_db::locks::LockSessionContext,
    ) -> craftcloud_db::locks::AcquireOutcome {
        let mut tx = craftcloud_db::members::OrgTx::begin(pool, org_id)
            .await
            .unwrap()
            .unwrap();
        let outcome = tx
            .lock_acquire(file_id, context, "test", false)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        outcome
    }

    async fn lease_count(pool: &PgPool, file_id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM file_locks WHERE file_id=$1")
            .bind(file_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn device_session_holds_a_lease_that_a_browser_session_of_the_same_user_does_not_share() {
        use craftcloud_db::locks::{AcquireOutcome, LockSessionKind};
        let db = migrated().await;
        let f = lease_fixture(&db, "dev-lease").await;
        let org_id = f.scope.org_id();
        let device =
            insert_device_session(&db.pool, f.user.id, 11, time::Duration::minutes(15)).await;
        let browser = insert_browser_session(&db.pool, f.user.id, 12).await;

        let acquired = acquire_lease(
            &db.pool,
            org_id,
            f.file_id,
            lease_context(f.user.id, device, LockSessionKind::Device),
        )
        .await;
        let AcquireOutcome::Acquired {
            current, created, ..
        } = acquired
        else {
            panic!("a valid device session acquires a lease");
        };
        assert!(created);
        assert_eq!(current.holder_session_kind, "device");
        assert_eq!(current.holder_session_id, device);

        // The browser session of the same user is not the holder.
        let browser_attempt = acquire_lease(
            &db.pool,
            org_id,
            f.file_id,
            lease_context(f.user.id, browser, LockSessionKind::Browser),
        )
        .await;
        assert!(matches!(
            browser_attempt,
            AcquireOutcome::Locked { same_user: true, ref current } if current.holder_session_id == device
        ));
        let mut tx = craftcloud_db::members::OrgTx::begin(&db.pool, org_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            tx.active_project_lock(f.project_id, browser)
                .await
                .unwrap()
                .is_some(),
            "the device lease blocks writes from the browser session"
        );
        assert!(
            tx.active_project_lock(f.project_id, device)
                .await
                .unwrap()
                .is_none(),
            "the holding device session is not blocked by its own lease"
        );
        tx.commit().await.unwrap();

        // A browser session id presented as a device session is not valid.
        let wrong_kind = acquire_lease(
            &db.pool,
            org_id,
            f.file_id,
            lease_context(f.user.id, browser, LockSessionKind::Device),
        )
        .await;
        assert!(matches!(wrong_kind, AcquireOutcome::SessionExpired));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn revoking_a_session_removes_its_leases_in_the_same_transaction() {
        use craftcloud_db::audit::AuditCtx;
        use craftcloud_db::locks::{AcquireOutcome, LockSessionKind};
        let db = migrated().await;
        let f = lease_fixture(&db, "revoke-lease").await;
        let org_id = f.scope.org_id();
        let user_id = f.user.id;
        let now = time::OffsetDateTime::now_utc();
        let file_id = f.file_id;
        let second_file = insert_file(
            &db.pool,
            &f.scope,
            f.workspace_id,
            f.project_id,
            f.owner.id,
            "second",
        )
        .await;
        let hold = |file: Uuid, session: Uuid, kind: LockSessionKind| {
            let pool = db.pool.clone();
            async move {
                assert!(matches!(
                    acquire_lease(&pool, org_id, file, lease_context(user_id, session, kind)).await,
                    AcquireOutcome::Acquired { .. }
                ));
            }
        };

        // Device logout.
        let device =
            insert_device_session(&db.pool, user_id, 21, time::Duration::minutes(15)).await;
        hold(file_id, device, LockSessionKind::Device).await;
        assert_eq!(lease_count(&db.pool, file_id).await, 1);
        assert!(
            craftcloud_db::device_sessions::revoke_current(
                &db.pool,
                device,
                user_id,
                now,
                &AuditCtx::system()
            )
            .await
            .unwrap()
        );
        assert_eq!(
            lease_count(&db.pool, file_id).await,
            0,
            "device logout removes the lease"
        );
        assert!(
            matches!(
                acquire_lease(
                    &db.pool,
                    org_id,
                    file_id,
                    lease_context(user_id, device, LockSessionKind::Device)
                )
                .await,
                AcquireOutcome::SessionExpired
            ),
            "a revoked device session cannot acquire"
        );

        // Device revoke from the account page keeps other sessions' leases.
        let device =
            insert_device_session(&db.pool, user_id, 22, time::Duration::minutes(15)).await;
        let browser = insert_browser_session(&db.pool, user_id, 23).await;
        hold(file_id, device, LockSessionKind::Device).await;
        hold(second_file, browser, LockSessionKind::Browser).await;
        assert!(
            craftcloud_db::device_sessions::revoke_owned(
                &db.pool,
                device,
                user_id,
                now,
                "user_revoked",
                &AuditCtx::system()
            )
            .await
            .unwrap()
        );
        assert_eq!(lease_count(&db.pool, file_id).await, 0);
        assert_eq!(lease_count(&db.pool, second_file).await, 1);
        assert!(sessions::revoke(&db.pool, browser, now).await.unwrap());
        assert_eq!(
            lease_count(&db.pool, second_file).await,
            0,
            "browser logout removes its lease"
        );

        // Logout-all and account disable clear leases of both kinds.
        for step in 0..2_u8 {
            let device =
                insert_device_session(&db.pool, user_id, 30 + step, time::Duration::minutes(15))
                    .await;
            let browser = insert_browser_session(&db.pool, user_id, 40 + step).await;
            hold(file_id, device, LockSessionKind::Device).await;
            hold(second_file, browser, LockSessionKind::Browser).await;
            if step == 0 {
                sessions::revoke_all_for_user_audited(&db.pool, user_id, now, &AuditCtx::system())
                    .await
                    .unwrap();
            } else {
                craftcloud_db::accounts::set_disabled(&db.pool, user_id, true, now)
                    .await
                    .unwrap();
            }
            assert_eq!(
                lease_count(&db.pool, file_id).await,
                0,
                "step {step}: device"
            );
            assert_eq!(
                lease_count(&db.pool, second_file).await,
                0,
                "step {step}: browser"
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn refresh_reuse_revokes_the_family_and_its_leases() {
        use craftcloud_db::audit::AuditCtx;
        use craftcloud_db::device_sessions::{self, RefreshOutcome};
        use craftcloud_db::locks::{AcquireOutcome, LockSessionKind};
        let db = migrated().await;
        let f = lease_fixture(&db, "reuse-lease").await;
        let device =
            insert_device_session(&db.pool, f.user.id, 51, time::Duration::minutes(15)).await;
        assert!(matches!(
            acquire_lease(
                &db.pool,
                f.scope.org_id(),
                f.file_id,
                lease_context(f.user.id, device, LockSessionKind::Device)
            )
            .await,
            AcquireOutcome::Acquired { .. }
        ));
        let now = time::OffsetDateTime::now_utc();
        let rotate = |old: u8, new: u8| {
            let pool = db.pool.clone();
            async move {
                device_sessions::refresh(
                    &pool,
                    &[old; 32],
                    &[new; 32],
                    &[new.wrapping_add(1); 32],
                    now,
                    Duration::from_secs(900),
                    Duration::from_secs(86_400),
                    &AuditCtx::system(),
                )
                .await
                .unwrap()
                .0
            }
        };
        assert!(matches!(rotate(51, 70).await, RefreshOutcome::Rotated));
        assert_eq!(lease_count(&db.pool, f.file_id).await, 1);
        assert!(matches!(rotate(51, 72).await, RefreshOutcome::Reused));
        assert_eq!(
            lease_count(&db.pool, f.file_id).await,
            0,
            "replay revokes the family and removes its lease"
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn upload_completion_check_ignores_access_token_and_idle_expiry_of_a_device_session() {
        let db = migrated().await;
        let f = lease_fixture(&db, "dev-valid").await;
        let now = time::OffsetDateTime::now_utc();
        // The access token expired a minute ago and the idle window ended: a long
        // upload that began with a valid token must still be able to finish.
        let device =
            insert_device_session(&db.pool, f.user.id, 61, -time::Duration::minutes(1)).await;
        sqlx::query("UPDATE device_sessions SET idle_expires_at=$2 WHERE id=$1")
            .bind(device)
            .bind(now - time::Duration::seconds(5))
            .execute(&db.pool)
            .await
            .unwrap();
        let check = |session: Uuid| {
            let pool = db.pool.clone();
            let org_id = f.scope.org_id();
            let user_id = f.user.id;
            async move {
                let mut tx = craftcloud_db::members::OrgTx::begin(&pool, org_id)
                    .await
                    .unwrap()
                    .unwrap();
                let valid = tx
                    .session_valid(session, user_id, time::OffsetDateTime::now_utc(), true)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
                valid
            }
        };
        assert!(check(device).await);
        sqlx::query("UPDATE device_sessions SET revoked_at=$2,revoke_reason='logout' WHERE id=$1")
            .bind(device)
            .bind(now)
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(!check(device).await, "a revoked device session is invalid");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn concurrent_member_removal_from_two_organizations_never_deadlocks() {
        use craftcloud_db::audit::AuditCtx;
        let db = migrated().await;
        let (scope_a, _owner_a) = org(&db.pool, "rm-a").await;
        let (scope_b, _owner_b) = org(&db.pool, "rm-b").await;
        for round in 0..15 {
            let shared = user(&db.pool, &format!("shared-{round}@example.test")).await;
            for scope in [&scope_a, &scope_b] {
                organizations::add_member(
                    &db.pool,
                    &sc(scope, craftcloud_core::policy::Action::MemberAdd),
                    shared.id,
                    OrgRole::Member,
                    MemberStatus::Active,
                )
                .await
                .unwrap();
            }
            let remove = |scope: &OrgScope| {
                let pool = db.pool.clone();
                let scope = sc(scope, craftcloud_core::policy::Action::MemberRemove);
                let user_id = shared.id;
                async move {
                    let mut tx = craftcloud_db::members::OrgTx::begin(&pool, scope.org_id())
                        .await
                        .unwrap()
                        .unwrap();
                    let outcome = tx
                        .remove_audited(&scope, user_id, &AuditCtx::system())
                        .await;
                    if outcome.is_ok() {
                        tx.commit().await.unwrap();
                    }
                    outcome
                }
            };
            let (a, b) = tokio::join!(remove(&scope_a), remove(&scope_b));
            a.unwrap_or_else(|error| panic!("round {round}: org A removal failed: {error:?}"));
            b.unwrap_or_else(|error| panic!("round {round}: org B removal failed: {error:?}"));
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn trashed_targets_reject_grants_and_leases_and_revoke_reports_the_change() {
        use craftcloud_core::policy::Action;
        use craftcloud_db::grants::{CreateOutcome, GrantRole, NewGrant, Target, TargetKind};
        use craftcloud_db::locks::{AcquireOutcome, LockSessionKind};
        let db = migrated().await;
        let f = lease_fixture(&db, "trashed").await;
        let org_id = f.scope.org_id();
        let session = insert_browser_session(&db.pool, f.user.id, 81).await;
        let now = time::OffsetDateTime::now_utc();
        let new_grant = |kind: TargetKind, id: Uuid| NewGrant {
            target: Target {
                org_id,
                workspace_id: f.workspace_id,
                kind,
                id,
            },
            grantee_user_id: f.user.id,
            role: GrantRole::Editor,
            created_by: f.owner.id,
            now,
        };
        let create = |grant: NewGrant| {
            let pool = db.pool.clone();
            let scope = sc(&f.scope, Action::GrantCreate);
            async move {
                let mut tx = craftcloud_db::members::OrgTx::begin(&pool, org_id)
                    .await
                    .unwrap()
                    .unwrap();
                let outcome = tx.grant_create(&scope, grant).await.unwrap();
                tx.commit().await.unwrap();
                outcome
            }
        };

        sqlx::query(
            "UPDATE files SET deleted_at=CURRENT_TIMESTAMP, deleted_by_user_id=$2 WHERE id=$1",
        )
        .bind(f.file_id)
        .bind(f.owner.id)
        .execute(&db.pool)
        .await
        .unwrap();
        assert!(matches!(
            create(new_grant(TargetKind::File, f.file_id)).await,
            CreateOutcome::FileDeleted
        ));
        assert!(matches!(
            acquire_lease(
                &db.pool,
                org_id,
                f.file_id,
                lease_context(f.user.id, session, LockSessionKind::Browser)
            )
            .await,
            AcquireOutcome::FileDeleted
        ));
        sqlx::query("UPDATE files SET deleted_at=NULL, deleted_by_user_id=NULL WHERE id=$1")
            .bind(f.file_id)
            .execute(&db.pool)
            .await
            .unwrap();

        sqlx::query(
            "UPDATE projects SET deleted_at=CURRENT_TIMESTAMP, deleted_by_user_id=$2 WHERE id=$1",
        )
        .bind(f.project_id)
        .bind(f.owner.id)
        .execute(&db.pool)
        .await
        .unwrap();
        assert!(matches!(
            create(new_grant(TargetKind::Project, f.project_id)).await,
            CreateOutcome::ProjectDeleted
        ));
        assert!(matches!(
            create(new_grant(TargetKind::File, f.file_id)).await,
            CreateOutcome::ProjectDeleted
        ));
        assert!(matches!(
            acquire_lease(
                &db.pool,
                org_id,
                f.file_id,
                lease_context(f.user.id, session, LockSessionKind::Browser)
            )
            .await,
            AcquireOutcome::ProjectDeleted
        ));
        sqlx::query("UPDATE projects SET deleted_at=NULL, deleted_by_user_id=NULL WHERE id=$1")
            .bind(f.project_id)
            .execute(&db.pool)
            .await
            .unwrap();

        let CreateOutcome::Created(grant) = create(new_grant(TargetKind::File, f.file_id)).await
        else {
            panic!("a live target accepts a grant");
        };
        let revoke = || {
            let pool = db.pool.clone();
            let scope = sc(&f.scope, Action::GrantChange);
            let id = grant.id;
            async move {
                let mut tx = craftcloud_db::members::OrgTx::begin(&pool, org_id)
                    .await
                    .unwrap()
                    .unwrap();
                let result = tx.grant_revoke(&scope, id, now).await.unwrap();
                tx.commit().await.unwrap();
                result.map(|(_, changed)| changed)
            }
        };
        assert_eq!(revoke().await, Some(true), "first revoke changes a row");
        assert_eq!(revoke().await, Some(false), "second revoke changes nothing");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn migrating_twice_is_idempotent() {
        let db = TestDb::new().await;
        migrate::run(&db.pool).await.unwrap();
        assert!(migrate::run(&db.pool).await.unwrap().is_empty());
        migrate::verify(&db.pool).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn concurrent_migrators_apply_once() {
        let db = TestDb::new().await;
        let other = db.extra_pool().await;
        let (a, b) = tokio::join!(migrate::run(&db.pool), migrate::run(&other));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(
            a.len() + b.len(),
            21,
            "each migration applied exactly once in total"
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count, 21);
        migrate::verify(&db.pool).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn upgrade_keeps_existing_data() {
        let db = TestDb::new().await;
        let first = migrate::run_with(&db.pool, &migrator_up_to(1))
            .await
            .unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(
            migrate::verify(&db.pool).await,
            Err(DbError::Schema(SchemaProblem::Pending(vec![
                2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21
            ])))
        );

        let (scope, owner) = legacy_org(&db.pool, "acme").await;

        let second = migrate::run(&db.pool).await.unwrap();
        assert_eq!(
            second.iter().map(|a| a.version).collect::<Vec<_>>(),
            vec![
                2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21
            ]
        );
        migrate::verify(&db.pool).await.unwrap();

        let row = sqlx::query(
            "SELECT u.normalized_email, o.slug, m.role FROM users u \
             JOIN organizations o ON o.owner_user_id = u.id \
             JOIN organization_members m ON m.org_id = o.id AND m.user_id = u.id \
             WHERE o.id = $1",
        )
        .bind(scope.org_id())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            row.get::<String, _>("normalized_email"),
            owner.normalized_email
        );
        assert_eq!(row.get::<String, _>("slug"), "acme");
        assert_eq!(row.get::<String, _>("role"), "owner");
        // New tables are usable after the upgrade.
        workspaces::create(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("main"),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn tampered_checksum_is_detected() {
        let db = migrated().await;
        sqlx::query("UPDATE _sqlx_migrations SET checksum = '\\x00'::bytea WHERE version = 1")
            .execute(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            migrate::verify(&db.pool).await,
            Err(DbError::Schema(SchemaProblem::ChecksumMismatch(1)))
        );
        assert!(matches!(
            migrate::run(&db.pool).await,
            Err(DbError::Migration(_))
        ));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn unknown_and_failed_migrations_are_detected() {
        let db = migrated().await;
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES (99, 'future', true, '\\x00'::bytea, 0)",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            migrate::verify(&db.pool).await,
            Err(DbError::Schema(SchemaProblem::UnknownApplied(99)))
        );
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 99")
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE _sqlx_migrations SET success = false WHERE version = 2")
            .execute(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            migrate::verify(&db.pool).await,
            Err(DbError::Schema(SchemaProblem::Dirty(2)))
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn workspace_member_requires_org_membership() {
        let db = migrated().await;
        let (scope, _) = org(&db.pool, "acme").await;
        let ws = workspaces::create(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("main"),
        )
        .await
        .unwrap();
        let outsider = user(&db.pool, "outsider@example.test").await;
        let err = workspaces::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceMemberAdd),
            ws.id,
            outsider.id,
            WorkspaceRole::Viewer,
        )
        .await
        .unwrap_err();
        assert_eq!(err.constraint(), Some("workspace_members_org_member_fk"));
        assert!(matches!(err, DbError::ForeignKeyViolation { .. }));

        // Becomes possible once the user is an organization member.
        organizations::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            outsider.id,
            OrgRole::Member,
            MemberStatus::Active,
        )
        .await
        .unwrap();
        workspaces::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceMemberAdd),
            ws.id,
            outsider.id,
            WorkspaceRole::Viewer,
        )
        .await
        .unwrap();
        let again = workspaces::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceMemberAdd),
            ws.id,
            outsider.id,
            WorkspaceRole::Editor,
        )
        .await
        .unwrap_err();
        assert!(matches!(again, DbError::UniqueViolation { .. }));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn workspace_member_cannot_cross_organizations() {
        let db = migrated().await;
        let (scope_a, _) = org(&db.pool, "org-a").await;
        let (scope_b, owner_b) = org(&db.pool, "org-b").await;
        let ws_a = workspaces::create(
            &db.pool,
            &sc(&scope_a, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("main"),
        )
        .await
        .unwrap();
        // owner_b is an org-b member, but ws_a belongs to org-a.
        let err = workspaces::add_member(
            &db.pool,
            &sc(
                &scope_b,
                craftcloud_core::policy::Action::WorkspaceMemberAdd,
            ),
            ws_a.id,
            owner_b.id,
            WorkspaceRole::Admin,
        )
        .await
        .unwrap_err();
        assert_eq!(err.constraint(), Some("workspace_members_workspace_fk"));
        // Raw attempt to attach an org-a workspace row to org-b is rejected too.
        let raw = sqlx::query(
            "INSERT INTO workspace_members (org_id, workspace_id, user_id, role) VALUES ($1, $2, $3, 'admin')",
        )
        .bind(scope_a.org_id())
        .bind(ws_a.id)
        .bind(owner_b.id)
        .execute(&db.pool)
        .await
        .unwrap_err();
        assert!(matches!(db_err(raw), DbError::ForeignKeyViolation { .. }));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn slug_is_unique_per_organization_only() {
        let db = migrated().await;
        let (scope_a, _) = org(&db.pool, "org-a").await;
        let (scope_b, _) = org(&db.pool, "org-b").await;
        workspaces::create(
            &db.pool,
            &sc(&scope_a, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("main"),
        )
        .await
        .unwrap();
        let dup = workspaces::create(
            &db.pool,
            &sc(&scope_a, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("main"),
        )
        .await
        .unwrap_err();
        assert!(matches!(dup, DbError::UniqueViolation { .. }));
        assert_eq!(dup.constraint(), Some("workspaces_org_slug_unique"));
        workspaces::create(
            &db.pool,
            &sc(&scope_b, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("main"),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn check_constraints_reject_bad_values() {
        let db = migrated().await;
        let bad_email = users::insert(
            &db.pool,
            &NewUser {
                normalized_email: "Upper@Example.test".to_owned(),
                display_name: "X".to_owned(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(bad_email.constraint(), Some("users_email_normalized"));

        let bad_name = users::insert(
            &db.pool,
            &NewUser {
                normalized_email: "ok@example.test".to_owned(),
                display_name: String::new(),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(bad_name, DbError::CheckViolation { .. }));

        user(&db.pool, "dup@example.test").await;
        let dup = users::insert(
            &db.pool,
            &NewUser {
                normalized_email: "dup@example.test".to_owned(),
                display_name: "Again".to_owned(),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(dup, DbError::UniqueViolation { .. }));

        let (scope, _) = org(&db.pool, "acme").await;
        let negative = workspaces::create(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceCreate),
            &NewWorkspace {
                quota_bytes: Some(-1),
                ..new_ws("neg")
            },
        )
        .await
        .unwrap_err();
        assert_eq!(negative.constraint(), Some("workspaces_quota_non_negative"));
        workspaces::create(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceCreate),
            &NewWorkspace {
                quota_bytes: Some(0),
                ..new_ws("zero")
            },
        )
        .await
        .unwrap();

        for bad_slug in ["Upper", "-lead", "has space", "", &"a".repeat(64)] {
            let err = workspaces::create(
                &db.pool,
                &sc(&scope, craftcloud_core::policy::Action::WorkspaceCreate),
                &new_ws(bad_slug),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(err, DbError::CheckViolation { .. }),
                "{bad_slug:?}"
            );
        }
        let bad_role = sqlx::query(
            "INSERT INTO organization_members (org_id, user_id, role, status) VALUES ($1, $2, 'god', 'active')",
        )
        .bind(scope.org_id())
        .bind(Uuid::new_v4())
        .execute(&db.pool)
        .await
        .unwrap_err();
        assert!(matches!(
            db_err(bad_role),
            DbError::CheckViolation { .. } | DbError::ForeignKeyViolation { .. }
        ));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn queries_never_cross_organizations() {
        let db = migrated().await;
        let (scope_a, _) = org(&db.pool, "org-a").await;
        let (scope_b, _) = org(&db.pool, "org-b").await;
        let ws_a = workspaces::create(
            &db.pool,
            &sc(&scope_a, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("a-one"),
        )
        .await
        .unwrap();
        let ws_b = workspaces::create(
            &db.pool,
            &sc(&scope_b, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("b-one"),
        )
        .await
        .unwrap();

        assert_eq!(
            workspaces::get(&db.pool, &scope_a, ws_a.id)
                .await
                .unwrap()
                .map(|w| w.id),
            Some(ws_a.id)
        );
        // Another organization's id looks exactly like a missing one.
        assert!(
            workspaces::get(&db.pool, &scope_a, ws_b.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            workspaces::get(&db.pool, &scope_a, Uuid::new_v4())
                .await
                .unwrap()
                .is_none()
        );

        let listed = workspaces::list(&db.pool, &scope_a, 50).await.unwrap();
        assert_eq!(
            listed.iter().map(|w| w.id).collect::<Vec<_>>(),
            vec![ws_a.id]
        );
        assert!(listed.iter().all(|w| w.org_id == scope_a.org_id()));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn list_enforces_hard_cap() {
        let db = migrated().await;
        let (scope, _) = org(&db.pool, "acme").await;
        sqlx::query(
            "INSERT INTO workspaces (id, org_id, slug, name) \
             SELECT gen_random_uuid(), $1, 'w' || g, 'W ' || g FROM generate_series(1, $2) g",
        )
        .bind(scope.org_id())
        .bind(i32::try_from(MAX_LIST_LIMIT).unwrap() + 5)
        .execute(&db.pool)
        .await
        .unwrap();
        let capped = workspaces::list(&db.pool, &scope, 10_000).await.unwrap();
        assert_eq!(capped.len(), MAX_LIST_LIMIT as usize);
        assert_eq!(
            workspaces::list(&db.pool, &scope, 0).await.unwrap().len(),
            1
        );
        assert_eq!(
            workspaces::list(&db.pool, &scope, 5).await.unwrap().len(),
            5
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn deletes_are_restricted() {
        let db = migrated().await;
        let (scope, owner) = org(&db.pool, "acme").await;
        let ws = workspaces::create(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("main"),
        )
        .await
        .unwrap();
        workspaces::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceMemberAdd),
            ws.id,
            owner.id,
            WorkspaceRole::Admin,
        )
        .await
        .unwrap();

        let e1 = sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(ws.id)
            .execute(&db.pool)
            .await
            .unwrap_err();
        let e2 = sqlx::query("DELETE FROM organization_members WHERE org_id = $1")
            .bind(scope.org_id())
            .execute(&db.pool)
            .await
            .unwrap_err();
        let e3 = sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(scope.org_id())
            .execute(&db.pool)
            .await
            .unwrap_err();
        let e4 = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(owner.id)
            .execute(&db.pool)
            .await
            .unwrap_err();
        for e in [e1, e2, e3, e4] {
            assert!(matches!(db_err(e), DbError::ForeignKeyViolation { .. }));
        }
        assert!(
            workspaces::get(&db.pool, &scope, ws.id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn connect_rejects_bad_urls_without_echo() {
        let err = craftcloud_db::connect("not a url", 1, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(err, DbError::InvalidUrl);
        let unreachable = "postgres://user:hunter2@127.0.0.1:1/db";
        let err = craftcloud_db::connect(unreachable, 1, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(!err.to_string().contains("hunter2"));
        assert!(!format!("{err:?}").contains("hunter2"));
    }

    fn new_instance(slug: &str) -> NewInstance {
        NewInstance {
            operator_email: "operator@example.test".to_owned(),
            operator_display_name: "Operator".to_owned(),
            operator_password_hash: "$argon2id$placeholder".to_owned(),
            organization_name: "First Org".to_owned(),
            organization_slug: slug.to_owned(),
        }
    }

    async fn count(pool: &PgPool, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn setup_creates_operator_org_and_membership_once() {
        let db = migrated().await;
        assert!(!setup::is_completed(&db.pool).await.unwrap());
        let created = setup::complete(&db.pool, &new_instance("acme"))
            .await
            .unwrap();
        assert!(setup::is_completed(&db.pool).await.unwrap());

        let row = sqlx::query("SELECT instance_role, password_hash FROM users WHERE id = $1")
            .bind(created.user_id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            row.get::<Option<String>, _>("instance_role").as_deref(),
            Some("operator")
        );
        assert_eq!(
            row.get::<Option<String>, _>("password_hash").as_deref(),
            Some("$argon2id$placeholder")
        );
        let member = sqlx::query(
            "SELECT role, status FROM organization_members WHERE org_id = $1 AND user_id = $2",
        )
        .bind(created.organization_id)
        .bind(created.user_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(member.get::<String, _>("role"), "owner");
        assert_eq!(member.get::<String, _>("status"), "active");
        let owner: Uuid =
            sqlx::query_scalar("SELECT owner_user_id FROM organizations WHERE id = $1")
                .bind(created.organization_id)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(owner, created.user_id);
        let marker: Uuid = sqlx::query_scalar("SELECT operator_user_id FROM instance_setup")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(marker, created.user_id);

        let again = setup::complete(
            &db.pool,
            &NewInstance {
                operator_email: "other@example.test".to_owned(),
                ..new_instance("other")
            },
        )
        .await
        .unwrap_err();
        assert_eq!(again, SetupError::AlreadyCompleted);
        assert_eq!(count(&db.pool, "users").await, 1);
        assert_eq!(count(&db.pool, "organizations").await, 1);
        assert_eq!(count(&db.pool, "instance_setup").await, 1);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn setup_failure_rolls_back_everything() {
        let db = migrated().await;
        // A pre-existing organization makes the slug insert fail after the
        // singleton row and the operator user were already written.
        org(&db.pool, "acme").await;
        let before_users = count(&db.pool, "users").await;

        let err = setup::complete(&db.pool, &new_instance("acme"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, SetupError::Db(DbError::UniqueViolation { .. })),
            "{err:?}"
        );
        assert_eq!(count(&db.pool, "users").await, before_users);
        assert_eq!(count(&db.pool, "instance_setup").await, 0);
        assert!(!setup::is_completed(&db.pool).await.unwrap());
        let operators: i64 =
            sqlx::query_scalar("SELECT count(*) FROM users WHERE instance_role = 'operator'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(operators, 0);

        // The failed attempt did not close setup.
        setup::complete(&db.pool, &new_instance("fresh"))
            .await
            .unwrap();
        assert!(setup::is_completed(&db.pool).await.unwrap());
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn concurrent_setups_produce_exactly_one_winner() {
        let db = migrated().await;
        let mut handles = Vec::new();
        for i in 0..10 {
            let pool = db.pool.clone();
            handles.push(tokio::spawn(async move {
                let new = NewInstance {
                    operator_email: format!("op{i}@example.test"),
                    ..new_instance(&format!("org-{i}"))
                };
                setup::complete(&pool, &new).await
            }));
        }
        let mut ok = 0;
        let mut already = 0;
        for handle in handles {
            match handle.await.unwrap() {
                Ok(_) => ok += 1,
                Err(SetupError::AlreadyCompleted) => already += 1,
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert_eq!((ok, already), (1, 9));
        assert_eq!(count(&db.pool, "users").await, 1);
        assert_eq!(count(&db.pool, "organizations").await, 1);
        assert_eq!(count(&db.pool, "organization_members").await, 1);
        assert_eq!(count(&db.pool, "instance_setup").await, 1);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn upgrade_from_0002_keeps_data_and_leaves_setup_open() {
        let db = TestDb::new().await;
        migrate::run_with(&db.pool, &migrator_up_to(2))
            .await
            .unwrap();
        let (scope, _) = legacy_org(&db.pool, "acme").await;
        // Created with raw SQL: the repository functions target the current schema.
        let ws_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO workspaces (id, org_id, slug, name) VALUES ($1, $2, 'main', 'Main')",
        )
        .bind(ws_id)
        .bind(scope.org_id())
        .execute(&db.pool)
        .await
        .unwrap();

        let applied = migrate::run(&db.pool).await.unwrap();
        assert_eq!(
            applied.iter().map(|a| a.version).collect::<Vec<_>>(),
            vec![
                3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21
            ]
        );
        migrate::verify(&db.pool).await.unwrap();
        let ws = workspaces::get(&db.pool, &scope, ws_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            ws.version, 1,
            "existing workspaces start at version 1 after migration 0006"
        );
        // Existing organizations start at version 1 (0005) with the invite setting on (0006).
        let upgraded = organizations::get(&db.pool, &scope).await.unwrap().unwrap();
        assert_eq!(upgraded.version, 1);
        assert!(upgraded.workspace_admin_invites_enabled);
        assert!(!setup::is_completed(&db.pool).await.unwrap());
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn instance_setup_is_a_singleton() {
        let db = migrated().await;
        let created = setup::complete(&db.pool, &new_instance("acme"))
            .await
            .unwrap();
        // A second row cannot be inserted, and id=false is rejected.
        let dup = sqlx::query("INSERT INTO instance_setup (operator_user_id) VALUES ($1)")
            .bind(created.user_id)
            .execute(&db.pool)
            .await
            .unwrap_err();
        assert!(matches!(db_err(dup), DbError::UniqueViolation { .. }));
        let falsy =
            sqlx::query("INSERT INTO instance_setup (id, operator_user_id) VALUES (false, $1)")
                .bind(created.user_id)
                .execute(&db.pool)
                .await
                .unwrap_err();
        assert!(matches!(db_err(falsy), DbError::CheckViolation { .. }));
        // The operator cannot be deleted while referenced.
        let del = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(created.user_id)
            .execute(&db.pool)
            .await
            .unwrap_err();
        assert!(matches!(db_err(del), DbError::ForeignKeyViolation { .. }));
    }

    /// Documents the offline recovery path of docs/06-operations/deployment.md:
    /// deleting the marker row reopens setup, existing rows stay.
    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn deleting_the_marker_row_reopens_setup_without_touching_existing_data() {
        let db = migrated().await;
        setup::complete(&db.pool, &new_instance("acme"))
            .await
            .unwrap();
        sqlx::query("DELETE FROM instance_setup")
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(!setup::is_completed(&db.pool).await.unwrap());

        // The old operator still owns its e-mail address and the old slug.
        let same_email = setup::complete(&db.pool, &new_instance("second"))
            .await
            .unwrap_err();
        assert!(matches!(
            same_email,
            SetupError::Db(DbError::UniqueViolation { .. })
        ));
        let fresh = NewInstance {
            operator_email: "second@example.test".to_owned(),
            ..new_instance("second")
        };
        setup::complete(&db.pool, &fresh).await.unwrap();
        assert_eq!(count(&db.pool, "users").await, 2);
        assert_eq!(count(&db.pool, "organizations").await, 2);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn setup_race_is_correct_under_a_serializable_server_default() {
        let db = migrated().await;
        // New sessions of this database default to SERIALIZABLE; the setup
        // transaction must still behave as under READ COMMITTED.
        let mut admin = PgConnection::connect_with(&db.admin).await.unwrap();
        sqlx::query(&format!(
            "ALTER DATABASE \"{}\" SET default_transaction_isolation = 'serializable'",
            db.name
        ))
        .execute(&mut admin)
        .await
        .unwrap();
        let fresh = db.extra_pool().await;
        let level: String = sqlx::query_scalar("SHOW default_transaction_isolation")
            .fetch_one(&fresh)
            .await
            .unwrap();
        assert_eq!(level, "serializable");

        let mut handles = Vec::new();
        for i in 0..10 {
            let pool = fresh.clone();
            handles.push(tokio::spawn(async move {
                let new = NewInstance {
                    operator_email: format!("op{i}@example.test"),
                    ..new_instance(&format!("org-{i}"))
                };
                setup::complete(&pool, &new).await
            }));
        }
        let (mut ok, mut already) = (0, 0);
        for handle in handles {
            match handle.await.unwrap() {
                Ok(_) => ok += 1,
                Err(SetupError::AlreadyCompleted) => already += 1,
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert_eq!((ok, already), (1, 9));
        assert_eq!(count(&fresh, "users").await, 1);
        assert_eq!(count(&fresh, "instance_setup").await, 1);
    }

    async fn user_with_hash(pool: &PgPool, email: &str, hash: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users (id, normalized_email, display_name, password_hash) VALUES ($1, $2, 'U', $3)",
        )
        .bind(id)
        .bind(email)
        .bind(hash)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    fn new_session(user_id: Uuid, marker: u8) -> NewSession {
        let now = time::OffsetDateTime::now_utc();
        NewSession {
            user_id,
            token_hash: [marker; 32],
            now,
            absolute_expires_at: now + time::Duration::days(7),
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn verified_login_insert_requires_the_current_hash_and_an_enabled_account() {
        let db = migrated().await;
        let id = user_with_hash(&db.pool, "u@example.test", "h1").await;
        assert!(
            sessions::create_for_verified_login(&db.pool, &new_session(id, 1), "h1")
                .await
                .unwrap()
                .is_some()
        );
        // A hash that is not the stored one (for example the one that was verified
        // before a reset committed) creates nothing.
        assert!(
            sessions::create_for_verified_login(&db.pool, &new_session(id, 2), "h0")
                .await
                .unwrap()
                .is_none()
        );
        sqlx::query("UPDATE users SET password_hash = 'h2' WHERE id = $1")
            .bind(id)
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(
            sessions::create_for_verified_login(&db.pool, &new_session(id, 3), "h1")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            sessions::create_for_verified_login(&db.pool, &new_session(id, 4), "h2")
                .await
                .unwrap()
                .is_some()
        );
        sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
            .bind(id)
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(
            sessions::create_for_verified_login(&db.pool, &new_session(id, 5), "h2")
                .await
                .unwrap()
                .is_none()
        );
        // Unknown user.
        assert!(
            sessions::create_for_verified_login(&db.pool, &new_session(Uuid::new_v4(), 6), "h2")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(count(&db.pool, "sessions").await, 2);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn second_factor_completion_binds_secret_generation_and_consumes_challenge_once() {
        use craftcloud_db::two_factor::{self, VerifiedFactor};
        use sha2::{Digest, Sha256};

        let db = migrated().await;
        let user_id = user_with_hash(&db.pool, "mfa@example.test", "h1").await;
        let now = time::OffsetDateTime::now_utc();
        let ciphertext = vec![7_u8; 32];
        sqlx::query("INSERT INTO user_two_factor(user_id,secret_ciphertext,enabled_at,last_accepted_step,created_at) VALUES($1,$2,$3,NULL,$3)")
            .bind(user_id).bind(&ciphertext).bind(now).execute(&db.pool).await.unwrap();
        let challenge_id = [9_u8; 32];
        let password_digest: [u8; 32] = Sha256::digest(b"h1").into();
        sqlx::query("INSERT INTO two_factor_login_challenges(id_hash,user_id,password_hash_digest,login_kind,client_address,created_at,expires_at) VALUES($1,$2,$3,'browser','192.0.2.9',$4,$4 + interval '5 minutes')")
            .bind(challenge_id.as_slice()).bind(user_id).bind(password_digest.as_slice()).bind(now).execute(&db.pool).await.unwrap();
        let audit = craftcloud_db::audit::AuditCtx {
            actor_id: Some(user_id),
            request_id: Some("test-request".into()),
        };
        let new_session = |marker| NewSession {
            user_id,
            token_hash: [marker; 32],
            now,
            absolute_expires_at: now + time::Duration::days(1),
        };

        let stale_generation = two_factor::complete_browser_login(
            &db.pool,
            &challenge_id,
            user_id,
            "192.0.2.9",
            "h1",
            &[8_u8; 32],
            VerifiedFactor::TotpStep(100),
            &new_session(1),
            &audit,
        )
        .await
        .unwrap();
        assert_eq!(stale_generation, None);

        let completed = two_factor::complete_browser_login(
            &db.pool,
            &challenge_id,
            user_id,
            "192.0.2.9",
            "h1",
            &ciphertext,
            VerifiedFactor::TotpStep(100),
            &new_session(2),
            &audit,
        )
        .await
        .unwrap();
        assert!(completed.is_some());
        let replay = two_factor::complete_browser_login(
            &db.pool,
            &challenge_id,
            user_id,
            "192.0.2.9",
            "h1",
            &ciphertext,
            VerifiedFactor::TotpStep(100),
            &new_session(3),
            &audit,
        )
        .await
        .unwrap();
        assert_eq!(replay, None);
        let step: Option<i64> =
            sqlx::query_scalar("SELECT last_accepted_step FROM user_two_factor WHERE user_id=$1")
                .bind(user_id)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(step, Some(100));
        let live_sessions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sessions WHERE user_id=$1 AND revoked_at IS NULL",
        )
        .bind(user_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(live_sessions, 1);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn second_factor_failures_are_counted_reset_by_success_and_capped_persistently() {
        use craftcloud_db::two_factor::{self, MAX_FAILED_ATTEMPTS, VerifiedFactor};
        use sha2::{Digest, Sha256};

        let db = migrated().await;
        let user_id = user_with_hash(&db.pool, "mfa-cap@example.test", "h1").await;
        let now = time::OffsetDateTime::now_utc();
        let ciphertext = vec![7_u8; 32];
        sqlx::query("INSERT INTO user_two_factor(user_id,secret_ciphertext,enabled_at,last_accepted_step,created_at) VALUES($1,$2,$3,NULL,$3)")
            .bind(user_id).bind(&ciphertext).bind(now).execute(&db.pool).await.unwrap();
        let audit = craftcloud_db::audit::AuditCtx {
            actor_id: Some(user_id),
            request_id: Some("test-request".into()),
        };
        let counter = || async {
            sqlx::query_scalar::<_, i32>(
                "SELECT failed_attempts FROM user_two_factor WHERE user_id=$1",
            )
            .bind(user_id)
            .fetch_one(&db.pool)
            .await
            .unwrap()
        };
        let password_digest: [u8; 32] = Sha256::digest(b"h1").into();
        let challenge = |marker: u8| {
            let db = &db;
            async move {
                let id = [marker; 32];
                sqlx::query("INSERT INTO two_factor_login_challenges(id_hash,user_id,password_hash_digest,login_kind,client_address,created_at,expires_at) VALUES($1,$2,$3,'browser','192.0.2.9',$4,$4 + interval '5 minutes')")
                    .bind(id.as_slice()).bind(user_id).bind(password_digest.as_slice()).bind(now).execute(&db.pool).await.unwrap();
                id
            }
        };
        let new_session = |marker| NewSession {
            user_id,
            token_hash: [marker; 32],
            now,
            absolute_expires_at: now + time::Duration::days(1),
        };

        // Failures accumulate; one success resets the counter.
        for expected in 1..=3 {
            assert!(
                !two_factor::record_failed_proof(&db.pool, user_id, &audit)
                    .await
                    .unwrap()
            );
            assert_eq!(counter().await, expected);
        }
        let first = challenge(1).await;
        let completed = two_factor::complete_browser_login(
            &db.pool,
            &first,
            user_id,
            "192.0.2.9",
            "h1",
            &ciphertext,
            VerifiedFactor::TotpStep(10),
            &new_session(1),
            &audit,
        )
        .await
        .unwrap();
        assert!(completed.is_some());
        assert_eq!(counter().await, 0);

        // The failure that reaches the cap reports the lock and audits it once.
        sqlx::query("UPDATE user_two_factor SET failed_attempts=$2 WHERE user_id=$1")
            .bind(user_id)
            .bind(MAX_FAILED_ATTEMPTS - 1)
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(
            two_factor::record_failed_proof(&db.pool, user_id, &audit)
                .await
                .unwrap()
        );
        assert_eq!(counter().await, MAX_FAILED_ATTEMPTS);
        assert!(
            !two_factor::record_failed_proof(&db.pool, user_id, &audit)
                .await
                .unwrap()
        );
        assert_eq!(counter().await, MAX_FAILED_ATTEMPTS);
        let locked_events: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events WHERE action='auth.two_factor_locked'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(locked_events, 1);

        // A valid proof (TOTP or recovery) no longer completes a login.
        let second = challenge(2).await;
        let refused = two_factor::complete_browser_login(
            &db.pool,
            &second,
            user_id,
            "192.0.2.9",
            "h1",
            &ciphertext,
            VerifiedFactor::TotpStep(20),
            &new_session(2),
            &audit,
        )
        .await
        .unwrap();
        assert_eq!(refused, None);
        assert_eq!(counter().await, MAX_FAILED_ATTEMPTS);

        // A vanished session is reported distinctly from an enabled factor.
        let outcome = two_factor::begin_setup(
            &db.pool,
            user_id,
            Uuid::new_v4(),
            "browser",
            "h1",
            &ciphertext,
            now,
        )
        .await
        .unwrap();
        assert_eq!(outcome, two_factor::SetupOutcome::SessionGone);

        // An operator reset removes the row, so a new setup starts at zero.
        assert!(
            two_factor::reset_for_user(
                &db.pool,
                user_id,
                None,
                None,
                None,
                None,
                None,
                now,
                &audit,
                "auth.two_factor_reset"
            )
            .await
            .unwrap()
        );
        let user_rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM user_two_factor WHERE user_id=$1")
                .bind(user_id)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(user_rows, 0);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn verified_login_insert_waits_for_a_password_change_in_flight_and_then_refuses() {
        let db = migrated().await;
        let id = user_with_hash(&db.pool, "race@example.test", "h1").await;

        // A reset/password change has updated the row but not committed yet.
        let mut tx = db.pool.begin().await.unwrap();
        sqlx::query("UPDATE users SET password_hash = 'h2' WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .unwrap();

        // The login verified "h1" earlier and now tries to create its session.
        let pool = db.pool.clone();
        let login = tokio::spawn(async move {
            sessions::create_for_verified_login(&pool, &new_session(id, 9), "h1").await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !login.is_finished(),
            "the insert must wait for the row lock"
        );

        tx.commit().await.unwrap();
        let outcome = login.await.unwrap().unwrap();
        assert!(
            outcome.is_none(),
            "a session for the replaced password was created"
        );
        assert_eq!(count(&db.pool, "sessions").await, 0);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn password_change_requires_the_verified_hash_and_a_live_session() {
        use craftcloud_db::accounts;
        let db = migrated().await;
        let id = user_with_hash(&db.pool, "chg@example.test", "h1").await;
        let now = time::OffsetDateTime::now_utc();
        let keep = sessions::create_for_verified_login(&db.pool, &new_session(id, 1), "h1")
            .await
            .unwrap()
            .unwrap();
        let other = sessions::create_for_verified_login(&db.pool, &new_session(id, 2), "h1")
            .await
            .unwrap()
            .unwrap();
        sqlx::query("INSERT INTO password_reset_tokens (id, user_id, token_hash, created_at, expires_at) VALUES (gen_random_uuid(), $1, decode(repeat('cd', 32), 'hex'), now(), now() + interval '1 hour')")
            .bind(id)
            .execute(&db.pool)
            .await
            .unwrap();

        // A stale verified hash (a reset committed meanwhile): nothing changes.
        assert_eq!(
            accounts::change_password(&db.pool, id, "h0", "h3", keep, now)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            accounts::password_hash(&db.pool, id)
                .await
                .unwrap()
                .as_deref(),
            Some("h1")
        );
        // The kept session was revoked meanwhile (link issuance, logout): nothing changes,
        // including the password.
        sessions::revoke(&db.pool, keep, now).await.unwrap();
        assert_eq!(
            accounts::change_password(&db.pool, id, "h1", "h3", keep, now)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            accounts::password_hash(&db.pool, id)
                .await
                .unwrap()
                .as_deref(),
            Some("h1")
        );
        // A session of another user cannot be used as the kept session.
        let stranger = user_with_hash(&db.pool, "stranger@example.test", "x").await;
        let foreign = sessions::create_for_verified_login(&db.pool, &new_session(stranger, 3), "x")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            accounts::change_password(&db.pool, id, "h1", "h3", foreign, now)
                .await
                .unwrap(),
            None
        );

        // With a live session and the verified hash it works and cleans up.
        let live = sessions::create_for_verified_login(&db.pool, &new_session(id, 4), "h1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            accounts::change_password(&db.pool, id, "h1", "h3", live, now)
                .await
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            accounts::password_hash(&db.pool, id)
                .await
                .unwrap()
                .as_deref(),
            Some("h3")
        );
        let active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sessions WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(active, 1, "only the kept session stays active");
        let pending: i64 =
            sqlx::query_scalar("SELECT count(*) FROM password_reset_tokens WHERE used_at IS NULL")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(pending, 0);
        let _ = other;
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn password_change_racing_a_reset_link_issue_changes_nothing() {
        use craftcloud_db::{accounts, password_reset};
        let db = migrated().await;
        let id = user_with_hash(&db.pool, "race2@example.test", "h1").await;
        let now = time::OffsetDateTime::now_utc();
        let keep = sessions::create_for_verified_login(&db.pool, &new_session(id, 1), "h1")
            .await
            .unwrap()
            .unwrap();

        // The issuance holds the user row lock (not yet committed): it will
        // revoke every session of the user.
        let mut issue = db.pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM users WHERE id = $1 FOR UPDATE")
            .bind(id)
            .execute(&mut *issue)
            .await
            .unwrap();

        let pool = db.pool.clone();
        let change = tokio::spawn(async move {
            accounts::change_password(&pool, id, "h1", "h3", keep, now).await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !change.is_finished(),
            "the change must wait for the issuance"
        );

        sqlx::query(
            "UPDATE sessions SET revoked_at = $2 WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(id)
        .bind(now)
        .execute(&mut *issue)
        .await
        .unwrap();
        issue.commit().await.unwrap();

        // The user row was locked but not changed, so the hash matches; the
        // session was revoked meanwhile, so the change is refused and rolled back.
        assert_eq!(change.await.unwrap().unwrap(), None);
        assert_eq!(
            accounts::password_hash(&db.pool, id)
                .await
                .unwrap()
                .as_deref(),
            Some("h1")
        );

        // The real issuance function produces the same outcome in sequence.
        let live = sessions::create_for_verified_login(&db.pool, &new_session(id, 2), "h1")
            .await
            .unwrap()
            .unwrap();
        let outcome = password_reset::create(
            &db.pool,
            &password_reset::NewResetToken {
                user_id: id,
                token_hash: [7; 32],
                created_by: None,
                reset_two_factor: false,
                now,
                expires_at: now + time::Duration::hours(1),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            outcome,
            password_reset::IssueOutcome::Issued {
                revoked_sessions: 1
            }
        ));
        assert_eq!(
            accounts::change_password(&db.pool, id, "h1", "h3", live, now)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn concurrent_consume_issue_and_change_never_deadlock() {
        use craftcloud_db::{accounts, password_reset};
        let db = migrated().await;
        let id = user_with_hash(&db.pool, "dl@example.test", "h1").await;
        let now = time::OffsetDateTime::now_utc();
        for round in 0..40u8 {
            let issue = |marker: u8| {
                let pool = db.pool.clone();
                async move {
                    password_reset::create(
                        &pool,
                        &password_reset::NewResetToken {
                            user_id: id,
                            token_hash: [marker; 32],
                            created_by: None,
                            reset_two_factor: false,
                            now,
                            expires_at: now + time::Duration::hours(1),
                        },
                    )
                    .await
                }
            };
            issue(round.wrapping_mul(2)).await.unwrap();
            let live = {
                let hash = accounts::password_hash(&db.pool, id)
                    .await
                    .unwrap()
                    .unwrap();
                (
                    sessions::create_for_verified_login(
                        &db.pool,
                        &new_session(id, 200 + (round % 50)),
                        &hash,
                    )
                    .await
                    .unwrap(),
                    hash,
                )
            };
            let (pool_a, pool_b, pool_c) = (db.pool.clone(), db.pool.clone(), db.pool.clone());
            let token = [round.wrapping_mul(2); 32];
            let current = live.1.clone();
            let keep = live.0;
            let consume = tokio::spawn(async move {
                password_reset::consume(&pool_a, &token, "h-consumed", now).await
            });
            let create = tokio::spawn(async move {
                password_reset::create(
                    &pool_b,
                    &password_reset::NewResetToken {
                        user_id: id,
                        token_hash: [round.wrapping_mul(2).wrapping_add(1); 32],
                        created_by: None,
                        reset_two_factor: false,
                        now,
                        expires_at: now + time::Duration::hours(1),
                    },
                )
                .await
            });
            let change = tokio::spawn(async move {
                match keep {
                    Some(keep) => {
                        accounts::change_password(&pool_c, id, &current, "h-changed", keep, now)
                            .await
                            .map(|_| ())
                    }
                    None => Ok(()),
                }
            });
            // Any deadlock would surface as an internal error here.
            consume.await.unwrap().unwrap();
            create.await.unwrap().unwrap();
            change.await.unwrap().unwrap();
            // Reset the hash so the next round starts from a known state.
            sqlx::query("UPDATE users SET password_hash = 'h1' WHERE id = $1")
                .bind(id)
                .execute(&db.pool)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn removing_a_member_removes_their_workspace_grants_and_scopes_are_checked() {
        use craftcloud_db::members::{OrgTx, RemoveOutcome, RoleOutcome};
        let db = migrated().await;
        let (scope, owner) = org(&db.pool, "acme").await;
        let (other_scope, _) = org(&db.pool, "other").await;
        let member = user(&db.pool, "m@example.test").await;
        organizations::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            member.id,
            OrgRole::Member,
            MemberStatus::Active,
        )
        .await
        .unwrap();
        let ws = workspaces::create(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("main"),
        )
        .await
        .unwrap();
        workspaces::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceMemberAdd),
            ws.id,
            member.id,
            WorkspaceRole::Editor,
        )
        .await
        .unwrap();

        // A scope of another organization is refused by the transaction.
        let mut tx = OrgTx::begin(&db.pool, scope.org_id())
            .await
            .unwrap()
            .unwrap();
        assert!(tx.remove(&other_scope, member.id).await.is_err());
        // A role change keeps the workspace grants; removal drops them.
        assert!(matches!(
            tx.set_role(
                &sc(&scope, craftcloud_core::policy::Action::MemberChangeRole),
                member.id,
                OrgRole::Admin
            )
            .await
            .unwrap(),
            RoleOutcome::Changed(_)
        ));
        assert_eq!(count(&db.pool, "workspace_members").await, 1);
        assert_eq!(
            tx.remove(
                &sc(&scope, craftcloud_core::policy::Action::MemberRemove),
                member.id
            )
            .await
            .unwrap(),
            RemoveOutcome::Removed
        );
        tx.commit().await.unwrap();
        assert_eq!(count(&db.pool, "workspace_members").await, 0);
        assert_eq!(count(&db.pool, "organization_members").await, 2);
        // The sole owner is protected, a missing member is reported.
        let mut tx = OrgTx::begin(&db.pool, scope.org_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            tx.remove(
                &sc(&scope, craftcloud_core::policy::Action::MemberRemove),
                owner.id
            )
            .await
            .unwrap(),
            RemoveOutcome::LastOwner
        );
        assert_eq!(
            tx.remove(
                &sc(&scope, craftcloud_core::policy::Action::MemberRemove),
                member.id
            )
            .await
            .unwrap(),
            RemoveOutcome::NotMember
        );
        // Unknown organizations have no transaction.
        assert!(
            OrgTx::begin(&db.pool, Uuid::new_v4())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn organization_creation_with_owner_is_atomic_and_account_state_changes_are_guarded() {
        use craftcloud_db::accounts::{self, DisableOutcome};
        use craftcloud_db::organizations::CreateOutcome;
        let db = migrated().await;
        let owner = user(&db.pool, "own@example.test").await;
        let new = |slug: &str, owner_user_id: Uuid| NewOrganization {
            slug: slug.to_owned(),
            name: "N".to_owned(),
            owner_user_id,
        };
        let created = organizations::create_with_owner(&db.pool, &new("one", owner.id))
            .await
            .unwrap();
        let CreateOutcome::Created(org) = created else {
            panic!("not created")
        };
        assert_eq!(org.version, 1);
        assert_eq!(count(&db.pool, "organization_members").await, 1);
        // Duplicate slug: nothing is stored (no orphan membership).
        let dup = organizations::create_with_owner(&db.pool, &new("one", owner.id))
            .await
            .unwrap_err();
        assert!(matches!(dup, DbError::UniqueViolation { .. }));
        assert_eq!(count(&db.pool, "organizations").await, 1);
        assert_eq!(count(&db.pool, "organization_members").await, 1);
        assert!(matches!(
            organizations::create_with_owner(&db.pool, &new("two", Uuid::new_v4()))
                .await
                .unwrap(),
            CreateOutcome::OwnerNotFound
        ));

        // Disabling is idempotent and never applies to the setup operator.
        let now = time::OffsetDateTime::now_utc();
        assert_eq!(
            accounts::set_disabled(&db.pool, owner.id, true, now)
                .await
                .unwrap(),
            DisableOutcome::Changed {
                orphaned_organizations: vec![org.id]
            }
        );
        assert_eq!(
            accounts::set_disabled(&db.pool, owner.id, true, now)
                .await
                .unwrap(),
            DisableOutcome::Unchanged {
                orphaned_organizations: vec![org.id]
            }
        );
        assert!(matches!(
            organizations::create_with_owner(&db.pool, &new("three", owner.id))
                .await
                .unwrap(),
            CreateOutcome::OwnerDisabled
        ));
        assert_eq!(
            accounts::set_disabled(&db.pool, owner.id, false, now)
                .await
                .unwrap(),
            DisableOutcome::Changed {
                orphaned_organizations: vec![]
            }
        );
        assert_eq!(
            accounts::set_disabled(&db.pool, Uuid::new_v4(), true, now)
                .await
                .unwrap(),
            DisableOutcome::NoSuchUser
        );
        let operator = setup::complete(&db.pool, &new_instance("operator-org"))
            .await
            .unwrap()
            .user_id;
        assert_eq!(
            accounts::set_disabled(&db.pool, operator, true, now)
                .await
                .unwrap(),
            DisableOutcome::IsOperator
        );
        assert_eq!(
            accounts::set_disabled(&db.pool, operator, false, now)
                .await
                .unwrap(),
            DisableOutcome::Unchanged {
                orphaned_organizations: vec![]
            }
        );
    }

    async fn disable(db: &TestDb, user: Uuid) {
        use craftcloud_db::accounts;
        accounts::set_disabled(&db.pool, user, true, time::OffsetDateTime::now_utc())
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn disabled_owners_do_not_count_for_the_last_owner_rule() {
        use craftcloud_db::members::{OrgTx, RemoveOutcome, RoleOutcome};
        let db = migrated().await;
        let (scope, x) = org(&db.pool, "acme").await;
        let y = user(&db.pool, "y@example.test").await;
        organizations::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            y.id,
            OrgRole::Owner,
            MemberStatus::Active,
        )
        .await
        .unwrap();
        disable(&db, y.id).await;

        // Only a disabled owner would remain: the enabled owner can neither leave nor be demoted.
        let mut tx = OrgTx::begin(&db.pool, scope.org_id())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            tx.set_role(
                &sc(&scope, craftcloud_core::policy::Action::MemberChangeRole),
                x.id,
                OrgRole::Admin
            )
            .await
            .unwrap(),
            RoleOutcome::LastOwner
        ));
        assert_eq!(
            tx.remove(
                &sc(&scope, craftcloud_core::policy::Action::MemberRemove),
                x.id
            )
            .await
            .unwrap(),
            RemoveOutcome::LastOwner
        );
        // The disabled owner itself can be demoted or removed (it does not count).
        assert!(matches!(
            tx.set_role(
                &sc(&scope, craftcloud_core::policy::Action::MemberChangeRole),
                y.id,
                OrgRole::Member
            )
            .await
            .unwrap(),
            RoleOutcome::Changed(_)
        ));
        tx.commit().await.unwrap();

        // With a second enabled owner the first may leave.
        let z = user(&db.pool, "z@example.test").await;
        let mut tx = OrgTx::begin(&db.pool, scope.org_id())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            tx.add(
                &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
                z.id,
                OrgRole::Owner
            )
            .await
            .unwrap(),
            craftcloud_db::members::AddOutcome::Added(_)
        ));
        assert_eq!(
            tx.remove(
                &sc(&scope, craftcloud_core::policy::Action::MemberRemove),
                x.id
            )
            .await
            .unwrap(),
            RemoveOutcome::Removed
        );
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn disabling_the_last_enabled_owner_is_allowed_and_reports_the_orphans() {
        use craftcloud_db::accounts::{self, DisableOutcome};
        let db = migrated().await;
        let (scope_a, a) = org(&db.pool, "org-a").await;
        let (scope_b, _) = org(&db.pool, "org-b").await;
        // `a` also co-owns org-b together with its own owner, so org-b is not orphaned.
        organizations::add_member(
            &db.pool,
            &sc(&scope_b, craftcloud_core::policy::Action::MemberAdd),
            a.id,
            OrgRole::Owner,
            MemberStatus::Active,
        )
        .await
        .unwrap();
        let now = time::OffsetDateTime::now_utc();
        let outcome = accounts::set_disabled(&db.pool, a.id, true, now)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            DisableOutcome::Changed {
                orphaned_organizations: vec![scope_a.org_id()]
            }
        );
        // Idempotent, and the orphan list reflects the current state.
        let again = accounts::set_disabled(&db.pool, a.id, true, now)
            .await
            .unwrap();
        assert_eq!(
            again,
            DisableOutcome::Unchanged {
                orphaned_organizations: vec![scope_a.org_id()]
            }
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn break_glass_recovery_makes_an_enabled_user_owner() {
        use craftcloud_db::recovery::{SetOwnerOutcome, set_org_owner};
        let db = migrated().await;
        let (scope, owner) = org(&db.pool, "acme").await;
        let org_id = scope.org_id();
        disable(&db, owner.id).await;
        let newcomer = user(&db.pool, "new@example.test").await;
        let member = user(&db.pool, "member@example.test").await;
        let invited = user(&db.pool, "invited@example.test").await;
        organizations::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            member.id,
            OrgRole::Member,
            MemberStatus::Active,
        )
        .await
        .unwrap();
        organizations::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            invited.id,
            OrgRole::Admin,
            MemberStatus::Invited,
        )
        .await
        .unwrap();

        assert_eq!(
            set_org_owner(&db.pool, "acme", "new@example.test")
                .await
                .unwrap(),
            SetOwnerOutcome::Added {
                org_id,
                user_id: newcomer.id
            }
        );
        assert_eq!(
            set_org_owner(&db.pool, "acme", "new@example.test")
                .await
                .unwrap(),
            SetOwnerOutcome::AlreadyOwner {
                org_id,
                user_id: newcomer.id
            }
        );
        assert_eq!(
            set_org_owner(&db.pool, "acme", "member@example.test")
                .await
                .unwrap(),
            SetOwnerOutcome::Promoted {
                org_id,
                user_id: member.id
            }
        );
        assert_eq!(
            set_org_owner(&db.pool, "acme", "invited@example.test")
                .await
                .unwrap(),
            SetOwnerOutcome::Promoted {
                org_id,
                user_id: invited.id
            }
        );
        assert_eq!(
            set_org_owner(&db.pool, "acme", "nobody@example.test")
                .await
                .unwrap(),
            SetOwnerOutcome::UserNotFound
        );
        assert_eq!(
            set_org_owner(&db.pool, "no-such-org", "new@example.test")
                .await
                .unwrap(),
            SetOwnerOutcome::OrgNotFound
        );
        // A disabled account cannot be made owner.
        disable(&db, newcomer.id).await;
        let gone = user(&db.pool, "gone@example.test").await;
        disable(&db, gone.id).await;
        assert_eq!(
            set_org_owner(&db.pool, "acme", "gone@example.test")
                .await
                .unwrap(),
            SetOwnerOutcome::UserDisabled
        );
        let owners: i64 = sqlx::query_scalar("SELECT count(*) FROM organization_members m JOIN users u ON u.id = m.user_id WHERE m.org_id = $1 AND m.role = 'owner' AND m.status = 'active' AND u.disabled_at IS NULL").bind(org_id).fetch_one(&db.pool).await.unwrap();
        assert_eq!(owners, 2, "member and invited users are now enabled owners");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn non_active_membership_rows_are_handled_explicitly() {
        use craftcloud_db::members::{self, AddOutcome, OrgTx, RemoveOutcome, RoleOutcome};
        let db = migrated().await;
        let (scope, _owner) = org(&db.pool, "acme").await;
        let (invited, disabled, old_owner) = (
            user(&db.pool, "inv@example.test").await,
            user(&db.pool, "dis@example.test").await,
            user(&db.pool, "oldowner@example.test").await,
        );
        organizations::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            invited.id,
            OrgRole::Admin,
            MemberStatus::Invited,
        )
        .await
        .unwrap();
        organizations::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            disabled.id,
            OrgRole::Member,
            MemberStatus::Disabled,
        )
        .await
        .unwrap();
        organizations::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            old_owner.id,
            OrgRole::Owner,
            MemberStatus::Disabled,
        )
        .await
        .unwrap();

        // Non-active rows are invisible as members ...
        assert_eq!(
            organizations::active_role(&db.pool, scope.org_id(), invited.id)
                .await
                .unwrap(),
            None
        );
        let listed = members::list(&db.pool, &scope, 50, None).await.unwrap();
        assert_eq!(listed.len(), 1, "only the active owner is listed");
        let mut tx = OrgTx::begin(&db.pool, scope.org_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tx.role_of(invited.id).await.unwrap(), None);
        // ... but cleanup tools see the stored role.
        assert_eq!(
            tx.stored_role_of(invited.id).await.unwrap(),
            Some(OrgRole::Admin)
        );
        // Changing the role of a non-active row is "not a member".
        assert!(matches!(
            tx.set_role(
                &sc(&scope, craftcloud_core::policy::Action::MemberChangeRole),
                invited.id,
                OrgRole::Member
            )
            .await
            .unwrap(),
            RoleOutcome::NotMember
        ));

        // Adding reactivates a non-owner row with the given role ...
        let AddOutcome::Added(m) = tx
            .add(
                &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
                invited.id,
                OrgRole::Member,
            )
            .await
            .unwrap()
        else {
            panic!("not added")
        };
        assert_eq!(m.role, "member");
        assert!(matches!(
            tx.add(
                &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
                disabled.id,
                OrgRole::Admin
            )
            .await
            .unwrap(),
            AddOutcome::Added(_)
        ));
        assert_eq!(tx.role_of(disabled.id).await.unwrap(), Some(OrgRole::Admin));
        // ... an active member stays "already a member" ...
        assert!(matches!(
            tx.add(
                &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
                invited.id,
                OrgRole::Member
            )
            .await
            .unwrap(),
            AddOutcome::AlreadyMember
        ));
        // ... and a non-active owner row is never reactivated through add.
        assert!(matches!(
            tx.add(
                &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
                old_owner.id,
                OrgRole::Member
            )
            .await
            .unwrap(),
            AddOutcome::AlreadyMember
        ));
        assert_eq!(tx.role_of(old_owner.id).await.unwrap(), None);
        // Remove deletes a row of any status.
        assert_eq!(
            tx.remove(
                &sc(&scope, craftcloud_core::policy::Action::MemberRemove),
                old_owner.id
            )
            .await
            .unwrap(),
            RemoveOutcome::Removed
        );
        assert_eq!(
            tx.remove(
                &sc(&scope, craftcloud_core::policy::Action::MemberRemove),
                old_owner.id
            )
            .await
            .unwrap(),
            RemoveOutcome::NotMember
        );
        tx.commit().await.unwrap();
        assert_eq!(count(&db.pool, "organization_members").await, 3);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_scope_only_drives_the_repository_function_of_its_own_action() {
        use craftcloud_core::policy::Action;
        use craftcloud_db::members::OrgTx;
        let db = migrated().await;
        let (base, _) = org(&db.pool, "acme").await;
        let org_id = base.org_id();
        let (read, remove_scope, add_scope) = (
            read_scope_for(org_id),
            sc(&base, Action::MemberRemove),
            sc(&base, Action::MemberAdd),
        );
        let stranger = user(&db.pool, "s@example.test").await;

        let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
        // A read permit drives no write ...
        assert!(tx.add(&read, stranger.id, OrgRole::Member).await.is_err());
        assert!(
            tx.update_settings(&read, Some("Renamed"), None, None, 1)
                .await
                .is_err()
        );
        // ... and neither does the permit of another write action.
        assert!(
            tx.add(&remove_scope, stranger.id, OrgRole::Member)
                .await
                .is_err()
        );
        assert!(
            tx.set_role(&remove_scope, stranger.id, OrgRole::Admin)
                .await
                .is_err()
        );
        assert!(
            tx.update_settings(&add_scope, Some("Renamed"), None, None, 1)
                .await
                .is_err()
        );
        assert!(tx.remove(&add_scope, stranger.id).await.is_err());
        drop(tx);
        assert!(
            workspaces::create(&db.pool, &add_scope, &new_ws("main"))
                .await
                .is_err()
        );
        assert!(
            workspaces::create(&db.pool, &read, &new_ws("main"))
                .await
                .is_err()
        );
        assert!(
            organizations::add_member(
                &db.pool,
                &remove_scope,
                stranger.id,
                OrgRole::Member,
                MemberStatus::Active
            )
            .await
            .is_err()
        );
        assert_eq!(count(&db.pool, "organization_members").await, 1);
        assert_eq!(count(&db.pool, "workspaces").await, 0);
        // The matching scope works; reads work with any scope.
        let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
        assert!(
            tx.add(&add_scope, stranger.id, OrgRole::Member)
                .await
                .is_ok()
        );
        assert!(tx.remove(&remove_scope, stranger.id).await.is_ok());
        assert!(
            tx.update_settings(
                &sc(&base, Action::OrganizationUpdate),
                Some("Renamed"),
                None,
                None,
                1
            )
            .await
            .is_ok()
        );
        tx.commit().await.unwrap();
        assert!(organizations::get(&db.pool, &read).await.unwrap().is_some());
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_last_owner_check_waits_for_a_concurrent_account_disable() {
        use craftcloud_core::policy::Action;
        use craftcloud_db::members::{OrgTx, RoleOutcome};
        let db = migrated().await;
        let (base, x) = org(&db.pool, "acme").await;
        let y = user(&db.pool, "y@example.test").await;
        organizations::add_member(
            &db.pool,
            &sc(&base, Action::MemberAdd),
            y.id,
            OrgRole::Owner,
            MemberStatus::Active,
        )
        .await
        .unwrap();

        // The other owner's account is being disabled: the transaction holds the
        // user row and has changed it, but has not committed yet.
        let mut disabling = db.pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM users WHERE id = $1 FOR UPDATE")
            .bind(y.id)
            .execute(&mut *disabling)
            .await
            .unwrap();
        sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
            .bind(y.id)
            .execute(&mut *disabling)
            .await
            .unwrap();

        // x demotes themselves; with two owners (one about to be disabled) that looks allowed
        // unless the check waits for the disable and re-reads it.
        let pool = db.pool.clone();
        let scope = sc(&base, Action::MemberChangeRole);
        let org_id = base.org_id();
        let demote = tokio::spawn(async move {
            let mut tx = OrgTx::begin(&pool, org_id).await.unwrap().unwrap();
            tx.set_role(&scope, x.id, OrgRole::Admin).await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !demote.is_finished(),
            "the owner count must wait for the in-flight disable"
        );

        disabling.commit().await.unwrap();
        let outcome = demote.await.unwrap().unwrap();
        assert!(matches!(outcome, RoleOutcome::LastOwner), "{outcome:?}");
        assert_eq!(
            organizations::active_role(&db.pool, org_id, x.id)
                .await
                .unwrap(),
            Some(OrgRole::Owner)
        );
    }

    // ---- audit events (CC-F11) -------------------------------------------------

    mod audit_tests {
        use super::*;
        use craftcloud_core::policy::Action;
        use craftcloud_db::audit::{
            self, AggScope, AuditCtx, AuditFilter, AuditResult, Detail, NewEvent, record_aggregated,
        };
        use craftcloud_db::members::OrgTx;
        use time::OffsetDateTime;

        async fn rows(pool: &PgPool, sql: &str) -> i64 {
            sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
        }

        /// Seeds `n` events of the organization with one shared timestamp.
        async fn seed_equal(pool: &PgPool, org: Uuid, n: i32, at: OffsetDateTime, action: &str) {
            sqlx::query(
                "INSERT INTO audit_events (id, org_id, action, result, created_at, last_at) \
                 SELECT gen_random_uuid(), $1, $2, 'success', $3, $3 FROM generate_series(1, $4)",
            )
            .bind(org)
            .bind(action)
            .bind(at)
            .bind(n)
            .execute(pool)
            .await
            .unwrap();
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn rows_cannot_be_updated_deleted_or_truncated() {
            let db = migrated().await;
            let ctx = AuditCtx::system();
            audit::record(&db.pool, &ctx, &NewEvent::new("organization.create"))
                .await
                .unwrap();
            for sql in [
                "UPDATE audit_events SET action = 'member.add'",
                "UPDATE audit_events SET result = 'failed'",
                "UPDATE audit_events SET details = '{\"x\":1}'::jsonb",
                "UPDATE audit_events SET event_count = event_count + 1",
                "DELETE FROM audit_events",
                "TRUNCATE audit_events",
            ] {
                let err = sqlx::query(sql).execute(&db.pool).await.unwrap_err();
                let text = err.to_string();
                assert!(
                    text.contains("append-only")
                        || text.contains("audit_purge")
                        || text.contains("truncated"),
                    "{sql}: {text}"
                );
            }
            assert_eq!(rows(&db.pool, "SELECT count(*) FROM audit_events").await, 1);
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn invalid_values_are_rejected_by_the_database() {
            let db = migrated().await;
            for (action, result) in [
                ("Member.Add", "success"),
                ("member add", "success"),
                ("member.add", "maybe"),
                ("", "success"),
            ] {
                let outcome = sqlx::query(
                    "INSERT INTO audit_events (id, action, result) VALUES (gen_random_uuid(), $1, $2)",
                )
                .bind(action)
                .bind(result)
                .execute(&db.pool)
                .await;
                assert!(outcome.is_err(), "{action:?} {result:?}");
            }
            // A request id with odd characters is refused as well.
            let outcome = sqlx::query(
                "INSERT INTO audit_events (id, action, result, request_id) \
                 VALUES (gen_random_uuid(), 'member.add', 'success', 'a b')",
            )
            .execute(&db.pool)
            .await;
            assert!(outcome.is_err());
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn details_take_only_typed_values_with_plain_keys() {
            let db = migrated().await;
            let ctx = AuditCtx::system();
            let bad_key = NewEvent::new("member.add").detail("E-Mail", Detail::Flag(true));
            assert!(matches!(
                audit::record(&db.pool, &ctx, &bad_key).await,
                Err(DbError::CheckViolation { .. })
            ));
            let user_id = Uuid::new_v4();
            let ok = NewEvent::new("member.add")
                .detail("role", Detail::Code("admin"))
                .detail("count", Detail::Count(3))
                .detail("self_removal", Detail::Flag(false))
                .detail("user_id", Detail::Id(user_id));
            audit::record(&db.pool, &ctx, &ok).await.unwrap();
            let details: String = sqlx::query_scalar("SELECT details::text FROM audit_events")
                .fetch_one(&db.pool)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_str(&details).unwrap();
            assert_eq!(value["role"], "admin");
            assert_eq!(value["count"], 3);
            assert_eq!(value["self_removal"], false);
            assert_eq!(value["user_id"], user_id.to_string());
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn an_event_in_the_transaction_rolls_back_with_it() {
            let db = migrated().await;
            let (scope, owner) = org(&db.pool, "a1").await;
            let ctx = AuditCtx {
                actor_id: Some(owner.id),
                request_id: Some("req-1".to_owned()),
            };
            // Rolled back: no event.
            let mut tx = OrgTx::begin(&db.pool, scope.org_id())
                .await
                .unwrap()
                .unwrap();
            tx.audit(&ctx, NewEvent::new("member.add").target("user", owner.id))
                .await
                .unwrap();
            drop(tx);
            assert_eq!(rows(&db.pool, "SELECT count(*) FROM audit_events").await, 0);
            // Committed: exactly one event, bound to the transaction's organization.
            let mut tx = OrgTx::begin(&db.pool, scope.org_id())
                .await
                .unwrap()
                .unwrap();
            tx.audit(&ctx, NewEvent::new("member.add").target("user", owner.id))
                .await
                .unwrap();
            tx.commit().await.unwrap();
            let (org_id, actor, request): (Option<Uuid>, Option<Uuid>, Option<String>) =
                sqlx::query_as("SELECT org_id, actor_id, request_id FROM audit_events")
                    .fetch_one(&db.pool)
                    .await
                    .unwrap();
            assert_eq!(
                (org_id, actor, request.as_deref()),
                (Some(scope.org_id()), Some(owner.id), Some("req-1"))
            );
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn aggregated_events_share_a_row_and_count_concurrent_writers() {
            let db = migrated().await;
            let pool = db.pool.clone();
            let actor = Uuid::new_v4();
            let mut tasks = Vec::new();
            for _ in 0..20 {
                let pool = pool.clone();
                tasks.push(tokio::spawn(async move {
                    let ctx = AuditCtx {
                        actor_id: Some(actor),
                        request_id: None,
                    };
                    record_aggregated(
                        &pool,
                        &ctx,
                        &NewEvent::new("auth.login").result(AuditResult::Failed),
                        AggScope::Actor,
                    )
                    .await
                    .unwrap();
                }));
            }
            for task in tasks {
                task.await.unwrap();
            }
            let (n, count): (i64, i32) =
                sqlx::query_as("SELECT count(*), max(event_count) FROM audit_events")
                    .fetch_one(&db.pool)
                    .await
                    .unwrap();
            assert_eq!((n, count), (1, 20));
            // The counter may only grow; no other column may change.
            for sql in [
                "UPDATE audit_events SET event_count = 1",
                "UPDATE audit_events SET result = 'success'",
            ] {
                assert!(sqlx::query(sql).execute(&db.pool).await.is_err(), "{sql}");
            }
            // Another actor gets its own row.
            record_aggregated(
                &db.pool,
                &AuditCtx {
                    actor_id: Some(Uuid::new_v4()),
                    request_id: None,
                },
                &NewEvent::new("auth.login").result(AuditResult::Failed),
                AggScope::Actor,
            )
            .await
            .unwrap();
            assert_eq!(rows(&db.pool, "SELECT count(*) FROM audit_events").await, 2);
            // With the target in the key, the same actor and action split per target.
            let ctx = AuditCtx {
                actor_id: None,
                request_id: None,
            };
            for target in [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()] {
                for _ in 0..2 {
                    record_aggregated(
                        &db.pool,
                        &ctx,
                        &NewEvent::new("file.download").target("file_version", target),
                        AggScope::Target,
                    )
                    .await
                    .unwrap();
                }
            }
            assert_eq!(
                rows(
                    &db.pool,
                    "SELECT count(*) FROM audit_events WHERE action = 'file.download'"
                )
                .await,
                3
            );
            // Without it, probing many targets stays one row.
            for _ in 0..5 {
                record_aggregated(
                    &db.pool,
                    &ctx,
                    &NewEvent::new("member.change_role")
                        .result(AuditResult::Denied)
                        .target("user", Uuid::new_v4()),
                    AggScope::Actor,
                )
                .await
                .unwrap();
            }
            assert_eq!(
                rows(
                    &db.pool,
                    "SELECT count(*) FROM audit_events WHERE action = 'member.change_role'"
                )
                .await,
                1
            );
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn retention_purge_deletes_old_events_in_batches_only() {
            let db = migrated().await;
            let (scope, _) = org(&db.pool, "b1").await;
            let org_id = scope.org_id();
            let now = OffsetDateTime::now_utc();
            seed_equal(
                &db.pool,
                org_id,
                25,
                now - time::Duration::days(400),
                "file.purge",
            )
            .await;
            seed_equal(
                &db.pool,
                org_id,
                5,
                now - time::Duration::days(100),
                "file.purge",
            )
            .await;
            seed_equal(&db.pool, org_id, 3, now, "file.purge").await;
            assert_eq!(audit::count_expired(&db.pool, 365).await.unwrap(), 25);
            assert_eq!(audit::purge(&db.pool, 365, 10).await.unwrap(), 10);
            assert_eq!(audit::purge(&db.pool, 365, 10).await.unwrap(), 10);
            assert_eq!(audit::purge(&db.pool, 365, 10).await.unwrap(), 5);
            assert_eq!(audit::purge(&db.pool, 365, 10).await.unwrap(), 0);
            assert_eq!(rows(&db.pool, "SELECT count(*) FROM audit_events").await, 8);
            // The delete guard is lifted only inside the purge function.
            assert!(
                sqlx::query("DELETE FROM audit_events")
                    .execute(&db.pool)
                    .await
                    .is_err()
            );
            // A shorter retention reaches the 100 day old events.
            assert_eq!(audit::purge(&db.pool, 30, 100).await.unwrap(), 5);
            assert_eq!(rows(&db.pool, "SELECT count(*) FROM audit_events").await, 3);
            // Whatever cutoff is passed, nothing newer than 30 days goes.
            let removed: i64 =
                sqlx::query_scalar("SELECT audit_purge('infinity'::timestamptz, 1000)")
                    .fetch_one(&db.pool)
                    .await
                    .unwrap();
            assert_eq!(removed, 0);
            assert_eq!(rows(&db.pool, "SELECT count(*) FROM audit_events").await, 3);
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn paging_is_stable_under_equal_timestamps_and_filters_narrow() {
            let db = migrated().await;
            let (scope, _) = org(&db.pool, "c1").await;
            let (other, _) = org(&db.pool, "c2").await;
            let at = OffsetDateTime::now_utc() - time::Duration::hours(1);
            seed_equal(&db.pool, scope.org_id(), 25, at, "member.add").await;
            seed_equal(&db.pool, scope.org_id(), 4, at, "workspace.member.add").await;
            seed_equal(&db.pool, other.org_id(), 6, at, "member.add").await;
            let read = sc(&scope, Action::AuditRead);

            // Walk all pages of 7: every event once, strictly descending (time, id).
            let mut seen = Vec::new();
            let mut after = None;
            loop {
                // One more than the page size tells whether another page follows.
                let page = audit::list(&db.pool, &read, &AuditFilter::default(), 8, after)
                    .await
                    .unwrap();
                let more = page.len() > 7;
                let take = page.len().min(7);
                seen.extend(page.iter().take(take).map(|e| (e.created_at, e.id)));
                if !more {
                    break;
                }
                after = page.get(take - 1).map(|e| (e.created_at, e.id));
            }
            assert_eq!(seen.len(), 29, "only this organization's events");
            let mut sorted = seen.clone();
            sorted.sort_by(|a, b| b.cmp(a));
            sorted.dedup();
            assert_eq!(sorted, seen, "no repeats, descending order");

            let prefix = AuditFilter {
                action_prefix: Some("member".to_owned()),
                ..AuditFilter::default()
            };
            assert_eq!(
                audit::list(&db.pool, &read, &prefix, 100, None)
                    .await
                    .unwrap()
                    .len(),
                25,
                "`member` does not match `workspace.member.add`"
            );
            let nested = AuditFilter {
                action_prefix: Some("workspace".to_owned()),
                ..AuditFilter::default()
            };
            assert_eq!(
                audit::list(&db.pool, &read, &nested, 100, None)
                    .await
                    .unwrap()
                    .len(),
                4
            );
            // `_` in a prefix is no wildcard.
            let underscore = AuditFilter {
                action_prefix: Some("workspace.member_add".to_owned()),
                ..AuditFilter::default()
            };
            assert!(
                audit::list(&db.pool, &read, &underscore, 100, None)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let window = AuditFilter {
                from: Some(at + time::Duration::seconds(1)),
                ..AuditFilter::default()
            };
            assert!(
                audit::list(&db.pool, &read, &window, 100, None)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let result = AuditFilter {
                result: Some(AuditResult::Denied),
                ..AuditFilter::default()
            };
            assert!(
                audit::list(&db.pool, &read, &result, 100, None)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let bad = AuditFilter {
                action_prefix: Some("a%".to_owned()),
                ..AuditFilter::default()
            };
            assert!(audit::list(&db.pool, &read, &bad, 10, None).await.is_err());
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn listing_needs_the_audit_scope_and_instance_events_stay_apart() {
            let db = migrated().await;
            let (scope, _) = org(&db.pool, "d1").await;
            audit::record(
                &db.pool,
                &AuditCtx::system(),
                &NewEvent::new("storage.rebind"),
            )
            .await
            .unwrap();
            audit::record(
                &db.pool,
                &AuditCtx::system(),
                &NewEvent::new("file.purge").org(scope.org_id()),
            )
            .await
            .unwrap();
            // A scope of another action cannot read the log.
            let wrong = sc(&scope, Action::MemberAdd);
            assert!(
                audit::list(&db.pool, &wrong, &AuditFilter::default(), 10, None)
                    .await
                    .is_err()
            );
            let read = sc(&scope, Action::AuditRead);
            let own = audit::list(&db.pool, &read, &AuditFilter::default(), 10, None)
                .await
                .unwrap();
            assert_eq!(own.len(), 1);
            assert_eq!(own[0].action, "file.purge");
            let instance = audit::list_instance(&db.pool, &AuditFilter::default(), 10, None)
                .await
                .unwrap();
            assert_eq!(instance.len(), 1);
            assert_eq!(instance[0].action, "storage.rebind");
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn repository_changes_record_exactly_one_event_and_replays_none() {
            use craftcloud_db::accounts;
            use craftcloud_db::password_reset::{self, NewResetToken};
            let db = migrated().await;
            let user = user(&db.pool, "u1@example.test").await;
            let ctx = AuditCtx {
                actor_id: Some(Uuid::new_v4()),
                request_id: Some("r-1".to_owned()),
            };
            let now = OffsetDateTime::now_utc();
            // Disable: one event; the same state again: none.
            for _ in 0..2 {
                accounts::set_disabled_audited(&db.pool, user.id, true, now, &ctx)
                    .await
                    .unwrap();
            }
            assert_eq!(
                rows(
                    &db.pool,
                    "SELECT count(*) FROM audit_events WHERE action = 'user.disable'"
                )
                .await,
                1
            );
            accounts::set_disabled_audited(&db.pool, user.id, false, now, &ctx)
                .await
                .unwrap();
            assert_eq!(
                rows(
                    &db.pool,
                    "SELECT count(*) FROM audit_events WHERE action = 'user.enable'"
                )
                .await,
                1
            );
            // A reset link: issue once, use once; a replay of the token records nothing.
            let hash = [7_u8; 32];
            password_reset::create_audited(
                &db.pool,
                &NewResetToken {
                    user_id: user.id,
                    token_hash: hash,
                    created_by: None,
                    reset_two_factor: false,
                    now,
                    expires_at: now + time::Duration::hours(1),
                },
                &ctx,
            )
            .await
            .unwrap();
            for _ in 0..2 {
                password_reset::consume_audited(&db.pool, &hash, "hash-value", now, &ctx)
                    .await
                    .unwrap();
            }
            assert_eq!(
                rows(
                    &db.pool,
                    "SELECT count(*) FROM audit_events WHERE action IN ('password_reset.issue', 'password_reset.use')"
                )
                .await,
                2
            );
            // Nothing sensitive reached the table.
            let dump: String = sqlx::query_scalar(
                "SELECT COALESCE(string_agg(row_to_json(a)::text, ' '), '') FROM audit_events a",
            )
            .fetch_one(&db.pool)
            .await
            .unwrap();
            for needle in ["example.test", "hash-value", "@"] {
                assert!(!dump.contains(needle), "{needle} in {dump}");
            }
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn the_runtime_role_cannot_bypass_the_audit_guard() {
            let admin = PgConnectOptions::from_str(&std::env::var(ENV).unwrap()).unwrap();
            let suffix = Uuid::new_v4().simple().to_string();
            let (owner, runtime, other) = (
                format!("cc_owner_{suffix}"),
                format!("cc_runtime_{suffix}"),
                format!("cc_other_{suffix}"),
            );
            let (member, sup, runtime2) = (
                format!("cc_member_{suffix}"),
                format!("cc_super_{suffix}"),
                format!("cc_runtime2_{suffix}"),
            );
            let db_name = format!("cc_test_{suffix}");
            let mut conn = PgConnection::connect_with(&admin).await.unwrap();
            for sql in [
                format!("CREATE ROLE \"{owner}\" LOGIN PASSWORD 'pw'"),
                format!("CREATE ROLE \"{runtime}\" LOGIN PASSWORD 'pw'"),
                format!("CREATE ROLE \"{other}\" LOGIN PASSWORD 'pw'"),
                format!("CREATE ROLE \"{member}\" LOGIN PASSWORD 'pw'"),
                format!("CREATE ROLE \"{sup}\" LOGIN SUPERUSER PASSWORD 'pw'"),
                format!("CREATE ROLE \"{runtime2}\" LOGIN PASSWORD 'pw'"),
                format!("CREATE DATABASE \"{db_name}\" OWNER \"{owner}\""),
            ] {
                sqlx::query(&sql).execute(&mut conn).await.unwrap();
            }
            let pool_of = |role: &str| {
                let options = admin
                    .clone()
                    .username(role)
                    .password("pw")
                    .database(&db_name);
                async move {
                    PgPoolOptions::new()
                        .max_connections(2)
                        .acquire_timeout(Duration::from_secs(10))
                        .connect_with(options)
                        .await
                        .unwrap()
                }
            };
            let owner_pool = pool_of(&owner).await;
            migrate::run(&owner_pool).await.unwrap();
            migrate::grant_runtime_role(&owner_pool, &runtime)
                .await
                .unwrap();
            // The grant step is repeatable and refuses odd role names.
            migrate::grant_runtime_role(&owner_pool, &runtime)
                .await
                .unwrap();
            assert!(
                migrate::grant_runtime_role(&owner_pool, "x\"; DROP TABLE users; --")
                    .await
                    .is_err()
            );
            let rt = pool_of(&runtime).await;
            let stranger = pool_of(&other).await;

            // The server's role starts, reads and writes everything it needs.
            migrate::verify(&rt).await.unwrap();
            assert_eq!(
                migrate::audit_guard_is_advisory(&rt).await.unwrap(),
                Some(false)
            );
            assert_eq!(
                migrate::audit_guard_is_advisory(&owner_pool).await.unwrap(),
                Some(true)
            );
            let (scope, _) = org(&rt, "r1").await;
            audit::record(
                &rt,
                &AuditCtx::system(),
                &NewEvent::new("file.purge").org(scope.org_id()),
            )
            .await
            .unwrap();
            let now = OffsetDateTime::now_utc();
            for (days, n) in [(400, 5), (100, 5)] {
                seed_equal(
                    &owner_pool,
                    scope.org_id(),
                    n,
                    now - time::Duration::days(days),
                    "file.purge",
                )
                .await;
            }

            // An insert cannot forge the time or the counter.
            sqlx::query(
                "INSERT INTO audit_events (id, action, result, created_at, last_at, event_count) \
                 VALUES (gen_random_uuid(), 'file.purge', 'success', '2000-01-01', '2000-01-01', 99)",
            )
            .execute(&rt)
            .await
            .unwrap();
            assert_eq!(
                rows(&rt, "SELECT count(*) FROM audit_events WHERE created_at < '2001-01-01' OR event_count = 99").await,
                0,
                "the insert guard resets time and counter"
            );

            // ... but cannot touch the history of the log or the table itself.
            for sql in [
                "DELETE FROM audit_events",
                "TRUNCATE audit_events",
                "UPDATE audit_events SET action = 'member.add'",
                "ALTER TABLE audit_events DISABLE TRIGGER ALL",
                "ALTER TABLE audit_events DISABLE TRIGGER audit_events_no_update_delete",
                "DROP TRIGGER audit_events_no_update_delete ON audit_events",
                "DROP TABLE audit_events",
                "SET session_replication_role = replica",
                "DELETE FROM _sqlx_migrations",
                "CREATE OR REPLACE FUNCTION audit_purge(timestamptz, integer) RETURNS bigint AS 'SELECT 0::bigint' LANGUAGE sql",
            ] {
                assert!(
                    sqlx::query(sql).execute(&rt).await.is_err(),
                    "the runtime role ran: {sql}"
                );
            }
            // Setting the old session switch changes nothing for this role.
            let mut one = rt.acquire().await.unwrap();
            sqlx::query("SELECT set_config('craftcloud.audit_purge', 'on', false)")
                .execute(&mut *one)
                .await
                .unwrap();
            assert!(
                sqlx::query("DELETE FROM audit_events")
                    .execute(&mut *one)
                    .await
                    .is_err()
            );
            drop(one);
            assert_eq!(rows(&rt, "SELECT count(*) FROM audit_events").await, 12);

            // Retention goes through `audit_purge`, which never reaches below 30 days,
            // whatever cutoff is passed.
            let removed: i64 =
                sqlx::query_scalar("SELECT audit_purge('infinity'::timestamptz, 1000)")
                    .fetch_one(&rt)
                    .await
                    .unwrap();
            assert_eq!(removed, 10);
            assert_eq!(rows(&rt, "SELECT count(*) FROM audit_events").await, 2);
            // A role without the grant cannot call it.
            assert!(
                sqlx::query("SELECT audit_purge('infinity'::timestamptz, 1)")
                    .execute(&stranger)
                    .await
                    .is_err()
            );
            // Nor can it read or write the table (no grant at all).
            assert!(
                sqlx::query("SELECT 1 FROM audit_events")
                    .execute(&stranger)
                    .await
                    .is_err()
            );

            // Roles that could undo the guard are refused, and the grant checks itself.
            sqlx::query(&format!("GRANT \"{owner}\" TO \"{member}\""))
                .execute(&mut conn)
                .await
                .unwrap();
            assert!(matches!(
                migrate::grant_runtime_role(&owner_pool, &member).await,
                Err(migrate::GrantError::Privileged(_))
            ));
            assert!(matches!(
                migrate::grant_runtime_role(&owner_pool, &sup).await,
                Err(migrate::GrantError::Privileged(_))
            ));
            assert_eq!(
                migrate::grant_runtime_role(&owner_pool, &owner).await,
                Err(migrate::GrantError::SameAsOwner)
            );
            assert_eq!(
                migrate::grant_runtime_role(&owner_pool, &format!("nobody_{suffix}")).await,
                Err(migrate::GrantError::UnknownRole)
            );
            // The startup check: a member of the owner and a superuser are warned about.
            let member_pool = pool_of(&member).await;
            assert_eq!(
                migrate::audit_guard_is_advisory(&member_pool)
                    .await
                    .unwrap(),
                Some(true)
            );
            let sup_pool = pool_of(&sup).await;
            assert_eq!(
                migrate::audit_guard_is_advisory(&sup_pool).await.unwrap(),
                Some(true)
            );
            // A migrating role that does not own the tables cannot grant: the
            // statements only warn, so the check afterwards fails the call.
            // (it has some rights on the objects, so PostgreSQL only warns)
            for sql in [
                "GRANT SELECT ON ALL TABLES IN SCHEMA public TO PUBLIC".to_owned(),
                "GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO PUBLIC".to_owned(),
            ] {
                sqlx::query(&sql).execute(&owner_pool).await.unwrap();
            }
            sqlx::query("GRANT EXECUTE ON FUNCTION audit_purge(timestamptz, integer) TO PUBLIC")
                .execute(&owner_pool)
                .await
                .unwrap();
            assert!(matches!(
                migrate::grant_runtime_role(&stranger, &runtime2).await,
                Err(migrate::GrantError::PrivilegesNotApplied(_))
            ));

            for pool in [rt, stranger, owner_pool, member_pool, sup_pool] {
                pool.close().await;
            }
            for sql in [
                format!("DROP DATABASE \"{db_name}\" WITH (FORCE)"),
                format!("DROP ROLE \"{owner}\""),
                format!("DROP ROLE \"{runtime}\""),
                format!("DROP ROLE \"{other}\""),
                format!("DROP ROLE \"{member}\""),
                format!("DROP ROLE \"{sup}\""),
                format!("DROP ROLE \"{runtime2}\""),
            ] {
                sqlx::query(&sql).execute(&mut conn).await.unwrap();
            }
        }
    }
}
