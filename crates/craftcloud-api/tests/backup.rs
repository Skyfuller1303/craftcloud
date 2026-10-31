//! Backup, verify and restore (CC-F14, CC-P03-08): the restore drill with two
//! users, two workspaces and a file with several versions, tampering,
//! non-empty targets, garbage collection racing a backup, uploads in
//! progress, the health view and the audit trail. All tests need PostgreSQL
//! (`cargo xtask db-test`).

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use axum::Router;
    use axum::body::Body;
    use axum::http::StatusCode;
    use craftcloud_api::maintenance::{
        Binding, Maintenance, MaintenanceSettings, RunOptions, RunOutcome, Trigger,
        check_store_binding,
    };
    use craftcloud_api::{ApiConfig, Services, router_with_shared};
    use craftcloud_backup::{
        BackupError, CreateOptions, CreateReport, ErrorKind, RestoreOptions, RestoreReport,
        tables::{BACKUP_TABLES, EXCLUDED_TABLES, MIGRATIONS_TABLE},
    };
    use craftcloud_core::Capabilities;
    use craftcloud_db::PgPool;
    use craftcloud_storage::{LocalStore, ObjectStore, WriteSink};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use sqlx::{Connection, PgConnection};
    use tokio::sync::watch;
    use uuid::Uuid;

    use crate::common::{ENV, Req, Resp, Session, TestEnv, call, settings};

    // ---- helpers ---------------------------------------------------------------------

    fn sha_hex(data: &[u8]) -> String {
        Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_add(seed).wrapping_mul(31))
            .collect()
    }

    fn id_of(r: &Resp) -> Uuid {
        Uuid::parse_str(r.body["id"].as_str().unwrap()).unwrap()
    }

    /// A directory below the system temp directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!("cc-{label}-{}", Uuid::new_v4().simple()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        /// A not yet existing child, for `backup create --out`.
        fn child(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A second, empty instance: an unmigrated database and an empty store
    /// directory, as on a new host.
    struct Target {
        name: String,
        admin: PgConnectOptions,
        pool: PgPool,
        store_dir: TempDir,
    }

    impl Target {
        async fn new() -> Self {
            let url = std::env::var(ENV).expect("CC_TEST_DATABASE_URL must be set");
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
                store_dir: TempDir::new("restore-store"),
            }
        }

        async fn store(&self) -> LocalStore {
            LocalStore::open(self.store_dir.path()).await.unwrap()
        }

        async fn restore(&self, dir: &Path) -> Result<RestoreReport, BackupError> {
            craftcloud_backup::restore(
                &self.pool,
                &self.store().await,
                dir,
                &RestoreOptions::default(),
            )
            .await
        }

        /// The HTTP API on top of the restored database and store.
        async fn router(&self) -> Router {
            let services =
                Services::new(self.pool.clone(), None, settings()).with_storage(self.store().await);
            router_with_shared(
                Capabilities::development_defaults(),
                &ApiConfig::default(),
                Some(Arc::new(services)),
            )
        }

        async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
        }
    }

    impl Drop for Target {
        fn drop(&mut self) {
            let admin = self.admin.clone();
            let name = self.name.clone();
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
                    }
                });
            })
            .join();
        }
    }

    struct W {
        env: TestEnv,
        op: Session,
    }

    impl W {
        async fn new() -> Self {
            let env = TestEnv::new().await;
            let op = env.login_ok("op").await;
            let w = Self { env, op };
            w.bind().await;
            w
        }

        /// The server binds database and store at its first start; do the same.
        async fn bind(&self) {
            let binding = check_store_binding(
                &self.env.pool,
                &self.store().await,
                time::OffsetDateTime::now_utc(),
                true,
            )
            .await
            .unwrap();
            assert!(matches!(binding, Binding::Created(_)), "{binding:?}");
        }

        async fn store(&self) -> LocalStore {
            LocalStore::open(&self.env.storage_dir).await.unwrap()
        }

        async fn user(&self, name: &str) -> (Uuid, Session) {
            let id = self.env.user(name).await;
            (id, self.env.login_ok(name).await)
        }

        async fn send(&self, req: Req) -> Resp {
            call(&self.env.app, req).await
        }

        async fn json(&self, method: &'static str, path: &str, who: &Session, body: Value) -> Resp {
            self.send(Req::new(method, path).session(who).json(body))
                .await
        }

        async fn org(&self, slug: &str, owner: Uuid) -> Uuid {
            let r = self
                .json(
                    "POST",
                    "/api/v1/organizations",
                    &self.op,
                    json!({"name": format!("Org {slug}"), "slug": slug, "owner_user_id": owner.to_string()}),
                )
                .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            id_of(&r)
        }

        async fn org_member(&self, actor: &Session, org: Uuid, user: Uuid) {
            let r = self
                .json(
                    "POST",
                    &format!("/api/v1/organizations/{org}/members"),
                    actor,
                    json!({"user_id": user.to_string(), "role": "member"}),
                )
                .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
        }

        async fn ws(&self, actor: &Session, org: Uuid, slug: &str) -> Uuid {
            let r = self
                .json(
                    "POST",
                    &format!("/api/v1/organizations/{org}/workspaces"),
                    actor,
                    json!({"name": format!("WS {slug}"), "slug": slug}),
                )
                .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            id_of(&r)
        }

        async fn ws_add(&self, actor: &Session, ws: Uuid, user: Uuid, role: &str) {
            let r = self
                .json(
                    "POST",
                    &format!("/api/v1/workspaces/{ws}/members"),
                    actor,
                    json!({"user_id": user.to_string(), "role": role}),
                )
                .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
        }

        async fn project(&self, actor: &Session, ws: Uuid, name: &str) -> Uuid {
            let r = self
                .json(
                    "POST",
                    &format!("/api/v1/workspaces/{ws}/projects"),
                    actor,
                    json!({"name": name}),
                )
                .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            id_of(&r)
        }

        async fn file(&self, actor: &Session, project: Uuid, name: &str) -> Uuid {
            let r = self
                .json(
                    "POST",
                    &format!("/api/v1/projects/{project}/files"),
                    actor,
                    json!({"name": name, "app": "figcraft", "format": "fig"}),
                )
                .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            id_of(&r)
        }

        /// Creates an upload session (nothing sent yet).
        async fn open_upload(&self, who: &Session, file: Uuid, data: &[u8]) -> Uuid {
            let created = self
                .json(
                    "POST",
                    &format!("/api/v1/files/{file}/uploads"),
                    who,
                    json!({"size_bytes": data.len(), "sha256": sha_hex(data)}),
                )
                .await;
            assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
            Uuid::parse_str(created.body["upload_id"].as_str().unwrap()).unwrap()
        }

        /// Creates an upload and sends the content (state `stored`).
        async fn stored_upload(&self, who: &Session, file: Uuid, data: &[u8]) -> Uuid {
            let id = self.open_upload(who, file, data).await;
            let put = self
                .send(
                    Req::new("PUT", &format!("/api/v1/uploads/{id}/content"))
                        .session(who)
                        .body(Body::from(data.to_vec())),
                )
                .await;
            assert_eq!(put.status, StatusCode::OK, "{}", put.text);
            id
        }

        /// Uploads and commits `data` as the next version; returns the version id.
        async fn push(&self, who: &Session, file: Uuid, data: &[u8]) -> Uuid {
            let head = self
                .send(Req::new("GET", &format!("/api/v1/files/{file}")).cookie_only(who))
                .await
                .body["head_version_id"]
                .as_str()
                .map(|s| Uuid::parse_str(s).unwrap());
            let upload = self.stored_upload(who, file, data).await;
            let (name, value) = match head {
                None => ("If-None-Match", "*".to_owned()),
                Some(head) => ("If-Match", format!("\"version:{head}\"")),
            };
            let r = self
                .send(
                    Req::new("POST", &format!("/api/v1/files/{file}/versions"))
                        .session(who)
                        .header(name, &value)
                        .json(json!({"upload_id": upload.to_string()})),
                )
                .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            id_of(&r)
        }

        async fn create_backup(&self, out: &Path) -> Result<CreateReport, BackupError> {
            craftcloud_backup::create(
                &self.env.pool,
                &self.store().await,
                out,
                &CreateOptions {
                    server_version: "test",
                    lock_wait: Duration::from_secs(30),
                    on_phase: None,
                },
            )
            .await
        }

        async fn maintenance(&self, trash: Duration) -> Maintenance {
            let mut settings = MaintenanceSettings::new(
                trash,
                Duration::from_secs(30 * 86_400),
                Duration::from_secs(86_400),
                1 << 30,
            );
            settings.max_clock_drift = None;
            Maintenance::new(
                self.env.pool.clone(),
                Some(self.store().await),
                self.env.clock.clone(),
                settings,
            )
        }
    }

    async fn run_once(job: &Maintenance) -> RunOutcome {
        let (_keep, stop) = watch::channel(false);
        job.run_once(
            RunOptions {
                dry_run: false,
                trigger: Trigger::Cli,
            },
            &stop,
        )
        .await
        .unwrap()
    }

    /// All rows of a table as comparable text.
    async fn dump(pool: &PgPool, table: &str) -> String {
        sqlx::query_scalar(&format!(
            "SELECT COALESCE(string_agg(t::text, E'\\n' ORDER BY t::text), '') FROM \"{table}\" t"
        ))
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Path of an object in a store directory.
    fn object_path(store_dir: &Path, key: &str) -> PathBuf {
        store_dir
            .join("objects")
            .join(&key[0..2])
            .join(&key[2..4])
            .join(key)
    }

    async fn download(app: &Router, who: &Session, path: &str) -> Resp {
        call(app, Req::new("GET", path).cookie_only(who)).await
    }

    async fn login(app: &Router, name: &str) -> Session {
        let r = call(
            app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email": format!("{name}@example.test"), "password": crate::common::PASSWORD})),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        crate::common::session_from(&r)
    }

    // ---- the restore drill -----------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn restore_drill_two_users_two_workspaces_and_a_version_history() {
        let w = W::new().await;
        let alice = w.user("alice").await;
        let bob = w.user("bob").await;
        let carol = w.user("carol").await;
        let factor_now = time::OffsetDateTime::now_utc();
        let factor_key = [31_u8; 32];
        let factor_secret = [23_u8; 20];
        let factor_ciphertext =
            craftcloud_auth::encrypt_secret_for(&factor_key, alice.0, b"totp", &factor_secret)
                .unwrap();
        let recovery = craftcloud_auth::recovery_codes().unwrap().remove(0);
        let (recovery_salt, recovery_hash) =
            craftcloud_auth::hash_recovery_code(&recovery).unwrap();
        sqlx::query("INSERT INTO user_two_factor(user_id,secret_ciphertext,enabled_at,last_accepted_step,created_at) VALUES($1,$2,$3,42,$3)")
            .bind(alice.0).bind(&factor_ciphertext).bind(factor_now).execute(&w.env.pool).await.unwrap();
        sqlx::query("INSERT INTO user_two_factor_recovery_codes(user_id,salt,code_hash,created_at) VALUES($1,$2,$3,$4)")
            .bind(alice.0).bind(recovery_salt.as_slice()).bind(recovery_hash.as_slice()).bind(factor_now).execute(&w.env.pool).await.unwrap();
        sqlx::query("INSERT INTO two_factor_login_challenges(id_hash,user_id,password_hash_digest,login_kind,client_address,created_at,expires_at) VALUES($1,$2,$3,'browser','192.0.2.42',$4,$4 + interval '5 minutes')")
            .bind(vec![11_u8; 32]).bind(alice.0).bind(vec![13_u8; 32]).bind(factor_now).execute(&w.env.pool).await.unwrap();
        let acme = w.org("acme", alice.0).await;
        let bobco = w.org("bobco", bob.0).await;
        w.org_member(&alice.1, acme, carol.0).await;
        let w1 = w.ws(&alice.1, acme, "main").await;
        let w2 = w.ws(&bob.1, bobco, "main").await;
        w.ws_add(&alice.1, w1, carol.0, "editor").await;
        let p1 = w.project(&alice.1, w1, "Brand").await;
        let p2 = w.project(&bob.1, w2, "Site").await;
        let f1 = w.file(&alice.1, p1, "logo.fig").await;
        let f2 = w.file(&bob.1, p2, "home.fig").await;

        let blobs1 = [
            pattern(150_000, 1),
            pattern(70_001, 2),
            pattern(5, 3),
            pattern(1, 4),
        ];
        let mut versions1 = Vec::new();
        for data in &blobs1 {
            versions1.push(w.push(&alice.1, f1, data).await);
        }
        let data2 = pattern(40_000, 9);
        let v2 = w.push(&bob.1, f2, &data2).await;

        // ---- backup ----
        let backups = TempDir::new("backup-drill");
        let out = backups.child("b1");
        let started = Instant::now();
        let created = w.create_backup(&out).await.unwrap();
        let backup_time = started.elapsed();
        assert!(created.audit_recorded);
        assert_eq!(created.blobs, 5, "{:?}", created.manifest.blobs);
        assert_eq!(created.manifest.migration_version, 18);

        let report = craftcloud_backup::verify(&out, Some(&created.manifest_sha256)).unwrap();
        assert_eq!(report.blobs, 5);
        assert_eq!(report.rows, created.rows);

        // ---- restore into a separate, empty instance ----
        let target = Target::new().await;
        let started = Instant::now();
        let restored = target.restore(&out).await.unwrap();
        let restore_time = started.elapsed();
        eprintln!(
            "DRILL small dataset ({} rows, {} blobs, {} bytes) on local disk: backup {:?} (report {:?}), restore {:?} (report {:?})",
            created.rows,
            created.blobs,
            created.blob_bytes,
            backup_time,
            created.duration,
            restore_time,
            restored.duration
        );
        assert_eq!((restored.blobs, restored.aborted_uploads), (5, 0));
        assert!(!restored.needs_migration);

        // Row data: identical, table by table (the audit log gains one event).
        for spec in BACKUP_TABLES {
            if spec.name == "audit_events" {
                continue;
            }
            assert_eq!(
                dump(&w.env.pool, spec.name).await,
                dump(&target.pool, spec.name).await,
                "table {} differs",
                spec.name
            );
        }
        let source_events = w.env.count("SELECT count(*) FROM audit_events").await;
        // The source also holds the `backup.created` event written after the snapshot.
        assert_eq!(
            target.count("SELECT count(*) FROM audit_events").await,
            source_events
        );
        assert_eq!(
            target
                .count("SELECT count(*) FROM audit_events WHERE action = 'backup.restored' AND org_id IS NULL AND result = 'success'")
                .await,
            1
        );
        assert_eq!(
            target
                .count("SELECT count(*) FROM audit_events WHERE action = 'backup.created'")
                .await,
            0
        );
        assert_eq!(
            target.count("SELECT count(*) FROM user_two_factor").await,
            1
        );
        assert_eq!(
            target
                .count("SELECT count(*) FROM user_two_factor_recovery_codes")
                .await,
            1
        );
        assert_eq!(
            target
                .count("SELECT count(*) FROM two_factor_login_challenges")
                .await,
            0
        );

        // Bytes: every blob of every version is byte-identical in the new store.
        let rows: Vec<(String, Vec<u8>, i64)> = sqlx::query_as(
            "SELECT b.object_key, b.sha256, b.size_bytes FROM blobs b ORDER BY b.object_key",
        )
        .fetch_all(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 5);
        for (key, sha, size) in &rows {
            let original = std::fs::read(object_path(&w.env.storage_dir, key)).unwrap();
            let copy = std::fs::read(object_path(target.store_dir.path(), key)).unwrap();
            assert_eq!(original, copy, "object {key}");
            assert_eq!(copy.len() as i64, *size);
            assert_eq!(Sha256::digest(&copy).as_slice(), sha.as_slice());
        }

        // The new pair is bound: fresh identity, epoch 0, matching marker.
        let store = target.store().await;
        let marker = store.read_marker_info().await.unwrap().unwrap();
        let bound: (Uuid, i64) = sqlx::query_as("SELECT store_id, epoch FROM storage_binding")
            .fetch_one(&target.pool)
            .await
            .unwrap();
        assert_eq!((marker.store_id, marker.epoch as i64), bound);
        assert_eq!(marker.store_id, restored.store_id);
        assert_ne!(marker.store_id.to_string(), created.manifest.store.store_id);

        // Permissions and content through the API of the restored instance.
        let restored_ciphertext: Vec<u8> =
            sqlx::query_scalar("SELECT secret_ciphertext FROM user_two_factor WHERE user_id=$1")
                .bind(alice.0)
                .fetch_one(&target.pool)
                .await
                .unwrap();
        assert_eq!(restored_ciphertext, factor_ciphertext);
        assert_eq!(
            craftcloud_auth::decrypt_secret_for(
                &factor_key,
                &restored_ciphertext,
                alice.0,
                b"totp"
            )
            .unwrap(),
            factor_secret
        );
        let services = Services::new(target.pool.clone(), None, settings())
            .with_storage(target.store().await)
            .with_two_factor_keys(Some(factor_key), None);
        let app = router_with_shared(
            Capabilities::development_defaults(),
            &ApiConfig::default(),
            Some(Arc::new(services)),
        );
        let challenge = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email":"alice@example.test","password":crate::common::PASSWORD})),
        )
        .await;
        assert_eq!(challenge.code(), "SECOND_FACTOR_REQUIRED");
        let alice_login = call(&app, Req::new("POST", "/api/v1/auth/login/second-factor").json(json!({"challenge_id":challenge.body["error"]["details"]["challenge_id"],"recovery_code":recovery}))).await;
        assert_eq!(alice_login.status, StatusCode::OK);
        let (a, b, c) = (
            crate::common::session_from(&alice_login),
            login(&app, "bob").await,
            login(&app, "carol").await,
        );
        let head = download(&app, &a, &format!("/api/v1/files/{f1}/content")).await;
        assert_eq!(head.status, StatusCode::OK);
        assert_eq!(
            &head.bytes,
            blobs1.last().unwrap(),
            "the head is the newest version"
        );
        for (version, data) in versions1.iter().zip(&blobs1) {
            let r = download(
                &app,
                &a,
                &format!("/api/v1/files/{f1}/versions/{version}/content"),
            )
            .await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.text);
            assert_eq!(&r.bytes, data);
            // Roles survived: the editor of workspace 1 reads it too.
            let r = download(
                &app,
                &c,
                &format!("/api/v1/files/{f1}/versions/{version}/content"),
            )
            .await;
            assert_eq!(&r.bytes, data);
            // Bob has no role in alice's organization: hidden.
            let r = download(
                &app,
                &b,
                &format!("/api/v1/files/{f1}/versions/{version}/content"),
            )
            .await;
            assert_eq!((r.status, r.code()), (StatusCode::NOT_FOUND, "NOT_FOUND"));
        }
        let own = download(
            &app,
            &b,
            &format!("/api/v1/files/{f2}/versions/{v2}/content"),
        )
        .await;
        assert_eq!((own.status, &own.bytes), (StatusCode::OK, &data2));
        for outsider in [&a, &c] {
            let r = download(&app, outsider, &format!("/api/v1/files/{f2}/content")).await;
            assert_eq!((r.status, r.code()), (StatusCode::NOT_FOUND, "NOT_FOUND"));
        }
        // Carol is an editor, not an owner: she cannot manage members.
        let r = call(
            &app,
            Req::new("POST", &format!("/api/v1/workspaces/{w1}/members"))
                .session(&c)
                .json(json!({"user_id": bob.0.to_string(), "role": "viewer"})),
        )
        .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.text);

        // Sessions did not survive the restore.
        let old = call(
            &app,
            Req::new("GET", "/api/v1/auth/session").cookie_only(&alice.1),
        )
        .await;
        assert_eq!(old.status, StatusCode::UNAUTHORIZED, "{}", old.text);
        assert_eq!(
            target.count("SELECT count(*) FROM sessions WHERE user_id = (SELECT id FROM users WHERE normalized_email = 'alice@example.test') AND revoked_at IS NULL").await,
            1,
            "only the login of this test exists"
        );

        // The restored instance keeps working: a new version commits on top.
        let next = w2_push(&app, &a, f1, &pattern(1000, 7)).await;
        assert_eq!(next.status, StatusCode::CREATED, "{}", next.text);
    }

    /// Uploads and commits on the restored router (same steps as `W::push`).
    async fn w2_push(app: &Router, who: &Session, file: Uuid, data: &[u8]) -> Resp {
        let head = call(
            app,
            Req::new("GET", &format!("/api/v1/files/{file}")).cookie_only(who),
        )
        .await
        .body["head_version_id"]
            .as_str()
            .map(|s| Uuid::parse_str(s).unwrap());
        let created = call(
            app,
            Req::new("POST", &format!("/api/v1/files/{file}/uploads"))
                .session(who)
                .json(json!({"size_bytes": data.len(), "sha256": sha_hex(data)})),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        let upload = created.body["upload_id"].as_str().unwrap().to_owned();
        let put = call(
            app,
            Req::new("PUT", &format!("/api/v1/uploads/{upload}/content"))
                .session(who)
                .body(Body::from(data.to_vec())),
        )
        .await;
        assert_eq!(put.status, StatusCode::OK, "{}", put.text);
        let (name, value) = match head {
            None => ("If-None-Match", "*".to_owned()),
            Some(head) => ("If-Match", format!("\"version:{head}\"")),
        };
        call(
            app,
            Req::new("POST", &format!("/api/v1/files/{file}/versions"))
                .session(who)
                .header(name, &value)
                .json(json!({"upload_id": upload})),
        )
        .await
    }

    // ---- verify ----------------------------------------------------------------------

    /// A small instance with one file, one blob; returns the backup directory.
    async fn small_backup(w: &W, dir: &TempDir) -> (PathBuf, CreateReport) {
        let owner = w.user("owner").await;
        let org = w.org("small", owner.0).await;
        let ws = w.ws(&owner.1, org, "main").await;
        let project = w.project(&owner.1, ws, "P").await;
        let file = w.file(&owner.1, project, "a.fig").await;
        w.push(&owner.1, file, &pattern(30_000, 4)).await;
        let out = dir.child("backup");
        let report = w.create_backup(&out).await.unwrap();
        (out, report)
    }

    fn first_blob(out: &Path) -> PathBuf {
        let shard = std::fs::read_dir(out.join("blobs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::read_dir(shard)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }

    fn flip_byte(path: &Path) {
        let mut bytes = std::fs::read(path).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x01;
        std::fs::write(path, bytes).unwrap();
    }

    /// Rewrites the manifest and keeps `manifest.sha256` consistent, as a
    /// careful forger would.
    fn rewrite_manifest(out: &Path, edit: impl FnOnce(&mut Value)) {
        let mut manifest: Value =
            serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
        edit(&mut manifest);
        let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        std::fs::write(out.join("manifest.json"), &bytes).unwrap();
        std::fs::write(
            out.join("manifest.sha256"),
            format!("{}  manifest.json\n", sha_hex(&bytes)),
        )
        .unwrap();
    }

    fn expect(out: &Path, kind: ErrorKind, needle: &str) {
        let error = craftcloud_backup::verify(out, None).unwrap_err();
        assert_eq!(error.kind, kind, "{error}");
        assert!(error.message.contains(needle), "{error}");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn verify_rejects_tampered_missing_extra_and_forged_backups() {
        let w = W::new().await;
        let dir = TempDir::new("backup-verify");
        let (out, created) = small_backup(&w, &dir).await;
        craftcloud_backup::verify(&out, None).unwrap();

        // A table file with one flipped byte.
        let table = out.join("tables").join("users.copy");
        flip_byte(&table);
        expect(
            &out,
            ErrorKind::Integrity,
            "tables/users.copy does not match its checksum",
        );
        flip_byte(&table);
        craftcloud_backup::verify(&out, None).unwrap();

        // A blob with one flipped byte, with another size, and a missing one.
        let blob = first_blob(&out);
        let original = std::fs::read(&blob).unwrap();
        flip_byte(&blob);
        expect(&out, ErrorKind::Integrity, "does not match its checksum");
        std::fs::write(&blob, [original.as_slice(), b"x"].concat()).unwrap();
        expect(&out, ErrorKind::Integrity, "has the wrong size");
        std::fs::remove_file(&blob).unwrap();
        expect(&out, ErrorKind::Integrity, "is missing");
        std::fs::write(&blob, &original).unwrap();
        craftcloud_backup::verify(&out, None).unwrap();

        // Extra files and directories.
        let extra = out.join("tables").join("extra.copy");
        std::fs::write(&extra, b"x").unwrap();
        expect(
            &out,
            ErrorKind::Integrity,
            "unexpected file tables/extra.copy",
        );
        std::fs::remove_file(&extra).unwrap();
        let stray = blob.parent().unwrap().join("f".repeat(32));
        std::fs::write(&stray, b"x").unwrap();
        expect(&out, ErrorKind::Integrity, "holds 2 blob files but lists 1");
        std::fs::remove_file(&stray).unwrap();
        let loose = out.join("notes.txt");
        std::fs::write(&loose, b"x").unwrap();
        expect(&out, ErrorKind::Integrity, "unexpected entry notes.txt");
        std::fs::remove_file(&loose).unwrap();

        // A manifest that does not match its checksum file, a pinned digest
        // that differs, and a damaged checksum file.
        let manifest_path = out.join("manifest.json");
        let manifest_text = std::fs::read_to_string(&manifest_path).unwrap();
        std::fs::write(
            &manifest_path,
            manifest_text.replace("\"test\"", "\"forged\""),
        )
        .unwrap();
        expect(&out, ErrorKind::Integrity, "does not match manifest.sha256");
        std::fs::write(&manifest_path, &manifest_text).unwrap();
        let error = craftcloud_backup::verify(&out, Some(&"0".repeat(64))).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Integrity);
        assert!(
            error.message.contains("expected manifest digest"),
            "{error}"
        );
        let sums = std::fs::read(out.join("manifest.sha256")).unwrap();
        std::fs::write(out.join("manifest.sha256"), b"nonsense").unwrap();
        expect(&out, ErrorKind::Integrity, "manifest.sha256 is malformed");
        std::fs::write(out.join("manifest.sha256"), &sums).unwrap();
        craftcloud_backup::verify(&out, Some(&created.manifest_sha256)).unwrap();

        // Forged manifests with a matching checksum file: unknown versions.
        rewrite_manifest(&out, |m| m["migration_version"] = json!(99));
        expect(&out, ErrorKind::Format, "only knows up to 18");
        rewrite_manifest(&out, |m| {
            m["migration_version"] = json!(14);
            m["format_version"] = json!(2);
        });
        expect(
            &out,
            ErrorKind::Format,
            "unsupported backup format version 2",
        );
        rewrite_manifest(&out, |m| {
            m["format_version"] = json!(1);
            m["migration_version"] = json!(18);
            m["tables"][0]["rows"] = json!(77);
        });
        expect(&out, ErrorKind::Integrity, "rows but the manifest says 77");
        rewrite_manifest(&out, |m| {
            m["tables"][0]["rows"] = json!(0);
            m["tables"].as_array_mut().unwrap().pop();
        });
        expect(&out, ErrorKind::Format, "table list");

        // Not a backup at all, and a missing directory.
        let empty = dir.child("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(craftcloud_backup::verify(&empty, None).is_err());
        assert!(craftcloud_backup::verify(&dir.child("nowhere"), None).is_err());

        // A restore of a damaged backup writes nothing.
        flip_byte(&out.join("tables").join("users.copy"));
        let target = Target::new().await;
        let error = target.restore(&out).await.unwrap_err();
        assert!(
            matches!(error.kind, ErrorKind::Integrity | ErrorKind::Format),
            "{error}"
        );
        assert_eq!(
            target
                .count("SELECT count(*) FROM pg_tables WHERE schemaname = 'public'")
                .await,
            0,
            "no tables were created"
        );
        assert!(
            target
                .store()
                .await
                .read_marker_info()
                .await
                .unwrap()
                .is_none()
        );
    }

    // ---- targets that are not empty --------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn restore_never_overwrites_a_non_empty_target() {
        let w = W::new().await;
        let dir = TempDir::new("backup-nonempty");
        let (out, _) = small_backup(&w, &dir).await;

        // 1. The source database itself (migrated, full), with a clean store.
        let clean_store = TempDir::new("restore-clean");
        let users_before = w.env.count("SELECT count(*) FROM users").await;
        let error = craftcloud_backup::restore(
            &w.env.pool,
            &LocalStore::open(clean_store.path()).await.unwrap(),
            &out,
            &RestoreOptions::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::TargetNotEmpty, "{error}");
        assert_eq!(
            w.env.count("SELECT count(*) FROM users").await,
            users_before
        );
        assert_eq!(
            LocalStore::open(clean_store.path())
                .await
                .unwrap()
                .read_marker_info()
                .await
                .unwrap(),
            None
        );

        // 2. An empty database but a store that already holds an object.
        let target = Target::new().await;
        let store = target.store().await;
        let mut temp = store.begin_write(10).await.unwrap();
        temp.write(b"keep").await.unwrap();
        let digest: [u8; 32] = Sha256::digest(b"keep").into();
        let key = store.finalize(temp, 4, &digest).await.unwrap();
        let error = target.restore(&out).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::TargetNotEmpty, "{error}");
        assert_eq!(
            target
                .count("SELECT count(*) FROM pg_tables WHERE schemaname = 'public'")
                .await,
            0
        );
        assert_eq!(
            std::fs::read(object_path(target.store_dir.path(), key.as_str())).unwrap(),
            b"keep"
        );

        // 3. A database with tables but no migration history.
        let other = Target::new().await;
        sqlx::query("CREATE TABLE precious (id int)")
            .execute(&other.pool)
            .await
            .unwrap();
        let error = other.restore(&out).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::TargetNotEmpty, "{error}");
        assert_eq!(other.count("SELECT count(*) FROM precious").await, 0);

        // 4. A successful restore, then the same restore again: refused, data unchanged.
        let fresh = Target::new().await;
        fresh.restore(&out).await.unwrap();
        let users = fresh.count("SELECT count(*) FROM users").await;
        let events = fresh.count("SELECT count(*) FROM audit_events").await;
        let second_store = TempDir::new("restore-second");
        let error = craftcloud_backup::restore(
            &fresh.pool,
            &LocalStore::open(second_store.path()).await.unwrap(),
            &out,
            &RestoreOptions::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::TargetNotEmpty, "{error}");
        assert_eq!(fresh.count("SELECT count(*) FROM users").await, users);
        assert_eq!(
            fresh.count("SELECT count(*) FROM audit_events").await,
            events
        );
        let error = fresh.restore(&out).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::TargetNotEmpty, "{error}");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_migrated_empty_database_is_a_valid_target() {
        let w = W::new().await;
        let dir = TempDir::new("backup-migrated");
        let (out, _) = small_backup(&w, &dir).await;
        // As in Compose: `migrate` ran, nothing else.
        let target = Target::new().await;
        craftcloud_db::migrate::run(&target.pool).await.unwrap();
        let restored = target.restore(&out).await.unwrap();
        assert!(restored.rows > 0);
        assert_eq!(
            dump(&w.env.pool, "file_versions").await,
            dump(&target.pool, "file_versions").await
        );
        craftcloud_db::migrate::verify(&target.pool).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_backup_of_an_older_schema_is_restored_as_is_and_migrated_afterwards() {
        let w = W::new().await;
        let dir = TempDir::new("backup-older");
        let (out, _) = small_backup(&w, &dir).await;
        // Migrations 0015 and 0016 add excluded credential and lease tables and
        // the backed-up `grants` table. Remove their entries (and the grants
        // export) to model the manifest produced by a version 11 server. (No
        // older server build exists here to produce a genuine one.)
        std::fs::remove_file(out.join("tables").join("grants.copy")).unwrap();
        // AC-02 adds two persistent tables and appends require_two_factor to
        // organizations. Model the old COPY column order as well as its manifest.
        for table in ["user_two_factor", "user_two_factor_recovery_codes"] {
            std::fs::remove_file(out.join("tables").join(format!("{table}.copy"))).unwrap();
        }
        // AC-06 adds live state tables and an actor label on versions. The
        // older-schema fixture must model both changes in its COPY files.
        for table in ["figcraft_live_files", "figcraft_links", "figcraft_outbox"] {
            std::fs::remove_file(out.join("tables").join(format!("{table}.copy"))).unwrap();
        }
        let versions_path = out.join("tables").join("file_versions.copy");
        let current_versions = std::fs::read_to_string(&versions_path).unwrap();
        let old_versions: String = current_versions
            .lines()
            .map(|line| {
                let (columns, actor_kind) = line.rsplit_once('\t').unwrap();
                assert_eq!(actor_kind, "user");
                format!("{columns}\n")
            })
            .collect();
        std::fs::write(&versions_path, &old_versions).unwrap();
        let org_path = out.join("tables").join("organizations.copy");
        let current_orgs = std::fs::read_to_string(&org_path).unwrap();
        let old_orgs: String = current_orgs
            .lines()
            .map(|line| {
                let (old_columns, policy) = line.rsplit_once('\t').unwrap();
                assert_eq!(policy, "f");
                format!("{old_columns}\n")
            })
            .collect();
        std::fs::write(&org_path, &old_orgs).unwrap();
        rewrite_manifest(&out, |m| {
            m["migration_version"] = json!(11);
            m["tables"].as_array_mut().unwrap().retain(|t| {
                !matches!(
                    t["name"].as_str(),
                    Some(
                        "grants"
                            | "user_two_factor"
                            | "user_two_factor_recovery_codes"
                            | "figcraft_live_files"
                            | "figcraft_links"
                            | "figcraft_outbox"
                    )
                )
            });
            let org = m["tables"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|t| t["name"] == "organizations")
                .unwrap();
            org["bytes"] = json!(old_orgs.len());
            org["sha256"] = json!(sha_hex(old_orgs.as_bytes()));
            let versions = m["tables"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|t| t["name"] == "file_versions")
                .unwrap();
            versions["bytes"] = json!(old_versions.len());
            versions["sha256"] = json!(sha_hex(old_versions.as_bytes()));
            m["excluded_tables"] = json!([
                "sessions",
                "password_reset_tokens",
                "storage_binding",
                "maintenance_runs"
            ]);
        });
        craftcloud_backup::verify(&out, None).unwrap();

        let target = Target::new().await;
        let restored = target.restore(&out).await.unwrap();
        assert!(restored.needs_migration);
        let newest: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations")
            .fetch_one(&target.pool)
            .await
            .unwrap();
        assert_eq!(newest, 11, "restored at the schema of the backup");
        assert!(craftcloud_db::migrate::verify(&target.pool).await.is_err());

        // Restore, then migrate.
        let applied = craftcloud_db::migrate::run(&target.pool).await.unwrap();
        assert_eq!(applied.len(), 7);
        craftcloud_db::migrate::verify(&target.pool).await.unwrap();
        assert_eq!(
            dump(&w.env.pool, "users").await,
            dump(&target.pool, "users").await
        );
    }

    // ---- no garbage collection during a backup ---------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn maintenance_skips_while_a_backup_holds_the_lock_and_a_backup_waits_for_maintenance() {
        let w = W::new().await;
        let dir = TempDir::new("backup-lock");
        let owner = w.user("owner").await;
        let org = w.org("locked", owner.0).await;
        let ws = w.ws(&owner.1, org, "main").await;
        let project = w.project(&owner.1, ws, "P").await;
        let file = w.file(&owner.1, project, "a.fig").await;
        w.push(&owner.1, file, &pattern(2000, 1)).await;

        // What `backup create` holds while it runs.
        let lock = craftcloud_db::maintenance::try_lock(&w.env.pool)
            .await
            .unwrap()
            .unwrap();
        let job = w.maintenance(Duration::from_secs(1)).await;
        assert!(matches!(run_once(&job).await, RunOutcome::Skipped));

        // A second backup does not run alongside; it gives up with a clear error,
        // leaves nothing behind and records the failure.
        let out = dir.child("waits");
        let error = craftcloud_backup::create(
            &w.env.pool,
            &w.store().await,
            &out,
            &CreateOptions {
                server_version: "test",
                lock_wait: Duration::from_millis(600),
                on_phase: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Busy, "{error}");
        assert!(!out.exists());
        assert_eq!(
            w.env
                .count("SELECT count(*) FROM audit_events WHERE action = 'backup.created' AND result = 'failed'")
                .await,
            1
        );

        // Released (the same thing a crash does: the connection goes away).
        lock.release().await.unwrap();
        let report = w.create_backup(&out).await.unwrap();
        assert_eq!(report.blobs, 1);
        // And maintenance runs again afterwards.
        assert!(matches!(run_once(&job).await, RunOutcome::Completed(_)));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_crashed_backup_connection_frees_the_lock() {
        let w = W::new().await;
        // A dedicated pool stands for the backup process.
        let url = std::env::var(ENV).unwrap();
        let admin = PgConnectOptions::from_str(&url).unwrap();
        let db: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        let process = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(admin.database(&db))
            .await
            .unwrap();
        let lock = craftcloud_db::maintenance::try_lock(&process)
            .await
            .unwrap()
            .unwrap();
        let job = w.maintenance(Duration::from_secs(1)).await;
        assert!(matches!(run_once(&job).await, RunOutcome::Skipped));
        // The process dies: its connections close without releasing anything.
        drop(lock);
        process.close().await;
        let mut freed = false;
        for _ in 0..50 {
            if matches!(run_once(&job).await, RunOutcome::Completed(_)) {
                freed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(freed, "the lock must not outlive its connection");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn garbage_collection_during_a_backup_cannot_delete_what_the_snapshot_references() {
        let w = W::new().await;
        let dir = TempDir::new("backup-gc");
        let owner = w.user("owner").await;
        let org = w.org("gc", owner.0).await;
        let ws = w.ws(&owner.1, org, "main").await;
        let project = w.project(&owner.1, ws, "P").await;
        let mut files = Vec::new();
        for n in 0..4u8 {
            let file = w.file(&owner.1, project, &format!("f{n}.fig")).await;
            w.push(&owner.1, file, &pattern(20_000, n)).await;
            w.push(&owner.1, file, &pattern(30_000, n + 50)).await;
            files.push(file);
        }
        // Everything is in the trash and past retention: a maintenance run
        // purges the files and collects their blobs, if it may run.
        for file in &files {
            let r = w
                .send(Req::new("DELETE", &format!("/api/v1/files/{file}")).session(&owner.1))
                .await;
            assert!(r.status.is_success(), "{}", r.text);
        }
        // Past the retention and past the grace period for unreferenced blobs.
        w.env.clock.advance(Duration::from_secs(2 * 86_400));
        let job = Arc::new(w.maintenance(Duration::from_secs(1)).await);

        // The maintenance job runs, repeatedly, at every point of the backup
        // where it could do harm: before the snapshot and between the snapshot
        // and the end of the blob copy.
        let outcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hook: craftcloud_backup::PhaseHook = {
            let (job, outcomes) = (Arc::clone(&job), Arc::clone(&outcomes));
            Arc::new(move |phase| {
                let (job, outcomes) = (Arc::clone(&job), Arc::clone(&outcomes));
                Box::pin(async move {
                    for _ in 0..4 {
                        let skipped = matches!(run_once(&job).await, RunOutcome::Skipped);
                        outcomes.lock().unwrap().push((phase, skipped));
                    }
                })
            })
        };
        let out = dir.child("gc");
        let created = craftcloud_backup::create(
            &w.env.pool,
            &w.store().await,
            &out,
            &CreateOptions {
                server_version: "test",
                lock_wait: Duration::from_secs(30),
                on_phase: Some(hook),
            },
        )
        .await
        .unwrap_or_else(|e| panic!("the backup must not fail: {e}"));
        let seen = outcomes.lock().unwrap().clone();
        assert_eq!(seen.len(), 12, "{seen:?}");
        assert!(
            seen.iter().all(|(_, skipped)| *skipped),
            "maintenance must skip while the backup holds the lock: {seen:?}"
        );
        // The snapshot has the files and all their bytes.
        assert_eq!(created.blobs, 8);
        craftcloud_backup::verify(&out, Some(&created.manifest_sha256)).unwrap();
        let target = Target::new().await;
        target.restore(&out).await.unwrap();
        assert_eq!(target.count("SELECT count(*) FROM file_versions").await, 8);
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT b.object_key FROM file_versions v JOIN blobs b ON b.org_id = v.org_id AND b.id = v.blob_id",
        )
        .fetch_all(&target.pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 8);
        for (key,) in rows {
            assert!(
                object_path(target.store_dir.path(), &key).is_file(),
                "{key}"
            );
        }

        // Proof that the setup was dangerous: once the backup is over, the
        // same job really purges the files and deletes their objects.
        for _ in 0..6 {
            assert!(matches!(run_once(&job).await, RunOutcome::Completed(_)));
        }
        assert_eq!(w.env.count("SELECT count(*) FROM file_versions").await, 0);
        assert_eq!(w.env.count("SELECT count(*) FROM blobs").await, 0);
    }

    // ---- uploads in progress ---------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_backup_during_an_upload_is_consistent_and_the_restore_aborts_the_upload() {
        let w = W::new().await;
        let dir = TempDir::new("backup-upload");
        let owner = w.user("owner").await;
        let org = w.org("uploads", owner.0).await;
        let ws = w.ws(&owner.1, org, "main").await;
        let project = w.project(&owner.1, ws, "P").await;
        let file = w.file(&owner.1, project, "a.fig").await;
        let committed = pattern(10_000, 1);
        w.push(&owner.1, file, &committed).await;
        // One upload with its bytes stored but not committed, one only announced.
        let stored = w.stored_upload(&owner.1, file, &pattern(20_000, 2)).await;
        let announced = w.open_upload(&owner.1, file, &pattern(30_000, 3)).await;
        assert_eq!(
            w.env
                .count("SELECT count(*) FROM blobs WHERE state = 'staged'")
                .await,
            1
        );
        let reserved = w
            .env
            .count("SELECT COALESCE(sum(reserved_bytes), 0)::bigint FROM workspaces")
            .await;
        assert_eq!(reserved, 50_000);

        let out = dir.child("b");
        let created = w.create_backup(&out).await.unwrap();
        assert_eq!(created.blobs, 1, "only the committed blob is copied");
        craftcloud_backup::verify(&out, None).unwrap();

        let target = Target::new().await;
        let restored = target.restore(&out).await.unwrap();
        assert_eq!(restored.aborted_uploads, 2);
        assert_eq!(
            target
                .count("SELECT count(*) FROM upload_sessions WHERE state IN ('created', 'receiving', 'stored')")
                .await,
            0
        );
        assert_eq!(
            target
                .count("SELECT count(*) FROM upload_sessions WHERE state = 'aborted'")
                .await,
            2
        );
        assert_eq!(
            target
                .count("SELECT count(*) FROM blobs WHERE state = 'staged'")
                .await,
            0
        );
        // Only the orphaned blob of the stored upload stays reserved until it is collected.
        assert_eq!(
            target
                .count("SELECT COALESCE(sum(reserved_bytes), 0)::bigint FROM workspaces")
                .await,
            20_000
        );
        assert_eq!(
            target
                .count("SELECT COALESCE(sum(reserved_bytes), 0)::bigint FROM organizations")
                .await,
            20_000
        );
        assert_eq!(
            target
                .count("SELECT COALESCE(sum(committed_bytes), 0)::bigint FROM organizations")
                .await,
            committed.len() as i64
        );

        // The aborted upload cannot be committed (its bytes are not here), and
        // the maintenance job of the restored instance copes with the leftovers.
        let app = target.router().await;
        let me = login(&app, "owner").await;
        let head: Uuid = sqlx::query_scalar("SELECT head_version_id FROM files WHERE id = $1")
            .bind(file)
            .fetch_one(&target.pool)
            .await
            .unwrap();
        for upload in [stored, announced] {
            let r = call(
                &app,
                Req::new("POST", &format!("/api/v1/files/{file}/versions"))
                    .session(&me)
                    .header("If-Match", &format!("\"version:{head}\""))
                    .json(json!({"upload_id": upload.to_string()})),
            )
            .await;
            assert!(!r.status.is_success(), "{} {}", r.status, r.text);
        }
        assert_eq!(target.count("SELECT count(*) FROM file_versions").await, 1);
        let mut settings = MaintenanceSettings::new(
            Duration::from_secs(86_400),
            Duration::from_secs(86_400),
            Duration::from_secs(86_400),
            1 << 30,
        );
        settings.max_clock_drift = None;
        let job = Maintenance::new(
            target.pool.clone(),
            Some(target.store().await),
            Arc::new(craftcloud_api::SystemClock),
            settings,
        );
        match run_once(&job).await {
            RunOutcome::Completed(report) => assert!(!report.has_errors(), "{report:?}"),
            RunOutcome::Skipped => panic!("nothing else holds the lock"),
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_version_committed_during_a_backup_is_after_the_recovery_point() {
        let w = W::new().await;
        let dir = TempDir::new("backup-commit");
        let owner = w.user("owner").await;
        let org = w.org("during", owner.0).await;
        let ws = w.ws(&owner.1, org, "main").await;
        let project = w.project(&owner.1, ws, "P").await;
        let file = w.file(&owner.1, project, "a.fig").await;
        let first = pattern(10_000, 1);
        let first_version = w.push(&owner.1, file, &first).await;

        // Once the snapshot is taken, a new version is uploaded and committed.
        let hook: craftcloud_backup::PhaseHook = {
            let (app, session) = (w.env.app.clone(), owner.1.clone());
            Arc::new(move |phase| {
                let (app, session) = (app.clone(), session.clone());
                Box::pin(async move {
                    if phase == craftcloud_backup::Phase::SnapshotTaken {
                        let r = w2_push(&app, &session, file, &pattern(20_000, 2)).await;
                        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
                    }
                })
            })
        };
        let out = dir.child("b");
        let created = craftcloud_backup::create(
            &w.env.pool,
            &w.store().await,
            &out,
            &CreateOptions {
                server_version: "test",
                lock_wait: Duration::from_secs(30),
                on_phase: Some(hook),
            },
        )
        .await
        .unwrap();
        // The source has two versions now; the backup is the instant before.
        assert_eq!(w.env.count("SELECT count(*) FROM file_versions").await, 2);
        assert_eq!(created.blobs, 1);
        craftcloud_backup::verify(&out, None).unwrap();
        let target = Target::new().await;
        target.restore(&out).await.unwrap();
        assert_eq!(target.count("SELECT count(*) FROM file_versions").await, 1);
        let head: Uuid = sqlx::query_scalar("SELECT head_version_id FROM files WHERE id = $1")
            .bind(file)
            .fetch_one(&target.pool)
            .await
            .unwrap();
        assert_eq!(head, first_version);
        assert_eq!(
            target
                .count("SELECT COALESCE(sum(committed_bytes), 0)::bigint FROM organizations")
                .await,
            first.len() as i64
        );
    }

    // ---- health, audit, secrets, permissions -----------------------------------------

    async fn get_health(w: &W) -> Resp {
        call(
            &w.env.app,
            Req::new("GET", "/api/v1/admin/health").cookie_only(&w.op),
        )
        .await
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn health_audit_and_the_manifest_after_a_backup() {
        let w = W::new().await;
        let dir = TempDir::new("backup-health");
        let before = get_health(&w).await;
        assert_eq!(before.status, StatusCode::OK, "{}", before.text);
        assert_eq!(
            before.body["backup"]["last_success_age_seconds"],
            Value::Null
        );

        let (out, created) = small_backup(&w, &dir).await;
        let after = get_health(&w).await;
        let age = after.body["backup"]["last_success_age_seconds"]
            .as_u64()
            .unwrap_or_else(|| panic!("an age after a backup: {}", after.text));
        assert!(age < 60, "{age}");
        w.env.clock.advance(Duration::from_secs(3600));
        let later = get_health(&w).await;
        // The test clock started a few seconds before the backup.
        assert!(
            later.body["backup"]["last_success_age_seconds"]
                .as_u64()
                .unwrap()
                >= 3500
        );

        // The audit event: instance level, success, counts only.
        let row: (Option<Uuid>, Option<Uuid>, String, Value) = sqlx::query_as(
            "SELECT org_id, actor_id, result, details FROM audit_events WHERE action = 'backup.created'",
        )
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!((row.0, row.1, row.2.as_str()), (None, None, "success"));
        assert_eq!(row.3["blobs"], 1);
        assert_eq!(row.3["migration"], 18);

        // The manifest holds no secrets: neither a connection string nor the
        // password of the database URL, and no names.
        let manifest = std::fs::read_to_string(out.join("manifest.json")).unwrap();
        let url = std::env::var(ENV).unwrap();
        let password = url
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split('@').next())
            .and_then(|credentials| credentials.split(':').nth(1))
            .unwrap_or("-")
            .to_owned();
        assert!(!manifest.contains("postgres://"));
        assert!(!manifest.contains(&password));
        assert!(!manifest.contains("example.test"));
        assert_eq!(
            created.manifest.excluded_tables.len(),
            EXCLUDED_TABLES.len()
        );

        // Only the owner may read the backup (unix); the digest file names the manifest.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&out), 0o700);
            assert_eq!(mode(&out.join("tables")), 0o700);
            assert_eq!(mode(&out.join("manifest.json")), 0o600);
            assert_eq!(mode(&first_blob(&out)), 0o600);
        }
        let sums = std::fs::read_to_string(out.join("manifest.sha256")).unwrap();
        assert_eq!(
            sums,
            format!("{}  manifest.json\n", created.manifest_sha256)
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn create_refuses_bad_targets_and_an_unpaired_store() {
        let w = W::new().await;
        let dir = TempDir::new("backup-refuse");

        // An output directory with content is never written into.
        let used = dir.child("used");
        std::fs::create_dir_all(&used).unwrap();
        std::fs::write(used.join("keep.txt"), b"keep").unwrap();
        let error = w.create_backup(&used).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Io, "{error}");
        assert_eq!(std::fs::read(used.join("keep.txt")).unwrap(), b"keep");
        // A file is not a directory either.
        let error = w.create_backup(&used.join("keep.txt")).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Io, "{error}");

        // A store directory that belongs to another database.
        let foreign = TempDir::new("foreign-store");
        let store = LocalStore::open(foreign.path()).await.unwrap();
        store.write_marker(Uuid::new_v4(), false).await.unwrap();
        let out = dir.child("foreign");
        let error = craftcloud_backup::create(
            &w.env.pool,
            &store,
            &out,
            &CreateOptions {
                server_version: "test",
                lock_wait: Duration::from_secs(1),
                on_phase: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Binding, "{error}");
        assert!(!out.exists(), "a failed backup removes what it created");

        // An empty empty directory is fine and is reused.
        let empty = dir.child("empty");
        std::fs::create_dir_all(&empty).unwrap();
        w.create_backup(&empty).await.unwrap();
        craftcloud_backup::verify(&empty, None).unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn every_table_is_either_backed_up_or_excluded_on_purpose() {
        let w = W::new().await;
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT tablename::text FROM pg_tables WHERE schemaname = 'public' ORDER BY 1",
        )
        .fetch_all(&w.env.pool)
        .await
        .unwrap();
        for name in &names {
            let known = BACKUP_TABLES.iter().any(|t| t.name == name)
                || EXCLUDED_TABLES.contains(&name.as_str())
                || name == MIGRATIONS_TABLE;
            assert!(
                known,
                "table {name} is neither in BACKUP_TABLES nor in EXCLUDED_TABLES: decide whether a backup needs it"
            );
        }
        for spec in BACKUP_TABLES {
            assert!(names.iter().any(|n| n == spec.name), "{}", spec.name);
        }
        // No serial columns in the backed-up tables: no sequence to reset.
        let serials: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_class c JOIN pg_depend d ON d.objid = c.oid AND d.deptype IN ('a', 'i') \
             JOIN pg_class t ON t.oid = d.refobjid \
             WHERE c.relkind = 'S' AND t.relname = ANY($1)",
        )
        .bind(
            BACKUP_TABLES
                .iter()
                .map(|t| t.name.to_owned())
                .collect::<Vec<_>>(),
        )
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(
            serials, 0,
            "a backed-up table has a sequence that a restore would have to reset"
        );
    }

    // ---- review fixes: changes after verify, owner role, locks, binding --------------

    /// A restore hook that runs `action` at `phase`.
    fn restore_hook(
        phase: craftcloud_backup::RestorePhase,
        action: impl Fn() + Send + Sync + 'static,
    ) -> craftcloud_backup::RestoreHook {
        let action = Arc::new(action);
        Arc::new(move |seen| {
            let action = Arc::clone(&action);
            Box::pin(async move {
                if seen == phase {
                    action();
                }
            })
        })
    }

    async fn restore_with_hook(
        target: &Target,
        out: &Path,
        hook: craftcloud_backup::RestoreHook,
    ) -> Result<RestoreReport, BackupError> {
        craftcloud_backup::restore(
            &target.pool,
            &target.store().await,
            out,
            &RestoreOptions {
                on_phase: Some(hook),
                ..RestoreOptions::default()
            },
        )
        .await
    }

    /// Nothing of a restore is left: no rows, no binding, no marker, no objects.
    async fn assert_nothing_restored(target: &Target) {
        for table in [
            "users",
            "organizations",
            "files",
            "blobs",
            "audit_events",
            "storage_binding",
        ] {
            assert_eq!(
                target.count(&format!("SELECT count(*) FROM {table}")).await,
                0,
                "{table}"
            );
        }
        let store = target.store().await;
        assert!(
            craftcloud_storage::ObjectStore::list_objects(&store, None, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_table_file_swapped_after_verify_fails_the_restore_and_commits_nothing() {
        let w = W::new().await;
        let dir = TempDir::new("backup-swap-table");
        let (out, created) = small_backup(&w, &dir).await;
        let target = Target::new().await;
        // Same length, still valid COPY text, different content: only the
        // checksum taken while loading can notice.
        let users = out.join("tables").join("users.copy");
        let hook = restore_hook(craftcloud_backup::RestorePhase::BlobsWritten, {
            let users = users.clone();
            move || {
                let text = std::fs::read_to_string(&users).unwrap();
                std::fs::write(&users, text.replacen("owner", "ownex", 1)).unwrap();
            }
        });
        let error = craftcloud_backup::restore(
            &target.pool,
            &target.store().await,
            &out,
            &RestoreOptions {
                expected_manifest_sha256: Some(&created.manifest_sha256),
                on_phase: Some(hook),
                ..RestoreOptions::default()
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Integrity, "{error}");
        assert!(
            error.message.contains("changed after verification"),
            "{error}"
        );
        assert_nothing_restored(&target).await;
        assert!(
            target
                .store()
                .await
                .read_marker_info()
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_blob_list_swapped_after_verify_fails_the_restore() {
        let w = W::new().await;
        let dir = TempDir::new("backup-swap-list");
        let (out, _) = small_backup(&w, &dir).await;
        let target = Target::new().await;
        let list = out.join("blobs.jsonl");
        let hook = restore_hook(craftcloud_backup::RestorePhase::Verified, {
            let list = list.clone();
            move || std::fs::write(&list, b"").unwrap()
        });
        let error = restore_with_hook(&target, &out, hook).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Integrity, "{error}");
        assert_nothing_restored(&target).await;
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn restore_refuses_a_role_that_does_not_own_the_audit_log() {
        let w = W::new().await;
        let dir = TempDir::new("backup-role");
        let (out, _) = small_backup(&w, &dir).await;
        let target = Target::new().await;
        craftcloud_db::migrate::run(&target.pool).await.unwrap();
        let role = format!("cc_rt_{}", Uuid::new_v4().simple());
        let password = "role-password-for-the-test";
        sqlx::query(&format!(
            "CREATE ROLE \"{role}\" LOGIN PASSWORD '{password}'"
        ))
        .execute(&target.pool)
        .await
        .unwrap();
        craftcloud_db::migrate::grant_runtime_role(&target.pool, &role)
            .await
            .unwrap();
        let runtime_pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(
                target
                    .admin
                    .clone()
                    .database(&target.name)
                    .username(&role)
                    .password(password),
            )
            .await
            .unwrap();
        let error = craftcloud_backup::restore(
            &runtime_pool,
            &target.store().await,
            &out,
            &RestoreOptions::default(),
        )
        .await
        .unwrap_err();
        runtime_pool.close().await;
        assert_eq!(error.kind, ErrorKind::Schema, "{error}");
        assert!(error.message.contains("table owner"), "{error}");
        assert!(error.message.contains(&role), "{error}");
        assert_nothing_restored(&target).await;
        // The owner (single-role mode: the connected user owns the tables) works.
        target.restore(&out).await.unwrap();
        sqlx::query(&format!("DROP OWNED BY \"{role}\""))
            .execute(&target.pool)
            .await
            .unwrap();
        sqlx::query(&format!("DROP ROLE \"{role}\""))
            .execute(&target.pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn two_concurrent_restores_into_one_target_leave_exactly_one_restore() {
        let w = W::new().await;
        let dir = TempDir::new("backup-concurrent");
        let (out, _) = small_backup(&w, &dir).await;
        let target = Target::new().await;

        // The lock is held by someone else: busy, nothing written.
        let lock = craftcloud_db::maintenance::try_lock(&target.pool)
            .await
            .unwrap()
            .unwrap();
        let error = target.restore(&out).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Busy, "{error}");
        assert_eq!(
            target
                .count("SELECT count(*) FROM pg_tables WHERE schemaname = 'public'")
                .await,
            0
        );
        lock.release().await.unwrap();

        // Two at once: exactly one wins, the loser fails cleanly and does not
        // remove the winner's objects.
        let (a, b) = tokio::join!(target.restore(&out), target.restore(&out));
        let (winner, loser) = match (a, b) {
            (Ok(w), Err(e)) | (Err(e), Ok(w)) => (w, e),
            other => panic!("exactly one restore must succeed: {other:?}"),
        };
        assert!(
            matches!(loser.kind, ErrorKind::Busy | ErrorKind::TargetNotEmpty),
            "{loser}"
        );
        assert_eq!(winner.blobs, 1);
        assert_eq!(
            target.count("SELECT count(*) FROM storage_binding").await,
            1
        );
        assert_eq!(
            target
                .count("SELECT count(*) FROM audit_events WHERE action = 'backup.restored'")
                .await,
            1
        );
        let rows: Vec<(String,)> = sqlx::query_as("SELECT object_key FROM blobs")
            .fetch_all(&target.pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(object_path(target.store_dir.path(), &rows[0].0).is_file());
        let marker = target
            .store()
            .await
            .read_marker_info()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(marker.store_id, winner.store_id);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_server_that_binds_or_marks_the_store_meanwhile_makes_the_restore_fail_cleanly() {
        let w = W::new().await;
        let dir = TempDir::new("backup-bind-race");
        let (out, _) = small_backup(&w, &dir).await;

        // A binding row appears in the database between blob copy and load.
        let target = Target::new().await;
        let hook = Arc::new({
            let pool = target.pool.clone();
            move |phase: craftcloud_backup::RestorePhase| {
                let pool = pool.clone();
                Box::pin(async move {
                    if phase == craftcloud_backup::RestorePhase::BlobsWritten {
                        sqlx::query("INSERT INTO storage_binding (id, store_id, bound_at) VALUES (true, gen_random_uuid(), now())")
                            .execute(&pool)
                            .await
                            .unwrap();
                    }
                })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            }
        });
        let error = restore_with_hook(&target, &out, hook).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Database, "{error}");
        // Only the foreign binding remains; no rows, no marker, no objects.
        assert_eq!(
            target.count("SELECT count(*) FROM storage_binding").await,
            1
        );
        assert_eq!(target.count("SELECT count(*) FROM users").await, 0);
        assert!(
            target
                .store()
                .await
                .read_marker_info()
                .await
                .unwrap()
                .is_none()
        );

        // A marker file appears in the store: the restore fails and leaves the
        // foreign marker alone.
        let target = Target::new().await;
        let marker_path = target.store_dir.path().join("craftcloud-store.json");
        let foreign = Uuid::new_v4();
        let hook = restore_hook(craftcloud_backup::RestorePhase::BlobsWritten, move || {
            std::fs::write(
                &marker_path,
                format!(
                    "{{\"store_id\":\"{foreign}\",\"epoch\":0}}
"
                ),
            )
            .unwrap();
        });
        let error = restore_with_hook(&target, &out, hook).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Storage, "{error}");
        assert_eq!(target.count("SELECT count(*) FROM users").await, 0);
        assert_eq!(
            target.count("SELECT count(*) FROM storage_binding").await,
            0
        );
        let marker = target
            .store()
            .await
            .read_marker_info()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(marker.store_id, foreign);
        assert!(
            craftcloud_storage::ObjectStore::list_objects(&target.store().await, None, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_backup_older_than_the_audit_log_is_refused_clearly() {
        let w = W::new().await;
        let dir = TempDir::new("backup-too-old");
        let (out, _) = small_backup(&w, &dir).await;
        rewrite_manifest(&out, |m| m["migration_version"] = json!(10));
        let error = craftcloud_backup::verify(&out, None).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Format);
        assert!(error.message.contains("backup support"), "{error}");
        let target = Target::new().await;
        assert!(target.restore(&out).await.is_err());
        assert_eq!(
            target
                .count("SELECT count(*) FROM pg_tables WHERE schemaname = 'public'")
                .await,
            0
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_failed_backup_into_an_existing_empty_directory_leaves_it_empty() {
        let w = W::new().await;
        let dir = TempDir::new("backup-existing");
        let out = dir.child("existing");
        std::fs::create_dir_all(&out).unwrap();
        let lock = craftcloud_db::maintenance::try_lock(&w.env.pool)
            .await
            .unwrap()
            .unwrap();
        let error = craftcloud_backup::create(
            &w.env.pool,
            &w.store().await,
            &out,
            &CreateOptions {
                server_version: "test",
                lock_wait: Duration::from_millis(300),
                on_phase: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Busy);
        lock.release().await.unwrap();
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);

        // Failure after files were written: a damaged object aborts the copy.
        let owner = w.user("owner").await;
        let org = w.org("existing", owner.0).await;
        let ws = w.ws(&owner.1, org, "main").await;
        let project = w.project(&owner.1, ws, "P").await;
        let file = w.file(&owner.1, project, "a.fig").await;
        w.push(&owner.1, file, &pattern(5000, 1)).await;
        let key: String = sqlx::query_scalar("SELECT object_key FROM blobs")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        let object = object_path(&w.env.storage_dir, &key);
        let mut permissions = std::fs::metadata(&object).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&object, permissions).unwrap();
        flip_byte(&object);
        let error = w.create_backup(&out).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::SourceDamaged, "{error}");
        assert_eq!(
            std::fs::read_dir(&out).unwrap().count(),
            0,
            "the entries written by the failed backup are removed"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&out).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_health_query_uses_the_instance_action_index() {
        let w = W::new().await;
        let mut tx = w.env.pool.begin().await.unwrap();
        sqlx::query("SET LOCAL enable_seqscan = off")
            .execute(&mut *tx)
            .await
            .unwrap();
        let plan: Vec<(String,)> = sqlx::query_as(
            "EXPLAIN SELECT max(created_at) FROM audit_events WHERE action = 'backup.created' AND org_id IS NULL AND result = 'success'",
        )
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        let plan: String = plan
            .into_iter()
            .map(|(l,)| l)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plan.contains("audit_events_instance_action_idx"), "{plan}");
    }
}
