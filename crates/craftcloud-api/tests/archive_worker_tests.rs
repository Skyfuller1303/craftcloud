//! PostgreSQL integration checks for AC-12 archive worker guarantees.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::{path::PathBuf, time::Duration};

use common::TestEnv;
use craftcloud_api::archive_transport::{
    ArchiveTransportError, upload_intent_verified, upload_verified,
};
use craftcloud_api::archive_worker::{
    ArchiveBatch, ArchiveVerifyBudget, archive_batch, archive_batch_with_verify_budget,
};
use craftcloud_db::maintenance::{self, ArchiveFinish};
use craftcloud_storage::{
    ArchiveCrypto, LocalStore, ObjectHead, ObjectInfo, ObjectKey, ObjectStore, ReadSource,
    StorageError, TempInfo, TempObject, WriteSink,
};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    file_id: Uuid,
    old_blob: Uuid,
    old_key: ObjectKey,
    head_key: ObjectKey,
}

struct MutatingArchive {
    store: LocalStore,
    source_path: PathBuf,
    changed_bytes: &'static [u8],
    mutate_on_begin: bool,
    fail_readback: bool,
}

#[allow(async_fn_in_trait)]
impl ObjectStore for MutatingArchive {
    type Temp = TempObject;
    type Reader = craftcloud_storage::ObjectReader;

    async fn begin_write(&self, max_bytes: u64) -> Result<Self::Temp, StorageError> {
        self.store.begin_write(max_bytes).await
    }

    async fn begin_write_as(
        &self,
        key: ObjectKey,
        max_bytes: u64,
    ) -> Result<Self::Temp, StorageError> {
        if self.mutate_on_begin {
            let mut permissions = std::fs::metadata(&self.source_path)
                .map_err(craftcloud_storage::map_io_error)?
                .permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            std::fs::set_permissions(&self.source_path, permissions)
                .map_err(craftcloud_storage::map_io_error)?;
            std::fs::write(&self.source_path, self.changed_bytes)
                .map_err(craftcloud_storage::map_io_error)?;
        }
        self.store.begin_write_as(key, max_bytes).await
    }

    async fn finalize(
        &self,
        temp: Self::Temp,
        expected_len: u64,
        expected_sha256: &craftcloud_storage::Sha256Digest,
    ) -> Result<ObjectKey, StorageError> {
        self.store
            .finalize(temp, expected_len, expected_sha256)
            .await
    }

    async fn abort(&self, temp: Self::Temp) {
        self.store.abort(temp).await;
    }

    async fn open_read(
        &self,
        key: &ObjectKey,
        range: Option<craftcloud_storage::ByteRange>,
    ) -> Result<Self::Reader, StorageError> {
        if self.fail_readback {
            return Err(StorageError::Io(std::io::Error::other(
                "injected readback failure",
            )));
        }
        self.store.open_read(key, range).await
    }

    async fn head(&self, key: &ObjectKey) -> Result<ObjectHead, StorageError> {
        self.store.head(key).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), StorageError> {
        self.store.delete(key).await
    }

    async fn verify(
        &self,
        key: &ObjectKey,
        expected_sha256: &craftcloud_storage::Sha256Digest,
    ) -> Result<(), StorageError> {
        self.store.verify(key, expected_sha256).await
    }

    async fn list_objects(
        &self,
        after: Option<&ObjectKey>,
        limit: usize,
    ) -> Result<Vec<ObjectInfo>, StorageError> {
        self.store.list_objects(after, limit).await
    }

    async fn list_temp(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TempInfo>, StorageError> {
        self.store.list_temp(after, limit).await
    }

    async fn remove_temp(&self, id: &str) -> Result<(), StorageError> {
        self.store.remove_temp(id).await
    }
}

async fn fixture(
    env: &TestEnv,
    now: OffsetDateTime,
    old_digest_override: Option<[u8; 32]>,
) -> Fixture {
    fixture_with_old_bytes(
        env,
        now,
        b"superseded private design bytes",
        old_digest_override,
    )
    .await
}

async fn fixture_with_old_bytes(
    env: &TestEnv,
    now: OffsetDateTime,
    old_bytes: &[u8],
    old_digest_override: Option<[u8; 32]>,
) -> Fixture {
    let local = LocalStore::open(&env.storage_dir).await.unwrap();
    let org_id: Uuid = sqlx::query_scalar("SELECT id FROM organizations WHERE slug='operator-org'")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    let workspace_id = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,org_id,slug,name) VALUES($1,$2,$3,'Archive test')")
        .bind(workspace_id)
        .bind(org_id)
        .bind(format!("archive-{}", Uuid::new_v4().simple()))
        .execute(&env.pool)
        .await
        .unwrap();
    let project_id = Uuid::new_v4();
    sqlx::query("INSERT INTO projects(id,org_id,workspace_id,name,name_key,created_by_user_id,created_at,updated_by_user_id,updated_at) VALUES($1,$2,$3,$4,$5,$6,$7,$6,$7)")
        .bind(project_id)
        .bind(org_id)
        .bind(workspace_id)
        .bind(format!("Archive {}", Uuid::new_v4().simple()))
        .bind(Uuid::new_v4().simple().to_string())
        .bind(env.operator_id)
        .bind(now - time::Duration::days(3))
        .execute(&env.pool)
        .await
        .unwrap();
    let file_id = Uuid::new_v4();
    sqlx::query("INSERT INTO files(id,org_id,workspace_id,project_id,name,name_key,app,format,created_by_user_id,created_at,updated_by_user_id,updated_at) VALUES($1,$2,$3,$4,'Design','design','photocraft','pcraft',$5,$6,$5,$6)")
        .bind(file_id)
        .bind(org_id)
        .bind(workspace_id)
        .bind(project_id)
        .bind(env.operator_id)
        .bind(now - time::Duration::days(3))
        .execute(&env.pool)
        .await
        .unwrap();

    let head_bytes = b"current local head bytes";
    let old_key = put(&local, old_bytes).await;
    let head_key = put(&local, head_bytes).await;
    let old_blob = Uuid::new_v4();
    let head_blob = Uuid::new_v4();
    let old_hash: [u8; 32] =
        old_digest_override.unwrap_or_else(|| Sha256::digest(old_bytes).into());
    let head_hash: [u8; 32] = Sha256::digest(head_bytes).into();
    for (blob_id, key, bytes, digest, created_at) in [
        (
            old_blob,
            &old_key,
            old_bytes,
            old_hash,
            now - time::Duration::days(2),
        ),
        (
            head_blob,
            &head_key,
            head_bytes.as_slice(),
            head_hash,
            now - time::Duration::days(1),
        ),
    ] {
        sqlx::query("INSERT INTO blobs(id,org_id,workspace_id,object_key,size_bytes,sha256,state,created_at) VALUES($1,$2,$3,$4,$5,$6,'committed',$7)")
            .bind(blob_id)
            .bind(org_id)
            .bind(workspace_id)
            .bind(key.as_str())
            .bind(i64::try_from(bytes.len()).unwrap())
            .bind(digest.as_slice())
            .bind(created_at)
            .execute(&env.pool)
            .await
            .unwrap();
    }
    let old_version = Uuid::new_v4();
    let head_version = Uuid::new_v4();
    for (version_id, number, blob_id, digest, bytes, base, created_at) in [
        (
            old_version,
            1_i64,
            old_blob,
            old_hash,
            old_bytes,
            None,
            now - time::Duration::days(2),
        ),
        (
            head_version,
            2_i64,
            head_blob,
            head_hash,
            head_bytes.as_slice(),
            Some(old_version),
            now - time::Duration::days(1),
        ),
    ] {
        sqlx::query("INSERT INTO file_versions(id,org_id,workspace_id,project_id,file_id,number,blob_id,size_bytes,sha256,base_version_id,created_by_user_id,created_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)")
            .bind(version_id)
            .bind(org_id)
            .bind(workspace_id)
            .bind(project_id)
            .bind(file_id)
            .bind(number)
            .bind(blob_id)
            .bind(i64::try_from(bytes.len()).unwrap())
            .bind(digest.as_slice())
            .bind(base)
            .bind(env.operator_id)
            .bind(created_at)
            .execute(&env.pool)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE files SET head_version_id=$2,size_bytes=$3,sha256=$4 WHERE id=$1")
        .bind(file_id)
        .bind(head_version)
        .bind(i64::try_from(head_bytes.len()).unwrap())
        .bind(head_hash.as_slice())
        .execute(&env.pool)
        .await
        .unwrap();
    let mut usage = craftcloud_db::members::OrgTx::begin(&env.pool, org_id)
        .await
        .unwrap()
        .unwrap();
    usage.recompute_usage().await.unwrap();
    usage.commit().await.unwrap();
    Fixture {
        file_id,
        old_blob,
        old_key,
        head_key,
    }
}

async fn put(store: &LocalStore, bytes: &[u8]) -> ObjectKey {
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    let mut temp = store.begin_write(bytes.len() as u64).await.unwrap();
    temp.write(bytes).await.unwrap();
    store
        .finalize(temp, bytes.len() as u64, &digest)
        .await
        .unwrap()
}

async fn read_all(store: &LocalStore, key: &ObjectKey) -> Vec<u8> {
    let mut reader = store.open_read(key, None).await.unwrap();
    let mut bytes = Vec::new();
    while let Some(chunk) = reader.next_chunk().await.unwrap() {
        bytes.extend_from_slice(&chunk);
    }
    bytes
}

async fn archive_store() -> (LocalStore, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("cc-ac12-archive-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    (LocalStore::open(&dir).await.unwrap(), dir)
}

fn archive_object_path(root: &std::path::Path, key: &ObjectKey) -> PathBuf {
    let value = key.as_str();
    root.join("objects")
        .join(&value[..2])
        .join(&value[2..4])
        .join(value)
}

async fn large_archived_fixture(
    env: &TestEnv,
    now: OffsetDateTime,
) -> (Fixture, LocalStore, PathBuf, ArchiveCrypto) {
    let bytes = vec![0x5a; 256 * 1024];
    let fixture = fixture_with_old_bytes(env, now, &bytes, None).await;
    let local = LocalStore::open(&env.storage_dir).await.unwrap();
    let (archive, archive_dir) = archive_store().await;
    let crypto = ArchiveCrypto::from_server_key(&[19; 32]).unwrap();
    archive_batch_with_verify_budget(
        &env.pool,
        &local,
        &archive,
        &crypto,
        now,
        Duration::ZERO,
        ArchiveVerifyBudget {
            max_bytes: 64 * 1024,
            max_time: Duration::from_secs(5),
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE blobs SET archive_verified_at=NULL WHERE id=$1")
        .bind(fixture.old_blob)
        .execute(&env.pool)
        .await
        .unwrap();
    archive_batch_with_verify_budget(
        &env.pool,
        &local,
        &archive,
        &crypto,
        now + time::Duration::seconds(1),
        Duration::ZERO,
        ArchiveVerifyBudget {
            max_bytes: 64 * 1024,
            max_time: Duration::from_secs(5),
        },
    )
    .await
    .unwrap();
    (fixture, archive, archive_dir, crypto)
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn large_archive_verification_resumes_across_bounded_runs() {
    let env = TestEnv::new().await;
    let now = OffsetDateTime::now_utc();
    let (fixture, archive, archive_dir, crypto) = large_archived_fixture(&env, now).await;
    let budget = ArchiveVerifyBudget {
        max_bytes: 64 * 1024,
        max_time: Duration::from_secs(5),
    };
    let mut offset: i64 = sqlx::query_scalar("SELECT archive_verify_offset FROM blobs WHERE id=$1")
        .bind(fixture.old_blob)
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(offset, 64 * 1024);
    let claim: Option<Uuid> =
        sqlx::query_scalar("SELECT archive_verify_claim_token FROM blobs WHERE id=$1")
            .bind(fixture.old_blob)
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(claim, None);
    for run in 1..4 {
        archive_batch_with_verify_budget(
            &env.pool,
            &LocalStore::open(&env.storage_dir).await.unwrap(),
            &archive,
            &crypto,
            now + time::Duration::seconds(run),
            Duration::ZERO,
            budget,
        )
        .await
        .unwrap();
        let next: i64 = sqlx::query_scalar("SELECT archive_verify_offset FROM blobs WHERE id=$1")
            .bind(fixture.old_blob)
            .fetch_one(&env.pool)
            .await
            .unwrap();
        assert!(next > offset || (run == 3 && next == 0));
        offset = next;
    }
    let (verified_at, error): (Option<OffsetDateTime>, Option<String>) =
        sqlx::query_as("SELECT archive_verified_at, archive_error_code FROM blobs WHERE id=$1")
            .bind(fixture.old_blob)
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert!(verified_at.is_some());
    assert_eq!(error, None);
    drop(archive);
    std::fs::remove_dir_all(archive_dir).unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn empty_archive_verification_accepts_the_authenticated_empty_block() {
    let env = TestEnv::new().await;
    let now = OffsetDateTime::now_utc();
    let fixture = fixture_with_old_bytes(&env, now, b"", None).await;
    let local = LocalStore::open(&env.storage_dir).await.unwrap();
    let (archive, archive_dir) = archive_store().await;
    let crypto = ArchiveCrypto::from_server_key(&[19; 32]).unwrap();
    archive_batch(&env.pool, &local, &archive, &crypto, now, Duration::ZERO)
        .await
        .unwrap();
    sqlx::query("UPDATE blobs SET archive_verified_at=NULL WHERE id=$1")
        .bind(fixture.old_blob)
        .execute(&env.pool)
        .await
        .unwrap();
    archive_batch(
        &env.pool,
        &local,
        &archive,
        &crypto,
        now + time::Duration::seconds(1),
        Duration::ZERO,
    )
    .await
    .unwrap();
    let (offset, error, verified_at): (i64, Option<String>, Option<OffsetDateTime>) = sqlx::query_as(
        "SELECT archive_verify_offset, archive_error_code, archive_verified_at FROM blobs WHERE id=$1",
    ).bind(fixture.old_blob).fetch_one(&env.pool).await.unwrap();
    assert_eq!(offset, 0);
    assert_eq!(error, None);
    assert!(verified_at.is_some());
    drop(archive);
    std::fs::remove_dir_all(archive_dir).unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn resumed_archive_verification_detects_a_manipulated_block() {
    let env = TestEnv::new().await;
    let now = OffsetDateTime::now_utc();
    let (fixture, archive, archive_dir, crypto) = large_archived_fixture(&env, now).await;
    let archive_key: String =
        sqlx::query_scalar("SELECT archive_object_key FROM blobs WHERE id=$1")
            .bind(fixture.old_blob)
            .fetch_one(&env.pool)
            .await
            .unwrap();
    let key = ObjectKey::parse(&archive_key).unwrap();
    let path = archive_object_path(&archive_dir, &key);
    let mut ciphertext = std::fs::read(&path).unwrap();
    let block_offset = craftcloud_storage::CHUNK_BYTES + 16;
    ciphertext[block_offset] ^= 1;
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&path, permissions).unwrap();
    std::fs::write(path, ciphertext).unwrap();
    archive_batch_with_verify_budget(
        &env.pool,
        &LocalStore::open(&env.storage_dir).await.unwrap(),
        &archive,
        &crypto,
        now + time::Duration::seconds(1),
        Duration::ZERO,
        ArchiveVerifyBudget {
            max_bytes: 64 * 1024,
            max_time: Duration::from_secs(5),
        },
    )
    .await
    .unwrap();
    let (offset, error): (i64, Option<String>) =
        sqlx::query_as("SELECT archive_verify_offset, archive_error_code FROM blobs WHERE id=$1")
            .bind(fixture.old_blob)
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(offset, 0);
    assert_eq!(error.as_deref(), Some("ARCHIVE_INTEGRITY_FAILED"));
    drop(archive);
    std::fs::remove_dir_all(archive_dir).unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn archive_batch_keeps_head_local_and_archives_old_version_encrypted() {
    let env = TestEnv::new().await;
    let now = OffsetDateTime::now_utc();
    let fixture = fixture(&env, now, None).await;
    let local = LocalStore::open(&env.storage_dir).await.unwrap();
    let (archive, archive_dir) = archive_store().await;
    let crypto = ArchiveCrypto::from_server_key(&[19; 32]).unwrap();

    let result = archive_batch(&env.pool, &local, &archive, &crypto, now, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(
        result,
        ArchiveBatch {
            attempted: 1,
            archived: 1,
            retried: 0
        }
    );
    assert_eq!(
        read_all(&local, &fixture.head_key).await,
        b"current local head bytes"
    );
    assert!(local.head(&fixture.old_key).await.is_err());
    let (tier, archive_key, wrapped): (String, String, Vec<u8>) = sqlx::query_as(
        "SELECT tier_state, archive_object_key, archive_wrapped_key FROM blobs WHERE id=$1",
    )
    .bind(fixture.old_blob)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(tier, "archived");
    let key = ObjectKey::parse(&archive_key).unwrap();
    assert_ne!(
        read_all(&archive, &key).await,
        b"superseded private design bytes"
    );
    assert!(!wrapped.is_empty());
    let (verified_at, verify_error): (Option<OffsetDateTime>, Option<String>) =
        sqlx::query_as("SELECT archive_verified_at, archive_error_code FROM blobs WHERE id=$1")
            .bind(fixture.old_blob)
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert!(verified_at.is_some());
    assert_eq!(verify_error, None);

    archive.delete(&key).await.unwrap();
    archive_batch(
        &env.pool,
        &local,
        &archive,
        &crypto,
        now + time::Duration::hours(1) + time::Duration::minutes(1),
        Duration::ZERO,
    )
    .await
    .unwrap();
    let verify_error: Option<String> =
        sqlx::query_scalar("SELECT archive_error_code FROM blobs WHERE id=$1")
            .bind(fixture.old_blob)
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(verify_error.as_deref(), Some("ARCHIVE_MISSING"));
    let audit_events_before: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='archive.verification_failed' AND target_id=$1",
    )
    .bind(fixture.old_blob)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(audit_events_before, 1);
    assert!(
        maintenance::record_archive_head_check(
            &env.pool,
            fixture.old_blob,
            now + time::Duration::hours(1) + time::Duration::minutes(30),
        )
        .await
        .unwrap()
    );
    let verify_error: Option<String> =
        sqlx::query_scalar("SELECT archive_error_code FROM blobs WHERE id=$1")
            .bind(fixture.old_blob)
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(verify_error.as_deref(), Some("ARCHIVE_MISSING"));
    archive_batch(
        &env.pool,
        &local,
        &archive,
        &crypto,
        now + time::Duration::hours(2) + time::Duration::minutes(2),
        Duration::ZERO,
    )
    .await
    .unwrap();
    let audit_events_after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='archive.verification_failed' AND target_id=$1",
    )
    .bind(fixture.old_blob)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(audit_events_after, audit_events_before);
    drop(archive);
    std::fs::remove_dir_all(archive_dir).unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn archive_failure_before_commit_keeps_local_object() {
    let env = TestEnv::new().await;
    let now = OffsetDateTime::now_utc();
    let fixture = fixture(&env, now, Some([0x91; 32])).await;
    let local = LocalStore::open(&env.storage_dir).await.unwrap();
    let (archive, archive_dir) = archive_store().await;
    let crypto = ArchiveCrypto::from_server_key(&[23; 32]).unwrap();

    let result = archive_batch(&env.pool, &local, &archive, &crypto, now, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(
        result,
        ArchiveBatch {
            attempted: 1,
            archived: 0,
            retried: 1
        }
    );
    assert_eq!(
        read_all(&local, &fixture.old_key).await,
        b"superseded private design bytes"
    );
    let (tier, token, intent_key, intent_envelope): (
        String,
        Option<Uuid>,
        Option<String>,
        Option<Vec<u8>>,
    ) = sqlx::query_as(
        "SELECT tier_state, archive_claim_token, archive_object_key, archive_wrapped_key \
             FROM blobs WHERE id=$1",
    )
    .bind(fixture.old_blob)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(tier, "local");
    assert_eq!(token, None);
    assert!(intent_key.is_some());
    assert!(intent_envelope.is_some());
    assert!(archive.list_objects(None, 10).await.unwrap().is_empty());

    let retried = archive_batch(
        &env.pool,
        &local,
        &archive,
        &crypto,
        now + time::Duration::minutes(2),
        Duration::ZERO,
    )
    .await
    .unwrap();
    assert_eq!(retried.retried, 1);
    let stored_intent: (String, Vec<u8>) =
        sqlx::query_as("SELECT archive_object_key, archive_wrapped_key FROM blobs WHERE id=$1")
            .bind(fixture.old_blob)
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(
        stored_intent,
        (intent_key.unwrap(), intent_envelope.unwrap())
    );
    assert!(archive.list_objects(None, 10).await.unwrap().is_empty());
    drop(archive);
    std::fs::remove_dir_all(archive_dir).unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn archive_batch_recovers_committed_pending_delete_after_local_copy_went_missing() {
    let env = TestEnv::new().await;
    let now = OffsetDateTime::now_utc();
    let fixture = fixture(&env, now, None).await;
    let local = LocalStore::open(&env.storage_dir).await.unwrap();
    let (archive, archive_dir) = archive_store().await;
    let crypto = ArchiveCrypto::from_server_key(&[29; 32]).unwrap();
    let old_digest: [u8; 32] = Sha256::digest(b"superseded private design bytes").into();
    let copy = upload_verified(
        &local,
        &archive,
        &crypto,
        fixture.old_blob,
        &fixture.old_key,
        b"superseded private design bytes".len() as u64,
        &old_digest,
    )
    .await
    .unwrap();
    let claim_token = Uuid::new_v4();
    sqlx::query("UPDATE blobs SET tier_state='archived',archive_object_key=$2,archive_wrapped_key=$3,archive_claim_token=$4,archive_claimed_at=$5,archive_local_delete_pending=true WHERE id=$1")
        .bind(fixture.old_blob)
        .bind(copy.object_key.as_str())
        .bind(copy.envelope.to_bytes().as_slice())
        .bind(claim_token)
        .bind(now)
        .execute(&env.pool)
        .await
        .unwrap();
    local.delete(&fixture.old_key).await.unwrap();

    let result = archive_batch(&env.pool, &local, &archive, &crypto, now, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(result.archived, 1);
    let (pending, token): (bool, Option<Uuid>) = sqlx::query_as(
        "SELECT archive_local_delete_pending,archive_claim_token FROM blobs WHERE id=$1",
    )
    .bind(fixture.old_blob)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert!(!pending);
    assert_eq!(token, None);
    drop(archive);
    std::fs::remove_dir_all(archive_dir).unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn archive_batch_skips_when_global_maintenance_lock_is_held() {
    let env = TestEnv::new().await;
    let now = OffsetDateTime::now_utc();
    let fixture = fixture(&env, now, None).await;
    let local = LocalStore::open(&env.storage_dir).await.unwrap();
    let (archive, archive_dir) = archive_store().await;
    let crypto = ArchiveCrypto::from_server_key(&[31; 32]).unwrap();
    let lock = maintenance::try_lock(&env.pool).await.unwrap().unwrap();

    let result = archive_batch(&env.pool, &local, &archive, &crypto, now, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(result, ArchiveBatch::default());
    assert_eq!(
        read_all(&local, &fixture.old_key).await,
        b"superseded private design bytes"
    );
    let tier: String = sqlx::query_scalar("SELECT tier_state FROM blobs WHERE id=$1")
        .bind(fixture.old_blob)
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(tier, "local");
    lock.release().await.unwrap();
    drop(archive);
    std::fs::remove_dir_all(archive_dir).unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn archive_local_delete_backoff_does_not_starve_thirty_third_pending_blob() {
    let env = TestEnv::new().await;
    let now = OffsetDateTime::now_utc();
    let fixture = fixture(&env, now, None).await;
    let local = LocalStore::open(&env.storage_dir).await.unwrap();
    let (archive, archive_dir) = archive_store().await;
    let crypto = ArchiveCrypto::from_server_key(&[37; 32]).unwrap();
    let digest: [u8; 32] = Sha256::digest(b"superseded private design bytes").into();
    let copy = upload_verified(
        &local,
        &archive,
        &crypto,
        fixture.old_blob,
        &fixture.old_key,
        b"superseded private design bytes".len() as u64,
        &digest,
    )
    .await
    .unwrap();
    let token = Uuid::new_v4();
    sqlx::query("UPDATE blobs SET tier_state='archived',archive_object_key=$2,archive_wrapped_key=$3,archive_claim_token=$4,archive_claimed_at=$5,archive_local_delete_pending=true WHERE id=$1")
        .bind(fixture.old_blob)
        .bind(copy.object_key.as_str())
        .bind(copy.envelope.to_bytes().as_slice())
        .bind(token)
        .bind(now)
        .execute(&env.pool)
        .await
        .unwrap();
    let retry_at = now + time::Duration::minutes(5);
    for _ in 0..32 {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO blobs(id,org_id,workspace_id,quota_group_id,object_key,size_bytes,sha256,state,created_at,tier_state,archive_object_key,archive_wrapped_key,archive_claim_token,archive_claimed_at,archive_next_attempt_at,archive_local_delete_error_code,archive_local_delete_error_at,archive_local_delete_attempts,archive_local_delete_pending) SELECT $1,org_id,workspace_id,quota_group_id,$2,size_bytes,sha256,'committed',created_at,'archived',$3,$4,$5,$6,$7,'LOCAL_DELETE_FAILED',$6,1,true FROM blobs WHERE id=$8")
            .bind(id)
            .bind(Uuid::new_v4().simple().to_string())
            .bind(Uuid::new_v4().simple().to_string())
            .bind([0x42_u8; 96].as_slice())
            .bind(Uuid::new_v4())
            .bind(now)
            .bind(retry_at)
            .bind(fixture.old_blob)
            .execute(&env.pool)
            .await
            .unwrap();
    }

    let pending = maintenance::pending_archive_local_deletes(&env.pool, now, 32)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].1, fixture.old_blob);

    let result = archive_batch(&env.pool, &local, &archive, &crypto, now, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(result.archived, 1);
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM blobs WHERE archive_local_delete_pending AND archive_next_attempt_at=$1",
    )
    .bind(retry_at)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(remaining, 32);
    drop(archive);
    std::fs::remove_dir_all(archive_dir).unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn archive_batch_reuses_persisted_intent_after_remote_publish_crash() {
    let env = TestEnv::new().await;
    let now = OffsetDateTime::now_utc();
    let fixture = fixture(&env, now, None).await;
    let local = LocalStore::open(&env.storage_dir).await.unwrap();
    let (archive, archive_dir) = archive_store().await;
    let crypto = ArchiveCrypto::from_server_key(&[41; 32]).unwrap();
    let claim_token = Uuid::new_v4();
    let stale_before = now - time::Duration::hours(1);
    let mut tx = craftcloud_db::members::OrgTx::begin(&env.pool, org_for(&env).await)
        .await
        .unwrap()
        .unwrap();
    let claim = tx
        .maint_archive_claim(fixture.old_blob, now, now, stale_before, claim_token)
        .await
        .unwrap()
        .unwrap();
    assert!(claim.archive_object_key.is_none());
    let (envelope, _data_key) = crypto
        .create_data_key(
            fixture.old_blob,
            b"superseded private design bytes".len() as u64,
        )
        .unwrap();
    let archive_key = ObjectKey::generate();
    assert!(
        tx.maint_archive_intent(
            fixture.old_blob,
            claim_token,
            archive_key.as_str(),
            &envelope.to_bytes(),
        )
        .await
        .unwrap()
    );
    tx.commit().await.unwrap();

    let digest: [u8; 32] = Sha256::digest(b"superseded private design bytes").into();
    upload_intent_verified(
        &local,
        &archive,
        &crypto,
        fixture.old_blob,
        &fixture.old_key,
        &archive_key,
        &envelope,
        b"superseded private design bytes".len() as u64,
        &digest,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE blobs SET archive_claimed_at=$2 WHERE id=$1")
        .bind(fixture.old_blob)
        .bind(now - time::Duration::minutes(16))
        .execute(&env.pool)
        .await
        .unwrap();

    let result = archive_batch(
        &env.pool,
        &local,
        &archive,
        &crypto,
        now + time::Duration::minutes(16),
        Duration::ZERO,
    )
    .await
    .unwrap();
    assert_eq!(result.archived, 1);
    let (tier, stored_key, stored_envelope): (String, String, Vec<u8>) = sqlx::query_as(
        "SELECT tier_state,archive_object_key,archive_wrapped_key FROM blobs WHERE id=$1",
    )
    .bind(fixture.old_blob)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(tier, "archived");
    assert_eq!(stored_key, archive_key.as_str());
    assert_eq!(stored_envelope, envelope.to_bytes());
    assert_eq!(archive.list_objects(None, 10).await.unwrap().len(), 1);
    assert!(local.head(&fixture.old_key).await.is_err());
    drop(archive);
    std::fs::remove_dir_all(archive_dir).unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn archive_intent_does_not_publish_if_local_source_changes_after_preflight() {
    let env = TestEnv::new().await;
    let now = OffsetDateTime::now_utc();
    let fixture = fixture(&env, now, None).await;
    let local = LocalStore::open(&env.storage_dir).await.unwrap();
    let (archive, archive_dir) = archive_store().await;
    let crypto = ArchiveCrypto::from_server_key(&[43; 32]).unwrap();
    let original = b"superseded private design bytes";
    let changed = b"Xuperseded private design bytes";
    assert_eq!(original.len(), changed.len());
    let source_path = env
        .storage_dir
        .join("objects")
        .join(&fixture.old_key.as_str()[..2])
        .join(&fixture.old_key.as_str()[2..4])
        .join(fixture.old_key.as_str());
    let archive_key = ObjectKey::generate();
    let (envelope, _data_key) = crypto
        .create_data_key(fixture.old_blob, original.len() as u64)
        .unwrap();
    let expected_sha: [u8; 32] = Sha256::digest(original).into();
    let mutating = MutatingArchive {
        store: archive.clone(),
        source_path: source_path.clone(),
        changed_bytes: changed,
        mutate_on_begin: true,
        fail_readback: false,
    };

    let result = upload_intent_verified(
        &local,
        &mutating,
        &crypto,
        fixture.old_blob,
        &fixture.old_key,
        &archive_key,
        &envelope,
        original.len() as u64,
        &expected_sha,
    )
    .await;
    let published = archive.list_objects(None, 10).await.unwrap();
    let mut permissions = std::fs::metadata(&source_path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&source_path, permissions).unwrap();
    std::fs::write(&source_path, original).unwrap();
    let mut permissions = std::fs::metadata(&source_path).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&source_path, permissions).unwrap();

    assert!(matches!(result, Err(ArchiveTransportError::Integrity)));
    assert!(published.is_empty());
    drop(archive);
    std::fs::remove_dir_all(archive_dir).unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn published_archive_object_survives_uncertain_readback() {
    let env = TestEnv::new().await;
    let now = OffsetDateTime::now_utc();
    let fixture = fixture(&env, now, None).await;
    let local = LocalStore::open(&env.storage_dir).await.unwrap();
    let (archive, archive_dir) = archive_store().await;
    let archive = MutatingArchive {
        store: archive.clone(),
        source_path: env.storage_dir.join("unused-for-readback-failure"),
        changed_bytes: b"unused",
        mutate_on_begin: false,
        fail_readback: true,
    };
    let content = b"superseded private design bytes";
    let digest: [u8; 32] = Sha256::digest(content).into();
    let result = upload_verified(
        &local,
        &archive,
        &ArchiveCrypto::from_server_key(&[59; 32]).unwrap(),
        fixture.old_blob,
        &fixture.old_key,
        content.len() as u64,
        &digest,
    )
    .await;
    assert!(matches!(result, Err(ArchiveTransportError::Storage(_))));
    assert_eq!(archive.store.list_objects(None, 10).await.unwrap().len(), 1);
    drop(archive);
    std::fs::remove_dir_all(archive_dir).unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn archive_head_race_keeps_local_copy_and_persisted_remote_reference() {
    let env = TestEnv::new().await;
    let now = OffsetDateTime::now_utc();
    let fixture = fixture(&env, now, None).await;
    let local = LocalStore::open(&env.storage_dir).await.unwrap();
    let (archive, archive_dir) = archive_store().await;
    let crypto = ArchiveCrypto::from_server_key(&[47; 32]).unwrap();
    let claim_token = Uuid::new_v4();
    let org_id = org_for(&env).await;
    let mut claim_tx = craftcloud_db::members::OrgTx::begin(&env.pool, org_id)
        .await
        .unwrap()
        .unwrap();
    let _claim = claim_tx
        .maint_archive_claim(
            fixture.old_blob,
            now,
            now,
            now - time::Duration::hours(1),
            claim_token,
        )
        .await
        .unwrap()
        .unwrap();
    let (envelope, _data_key) = crypto
        .create_data_key(
            fixture.old_blob,
            b"superseded private design bytes".len() as u64,
        )
        .unwrap();
    let archive_key = ObjectKey::generate();
    assert!(
        claim_tx
            .maint_archive_intent(
                fixture.old_blob,
                claim_token,
                archive_key.as_str(),
                &envelope.to_bytes(),
            )
            .await
            .unwrap()
    );
    claim_tx.commit().await.unwrap();
    let digest: [u8; 32] = Sha256::digest(b"superseded private design bytes").into();
    upload_intent_verified(
        &local,
        &archive,
        &crypto,
        fixture.old_blob,
        &fixture.old_key,
        &archive_key,
        &envelope,
        b"superseded private design bytes".len() as u64,
        &digest,
    )
    .await
    .unwrap();

    sqlx::query("UPDATE files SET head_version_id=(SELECT id FROM file_versions WHERE file_id=$1 AND number=1) WHERE id=$1")
        .bind(fixture.file_id)
        .execute(&env.pool)
        .await
        .unwrap();
    let mut finish_tx = craftcloud_db::members::OrgTx::begin(&env.pool, org_id)
        .await
        .unwrap()
        .unwrap();
    let finish = finish_tx
        .maint_archive_finish(
            fixture.old_blob,
            claim_token,
            archive_key.as_str(),
            &envelope.to_bytes(),
            now,
            now,
        )
        .await
        .unwrap();
    finish_tx.commit().await.unwrap();
    assert_eq!(finish, ArchiveFinish::KeepLocal);
    assert_eq!(
        read_all(&local, &fixture.old_key).await,
        b"superseded private design bytes"
    );
    let (tier, stored_key): (String, String) =
        sqlx::query_as("SELECT tier_state,archive_object_key FROM blobs WHERE id=$1")
            .bind(fixture.old_blob)
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(tier, "local");
    assert_eq!(stored_key, archive_key.as_str());
    assert_eq!(archive.list_objects(None, 10).await.unwrap().len(), 1);
    drop(archive);
    std::fs::remove_dir_all(archive_dir).unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn stale_archive_recovery_releases_new_head_without_losing_intent() {
    let env = TestEnv::new().await;
    let now = OffsetDateTime::now_utc();
    let fixture = fixture(&env, now, None).await;
    let local = LocalStore::open(&env.storage_dir).await.unwrap();
    let (archive, archive_dir) = archive_store().await;
    let crypto = ArchiveCrypto::from_server_key(&[53; 32]).unwrap();
    let org_id = org_for(&env).await;
    let claim_token = Uuid::new_v4();
    let mut tx = craftcloud_db::members::OrgTx::begin(&env.pool, org_id)
        .await
        .unwrap()
        .unwrap();
    let claim = tx
        .maint_archive_claim(
            fixture.old_blob,
            now,
            now,
            now - time::Duration::hours(1),
            claim_token,
        )
        .await
        .unwrap()
        .unwrap();
    let (envelope, _) = crypto
        .create_data_key(fixture.old_blob, claim.size_bytes as u64)
        .unwrap();
    let archive_key = ObjectKey::generate();
    assert!(
        tx.maint_archive_intent(
            fixture.old_blob,
            claim_token,
            archive_key.as_str(),
            &envelope.to_bytes(),
        )
        .await
        .unwrap()
    );
    tx.commit().await.unwrap();
    sqlx::query("UPDATE blobs SET archive_claimed_at=$2 WHERE id=$1")
        .bind(fixture.old_blob)
        .bind(now - time::Duration::minutes(16))
        .execute(&env.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE files SET head_version_id=(SELECT id FROM file_versions WHERE file_id=$1 AND number=1) WHERE id=$1")
        .bind(fixture.file_id)
        .execute(&env.pool)
        .await
        .unwrap();

    let result = archive_batch(&env.pool, &local, &archive, &crypto, now, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(result.attempted, 0);
    assert_eq!(
        read_all(&local, &fixture.old_key).await,
        b"superseded private design bytes"
    );
    let (tier, token, stored_key, stored_envelope): (String, Option<Uuid>, String, Vec<u8>) =
        sqlx::query_as("SELECT tier_state,archive_claim_token,archive_object_key,archive_wrapped_key FROM blobs WHERE id=$1")
            .bind(fixture.old_blob)
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(tier, "local");
    assert_eq!(token, None);
    assert_eq!(stored_key, archive_key.as_str());
    assert_eq!(stored_envelope, envelope.to_bytes());
    assert!(archive.list_objects(None, 10).await.unwrap().is_empty());

    sqlx::query("UPDATE files SET head_version_id=(SELECT id FROM file_versions WHERE file_id=$1 AND number=2) WHERE id=$1")
        .bind(fixture.file_id)
        .execute(&env.pool)
        .await
        .unwrap();
    let replacement = Uuid::new_v4();
    let mut tx = craftcloud_db::members::OrgTx::begin(&env.pool, org_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        tx.maint_archive_claim(
            fixture.old_blob,
            now,
            now,
            now - time::Duration::hours(1),
            replacement,
        )
        .await
        .unwrap()
        .is_some()
    );
    assert!(
        !tx.maint_archive_release_stale_not_due(
            fixture.old_blob,
            claim_token,
            now,
            now - time::Duration::minutes(15),
        )
        .await
        .unwrap()
    );
    tx.commit().await.unwrap();
    drop(archive);
    std::fs::remove_dir_all(archive_dir).unwrap();
}

async fn org_for(env: &TestEnv) -> Uuid {
    sqlx::query_scalar("SELECT id FROM organizations WHERE slug='operator-org'")
        .fetch_one(&env.pool)
        .await
        .unwrap()
}
