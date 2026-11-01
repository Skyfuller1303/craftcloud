//! AC-12 archive tier state transitions against PostgreSQL.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{str::FromStr, time::Duration};

use craftcloud_db::{
    PgPool,
    maintenance::{self, ArchiveFinish, GcClaim},
    members::OrgTx,
    migrate,
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection};
use time::OffsetDateTime;
use uuid::Uuid;

struct TestDb {
    name: String,
    admin: PgConnectOptions,
    pool: PgPool,
}

impl TestDb {
    async fn new() -> Self {
        let admin = PgConnectOptions::from_str(
            &std::env::var("CC_TEST_DATABASE_URL").expect("CC_TEST_DATABASE_URL must be set"),
        )
        .unwrap();
        let name = format!("cc_test_{}", Uuid::new_v4().simple());
        let mut conn = PgConnection::connect_with(&admin).await.unwrap();
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

impl Drop for TestDb {
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
                    let _ =
                        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                            .execute(&mut conn)
                            .await;
                    let _ = conn.close().await;
                }
            });
        })
        .join();
    }
}

async fn seed_file(pool: &PgPool, now: OffsetDateTime) -> (Uuid, Uuid, Uuid) {
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,normalized_email,display_name) VALUES($1,$2,'Archive test')")
        .bind(user_id)
        .bind(format!("{}@example.test", Uuid::new_v4().simple()))
        .execute(pool)
        .await
        .unwrap();
    let org_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO organizations(id,slug,name,owner_user_id) VALUES($1,$2,'Archive test',$3)",
    )
    .bind(org_id)
    .bind(format!("org-{}", Uuid::new_v4().simple()))
    .bind(user_id)
    .execute(pool)
    .await
    .unwrap();
    let workspace_id = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,org_id,slug,name) VALUES($1,$2,'main','Main')")
        .bind(workspace_id)
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();
    let project_id = Uuid::new_v4();
    sqlx::query("INSERT INTO projects(id,org_id,workspace_id,name,name_key,created_by_user_id,created_at,updated_by_user_id,updated_at) VALUES($1,$2,$3,'Project','project',$4,$5,$4,$5)")
        .bind(project_id)
        .bind(org_id)
        .bind(workspace_id)
        .bind(user_id)
        .bind(now - time::Duration::days(4))
        .execute(pool)
        .await
        .unwrap();
    let file_id = Uuid::new_v4();
    sqlx::query("INSERT INTO files(id,org_id,workspace_id,project_id,name,name_key,app,format,created_by_user_id,created_at,updated_by_user_id,updated_at) VALUES($1,$2,$3,$4,'File','file','photocraft','pcf',$5,$6,$5,$6)")
        .bind(file_id)
        .bind(org_id)
        .bind(workspace_id)
        .bind(project_id)
        .bind(user_id)
        .bind(now - time::Duration::days(4))
        .execute(pool)
        .await
        .unwrap();
    let blob_id = Uuid::new_v4();
    let blob_key = Uuid::new_v4().simple().to_string();
    let sha = [7_u8; 32];
    sqlx::query("INSERT INTO blobs(id,org_id,workspace_id,object_key,size_bytes,sha256,state,created_at) VALUES($1,$2,$3,$4,75,$5,'committed',$6)")
        .bind(blob_id)
        .bind(org_id)
        .bind(workspace_id)
        .bind(blob_key)
        .bind(sha.as_slice())
        .bind(now - time::Duration::days(4))
        .execute(pool)
        .await
        .unwrap();
    let current_blob_id = Uuid::new_v4();
    let current_blob_key = Uuid::new_v4().simple().to_string();
    sqlx::query("INSERT INTO blobs(id,org_id,workspace_id,object_key,size_bytes,sha256,state,created_at) VALUES($1,$2,$3,$4,100,$5,'committed',$6)")
        .bind(current_blob_id)
        .bind(org_id)
        .bind(workspace_id)
        .bind(current_blob_key)
        .bind(sha.as_slice())
        .bind(now - time::Duration::days(2))
        .execute(pool)
        .await
        .unwrap();
    let old_version = Uuid::new_v4();
    let head_version = Uuid::new_v4();
    for (version_id, number, version_blob, base, created_at, size) in [
        (
            old_version,
            1_i64,
            blob_id,
            None,
            now - time::Duration::days(4),
            75_i64,
        ),
        (
            head_version,
            2_i64,
            current_blob_id,
            Some(old_version),
            now - time::Duration::days(2),
            100_i64,
        ),
    ] {
        sqlx::query("INSERT INTO file_versions(id,org_id,workspace_id,project_id,file_id,number,blob_id,size_bytes,sha256,base_version_id,created_by_user_id,created_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)")
            .bind(version_id)
            .bind(org_id)
            .bind(workspace_id)
            .bind(project_id)
            .bind(file_id)
            .bind(number)
            .bind(version_blob)
            .bind(size)
            .bind(sha.as_slice())
            .bind(base)
            .bind(user_id)
            .bind(created_at)
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE files SET head_version_id=$2,size_bytes=100,sha256=$3 WHERE id=$1")
        .bind(file_id)
        .bind(head_version)
        .bind(sha.as_slice())
        .execute(pool)
        .await
        .unwrap();
    (org_id, file_id, blob_id)
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn archive_claims_are_tenant_scoped_due_non_head_and_gc_delete_is_retryable() {
    let db = TestDb::new().await;
    assert!(
        !maintenance::archive_gc_inventory_complete(&db.pool)
            .await
            .unwrap()
    );
    let now = OffsetDateTime::now_utc();
    let (org_id, file_id, blob_id) = seed_file(&db.pool, now).await;
    let workspace_id: Uuid = sqlx::query_scalar("SELECT workspace_id FROM blobs WHERE id=$1")
        .bind(blob_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let independent_group: Uuid = sqlx::query_scalar(
        "SELECT quota_group_id FROM blobs WHERE workspace_id=$1 AND size_bytes=100",
    )
    .bind(workspace_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_ne!(independent_group, blob_id);
    let copy_blob_id = Uuid::new_v4();
    let copy_key = Uuid::new_v4().simple().to_string();
    sqlx::query("INSERT INTO blobs(id,org_id,workspace_id,quota_group_id,object_key,size_bytes,sha256,state,created_at) SELECT $1,org_id,workspace_id,quota_group_id,$2,size_bytes,sha256,'committed',$3 FROM blobs WHERE id=$4")
        .bind(copy_blob_id)
        .bind(copy_key)
        .bind(now)
        .bind(blob_id)
        .execute(&db.pool)
        .await
        .unwrap();
    let mut usage_tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    usage_tx.recompute_usage().await.unwrap();
    usage_tx.commit().await.unwrap();
    let initial_usage: (i64, i64) = sqlx::query_as(
        "SELECT w.committed_bytes, o.committed_bytes FROM workspaces w \
         JOIN organizations o ON o.id = w.org_id WHERE w.id = $1",
    )
    .bind(workspace_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(initial_usage, (175, 175));
    assert!(!maintenance::has_nonlocal_archive(&db.pool).await.unwrap());
    let cutoff = now - time::Duration::hours(24);
    let due = maintenance::archive_candidates(&db.pool, cutoff, now, &[org_id], &[], 10)
        .await
        .unwrap();
    assert_eq!(due, vec![(org_id, blob_id)]);

    let other_user = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,normalized_email,display_name) VALUES($1,$2,'Other tenant')")
        .bind(other_user)
        .bind(format!("{}@example.test", Uuid::new_v4().simple()))
        .execute(&db.pool)
        .await
        .unwrap();
    let other_org = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO organizations(id,slug,name,owner_user_id) VALUES($1,$2,'Other tenant',$3)",
    )
    .bind(other_org)
    .bind(format!("org-{}", Uuid::new_v4().simple()))
    .bind(other_user)
    .execute(&db.pool)
    .await
    .unwrap();
    let other_workspace = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,org_id,slug,name) VALUES($1,$2,'main','Main')")
        .bind(other_workspace)
        .bind(other_org)
        .execute(&db.pool)
        .await
        .unwrap();
    let cross_tenant_copy = sqlx::query(
        "INSERT INTO blobs(id,org_id,workspace_id,quota_group_id,object_key,size_bytes,sha256,state,created_at) \
         VALUES($1,$2,$3,$4,$5,75,$6,'committed',$7)",
    )
    .bind(Uuid::new_v4())
    .bind(other_org)
    .bind(other_workspace)
    .bind(blob_id)
    .bind(Uuid::new_v4().simple().to_string())
    .bind([7_u8; 32].as_slice())
    .bind(now)
    .execute(&db.pool)
    .await;
    assert!(matches!(
        cross_tenant_copy,
        Err(sqlx::Error::Database(error)) if error.constraint() == Some("blobs_quota_group_fk")
    ));
    let wrong_size_copy = sqlx::query(
        "INSERT INTO blobs(id,org_id,workspace_id,quota_group_id,object_key,size_bytes,sha256,state,created_at) \
         VALUES($1,$2,$3,$4,$5,76,$6,'committed',$7)",
    )
    .bind(Uuid::new_v4())
    .bind(org_id)
    .bind(workspace_id)
    .bind(blob_id)
    .bind(Uuid::new_v4().simple().to_string())
    .bind([7_u8; 32].as_slice())
    .bind(now)
    .execute(&db.pool)
    .await;
    assert!(matches!(
        wrong_size_copy,
        Err(sqlx::Error::Database(error)) if error.constraint() == Some("blobs_quota_group_fk")
    ));
    let mut other_tx = OrgTx::begin(&db.pool, other_org).await.unwrap().unwrap();
    assert!(
        other_tx
            .maint_archive_claim(
                blob_id,
                cutoff,
                now,
                now - time::Duration::minutes(5),
                Uuid::new_v4()
            )
            .await
            .unwrap()
            .is_none()
    );
    other_tx.commit().await.unwrap();

    let token = Uuid::new_v4();
    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    let claim = tx
        .maint_archive_claim(
            blob_id,
            cutoff,
            now,
            now - time::Duration::minutes(5),
            token,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.blob_id, blob_id);
    tx.commit().await.unwrap();

    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    assert_eq!(
        tx.maint_archive_finish(
            blob_id,
            token,
            "archive/opaque-key",
            &[9_u8; 48],
            cutoff,
            now
        )
        .await
        .unwrap(),
        ArchiveFinish::Archived
    );
    tx.commit().await.unwrap();

    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    assert_eq!(
        tx.maint_archive_local_delete_target(blob_id, token)
            .await
            .unwrap()
            .as_deref(),
        Some(claim.object_key.as_str())
    );
    assert!(
        tx.maint_archive_local_delete_finish(blob_id, token, now)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    let tier: String = sqlx::query_scalar("SELECT tier_state FROM blobs WHERE id=$1")
        .bind(blob_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(tier, "archived");
    assert!(maintenance::has_nonlocal_archive(&db.pool).await.unwrap());

    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    let copy_claim = tx
        .maint_gc_claim(copy_blob_id, now + time::Duration::days(1))
        .await
        .unwrap();
    assert!(matches!(copy_claim, GcClaim::Claimed(_)));
    let GcClaim::Claimed(gc_copy) = copy_claim else {
        return;
    };
    assert!(!gc_copy.counted);
    tx.commit().await.unwrap();
    assert!(
        maintenance::gc_finish(&db.pool, copy_blob_id)
            .await
            .unwrap()
    );
    let after_copy_usage: (i64, i64) = sqlx::query_as(
        "SELECT w.committed_bytes, o.committed_bytes FROM workspaces w \
         JOIN organizations o ON o.id = w.org_id WHERE w.id = $1",
    )
    .bind(workspace_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(after_copy_usage, (175, 175));

    sqlx::query("UPDATE files SET head_version_id=NULL WHERE id=$1")
        .bind(file_id)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE file_versions SET base_version_id=NULL WHERE file_id=$1")
        .bind(file_id)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM file_versions WHERE file_id=$1")
        .bind(file_id)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM files WHERE id=$1")
        .bind(file_id)
        .execute(&db.pool)
        .await
        .unwrap();
    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    assert!(matches!(
        tx.maint_gc_claim(blob_id, now + time::Duration::days(1))
            .await
            .unwrap(),
        GcClaim::Skipped
    ));
    tx.commit().await.unwrap();
    sqlx::query("UPDATE archive_gc_state SET inventory_complete=true WHERE id=true")
        .execute(&db.pool)
        .await
        .unwrap();
    let backup_id = Uuid::new_v4();
    let manifest_sha256 = "a".repeat(64);
    sqlx::query("INSERT INTO archive_backups (backup_id,manifest_sha256) VALUES ($1,$2)")
        .bind(backup_id)
        .bind(&manifest_sha256)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO archive_backup_refs (backup_id,blob_id,archive_object_key) VALUES ($1,$2,$3)",
    )
    .bind(backup_id)
    .bind(blob_id)
    .bind("archive/opaque-key")
    .execute(&db.pool)
    .await
    .unwrap();
    assert!(
        maintenance::archive_backup_release(&db.pool, backup_id, &"b".repeat(64))
            .await
            .unwrap()
            .is_none()
    );
    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    assert!(matches!(
        tx.maint_gc_claim(blob_id, now + time::Duration::days(1))
            .await
            .unwrap(),
        GcClaim::Skipped
    ));
    tx.commit().await.unwrap();
    assert_eq!(
        maintenance::archive_backup_release(&db.pool, backup_id, &manifest_sha256)
            .await
            .unwrap(),
        Some(1)
    );
    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    let claim = tx
        .maint_gc_claim(blob_id, now + time::Duration::days(1))
        .await
        .unwrap();
    assert!(matches!(claim, GcClaim::Claimed(_)));
    let GcClaim::Claimed(gc) = claim else { return };
    assert_eq!(gc.archive_object_key.as_deref(), Some("archive/opaque-key"));
    assert!(gc.counted);
    tx.commit().await.unwrap();
    let after_last_copy_usage: (i64, i64) = sqlx::query_as(
        "SELECT w.committed_bytes, o.committed_bytes FROM workspaces w \
         JOIN organizations o ON o.id = w.org_id WHERE w.id = $1",
    )
    .bind(workspace_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(after_last_copy_usage, (100, 100));
    assert!(!maintenance::gc_finish(&db.pool, blob_id).await.unwrap());
    assert!(
        maintenance::gc_archive_delete_failed(
            &db.pool,
            blob_id,
            "ARCHIVE_UNAVAILABLE",
            now + time::Duration::minutes(10)
        )
        .await
        .unwrap()
    );
    let status = maintenance::archive_status(&db.pool, Duration::from_secs(24 * 60 * 60))
        .await
        .unwrap();
    assert_eq!(status.pending_archive_deletes, 1);
    assert_eq!(status.archive_delete_errors, 1);
    assert_eq!(
        status.last_error_code.as_deref(),
        Some("ARCHIVE_UNAVAILABLE")
    );
    assert!(
        maintenance::gc_archive_delete_finish(&db.pool, blob_id)
            .await
            .unwrap()
    );
    assert!(maintenance::gc_finish(&db.pool, blob_id).await.unwrap());
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn archive_status_counts_only_verified_objects_due_non_heads_and_both_delete_errors() {
    let db = TestDb::new().await;
    let now = OffsetDateTime::now_utc();
    let (_org_id, _workspace_id, old_blob) = seed_file(&db.pool, now).await;

    // A persisted intent is not a confirmed remote object. Only the old
    // version is eligible; the current head must not remain pending.
    sqlx::query(
        "UPDATE blobs SET archive_object_key='opaque-intent', archive_wrapped_key=$2 WHERE id=$1",
    )
    .bind(old_blob)
    .bind(vec![9_u8; 120])
    .execute(&db.pool)
    .await
    .unwrap();
    let pending = maintenance::archive_status(&db.pool, Duration::from_secs(24 * 60 * 60))
        .await
        .unwrap();
    assert_eq!(pending.archive_bytes, 0);
    assert_eq!(pending.pending_archives, 1);

    sqlx::query("UPDATE blobs SET tier_state='archived', archive_verified_at=$2, archive_local_delete_pending=true, archive_local_delete_error_code='LOCAL_DELETE_FAILED', archive_local_delete_error_at=$2 WHERE id=$1")
        .bind(old_blob)
        .bind(now)
        .execute(&db.pool)
        .await
        .unwrap();
    let recalled = maintenance::archive_status(&db.pool, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(recalled.archive_bytes, 75);
    assert_eq!(recalled.pending_archives, 0);
    assert_eq!(recalled.pending_archive_deletes, 1);
    assert_eq!(recalled.archive_delete_errors, 1);

    sqlx::query("UPDATE blobs SET state='deleting', archive_delete_pending=true, archive_delete_error_code='REMOTE_DELETE_FAILED', archive_delete_error_at=$2 WHERE id=$1")
        .bind(old_blob)
        .bind(now + time::Duration::seconds(1))
        .execute(&db.pool)
        .await
        .unwrap();
    let deleting = maintenance::archive_status(&db.pool, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(deleting.archive_bytes, 0);
    assert_eq!(deleting.pending_archive_deletes, 1);
    assert_eq!(deleting.archive_delete_errors, 1);
    assert_eq!(
        deleting.last_error_code.as_deref(),
        Some("REMOTE_DELETE_FAILED")
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn interrupted_recall_is_reclaimed_with_a_new_fencing_token() {
    let db = TestDb::new().await;
    let now = OffsetDateTime::now_utc();
    let (org_id, _, blob_id) = seed_file(&db.pool, now).await;
    sqlx::query("UPDATE blobs SET tier_state='archived', archive_object_key='opaque', archive_wrapped_key=$2 WHERE id=$1")
        .bind(blob_id).bind([9_u8; 48].as_slice()).execute(&db.pool).await.unwrap();
    let old_token = Uuid::new_v4();
    let new_token = Uuid::new_v4();
    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    assert!(
        tx.maint_archive_recall_claim(blob_id, now, now - time::Duration::minutes(5), old_token)
            .await
            .unwrap()
            .is_some()
    );
    tx.commit().await.unwrap();
    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    assert!(
        tx.maint_archive_recall_claim(blob_id, now, now - time::Duration::minutes(5), new_token)
            .await
            .unwrap()
            .is_none()
    );
    tx.commit().await.unwrap();
    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    assert!(
        tx.maint_archive_recall_claim(
            blob_id,
            now + time::Duration::minutes(6),
            now + time::Duration::minutes(1),
            new_token
        )
        .await
        .unwrap()
        .is_some()
    );
    assert!(
        !tx.maint_archive_recall_finish(blob_id, old_token, now)
            .await
            .unwrap()
    );
    assert!(
        tx.maint_archive_recall_finish(blob_id, new_token, now)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn trash_keeps_head_local_but_older_versions_can_archive() {
    let db = TestDb::new().await;
    let now = OffsetDateTime::now_utc();
    let (org_id, file_id, old_blob) = seed_file(&db.pool, now).await;
    sqlx::query(
        "UPDATE files SET deleted_at=$2, deleted_by_user_id=created_by_user_id WHERE id=$1",
    )
    .bind(file_id)
    .bind(now)
    .execute(&db.pool)
    .await
    .unwrap();
    let candidates = maintenance::archive_candidates(
        &db.pool,
        now - time::Duration::days(1),
        now,
        &[org_id],
        &[],
        10,
    )
    .await
    .unwrap();
    assert_eq!(candidates, vec![(org_id, old_blob)]);
}
