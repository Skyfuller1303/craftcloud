//! Transaction tests for account security notifications.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::{str::FromStr, time::Duration};

use craftcloud_db::{
    PgPool,
    audit::AuditCtx,
    migrate,
    users::{self, NewUser},
};
use sqlx::{
    Connection,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use uuid::Uuid;

struct Db {
    name: String,
    admin: PgConnectOptions,
    pool: PgPool,
}
impl Db {
    async fn new() -> Self {
        let admin = PgConnectOptions::from_str(
            &std::env::var("CC_TEST_DATABASE_URL").expect("CC_TEST_DATABASE_URL"),
        )
        .unwrap();
        let name = format!("cc_test_{}", Uuid::new_v4().simple());
        let mut conn = sqlx::PgConnection::connect_with(&admin).await.unwrap();
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(admin.clone().database(&name))
            .await
            .unwrap();
        migrate::run(&pool).await.unwrap();
        Self { name, admin, pool }
    }
}
impl Drop for Db {
    fn drop(&mut self) {
        let admin = self.admin.clone();
        let name = self.name.clone();
        let _ = std::thread::spawn(move || {
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                rt.block_on(async {
                    if let Ok(mut c) = sqlx::PgConnection::connect_with(&admin).await {
                        let _ = sqlx::query(&format!(
                            "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
                        ))
                        .execute(&mut c)
                        .await;
                        let _ = c.close().await;
                    }
                });
            }
        })
        .join();
    }
}

async fn account(pool: &PgPool, email: &str) -> Uuid {
    users::insert(
        pool,
        &NewUser {
            normalized_email: email.to_owned(),
            display_name: "Test User".to_owned(),
        },
    )
    .await
    .unwrap()
    .id
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run cargo xtask db-test"]
async fn password_change_and_notice_commit_together_and_disabled_mode_queues_nothing() {
    let db = Db::new().await;
    let user = account(&db.pool, "notice@example.test").await;
    let now = time::OffsetDateTime::now_utc();
    let hash = "old-hash";
    sqlx::query("UPDATE users SET password_hash=$2 WHERE id=$1")
        .bind(user)
        .bind(hash)
        .execute(&db.pool)
        .await
        .unwrap();
    let sid = Uuid::new_v4();
    sqlx::query("INSERT INTO sessions(id,user_id,token_hash,created_at,last_seen_at,absolute_expires_at) VALUES($1,$2,$3,$4,$4,$5)").bind(sid).bind(user).bind([7u8;32].as_slice()).bind(now).bind(now+time::Duration::hours(1)).execute(&db.pool).await.unwrap();
    let audit = AuditCtx {
        actor_id: Some(user),
        request_id: None,
    };
    assert!(matches!(
        craftcloud_db::accounts::change_password_audited_with_email(
            &db.pool, user, hash, "new-hash", sid, now, &audit, true
        )
        .await
        .unwrap(),
        craftcloud_db::accounts::PasswordChangeResult::Changed(_)
    ));
    let (security_version,count):(i64,i64)=sqlx::query_as("SELECT u.security_version,(SELECT count(*) FROM outbound_emails WHERE actor_id=u.id AND template='security_notice') FROM users u WHERE id=$1").bind(user).fetch_one(&db.pool).await.unwrap();
    assert_eq!(security_version, 1);
    assert_eq!(count, 1);
    // A second account change with delivery disabled advances security state without an outbox row.
    assert!(matches!(
        craftcloud_db::accounts::change_password_audited_with_email(
            &db.pool,
            user,
            "new-hash",
            "next-hash",
            sid,
            now,
            &audit,
            false
        )
        .await
        .unwrap(),
        craftcloud_db::accounts::PasswordChangeResult::Changed(_)
    ));
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbound_emails WHERE actor_id=$1 AND template='security_notice'",
    )
    .bind(user)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(count, 1);

    let limited = account(&db.pool, "limited@example.test").await;
    sqlx::query("UPDATE users SET password_hash='old-hash' WHERE id=$1")
        .bind(limited)
        .execute(&db.pool)
        .await
        .unwrap();
    let mut tx = db.pool.begin().await.unwrap();
    for _ in 0..5 {
        assert!(
            craftcloud_db::mail_security::enqueue_in(
                &mut tx,
                limited,
                now,
                craftcloud_db::mail_security::SecurityEvent::PasswordChanged
            )
            .await
            .unwrap()
        );
    }
    tx.commit().await.unwrap();
    let limited_session = Uuid::new_v4();
    sqlx::query("INSERT INTO sessions(id,user_id,token_hash,created_at,last_seen_at,absolute_expires_at) VALUES($1,$2,$3,$4,$4,$5)").bind(limited_session).bind(limited).bind([8u8;32].as_slice()).bind(now).bind(now+time::Duration::hours(1)).execute(&db.pool).await.unwrap();
    // A refused notice never blocks the security action: the password changes,
    // no sixth message is queued and the dropped notice is counted.
    assert!(matches!(
        craftcloud_db::accounts::change_password_audited_with_email(
            &db.pool,
            limited,
            "old-hash",
            "still-changes",
            limited_session,
            now,
            &AuditCtx {
                actor_id: Some(limited),
                request_id: None
            },
            true
        )
        .await
        .unwrap(),
        craftcloud_db::accounts::PasswordChangeResult::Changed(_)
    ));
    let (hash, version): (String, i64) =
        sqlx::query_as("SELECT password_hash,security_version FROM users WHERE id=$1")
            .bind(limited)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(hash, "still-changes");
    assert_eq!(version, 1);
    let (queued, suppressed): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE status='queued'), count(*) FILTER (WHERE status='suppressed')          FROM outbound_emails WHERE actor_id=$1 AND template='security_notice'",
    )
    .bind(limited)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!((queued, suppressed), (5, 1));
    assert_eq!(
        craftcloud_db::email_outbox::status(&db.pool)
            .await
            .unwrap()
            .suppressed_count,
        1
    );

    let factor_user = account(&db.pool, "factor@example.test").await;
    let factor_session = Uuid::new_v4();
    let secret = vec![9u8; 32];
    sqlx::query("INSERT INTO sessions(id,user_id,token_hash,created_at,last_seen_at,absolute_expires_at) VALUES($1,$2,$3,$4,$4,$5)").bind(factor_session).bind(factor_user).bind([9u8;32].as_slice()).bind(now).bind(now+time::Duration::hours(1)).execute(&db.pool).await.unwrap();
    sqlx::query("INSERT INTO user_two_factor(user_id,secret_ciphertext,pending_session_id,pending_session_kind,pending_expires_at,created_at) VALUES($1,$2,$3,'browser',$4,$5)").bind(factor_user).bind(&secret).bind(factor_session).bind(now+time::Duration::minutes(10)).bind(now).execute(&db.pool).await.unwrap();
    assert_eq!(
        craftcloud_db::two_factor::confirm_setup_with_email(
            &db.pool,
            factor_user,
            factor_session,
            "browser",
            &secret,
            100,
            &[],
            now,
            &AuditCtx {
                actor_id: Some(factor_user),
                request_id: None
            },
            true
        )
        .await
        .unwrap(),
        craftcloud_db::two_factor::FactorChangeOutcome::Changed
    );
    assert_eq!(
        craftcloud_db::two_factor::reset_for_user_with_email(
            &db.pool,
            factor_user,
            Some(factor_session),
            Some("browser"),
            None,
            Some(&secret),
            None,
            now,
            &AuditCtx {
                actor_id: Some(factor_user),
                request_id: None
            },
            "auth.two_factor_disabled",
            true
        )
        .await
        .unwrap(),
        craftcloud_db::two_factor::FactorChangeOutcome::Changed
    );
    let (version,count):(i64,i64)=sqlx::query_as("SELECT security_version,(SELECT count(*) FROM outbound_emails WHERE actor_id=$1 AND template='security_notice') FROM users WHERE id=$1").bind(factor_user).fetch_one(&db.pool).await.unwrap();
    assert_eq!(version, 2);
    assert_eq!(count, 2);

    let device_user = account(&db.pool, "device@example.test").await;
    sqlx::query("UPDATE users SET password_hash='device-hash' WHERE id=$1")
        .bind(device_user)
        .execute(&db.pool)
        .await
        .unwrap();
    let device = craftcloud_db::device_sessions::NewDeviceSession {
        user_id: device_user,
        name: "test device".to_owned(),
        client_kind: "app".to_owned(),
        client_id: "fixture".to_owned(),
        refresh_hash: [3; 32],
        access_hash: [4; 32],
        now,
        access_expires_at: now + time::Duration::minutes(5),
        idle_expires_at: now + time::Duration::hours(1),
        absolute_expires_at: now + time::Duration::days(1),
    };
    assert!(matches!(
        craftcloud_db::device_sessions::create_for_verified_login_with_email(
            &db.pool,
            &device,
            "device-hash",
            &AuditCtx {
                actor_id: Some(device_user),
                request_id: None
            },
            true
        )
        .await
        .unwrap(),
        craftcloud_db::device_sessions::LoginOutcome::Created(_)
    ));
    let (refresh, _) = craftcloud_db::device_sessions::refresh(
        &db.pool,
        &[3; 32],
        &[5; 32],
        &[6; 32],
        now,
        std::time::Duration::from_secs(300),
        std::time::Duration::from_secs(3600),
        &AuditCtx {
            actor_id: Some(device_user),
            request_id: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        refresh,
        craftcloud_db::device_sessions::RefreshOutcome::Rotated
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbound_emails WHERE actor_id=$1 AND template='security_notice'",
    )
    .bind(device_user)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(count, 1);

    let mfa_user = account(&db.pool, "mfa-device@example.test").await;
    sqlx::query("UPDATE users SET password_hash='mfa-hash' WHERE id=$1")
        .bind(mfa_user)
        .execute(&db.pool)
        .await
        .unwrap();
    let mfa_secret = vec![2u8; 32];
    sqlx::query("INSERT INTO user_two_factor(user_id,secret_ciphertext,enabled_at,created_at) VALUES($1,$2,$3,$3)").bind(mfa_user).bind(&mfa_secret).bind(now).execute(&db.pool).await.unwrap();
    let challenge = [7u8; 32];
    sqlx::query("INSERT INTO two_factor_login_challenges(id_hash,user_id,password_hash_digest,login_kind,client_address,device_client_kind,device_client_id,device_name,created_at,expires_at) VALUES($1,$2,$3,'device','127.0.0.1','app','fixture','test device',$4,$5)").bind(challenge.as_slice()).bind(mfa_user).bind([8u8;32].as_slice()).bind(now).bind(now+time::Duration::minutes(2)).execute(&db.pool).await.unwrap();
    let mfa_device = craftcloud_db::device_sessions::NewDeviceSession {
        user_id: mfa_user,
        name: "test device".to_owned(),
        client_kind: "app".to_owned(),
        client_id: "fixture".to_owned(),
        refresh_hash: [10; 32],
        access_hash: [11; 32],
        now,
        access_expires_at: now + time::Duration::minutes(5),
        idle_expires_at: now + time::Duration::hours(1),
        absolute_expires_at: now + time::Duration::days(1),
    };
    assert!(matches!(
        craftcloud_db::two_factor::complete_device_login_with_email(
            &db.pool,
            &challenge,
            mfa_user,
            "127.0.0.1",
            "mfa-hash",
            &mfa_secret,
            craftcloud_db::two_factor::VerifiedFactor::TotpStep(100),
            &mfa_device,
            &AuditCtx {
                actor_id: Some(mfa_user),
                request_id: None
            },
            true
        )
        .await
        .unwrap(),
        craftcloud_db::two_factor::LoginOutcome::Created(_)
    ));
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbound_emails WHERE actor_id=$1 AND template='security_notice'",
    )
    .bind(mfa_user)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run cargo xtask db-test"]
async fn login_notices_are_limited_without_blocking_logins_or_later_security_changes() {
    let db = Db::new().await;
    let user = account(&db.pool, "flood@example.test").await;
    sqlx::query("UPDATE users SET password_hash='flood-hash' WHERE id=$1")
        .bind(user)
        .execute(&db.pool)
        .await
        .unwrap();
    let now = time::OffsetDateTime::now_utc();
    let audit = AuditCtx {
        actor_id: Some(user),
        request_id: None,
    };
    for index in 0..6_u8 {
        let outcome = craftcloud_db::sessions::create_for_verified_login_with_email(
            &db.pool,
            &craftcloud_db::sessions::NewSession {
                user_id: user,
                token_hash: [index + 1; 32],
                now: now + time::Duration::seconds(i64::from(index)),
                absolute_expires_at: now + time::Duration::hours(2),
            },
            "flood-hash",
            &audit,
            true,
        )
        .await
        .unwrap();
        assert!(
            matches!(outcome, craftcloud_db::sessions::LoginOutcome::Created(_)),
            "login {index} must succeed"
        );
    }
    let (sessions, queued, suppressed): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM sessions WHERE user_id=$1), \
         count(*) FILTER (WHERE status='queued'), count(*) FILTER (WHERE status='suppressed') \
         FROM outbound_emails WHERE actor_id=$1 AND template='security_notice'",
    )
    .bind(user)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!((sessions, queued, suppressed), (6, 5, 1));

    // The exhausted notice bucket does not stop a later password change.
    let keep = Uuid::new_v4();
    sqlx::query("INSERT INTO sessions(id,user_id,token_hash,created_at,last_seen_at,absolute_expires_at) VALUES($1,$2,$3,$4,$4,$5)").bind(keep).bind(user).bind([99u8;32].as_slice()).bind(now).bind(now+time::Duration::hours(1)).execute(&db.pool).await.unwrap();
    assert!(matches!(
        craftcloud_db::accounts::change_password_audited_with_email(
            &db.pool,
            user,
            "flood-hash",
            "flood-hash-2",
            keep,
            now + time::Duration::seconds(10),
            &audit,
            true
        )
        .await
        .unwrap(),
        craftcloud_db::accounts::PasswordChangeResult::Changed(_)
    ));
    // Notices are counted per event: the new-login flood does not suppress the
    // password-change notice.
    let password_notice: String = sqlx::query_scalar(
        "SELECT status FROM outbound_emails WHERE actor_id=$1 AND template='security_notice' \
         AND params->>'security_event'='Your password was changed.'",
    )
    .bind(user)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(password_notice, "queued");
    let status = craftcloud_db::email_outbox::status(&db.pool).await.unwrap();
    assert_eq!(status.suppressed_count, 1);
    assert_eq!(status.failed_count, 0);
    // Suppressed rows are not counted against the recipient or actor windows.
    let counted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbound_emails WHERE actor_id=$1 AND status<>'suppressed'",
    )
    .bind(user)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(counted, 6);
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run cargo xtask db-test"]
async fn verification_mail_budget_is_separate_from_notices_and_test_mail() {
    let db = Db::new().await;
    let actor = Uuid::new_v4();
    let now = time::OffsetDateTime::now_utc();
    let draft = |template: &str, recipient: String| craftcloud_db::email_outbox::EmailDraft {
        id: Uuid::new_v4(),
        recipient,
        template: template.to_owned(),
        language: "en".to_owned(),
        params: serde_json::json!({}),
        encrypted_token: None,
        expires_at: now + time::Duration::hours(1),
        actor_id: Some(actor),
        is_resend: false,
        now,
    };
    let mut tx = db.pool.begin().await.unwrap();
    for index in 0..30 {
        assert_eq!(
            craftcloud_db::email_outbox::enqueue_in(
                &mut tx,
                &draft("email_verification", format!("bulk-{index}@example.test"))
            )
            .await
            .unwrap(),
            craftcloud_db::email_outbox::EnqueueOutcome::Queued
        );
    }
    assert_eq!(
        craftcloud_db::email_outbox::enqueue_in(
            &mut tx,
            &draft("email_verification", "bulk-30@example.test".to_owned())
        )
        .await
        .unwrap(),
        craftcloud_db::email_outbox::EnqueueOutcome::ActorLimit
    );
    // The operator's own test mail still has its own budget.
    assert_eq!(
        craftcloud_db::email_outbox::enqueue_in(
            &mut tx,
            &draft("test", "operator@example.test".to_owned())
        )
        .await
        .unwrap(),
        craftcloud_db::email_outbox::EnqueueOutcome::Queued
    );
    tx.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run cargo xtask db-test"]
async fn purge_removes_old_terminal_mail_and_expired_tokens_but_keeps_last_success() {
    let db = Db::new().await;
    let user = account(&db.pool, "purge@example.test").await;
    let now = time::OffsetDateTime::now_utc();
    let old = now - time::Duration::days(8);
    let insert = |status: &'static str, created: time::OffsetDateTime, sent: bool| {
        let pool = db.pool.clone();
        async move {
            sqlx::query("INSERT INTO outbound_emails(id,recipient,template,language,expires_at,status,created_at,available_at,sent_at) VALUES($1,'purge@example.test','test','en',$2,$3,$2,$2,$4)")
                .bind(Uuid::new_v4())
                .bind(created)
                .bind(status)
                .bind(sent.then_some(created))
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    insert("sent", old - time::Duration::days(1), true).await;
    insert("sent", old, true).await; // newest success: kept for the status
    insert("failed", old, false).await;
    insert("expired", old, false).await;
    insert("suppressed", old, false).await;
    insert("failed", now, false).await; // recent: kept
    insert("queued", old, false).await; // not terminal: kept
    sqlx::query("INSERT INTO email_verification_tokens(token_hash,user_id,normalized_email,created_at,expires_at) VALUES($1,$2,'purge@example.test',$3,$4),($5,$2,'purge@example.test',$3,$6)")
        .bind([1u8;32].as_slice()).bind(user).bind(old).bind(old + time::Duration::hours(1))
        .bind([2u8;32].as_slice()).bind(now + time::Duration::hours(1))
        .execute(&db.pool).await.unwrap();
    sqlx::query("INSERT INTO pending_email_changes(user_id,original_email,normalized_email,token_hash,security_version,requested_at,expires_at) VALUES($1,'purge@example.test','next@example.test',$2,0,$3,$4)")
        .bind(user).bind([3u8;32].as_slice()).bind(old).bind(old + time::Duration::hours(1))
        .execute(&db.pool).await.unwrap();

    assert_eq!(
        craftcloud_db::email_outbox::purge_terminal(&db.pool)
            .await
            .unwrap(),
        4
    );
    assert_eq!(
        craftcloud_db::email_verification::delete_expired(&db.pool)
            .await
            .unwrap(),
        2
    );
    let status = craftcloud_db::email_outbox::status(&db.pool).await.unwrap();
    assert_eq!(status.failed_count, 1);
    assert_eq!(status.suppressed_count, 0);
    assert!(status.last_success_at.is_some());
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM outbound_emails")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(remaining, 3);
    let tokens: i64 = sqlx::query_scalar("SELECT count(*) FROM email_verification_tokens")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(tokens, 1);
}
