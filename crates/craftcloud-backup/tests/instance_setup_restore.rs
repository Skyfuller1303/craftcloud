//! PostgreSQL round-trip coverage for AC-13 backup data.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::str::FromStr;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use craftcloud_auth::{decrypt_secret_for, encrypt_secret_for};
use craftcloud_backup::{CreateOptions, RestoreOptions, create, restore, verify};
use craftcloud_db::{maintenance, migrate, setup};
use craftcloud_storage::LocalStore;
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

    let report = create(
        &source.pool,
        &store,
        &backup_dir.0,
        &CreateOptions {
            server_version: "test",
            lock_wait: Duration::from_secs(3),
            on_phase: None,
        },
    )
    .await
    .expect("create backup");
    assert_eq!(report.manifest.migration_version, 23);
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
    let verified = verify(&backup_dir.0, Some(&report.manifest_sha256)).expect("verify backup");

    let target = TestDatabase::new().await;
    let target_root = TestDirectory::new("target-store");
    let target_store = LocalStore::open(&target_root.0)
        .await
        .expect("open empty target store");
    let restored = restore(
        &target.pool,
        &target_store,
        &backup_dir.0,
        &RestoreOptions {
            expected_manifest_sha256: Some(&verified.manifest_sha256),
            ..RestoreOptions::default()
        },
    )
    .await
    .expect("restore backup");
    assert_eq!(restored.rows, report.rows);

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
