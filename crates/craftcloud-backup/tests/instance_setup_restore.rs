//! PostgreSQL round-trip coverage for AC-13 backup data.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::str::FromStr;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use craftcloud_auth::{decrypt_secret_for, encrypt_secret_for};
use craftcloud_backup::{
    ArchiveAccess, CreateOptions, RestoreOptions, create_with_archive, restore_with_archive,
    verify, verify_with_archive,
};
use craftcloud_db::{maintenance, migrate, setup};
use craftcloud_storage::{
    ArchiveCrypto, ArchiveStore, LocalStore, ObjectKey, ObjectStore, ReadSource, WriteSink,
};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgConnectOptions, PgConnection, PgPoolOptions};
use sqlx::{Connection, PgPool};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

const DATABASE_URL_ENV: &str = "CC_TEST_DATABASE_URL";
const SECRET_KEY: [u8; 32] = [0x5a; 32];
const TEST_SECRET: &str = "backup-test-secret";

fn parse_timestamp(value: &str) -> OffsetDateTime {
    OffsetDateTime::parse(value, &Rfc3339).expect("valid RFC 3339 timestamp")
}

struct TestDatabase {
    name: String,
    admin: PgConnectOptions,
    pool: PgPool,
}

impl TestDatabase {
    async fn new() -> Self {
        let url = std::env::var(DATABASE_URL_ENV).expect("CC_TEST_DATABASE_URL must be set");
        let admin = PgConnectOptions::from_str(&url).expect("valid PostgreSQL test URL");
        let name = format!("cc_test_{}", Uuid::new_v4().simple());
        let mut connection = PgConnection::connect_with(&admin)
            .await
            .expect("connect to PostgreSQL test server");
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&mut connection)
            .await
            .expect("create isolated test database");
        connection.close().await.expect("close admin connection");
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(admin.clone().database(&name))
            .await
            .expect("connect to isolated test database");
        Self { name, admin, pool }
    }
}

impl Drop for TestDatabase {
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
                if let Ok(mut connection) = PgConnection::connect_with(&admin).await {
                    let _ =
                        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                            .execute(&mut connection)
                            .await;
                    let _ = connection.close().await;
                }
            });
        })
        .join();
    }
}

struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "cc-backup-ac13-{label}-{}",
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn instance_settings_and_setup_steps_survive_backup_restore() {
    let source = TestDatabase::new().await;
    migrate::run(&source.pool)
        .await
        .expect("migrate source database");
    let operator = setup::complete(
        &source.pool,
        &setup::NewInstance {
            operator_email: "backup-operator@example.test".to_owned(),
            operator_display_name: "Backup Operator".to_owned(),
            operator_password_hash:
                "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$eHh4eHh4eHh4eHh4eHh4eA".to_owned(),
            organization_name: "Backup Test Org".to_owned(),
            organization_slug: "backup-test-org".to_owned(),
        },
    )
    .await
    .expect("create instance operator and setup row");
    let store_root = TestDirectory::new("source-store");
    let backup_dir = TestDirectory::new("backup");
    let store = LocalStore::open(&store_root.0)
        .await
        .expect("open source store");
    let source_store_id = Uuid::new_v4();
    assert!(
        maintenance::bind_store_if_unbound(
            &source.pool,
            source_store_id,
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("bind source store")
    );
    store
        .write_marker(source_store_id, false)
        .await
        .expect("write source store marker");

    sqlx::query(
        "INSERT INTO instance_setup_steps (step, status, updated_at, updated_by) \
         VALUES ('system_check', 'done', '2026-11-01T12:00:00Z', $1), \
                ('email_delivery', 'skipped', '2026-11-01T12:05:00Z', $1)",
    )
    .bind(operator.user_id)
    .execute(&source.pool)
    .await
    .expect("insert setup progress");
    let encrypted_secret = encrypt_secret_for(
        &SECRET_KEY,
        operator.user_id,
        b"instance-setting:mail.password",
        TEST_SECRET.as_bytes(),
    )
    .expect("encrypt the test secret with production AAD");
    let stored_secret = serde_json::json!({
        "ciphertext": STANDARD.encode(&encrypted_secret),
        "configured_at": "2026-11-01T12:15:00Z"
    });
    sqlx::query(
        "INSERT INTO instance_settings (key, value, updated_at, updated_by) \
         VALUES ('mail.transport', $1::jsonb, '2026-11-01T12:10:00Z', $2), \
                ('mail.password', $3::jsonb, '2026-11-01T12:15:00Z', $2)",
    )
    .bind(serde_json::json!({ "value": "smtp" }).to_string())
    .bind(operator.user_id)
    .bind(stored_secret.to_string())
    .execute(&source.pool)
    .await
    .expect("insert instance settings");

    // Two versions share the same file-scoped physical chunks. Backup must
    // preserve each mapping and restore the refcount from those mappings.
    let workspace_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workspaces (id,org_id,slug,name) VALUES ($1,$2,'backup-test','Backup Test')",
    )
    .bind(workspace_id)
    .bind(operator.organization_id)
    .execute(&source.pool)
    .await
    .expect("insert test workspace");
    let project_id = Uuid::new_v4();
    let file_id = Uuid::new_v4();
    let blob_id = Uuid::new_v4();
    let head_blob_id = Uuid::new_v4();
    let first_version = Uuid::new_v4();
    let second_version = Uuid::new_v4();
    let chunk_one = Uuid::new_v4();
    let chunk_two = Uuid::new_v4();
    let chunk_three = Uuid::new_v4();
    let first = b"shared prefix ";
    let second = b"shared suffix";
    let head_suffix = b"current suffix";
    let mut content = first.to_vec();
    content.extend_from_slice(second);
    let mut head_content = first.to_vec();
    head_content.extend_from_slice(head_suffix);
    let full_sha: [u8; 32] = Sha256::digest(&content).into();
    let head_sha: [u8; 32] = Sha256::digest(&head_content).into();
    let first_sha: [u8; 32] = Sha256::digest(first).into();
    let second_sha: [u8; 32] = Sha256::digest(second).into();
    let third_sha: [u8; 32] = Sha256::digest(head_suffix).into();
    let first_key = ObjectKey::generate();
    let second_key = ObjectKey::generate();
    let first_key_text = first_key.to_string();
    let second_key_text = second_key.to_string();
    let third_key = ObjectKey::generate();
    let third_key_text = third_key.to_string();
    let mut temp = store
        .begin_write_as(first_key.clone(), first.len() as u64)
        .await
        .unwrap();
    temp.write(first).await.unwrap();
    store
        .finalize(temp, first.len() as u64, &first_sha)
        .await
        .unwrap();
    let mut temp = store
        .begin_write_as(third_key.clone(), head_suffix.len() as u64)
        .await
        .unwrap();
    temp.write(head_suffix).await.unwrap();
    store
        .finalize(temp, head_suffix.len() as u64, &third_sha)
        .await
        .unwrap();
    let archive_root = TestDirectory::new("archive-store");
    let archive_local = LocalStore::open(&archive_root.0)
        .await
        .expect("open local test archive");
    let archive_store = ArchiveStore::Local(archive_local);
    let archive_crypto = ArchiveCrypto::from_server_key(&[73; 32]).expect("archive test key");
    let (archive_envelope, archive_data_key) = archive_crypto
        .create_data_key(chunk_two, second.len() as u64)
        .expect("chunk archive envelope");
    let archive_object_key = ObjectKey::generate();
    let encrypted = archive_data_key
        .encrypt_block(&archive_envelope, 0, second)
        .expect("encrypt archived chunk");
    let mut archive_temp = archive_store
        .begin_write_as(archive_object_key.clone(), encrypted.len() as u64)
        .await
        .expect("begin archived chunk write");
    archive_temp
        .write(&encrypted)
        .await
        .expect("write archived chunk ciphertext");
    let encrypted_sha: [u8; 32] = Sha256::digest(&encrypted).into();
    archive_store
        .finalize(archive_temp, encrypted.len() as u64, &encrypted_sha)
        .await
        .expect("publish archived chunk");
    let archive_access = ArchiveAccess {
        store: &archive_store,
        crypto: &archive_crypto,
        verify_sample: 1,
    };
    sqlx::query("INSERT INTO projects (id,org_id,workspace_id,name,name_key,name_hash,created_by_user_id,created_at,updated_by_user_id,updated_at) VALUES ($1,$2,$3,'Backup test','backup-test',$4,$5,now(),$5,now())")
        .bind(project_id).bind(operator.organization_id).bind(workspace_id).bind(vec![0_u8;32]).bind(operator.user_id).execute(&source.pool).await.expect("insert test project");
    sqlx::query("INSERT INTO files (id,org_id,workspace_id,project_id,name,name_key,name_hash,app,format,size_bytes,sha256,created_by_user_id,created_at,updated_by_user_id,updated_at) VALUES ($1,$2,$3,$4,'chunked.bin','chunked.bin',$5,'testapp','binary',$6,$7,$8,now(),$8,now())")
        .bind(file_id).bind(operator.organization_id).bind(workspace_id).bind(project_id).bind(vec![0_u8;32]).bind(content.len() as i64).bind(full_sha.to_vec()).bind(operator.user_id).execute(&source.pool).await.expect("insert test file");
    sqlx::query("INSERT INTO blobs (id,org_id,workspace_id,object_key,size_bytes,sha256,state,created_at,layout) VALUES ($1,$2,$3,$4,$5,$6,'committed',now(),'chunked')")
        .bind(blob_id).bind(operator.organization_id).bind(workspace_id).bind("bb000000000000000000000000000001").bind(content.len() as i64).bind(full_sha.to_vec()).execute(&source.pool).await.expect("insert chunked blob");
    sqlx::query("INSERT INTO blobs (id,org_id,workspace_id,object_key,size_bytes,sha256,state,created_at,layout) VALUES ($1,$2,$3,$4,$5,$6,'committed',now(),'chunked')")
        .bind(head_blob_id).bind(operator.organization_id).bind(workspace_id).bind("bb000000000000000000000000000002").bind(head_content.len() as i64).bind(head_sha.to_vec()).execute(&source.pool).await.expect("insert head chunked blob");
    sqlx::query("INSERT INTO file_versions (id,org_id,workspace_id,project_id,file_id,number,blob_id,size_bytes,sha256,created_by_user_id,created_at) VALUES ($1,$2,$3,$4,$5,1,$6,$7,$8,$9,now())")
        .bind(first_version).bind(operator.organization_id).bind(workspace_id).bind(project_id).bind(file_id).bind(blob_id).bind(content.len() as i64).bind(full_sha.to_vec()).bind(operator.user_id).execute(&source.pool).await.expect("insert first file version");
    sqlx::query("INSERT INTO file_versions (id,org_id,workspace_id,project_id,file_id,number,blob_id,size_bytes,sha256,base_version_id,created_by_user_id,created_at) VALUES ($1,$2,$3,$4,$5,2,$6,$7,$8,$9,$10,now())")
        .bind(second_version).bind(operator.organization_id).bind(workspace_id).bind(project_id).bind(file_id).bind(head_blob_id).bind(head_content.len() as i64).bind(head_sha.to_vec()).bind(first_version).bind(operator.user_id).execute(&source.pool).await.expect("insert second file version");
    sqlx::query("UPDATE files SET head_version_id=$2,version=2 WHERE id=$1")
        .bind(file_id)
        .bind(second_version)
        .execute(&source.pool)
        .await
        .expect("set file head");
    sqlx::query("UPDATE files SET size_bytes=$2,sha256=$3 WHERE id=$1")
        .bind(file_id)
        .bind(head_content.len() as i64)
        .bind(head_sha.to_vec())
        .execute(&source.pool)
        .await
        .expect("update head file metadata");
    for (id, key, bytes, sha) in [(
        chunk_one,
        first_key_text.as_str(),
        first.as_slice(),
        first_sha,
    )] {
        sqlx::query("INSERT INTO chunks (id,file_id,org_id,workspace_id,digest,sha256,object_key,size_bytes,state,created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'staged',now())")
            .bind(id).bind(file_id).bind(operator.organization_id).bind(workspace_id).bind(sha.to_vec()).bind(sha.to_vec()).bind(key).bind(bytes.len() as i64).execute(&source.pool).await.expect("insert physical chunk");
    }
    sqlx::query("INSERT INTO chunks (id,file_id,org_id,workspace_id,digest,sha256,object_key,size_bytes,state,created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'staged',now())")
        .bind(chunk_three).bind(file_id).bind(operator.organization_id).bind(workspace_id).bind(third_sha.to_vec()).bind(third_sha.to_vec()).bind(third_key_text.as_str()).bind(head_suffix.len() as i64).execute(&source.pool).await.expect("insert head-only chunk");
    sqlx::query("INSERT INTO chunks (id,file_id,org_id,workspace_id,digest,sha256,object_key,size_bytes,state,storage_tier,archive_object_key,archive_wrapped_key,created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'staged','archive',$9,$10,now())")
        .bind(chunk_two).bind(file_id).bind(operator.organization_id).bind(workspace_id).bind(second_sha.to_vec()).bind(second_sha.to_vec()).bind(second_key_text.as_str()).bind(second.len() as i64).bind(archive_object_key.as_str()).bind(archive_envelope.to_bytes()).execute(&source.pool).await.expect("insert archived physical chunk");
    sqlx::query("INSERT INTO version_chunks (version_id,file_id,ordinal,chunk_id,byte_offset) VALUES ($1,$2,0,$3,0),($1,$2,1,$4,$5),($6,$2,0,$3,0),($6,$2,1,$7,$5)")
        .bind(first_version).bind(file_id).bind(chunk_one).bind(chunk_two).bind(first.len() as i64).bind(second_version).bind(chunk_three).execute(&source.pool).await.expect("map shared, old-only archived, and head-only local chunks");

    let report = create_with_archive(
        &source.pool,
        &store,
        &backup_dir.0,
        &CreateOptions {
            server_version: "test",
            lock_wait: Duration::from_secs(3),
            on_phase: None,
        },
        Some(archive_access),
    )
    .await
    .expect("create backup");
    assert_eq!(
        report.manifest.migration_version,
        migrate::embedded_versions().last().copied().unwrap()
    );
    assert_eq!(
        report
            .manifest
            .chunks
            .as_ref()
            .expect("v3 chunk index")
            .count,
        3
    );
    let version_chunks = report
        .manifest
        .tables
        .iter()
        .find(|table| table.name == "version_chunks")
        .expect("version chunk table");
    assert_eq!(
        version_chunks.rows, 4,
        "each version has the shared chunk plus its own old-archived or head-local suffix"
    );
    for (name, expected_rows) in [("instance_setup_steps", 2), ("instance_settings", 2)] {
        let table = report
            .manifest
            .tables
            .iter()
            .find(|table| table.name == name)
            .expect("AC-13 table is listed in backup manifest");
        assert_eq!(table.rows, expected_rows, "{name}");
        let contents = std::fs::read(backup_dir.0.join(&table.file)).expect("read table snapshot");
        assert!(!contents.is_empty(), "{name} snapshot contains rows");
        if name == "instance_settings" {
            assert!(
                !contents
                    .windows(TEST_SECRET.len())
                    .any(|window| window == TEST_SECRET.as_bytes()),
                "the backup must contain ciphertext, never the raw secret",
            );
        }
    }
    let setup_table = report
        .manifest
        .tables
        .iter()
        .find(|table| table.name == "instance_setup")
        .expect("instance identity table is listed");
    assert_eq!(setup_table.rows, 1);
    let verified =
        verify_with_archive(&backup_dir.0, Some(&report.manifest_sha256), archive_access)
            .await
            .expect("verify backup");
    let source_archive_pins: i64 =
        sqlx::query_scalar("SELECT count(*) FROM archive_chunk_backup_refs WHERE backup_id=$1")
            .bind(report.backup_id)
            .fetch_one(&source.pool)
            .await
            .expect("read durable source chunk pins");
    assert_eq!(source_archive_pins, 1);

    let target = TestDatabase::new().await;
    let target_root = TestDirectory::new("target-store");
    let target_store = LocalStore::open(&target_root.0)
        .await
        .expect("open empty target store");
    let restored = restore_with_archive(
        &target.pool,
        &target_store,
        &backup_dir.0,
        &RestoreOptions {
            expected_manifest_sha256: Some(&verified.manifest_sha256),
            ..RestoreOptions::default()
        },
        Some(archive_access),
    )
    .await
    .expect("restore backup");
    assert_eq!(restored.rows, report.rows);
    let restored_refs: Vec<(Uuid, i64)> = sqlx::query_as("SELECT id,refcount FROM chunks")
        .fetch_all(&target.pool)
        .await
        .expect("read restored chunk refs");
    let refs: std::collections::HashMap<Uuid, i64> = restored_refs.into_iter().collect();
    assert_eq!(refs.get(&chunk_one), Some(&2));
    assert_eq!(refs.get(&chunk_two), Some(&1));
    assert_eq!(refs.get(&chunk_three), Some(&1));
    for (key, expected) in [
        (first_key_text.as_str(), first.as_slice()),
        (third_key_text.as_str(), head_suffix.as_slice()),
    ] {
        let mut source = target_store
            .open_read(&ObjectKey::parse(key).unwrap(), None)
            .await
            .expect("open restored chunk");
        let mut bytes = Vec::new();
        while let Some(block) = source.next_chunk().await.expect("read restored chunk") {
            bytes.extend_from_slice(&block);
        }
        assert_eq!(bytes, expected);
    }
    let target_archive_pins: i64 =
        sqlx::query_scalar("SELECT count(*) FROM archive_chunk_backup_refs WHERE backup_id=$1")
            .bind(
                restored
                    .manifest
                    .backup_id
                    .as_deref()
                    .and_then(|id| Uuid::parse_str(id).ok())
                    .unwrap(),
            )
            .fetch_one(&target.pool)
            .await
            .expect("read durable restored chunk pins");
    assert_eq!(target_archive_pins, 1);
    let first_chunk_path = backup_dir.0.join("chunks").join(chunk_one.to_string());
    std::fs::write(&first_chunk_path, b"corrupted chunk").expect("corrupt one copied chunk");
    assert_eq!(
        verify(&backup_dir.0, None)
            .expect_err("verification rejects altered chunk bytes")
            .kind,
        craftcloud_backup::ErrorKind::Integrity
    );
    std::fs::write(&first_chunk_path, first).expect("restore original chunk bytes");
    let mapping_path = backup_dir.0.join("version_chunks.jsonl");
    let original_mappings = std::fs::read(&mapping_path).expect("read version chunk mappings");
    let mut lines = original_mappings.split_inclusive(|byte| *byte == b'\n');
    let first_line = lines.next().expect("mapping entry exists");
    let mut mapping: serde_json::Value =
        serde_json::from_slice(first_line).expect("parse mapping entry");
    mapping["byte_offset"] = serde_json::json!(1);
    let mut mapping_bytes = serde_json::to_vec(&mapping).expect("serialize changed mapping");
    mapping_bytes.push(b'\n');
    for line in lines {
        mapping_bytes.extend_from_slice(line);
    }
    std::fs::write(&mapping_path, &mapping_bytes).expect("write inconsistent mapping");
    let manifest_path = backup_dir.0.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).expect("read manifest"))
            .expect("parse manifest");
    manifest["version_chunks"]["sha256"] = serde_json::json!(hex_digest(&mapping_bytes));
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).expect("serialize manifest");
    let manifest_sha = hex_digest(&manifest_bytes);
    std::fs::write(&manifest_path, &manifest_bytes)
        .expect("write manifest with updated mapping checksum");
    std::fs::write(
        backup_dir.0.join("manifest.sha256"),
        format!("{manifest_sha}  manifest.json\n"),
    )
    .expect("update manifest checksum file");
    assert_eq!(
        verify(&backup_dir.0, None)
            .expect_err("verification rejects inconsistent assembled-version mapping")
            .kind,
        craftcloud_backup::ErrorKind::Integrity
    );

    let setup_rows: Vec<(String, String, OffsetDateTime, Option<Uuid>)> = sqlx::query_as(
        "SELECT step, status, updated_at, updated_by FROM instance_setup_steps ORDER BY step",
    )
    .fetch_all(&target.pool)
    .await
    .expect("read restored setup progress");
    assert_eq!(
        setup_rows,
        [
            (
                "email_delivery".to_owned(),
                "skipped".to_owned(),
                parse_timestamp("2026-11-01T12:05:00Z"),
                Some(operator.user_id),
            ),
            (
                "system_check".to_owned(),
                "done".to_owned(),
                parse_timestamp("2026-11-01T12:00:00Z"),
                Some(operator.user_id),
            ),
        ]
    );
    let restored_operator: Uuid =
        sqlx::query_scalar("SELECT operator_user_id FROM instance_setup WHERE id=true")
            .fetch_one(&target.pool)
            .await
            .expect("read restored instance operator");
    assert_eq!(restored_operator, operator.user_id);
    let settings: Vec<(String, serde_json::Value, Option<Uuid>)> =
        sqlx::query_as("SELECT key, value, updated_by FROM instance_settings ORDER BY key")
            .fetch_all(&target.pool)
            .await
            .expect("read restored instance settings");
    assert_eq!(
        settings,
        [
            (
                "mail.password".to_owned(),
                stored_secret,
                Some(operator.user_id),
            ),
            (
                "mail.transport".to_owned(),
                serde_json::json!({ "value": "smtp" }),
                Some(operator.user_id),
            ),
        ]
    );
    let password_setting = settings
        .iter()
        .find(|(key, _, _)| key == "mail.password")
        .map(|(_, value, _)| value)
        .expect("restored password setting");
    let ciphertext = password_setting
        .get("ciphertext")
        .and_then(serde_json::Value::as_str)
        .expect("restored ciphertext");
    assert_eq!(
        decrypt_secret_for(
            &SECRET_KEY,
            &STANDARD.decode(ciphertext).expect("valid stored base64"),
            restored_operator,
            b"instance-setting:mail.password",
        )
        .expect("decrypt restored setting with instance key and AAD"),
        TEST_SECRET.as_bytes(),
    );
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
