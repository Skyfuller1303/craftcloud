//! Tests of the maintenance job (CC-F10, CC-P02-06, CC-P02-07): retention
//! purge, version retention, blob garbage collection with its two phases,
//! reconciliation, integrity verification, usage drift, the run lock, dry
//! runs and the operator status route. All tests need PostgreSQL
//! (`cargo xtask db-test`); time is the manual test clock.

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::StatusCode;
    use craftcloud_api::Clock;
    use craftcloud_api::maintenance::{
        Binding, BindingMismatch, Maintenance, MaintenanceSettings, RunOptions, RunOutcome,
        RunReport, Trigger, check_existing_store_binding, check_store_binding,
    };
    use craftcloud_db::maintenance as dbm;
    use craftcloud_db::members::OrgTx;
    use craftcloud_storage::{LocalStore, ObjectStore, WriteSink};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use tokio::sync::watch;
    use uuid::Uuid;

    use crate::common::{Req, Resp, Session, TestEnv, call, capture_logs};

    const DAY: u64 = 24 * 3600;

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

    fn tag(id: Uuid) -> String {
        format!("\"version:{id}\"")
    }

    fn files_below(dir: &Path) -> usize {
        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    count += files_below(&path);
                } else {
                    count += 1;
                }
            }
        }
        count
    }

    fn make_writable(path: &Path) {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    /// Settings for tests with a manual clock that jumps by days: the clock
    /// check against the database is off (one test turns it on).
    fn settings(verify_bytes: u64) -> MaintenanceSettings {
        let mut settings = MaintenanceSettings::new(
            Duration::from_secs(30 * DAY),
            Duration::from_secs(30 * DAY),
            Duration::from_secs(DAY),
            verify_bytes,
        );
        settings.max_clock_drift = None;
        settings
    }

    struct W {
        env: TestEnv,
        op: Session,
        owner: (Uuid, Session),
        org: Uuid,
        ws: Uuid,
    }

    impl W {
        /// An organization with an owner and one workspace.
        async fn new() -> Self {
            let env = TestEnv::new().await;
            let op = env.login_ok("op").await;
            let owner_id = env.user("owner").await;
            let owner = (owner_id, env.login_ok("owner").await);
            let created = call(
                &env.app,
                Req::new("POST", "/api/v1/organizations")
                    .session(&op)
                    .json(json!({
                        "name": "Org acme", "slug": "acme", "owner_user_id": owner_id.to_string()
                    })),
            )
            .await;
            assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
            let org = id_of(&created);
            let ws = call(
                &env.app,
                Req::new("POST", &format!("/api/v1/organizations/{org}/workspaces"))
                    .session(&owner.1)
                    .json(json!({"name": "WS main", "slug": "main"})),
            )
            .await;
            assert_eq!(ws.status, StatusCode::CREATED, "{}", ws.text);
            let ws = id_of(&ws);
            Self {
                env,
                op,
                owner,
                org,
                ws,
            }
        }

        /// A new login of the owner (sessions idle out when the clock jumps).
        async fn relogin(&mut self) {
            self.owner.1 = self.env.login_ok("owner").await;
            self.op = self.env.login_ok("op").await;
        }

        async fn age(&mut self, days: u64) {
            self.env.clock.advance(Duration::from_secs(days * DAY));
            self.relogin().await;
        }

        async fn send(&self, req: Req) -> Resp {
            call(&self.env.app, req).await
        }

        async fn post(&self, path: &str, body: Value) -> Resp {
            self.send(Req::new("POST", path).session(&self.owner.1).json(body))
                .await
        }

        async fn delete(&self, path: &str) -> Resp {
            self.send(Req::new("DELETE", path).session(&self.owner.1))
                .await
        }

        async fn project(&self, name: &str) -> Uuid {
            let r = self
                .post(
                    &format!("/api/v1/workspaces/{}/projects", self.ws),
                    json!({"name": name}),
                )
                .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            id_of(&r)
        }

        async fn file(&self, project: Uuid, name: &str) -> Uuid {
            let r = self
                .post(
                    &format!("/api/v1/projects/{project}/files"),
                    json!({"name": name, "app": "figcraft", "format": "fig"}),
                )
                .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            id_of(&r)
        }

        async fn stored_upload(&self, file: Uuid, data: &[u8]) -> Uuid {
            let created = self
                .post(
                    &format!("/api/v1/files/{file}/uploads"),
                    json!({"size_bytes": data.len(), "sha256": sha_hex(data)}),
                )
                .await;
            assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
            let id = Uuid::parse_str(created.body["upload_id"].as_str().unwrap()).unwrap();
            let put = self
                .send(
                    Req::new("PUT", &format!("/api/v1/uploads/{id}/content"))
                        .session(&self.owner.1)
                        .body(Body::from(data.to_vec())),
                )
                .await;
            assert_eq!(put.status, StatusCode::OK, "{}", put.text);
            id
        }

        async fn head(&self, file: Uuid) -> Option<Uuid> {
            let r = self
                .send(Req::new("GET", &format!("/api/v1/files/{file}")).cookie_only(&self.owner.1))
                .await;
            r.body["head_version_id"]
                .as_str()
                .map(|s| Uuid::parse_str(s).unwrap())
        }

        /// Uploads and commits `data` as the next version; returns the version id.
        async fn push(&self, file: Uuid, data: &[u8]) -> Uuid {
            let head = self.head(file).await;
            let upload = self.stored_upload(file, data).await;
            let (name, value) = match head {
                None => ("If-None-Match", "*".to_owned()),
                Some(head) => ("If-Match", tag(head)),
            };
            let r = self
                .send(
                    Req::new("POST", &format!("/api/v1/files/{file}/versions"))
                        .session(&self.owner.1)
                        .header(name, &value)
                        .json(json!({"upload_id": upload.to_string()})),
                )
                .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            id_of(&r)
        }

        async fn restore(&self, file: Uuid, version: Uuid, head: Uuid) -> Resp {
            self.send(
                Req::new(
                    "POST",
                    &format!("/api/v1/files/{file}/versions/{version}/restore"),
                )
                .session(&self.owner.1)
                .header("If-Match", &tag(head)),
            )
            .await
        }

        async fn count(&self, sql: &str) -> i64 {
            self.env.count(sql).await
        }

        /// (org reserved, org committed, workspace reserved, workspace committed).
        async fn counters(&self) -> (i64, i64, i64, i64) {
            let org: (i64, i64) = sqlx::query_as(
                "SELECT COALESCE(sum(reserved_bytes),0)::bigint, COALESCE(sum(committed_bytes),0)::bigint FROM organizations",
            )
            .fetch_one(&self.env.pool)
            .await
            .unwrap();
            let ws: (i64, i64) = sqlx::query_as(
                "SELECT COALESCE(sum(reserved_bytes),0)::bigint, COALESCE(sum(committed_bytes),0)::bigint FROM workspaces",
            )
            .fetch_one(&self.env.pool)
            .await
            .unwrap();
            (org.0, org.1, ws.0, ws.1)
        }

        fn objects_dir(&self) -> PathBuf {
            self.env.storage_dir.join("objects")
        }

        fn object_path(&self, key: &str) -> PathBuf {
            self.objects_dir()
                .join(&key[0..2])
                .join(&key[2..4])
                .join(key)
        }

        async fn store(&self) -> LocalStore {
            LocalStore::open_with_faults(&self.env.storage_dir, self.env.faults.clone())
                .await
                .unwrap()
        }

        async fn job_with(&self, settings: MaintenanceSettings) -> Maintenance {
            Maintenance::new(
                self.env.pool.clone(),
                Some(self.store().await),
                self.env.clock.clone(),
                settings,
            )
        }

        async fn job(&self) -> Maintenance {
            self.job_with(settings(1024 * 1024 * 1024)).await
        }

        async fn blob_state(&self, key_size: i64) -> String {
            sqlx::query_scalar("SELECT state FROM blobs WHERE size_bytes = $1")
                .bind(key_size)
                .fetch_one(&self.env.pool)
                .await
                .unwrap()
        }

        async fn object_key(&self, size: i64) -> String {
            sqlx::query_scalar("SELECT object_key FROM blobs WHERE size_bytes = $1")
                .bind(size)
                .fetch_one(&self.env.pool)
                .await
                .unwrap()
        }
    }

    /// Row counts of the tables a run may change.
    async fn snapshot(w: &W) -> Vec<i64> {
        let mut out = Vec::new();
        for table in [
            "files",
            "file_versions",
            "blobs",
            "upload_sessions",
            "projects",
            "maintenance_runs",
            "storage_binding",
        ] {
            out.push(w.count(&format!("SELECT count(*) FROM {table}")).await);
        }
        out
    }

    async fn run_with(job: &Maintenance, dry_run: bool) -> RunReport {
        let (_keep, stop) = watch::channel(false);
        let options = RunOptions {
            dry_run,
            trigger: Trigger::Scheduled,
        };
        match job.run_once(options, &stop).await.unwrap() {
            RunOutcome::Completed(report) => report,
            RunOutcome::Skipped => panic!("the run was skipped"),
        }
    }

    async fn run(job: &Maintenance) -> RunReport {
        run_with(job, false).await
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn recall_binding_refuses_unbound_database_and_foreign_empty_store() {
        let env = TestEnv::new().await;
        let now = env.clock.now();
        let bound_store = LocalStore::open(&env.storage_dir).await.unwrap();

        assert!(matches!(
            check_existing_store_binding(&env.pool, &bound_store, now)
                .await
                .unwrap(),
            Binding::Mismatch(BindingMismatch::DatabaseUnbound)
        ));
        assert!(bound_store.read_marker_info().await.unwrap().is_none());

        assert!(matches!(
            check_store_binding(&env.pool, &bound_store, now, true)
                .await
                .unwrap(),
            Binding::Created(_)
        ));
        let binding_before = craftcloud_db::maintenance::store_binding_epoch(&env.pool)
            .await
            .unwrap();

        let foreign_dir = env.storage_dir.join("foreign-empty-store");
        std::fs::create_dir_all(&foreign_dir).unwrap();
        let foreign_store = LocalStore::open(&foreign_dir).await.unwrap();
        assert!(matches!(
            check_existing_store_binding(&env.pool, &foreign_store, now)
                .await
                .unwrap(),
            Binding::Mismatch(BindingMismatch::MarkerMissing)
        ));
        assert!(foreign_store.read_marker_info().await.unwrap().is_none());
        assert!(
            foreign_store
                .list_objects(None, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            craftcloud_db::maintenance::store_binding_epoch(&env.pool)
                .await
                .unwrap(),
            binding_before
        );
    }

    // ---- retention purge ---------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_trashed_file_is_purged_only_after_the_retention() {
        let mut w = W::new().await;
        let project = w.project("Docs").await;
        let doomed = w.file(project, "doomed.fig").await;
        let live = w.file(project, "live.fig").await;
        w.push(doomed, &pattern(111, 1)).await;
        w.push(doomed, &pattern(222, 2)).await;
        w.push(live, &pattern(444, 3)).await;
        assert_eq!(w.counters().await, (0, 777, 0, 777));
        let job = w.job().await;
        let deleted = w.delete(&format!("/api/v1/files/{doomed}")).await;
        assert!(deleted.status.is_success(), "{}", deleted.text);

        // Before the retention has passed nothing is removed.
        w.age(29).await;
        let early = run(&job).await;
        assert_eq!(early.count("purge_trash", "purged_files"), 0);
        assert_eq!(w.count("SELECT count(*) FROM files").await, 2);
        assert_eq!(w.count("SELECT count(*) FROM file_versions").await, 3);
        assert_eq!(files_below(&w.objects_dir()), 3);
        assert_eq!(w.counters().await, (0, 777, 0, 777));

        // Past the retention the file, its versions and (unreferenced) blobs go.
        w.age(2).await;
        let late = run(&job).await;
        assert!(!late.has_errors(), "{late:?}");
        assert_eq!(late.count("purge_trash", "purged_files"), 1);
        assert_eq!(late.count("purge_trash", "purged_versions"), 2);
        assert_eq!(late.count("collect_blobs", "blobs_deleted"), 2);
        assert_eq!(late.count("collect_blobs", "released_bytes"), 333);
        assert_eq!(w.count("SELECT count(*) FROM files").await, 1);
        assert_eq!(w.count("SELECT count(*) FROM file_versions").await, 1);
        assert_eq!(w.count("SELECT count(*) FROM blobs").await, 1);
        assert_eq!(files_below(&w.objects_dir()), 1);
        // The quota fell by exactly the removed bytes, in organization and workspace.
        assert_eq!(w.counters().await, (0, 444, 0, 444));
        // The live file is untouched.
        assert_eq!(w.blob_state(444).await, "committed");
        assert!(w.head(live).await.is_some());

        // Nothing left to do.
        let again = run(&job).await;
        assert_eq!(again.count("purge_trash", "purged_files"), 0);
        assert_eq!(again.count("collect_blobs", "claimed"), 0);
        assert_eq!(w.counters().await, (0, 444, 0, 444));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_trashed_project_is_purged_with_all_its_files() {
        let mut w = W::new().await;
        let doomed = w.project("Old project").await;
        let survivor = w.project("Other project").await;
        let a = w.file(doomed, "a.fig").await;
        let b = w.file(doomed, "b.fig").await;
        let kept = w.file(survivor, "kept.fig").await;
        w.push(a, &pattern(101, 1)).await;
        w.push(a, &pattern(102, 2)).await;
        w.push(b, &pattern(103, 3)).await;
        w.push(kept, &pattern(500, 4)).await;
        // b is in the trash on its own, a is live when the project goes.
        assert!(
            w.delete(&format!("/api/v1/files/{b}"))
                .await
                .status
                .is_success()
        );
        assert!(
            w.delete(&format!("/api/v1/projects/{doomed}"))
                .await
                .status
                .is_success()
        );
        let job = w.job().await;

        w.age(29).await;
        let early = run(&job).await;
        assert_eq!(early.count("purge_trash", "purged_projects"), 0);
        assert_eq!(w.count("SELECT count(*) FROM projects").await, 2);

        w.age(2).await;
        let late = run(&job).await;
        assert!(!late.has_errors(), "{late:?}");
        assert_eq!(late.count("purge_trash", "purged_projects"), 1);
        assert_eq!(late.count("purge_trash", "purged_files"), 2);
        assert_eq!(w.count("SELECT count(*) FROM projects").await, 1);
        assert_eq!(w.count("SELECT count(*) FROM files").await, 1);
        assert_eq!(w.count("SELECT count(*) FROM file_versions").await, 1);
        assert_eq!(w.count("SELECT count(*) FROM blobs").await, 1);
        assert_eq!(files_below(&w.objects_dir()), 1);
        assert_eq!(w.counters().await, (0, 500, 0, 500));
        assert!(w.head(kept).await.is_some());
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn purging_a_project_closes_open_uploads_and_releases_their_reservation() {
        let w = W::new().await;
        let project = w.project("Busy").await;
        let file = w.file(project, "busy.fig").await;
        w.push(file, &pattern(64, 1)).await;
        w.stored_upload(file, &pattern(300, 2)).await;
        assert_eq!(w.counters().await, (300, 64, 300, 64));
        assert!(
            w.delete(&format!("/api/v1/projects/{project}"))
                .await
                .status
                .is_success()
        );
        // The purge itself (the job calls this once the retention has passed);
        // the upload is still open here, which only a direct call can show.
        let now = w.env.clock.now();
        let cutoff = now + Duration::from_secs(DAY);
        let mut tx = OrgTx::begin(&w.env.pool, w.org).await.unwrap().unwrap();
        let counts = tx
            .maint_purge_project(project, cutoff, now, 100)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!((counts.projects, counts.files), (1, 1));
        assert_eq!(counts.versions, 1);
        assert_eq!(counts.sessions, 2);
        assert_eq!(w.count("SELECT count(*) FROM projects").await, 0);
        assert_eq!(w.count("SELECT count(*) FROM upload_sessions").await, 0);
        // The announced bytes wait for the garbage collection like the committed ones: the
        // object of the stored upload is still on disk.
        assert_eq!(w.counters().await, (300, 64, 300, 64));
        assert_eq!(w.blob_state(300).await, "orphaned");
        assert_eq!(w.blob_state(64).await, "committed");
        // A live project is never purged, whatever the cutoff.
        let live = w.project("Live").await;
        let mut tx = OrgTx::begin(&w.env.pool, w.org).await.unwrap().unwrap();
        let none = tx
            .maint_purge_project(live, cutoff, now, 100)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(none, dbm::PurgeCounts::default());
        assert_eq!(w.count("SELECT count(*) FROM projects").await, 1);
    }

    // ---- version retention ------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn version_retention_keeps_the_head_and_recent_versions() {
        let mut w = W::new().await;
        let project = w.project("Versions").await;
        let file = w.file(project, "v.fig").await;
        let job = w.job().await;
        let v1 = w.push(file, &pattern(100, 1)).await; // day 0
        w.age(5).await;
        let v2 = w.push(file, &pattern(200, 2)).await; // day 5
        w.age(45).await;
        let v3 = w.push(file, &pattern(300, 3)).await; // day 50
        w.age(10).await;
        let v4 = w.push(file, &pattern(400, 4)).await; // day 60
        w.age(2).await;
        // Restoring v2 (day 62) makes a head that shares the blob of v2.
        let restored = w.restore(file, v2, v4).await;
        assert_eq!(restored.status, StatusCode::CREATED, "{}", restored.text);
        let v5 = id_of(&restored);
        w.age(3).await; // day 65
        assert_eq!(w.counters().await, (0, 1000, 0, 1000));

        // v1 was superseded 60 days ago, v2 only 15 days ago.
        let first = run(&job).await;
        assert!(!first.has_errors(), "{first:?}");
        assert_eq!(first.count("trim_versions", "deleted_versions"), 1);
        assert_eq!(first.count("collect_blobs", "blobs_deleted"), 1);
        assert_eq!(w.count("SELECT count(*) FROM file_versions").await, 4);
        assert_eq!(
            w.count(&format!(
                "SELECT count(*) FROM file_versions WHERE id = '{v1}'"
            ))
            .await,
            0
        );
        // The lineage pointer to the deleted version is cleared, the version stays.
        assert_eq!(
            w.count(&format!(
                "SELECT count(*) FROM file_versions WHERE id = '{v2}' AND base_version_id IS NULL"
            ))
            .await,
            1
        );
        assert_eq!(w.counters().await, (0, 900, 0, 900));
        assert_eq!(w.head(file).await, Some(v5));

        // Day 100: v2, v3 and v4 are all older than the retention; the head stays.
        w.age(35).await;
        let second = run(&job).await;
        assert!(!second.has_errors(), "{second:?}");
        assert_eq!(second.count("trim_versions", "deleted_versions"), 3);
        assert_eq!(w.count("SELECT count(*) FROM file_versions").await, 1);
        assert_eq!(w.head(file).await, Some(v5));
        // The blob of v2 lives on through the restored head; the others are gone.
        assert_eq!(second.count("collect_blobs", "blobs_deleted"), 2);
        assert_eq!(w.count("SELECT count(*) FROM blobs").await, 1);
        assert_eq!(w.blob_state(200).await, "committed");
        assert_eq!(files_below(&w.objects_dir()), 1);
        assert_eq!(w.counters().await, (0, 200, 0, 200));
        assert_eq!(
            w.count(&format!(
                "SELECT count(*) FROM file_versions WHERE id = '{v5}' \
                 AND restored_from_version_id IS NULL AND base_version_id IS NULL"
            ))
            .await,
            1
        );
        // Idempotent.
        let third = run(&job).await;
        assert_eq!(third.count("trim_versions", "deleted_versions"), 0);
        assert_eq!(w.counters().await, (0, 200, 0, 200));
        let _ = (v3, v4);
    }

    // ---- blob garbage collection --------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn blob_gc_deletes_unreferenced_blobs_only() {
        let w = W::new().await;
        let project = w.project("Gc").await;
        let file = w.file(project, "gc.fig").await;
        let spare = w.file(project, "spare.fig").await;
        w.push(file, &pattern(111, 1)).await; // committed and referenced
        let open = w.stored_upload(spare, &pattern(222, 2)).await; // open session, staged blob
        let aborted = w.stored_upload(spare, &pattern(333, 3)).await;
        let r = w.delete(&format!("/api/v1/uploads/{aborted}")).await;
        assert!(r.status.is_success(), "{}", r.text);
        assert_eq!(w.blob_state(333).await, "orphaned");
        let job = w.job().await;

        // The orphan is too young (grace = upload TTL + 1 h).
        let young = run(&job).await;
        assert_eq!(young.count("collect_blobs", "candidates"), 0);
        assert_eq!(w.count("SELECT count(*) FROM blobs").await, 3);

        // Make every blob old without touching the sessions.
        sqlx::query("UPDATE blobs SET created_at = created_at - interval '3 days'")
            .execute(&w.env.pool)
            .await
            .unwrap();
        let dry = run_with(&job, true).await;
        assert_eq!(dry.count("collect_blobs", "candidates"), 1);
        assert_eq!(w.count("SELECT count(*) FROM blobs").await, 3);
        let old = run(&job).await;
        assert!(!old.has_errors(), "{old:?}");
        assert_eq!(old.count("collect_blobs", "blobs_deleted"), 1);
        // Only the orphan is gone; the referenced blob and the open upload's blob stay.
        assert_eq!(w.count("SELECT count(*) FROM blobs").await, 2);
        assert_eq!(w.blob_state(111).await, "committed");
        assert_eq!(w.blob_state(222).await, "staged");
        assert_eq!(files_below(&w.objects_dir()), 2);
        // The orphan was never counted as committed; the open upload keeps its reservation.
        assert_eq!(w.counters().await, (222, 111, 222, 111));
        let state: String = sqlx::query_scalar("SELECT state FROM upload_sessions WHERE id = $1")
            .bind(open)
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(state, "stored");
    }

    /// SR-01: aborting a stored upload must not free quota while the object is still on disk.
    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn aborting_stored_uploads_cannot_exceed_the_quota_until_the_gc_removes_the_object() {
        let w = W::new().await;
        let project = w.project("Quota").await;
        let file = w.file(project, "quota.fig").await;
        sqlx::query("UPDATE organizations SET quota_bytes = 1000")
            .execute(&w.env.pool)
            .await
            .unwrap();
        let data = pattern(600, 1);
        let create = |data: Vec<u8>| {
            let w = &w;
            async move {
                w.post(
                    &format!("/api/v1/files/{file}/uploads"),
                    json!({"size_bytes": data.len(), "sha256": sha_hex(&data)}),
                )
                .await
            }
        };
        let id = w.stored_upload(file, &data).await;
        for _ in 0..2 {
            // Aborting twice changes nothing.
            let r = w.delete(&format!("/api/v1/uploads/{id}")).await;
            assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);
        }
        assert_eq!(w.blob_state(600).await, "orphaned");
        assert_eq!(w.counters().await, (600, 0, 600, 0));
        // The second upload does not fit next to the object that is still stored.
        let refused = create(pattern(600, 2)).await;
        assert_eq!(
            refused.status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "{}",
            refused.text
        );
        assert_eq!(refused.code(), "QUOTA_EXCEEDED");
        // A repeated store and abort cycle never exceeds the quota either.
        let small = pattern(400, 3);
        let second = w.stored_upload(file, &small).await;
        let r = w.delete(&format!("/api/v1/uploads/{second}")).await;
        assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);
        assert_eq!(w.counters().await, (1000, 0, 1000, 0));
        assert_eq!(
            create(pattern(1, 4)).await.status,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(files_below(&w.objects_dir()), 2);

        // Reconciliation derives the same numbers from the rows and corrects nothing.
        let job = w.job().await;
        let quiet = run(&job).await;
        assert!(!quiet.has_errors(), "{quiet:?}");
        assert_eq!(quiet.count("fix_usage", "orgs_corrected"), 0);
        assert_eq!(w.counters().await, (1000, 0, 1000, 0));
        // Young orphans are not collected, so the quota stays occupied.
        assert_eq!(quiet.count("collect_blobs", "blobs_deleted"), 0);

        // Damaged counters are repaired to include the orphaned bytes.
        sqlx::query("UPDATE organizations SET reserved_bytes = 0")
            .execute(&w.env.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE workspaces SET reserved_bytes = 0")
            .execute(&w.env.pool)
            .await
            .unwrap();
        let mut tx = OrgTx::begin(&w.env.pool, w.org).await.unwrap().unwrap();
        let drift = tx.maint_fix_usage().await.unwrap().expect("drift");
        tx.commit().await.unwrap();
        assert_eq!(drift.reserved_delta, 1000);
        assert_eq!(w.counters().await, (1000, 0, 1000, 0));

        // After the grace period the GC deletes the objects and the quota is free again.
        sqlx::query("UPDATE blobs SET created_at = created_at - interval '3 days'")
            .execute(&w.env.pool)
            .await
            .unwrap();
        let old = run(&job).await;
        assert!(!old.has_errors(), "{old:?}");
        assert_eq!(old.count("collect_blobs", "blobs_deleted"), 2);
        assert_eq!(files_below(&w.objects_dir()), 0);
        assert_eq!(w.counters().await, (0, 0, 0, 0));
        assert_eq!(create(pattern(1000, 5)).await.status, StatusCode::CREATED);
    }

    /// M1: a database written by the old rule has orphaned blobs whose reservation was already
    /// released. The migration recomputes the counters; without it, claims and releases must
    /// still never violate the non-negative CHECK.
    async fn pre_upgrade_state(w: &W) -> (Uuid, Uuid) {
        let project = w.project("Upgrade").await;
        let file = w.file(project, "up.fig").await;
        let stored = w.stored_upload(file, &pattern(300, 1)).await;
        w.delete(&format!("/api/v1/uploads/{stored}")).await;
        let data = pattern(200, 2);
        let created = w
            .post(
                &format!("/api/v1/files/{file}/uploads"),
                json!({"size_bytes": data.len(), "sha256": sha_hex(&data)}),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        let open = Uuid::parse_str(created.body["upload_id"].as_str().unwrap()).unwrap();
        assert_eq!(w.counters().await, (500, 0, 500, 0));
        // The old rule had released the 300 bytes of the aborted upload.
        sqlx::query("UPDATE organizations SET reserved_bytes = 200 WHERE id = $1")
            .bind(w.org)
            .execute(&w.env.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE workspaces SET reserved_bytes = 200 WHERE id = $1")
            .bind(w.ws)
            .execute(&w.env.pool)
            .await
            .unwrap();
        let blob: Uuid = sqlx::query_scalar("SELECT id FROM blobs WHERE state = 'orphaned'")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        (open, blob)
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn migration_0014_recomputes_reservations_of_orphaned_blobs() {
        let w = W::new().await;
        let (open, blob) = pre_upgrade_state(&w).await;
        let sql =
            include_str!("../../craftcloud-db/migrations/0014_reserved_bytes_include_orphans.sql");
        sqlx::raw_sql(sql).execute(&w.env.pool).await.unwrap();
        assert_eq!(w.counters().await, (500, 0, 500, 0));
        // Running it again changes nothing.
        sqlx::raw_sql(sql).execute(&w.env.pool).await.unwrap();
        assert_eq!(w.counters().await, (500, 0, 500, 0));
        // The claim and the later abort release exactly what was reserved.
        let before = w.env.clock.now() + Duration::from_secs(DAY);
        let mut tx = OrgTx::begin(&w.env.pool, w.org).await.unwrap().unwrap();
        let claim = tx.maint_gc_claim(blob, before).await.unwrap();
        tx.commit().await.unwrap();
        let dbm::GcClaim::Claimed(claimed) = claim else {
            panic!("not claimed");
        };
        assert!(claimed.counted && !claimed.clamped);
        assert_eq!(w.counters().await, (200, 0, 200, 0));
        let r = w.delete(&format!("/api/v1/uploads/{open}")).await;
        assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);
        assert_eq!(w.counters().await, (0, 0, 0, 0));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn releases_never_violate_the_check_when_a_counter_is_too_small() {
        let w = W::new().await;
        let (open, blob) = pre_upgrade_state(&w).await;
        // No migration: the claim of the orphan finds 200 reserved for 300 bytes and clamps.
        let before = w.env.clock.now() + Duration::from_secs(DAY);
        let mut tx = OrgTx::begin(&w.env.pool, w.org).await.unwrap().unwrap();
        let claim = tx.maint_gc_claim(blob, before).await.unwrap();
        tx.commit().await.unwrap();
        let dbm::GcClaim::Claimed(claimed) = claim else {
            panic!("not claimed");
        };
        assert!(claimed.clamped);
        assert_eq!(w.counters().await, (0, 0, 0, 0));
        // The open reservation is now unbacked: aborting it clamps instead of failing with a 500.
        let r = w.delete(&format!("/api/v1/uploads/{open}")).await;
        assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);
        assert_eq!(w.counters().await, (0, 0, 0, 0));
        // The usage reconciliation derives the right numbers from the rows.
        let mut tx = OrgTx::begin(&w.env.pool, w.org).await.unwrap().unwrap();
        assert_eq!(tx.maint_fix_usage().await.unwrap(), None);
        tx.commit().await.unwrap();
    }

    /// SR-01: a crash between the claim and the object removal keeps counters consistent.
    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn an_orphaned_claim_releases_the_reservation_once_and_stays_consistent() {
        let w = W::new().await;
        let project = w.project("Orphan").await;
        let file = w.file(project, "orphan.fig").await;
        let id = w.stored_upload(file, &pattern(300, 1)).await;
        w.delete(&format!("/api/v1/uploads/{id}")).await;
        assert_eq!(w.counters().await, (300, 0, 300, 0));
        let now = w.env.clock.now();
        let before = now + Duration::from_secs(DAY);
        let blob: Uuid = sqlx::query_scalar("SELECT id FROM blobs")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        let mut tx = OrgTx::begin(&w.env.pool, w.org).await.unwrap().unwrap();
        let claim = tx.maint_gc_claim(blob, before).await.unwrap();
        tx.commit().await.unwrap();
        let dbm::GcClaim::Claimed(claimed) = claim else {
            panic!("the orphan was not claimed");
        };
        assert!(claimed.counted && !claimed.clamped);
        assert_eq!(w.counters().await, (0, 0, 0, 0));
        // The claim crashed before the object removal: a second claim changes nothing.
        let mut tx = OrgTx::begin(&w.env.pool, w.org).await.unwrap().unwrap();
        assert_eq!(
            tx.maint_gc_claim(blob, before).await.unwrap(),
            dbm::GcClaim::Skipped
        );
        tx.commit().await.unwrap();
        assert_eq!(w.counters().await, (0, 0, 0, 0));
        // Recomputing from the rows agrees (a `deleting` blob is not counted).
        let mut tx = OrgTx::begin(&w.env.pool, w.org).await.unwrap().unwrap();
        assert_eq!(tx.maint_fix_usage().await.unwrap(), None);
        tx.commit().await.unwrap();
        let job = w.job().await;
        let report = run(&job).await;
        assert_eq!(report.count("collect_blobs", "resumed"), 1);
        assert_eq!(files_below(&w.objects_dir()), 0);
        assert_eq!(w.counters().await, (0, 0, 0, 0));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_crash_between_the_gc_phases_is_completed_by_the_next_run() {
        let mut w = W::new().await;
        let project = w.project("Crash").await;
        let file = w.file(project, "crash.fig").await;
        w.push(file, &pattern(150, 1)).await;
        w.push(file, &pattern(250, 2)).await;
        // Both blobs belong to versions of a file in the trash.
        assert!(
            w.delete(&format!("/api/v1/files/{file}"))
                .await
                .status
                .is_success()
        );
        w.age(31).await;
        let now = w.env.clock.now();
        let cutoff = now - Duration::from_secs(30 * DAY);
        let mut tx = OrgTx::begin(&w.env.pool, w.org).await.unwrap().unwrap();
        let purged = tx.maint_purge_file(file, cutoff, now).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(purged.files, 1);
        assert_eq!(w.counters().await, (0, 400, 0, 400));

        // Phase 1 for one blob, then "crash": the object and the row stay.
        let created_before = now - Duration::from_secs(DAY + 3600);
        let candidates = dbm::gc_candidates(&w.env.pool, created_before, &[], 10)
            .await
            .unwrap();
        assert_eq!(candidates.len(), 2);
        let (org, blob) = candidates[0];
        let mut tx = OrgTx::begin(&w.env.pool, org).await.unwrap().unwrap();
        let claim = tx.maint_gc_claim(blob, created_before).await.unwrap();
        tx.commit().await.unwrap();
        let dbm::GcClaim::Claimed(claimed) = claim else {
            panic!("the blob was not claimed");
        };
        assert!(claimed.counted && !claimed.clamped);
        assert_eq!(
            w.count("SELECT count(*) FROM blobs WHERE state = 'deleting'")
                .await,
            1
        );
        assert_eq!(files_below(&w.objects_dir()), 2);
        // Counted exactly once, immediately.
        let after_claim = 400 - claimed.size_bytes;
        assert_eq!(w.counters().await, (0, after_claim, 0, after_claim));
        // A second claim of the same blob does nothing.
        let mut tx = OrgTx::begin(&w.env.pool, org).await.unwrap().unwrap();
        assert_eq!(
            tx.maint_gc_claim(blob, created_before).await.unwrap(),
            dbm::GcClaim::Skipped
        );
        tx.commit().await.unwrap();
        assert_eq!(w.counters().await, (0, after_claim, 0, after_claim));

        // The next run resumes the half-done blob and collects the other one.
        let job = w.job().await;
        let report = run(&job).await;
        assert!(!report.has_errors(), "{report:?}");
        assert_eq!(report.count("collect_blobs", "resumed"), 1);
        assert_eq!(report.count("collect_blobs", "blobs_deleted"), 2);
        assert_eq!(w.count("SELECT count(*) FROM blobs").await, 0);
        assert_eq!(files_below(&w.objects_dir()), 0);
        assert_eq!(w.counters().await, (0, 0, 0, 0));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn gc_claims_never_take_referenced_or_young_blobs_and_never_go_negative() {
        let mut w = W::new().await;
        let project = w.project("Claims").await;
        let file = w.file(project, "claims.fig").await;
        w.push(file, &pattern(70, 1)).await;
        w.age(40).await;
        let now = w.env.clock.now();
        let old = now - Duration::from_secs(DAY + 3600);
        let blob: Uuid = sqlx::query_scalar("SELECT id FROM blobs")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        // Referenced by a version: not claimable, whatever its age.
        let mut tx = OrgTx::begin(&w.env.pool, w.org).await.unwrap().unwrap();
        assert_eq!(
            tx.maint_gc_claim(blob, old).await.unwrap(),
            dbm::GcClaim::Skipped
        );
        tx.commit().await.unwrap();
        assert_eq!(w.blob_state(70).await, "committed");
        assert_eq!(w.counters().await, (0, 70, 0, 70));

        // The file leaves the trash for good; the blob is now unreferenced.
        assert!(
            w.delete(&format!("/api/v1/files/{file}"))
                .await
                .status
                .is_success()
        );
        let mut tx = OrgTx::begin(&w.env.pool, w.org).await.unwrap().unwrap();
        let purged = tx
            .maint_purge_file(file, now + Duration::from_secs(DAY), now)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(purged.files, 1);

        // Too young for the grace period: not claimable.
        let mut tx = OrgTx::begin(&w.env.pool, w.org).await.unwrap().unwrap();
        let young = now - Duration::from_secs(100 * DAY);
        assert_eq!(
            tx.maint_gc_claim(blob, young).await.unwrap(),
            dbm::GcClaim::Skipped
        );
        tx.commit().await.unwrap();
        assert_eq!(w.blob_state(70).await, "committed");

        // A counter that is already too small is clamped at zero, never negative.
        sqlx::query("UPDATE organizations SET committed_bytes = 0")
            .execute(&w.env.pool)
            .await
            .unwrap();
        let mut tx = OrgTx::begin(&w.env.pool, w.org).await.unwrap().unwrap();
        let claim = tx.maint_gc_claim(blob, old).await.unwrap();
        tx.commit().await.unwrap();
        let dbm::GcClaim::Claimed(claimed) = claim else {
            panic!("not claimed");
        };
        assert!(claimed.counted && claimed.clamped);
        assert_eq!(w.counters().await, (0, 0, 0, 0));
    }

    // ---- restore and commit versus the job -------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn commit_and_restore_never_reference_a_deleting_blob() {
        let w = W::new().await;
        let project = w.project("Deleting").await;
        let file = w.file(project, "d.fig").await;
        let v1 = w.push(file, &pattern(100, 1)).await;
        let v2 = w.push(file, &pattern(200, 2)).await;
        let versions = w.count("SELECT count(*) FROM file_versions").await;
        // An (abnormal) deleting blob behind an existing version: restore refuses it.
        sqlx::query("UPDATE blobs SET state = 'deleting' WHERE size_bytes = 100")
            .execute(&w.env.pool)
            .await
            .unwrap();
        let restore = w.restore(file, v1, v2).await;
        assert_eq!(restore.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(restore.code(), "BLOB_CORRUPT", "{}", restore.text);
        assert_eq!(
            w.count("SELECT count(*) FROM file_versions").await,
            versions
        );

        // A stored upload whose blob is no longer staged cannot be committed.
        let upload = w.stored_upload(file, &pattern(300, 3)).await;
        sqlx::query("UPDATE blobs SET state = 'deleting' WHERE size_bytes = 300")
            .execute(&w.env.pool)
            .await
            .unwrap();
        let commit = w
            .send(
                Req::new("POST", &format!("/api/v1/files/{file}/versions"))
                    .session(&w.owner.1)
                    .header("If-Match", &tag(v2))
                    .json(json!({"upload_id": upload.to_string()})),
            )
            .await;
        assert_eq!(commit.status, StatusCode::CONFLICT, "{}", commit.text);
        assert_eq!(commit.code(), "UPLOAD_CLOSED");
        assert_eq!(
            w.count("SELECT count(*) FROM file_versions").await,
            versions
        );
        assert_eq!(w.head(file).await, Some(v2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn concurrent_restores_and_a_run_never_leave_a_dangling_version() {
        let mut w = W::new().await;
        let project = w.project("Race").await;
        let mut files = Vec::new();
        for n in 0..6_u8 {
            let file = w.file(project, &format!("race-{n}.fig")).await;
            let v1 = w.push(file, &pattern(1000 + usize::from(n), n)).await;
            let v2 = w.push(file, &pattern(2000 + usize::from(n), n + 10)).await;
            files.push((file, v1, v2));
        }
        // v1 of every file is older than the version retention.
        w.age(40).await;
        let job = Arc::new(w.job().await);
        let mut tasks = Vec::new();
        for (file, v1, v2) in files.clone() {
            let app = w.env.app.clone();
            let session = w.owner.1.clone();
            tasks.push(tokio::spawn(async move {
                call(
                    &app,
                    Req::new(
                        "POST",
                        &format!("/api/v1/files/{file}/versions/{v1}/restore"),
                    )
                    .session(&session)
                    .header("If-Match", &tag(v2)),
                )
                .await
                .status
            }));
        }
        let runner = {
            let job = job.clone();
            tokio::spawn(async move {
                let (_keep, stop) = watch::channel(false);
                job.run_once(
                    RunOptions {
                        dry_run: false,
                        trigger: Trigger::Scheduled,
                    },
                    &stop,
                )
                .await
            })
        };
        for task in tasks {
            let status = task.await.unwrap();
            // The restore won (201) or the version was already gone (404).
            assert!(
                status == StatusCode::CREATED || status == StatusCode::NOT_FOUND,
                "{status}"
            );
        }
        runner.await.unwrap().unwrap();
        run(&job).await;
        // Every version points at a committed blob whose object exists.
        assert_eq!(
            w.count(
                "SELECT count(*) FROM file_versions v JOIN blobs b ON b.id = v.blob_id \
                 WHERE b.state <> 'committed'"
            )
            .await,
            0
        );
        let keys: Vec<String> = sqlx::query_scalar(
            "SELECT b.object_key FROM blobs b WHERE EXISTS (SELECT 1 FROM file_versions v WHERE v.blob_id = b.id)",
        )
        .fetch_all(&w.env.pool)
        .await
        .unwrap();
        assert!(!keys.is_empty());
        for key in &keys {
            assert!(
                w.object_path(key).exists(),
                "a referenced object is missing"
            );
        }
        // No blob row without an object, none stuck in deleting, counters exact.
        assert_eq!(
            w.count("SELECT count(*) FROM blobs WHERE state = 'deleting'")
                .await,
            0
        );
        assert_eq!(
            usize::try_from(w.count("SELECT count(*) FROM blobs").await).unwrap(),
            files_below(&w.objects_dir())
        );
        let (_, committed, _, ws_committed) = w.counters().await;
        let sum: i64 = sqlx::query_scalar("SELECT COALESCE(sum(size_bytes), 0)::bigint FROM blobs")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!((committed, ws_committed), (sum, sum));
    }

    // ---- reconciliation -------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn reconciliation_removes_old_strays_and_keeps_young_ones() {
        let mut w = W::new().await;
        let project = w.project("Strays").await;
        let file = w.file(project, "s.fig").await;
        w.push(file, &pattern(90, 1)).await;
        // A published object without a blob row (the crash window after publishing).
        let store = w.store().await;
        let data = pattern(55, 9);
        let sha: [u8; 32] = Sha256::digest(&data).into();
        let mut temp = store.begin_write(1024).await.unwrap();
        temp.write(&data).await.unwrap();
        let stray = store.finalize(temp, 55, &sha).await.unwrap();
        // An abandoned temporary file.
        let part = w
            .env
            .storage_dir
            .join("tmp")
            .join(format!("{}.part", Uuid::new_v4().simple()));
        std::fs::write(&part, b"half an upload").unwrap();
        let job = w.job().await;
        assert_eq!(files_below(&w.objects_dir()), 2);

        // Young: kept.
        let young = run(&job).await;
        assert!(!young.has_errors(), "{young:?}");
        assert_eq!(young.count("reconcile_storage", "quarantined_objects"), 0);
        assert_eq!(young.count("reconcile_storage", "removed_temp"), 0);
        assert!(young.count("reconcile_storage", "scanned_objects") >= 2);
        assert!(part.exists());

        // Old (two days later): a dry run only counts, a real run removes.
        w.age(2).await;
        let dry = run_with(&job, true).await;
        assert_eq!(
            dry.count("reconcile_storage", "would_quarantine_objects"),
            1
        );
        assert_eq!(dry.count("reconcile_storage", "would_remove_temp"), 1);
        assert!(part.exists());
        assert_eq!(files_below(&w.objects_dir()), 2);
        let old = run(&job).await;
        assert_eq!(old.count("reconcile_storage", "quarantined_objects"), 1);
        assert_eq!(old.count("reconcile_storage", "removed_temp"), 1);
        assert!(!part.exists());
        // Moved to the quarantine, not deleted.
        assert!(!w.object_path(stray.as_str()).exists());
        assert!(files_below(&w.env.storage_dir.join("quarantine")) == 1);
        // The object of the committed blob is still there.
        assert_eq!(files_below(&w.objects_dir()), 1);
        let key = w.object_key(90).await;
        assert!(w.object_path(&key).exists());
    }

    // ---- integrity verification ------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn verification_marks_tampered_blobs_corrupt_and_clears_after_repair() {
        let w = W::new().await;
        let project = w.project("Verify").await;
        let a = w.file(project, "a.fig").await;
        let b = w.file(project, "b.fig").await;
        let data_a = pattern(5000, 1);
        w.push(a, &data_a).await;
        w.push(b, &pattern(7000, 2)).await;
        let key_a = w.object_key(5000).await;
        let key_b = w.object_key(7000).await;

        // A tiny byte budget verifies one blob per run (never verified first).
        let small = w.job_with(settings(1)).await;
        let first = run(&small).await;
        assert_eq!(first.count("verify_blobs", "verified"), 1);
        assert_eq!(
            w.count("SELECT count(*) FROM blobs WHERE verified_at IS NOT NULL")
                .await,
            1
        );
        let second = run(&small).await;
        assert_eq!(second.count("verify_blobs", "verified"), 1);
        assert_eq!(
            w.count("SELECT count(*) FROM blobs WHERE verified_at IS NOT NULL")
                .await,
            2
        );

        let job = w.job().await;
        let path_a = w.object_path(&key_a);
        make_writable(&path_a);
        let mut tampered = data_a.clone();
        tampered[2500] ^= 0xff;
        std::fs::write(&path_a, &tampered).unwrap();
        let counters = w.counters().await;
        let found = run(&job).await;
        assert!(!found.has_errors(), "{found:?}");
        assert_eq!(found.count("verify_blobs", "newly_corrupt"), 1);
        assert_eq!(found.count("verify_blobs", "verified"), 1);
        assert_eq!(w.blob_state(5000).await, "corrupt");
        assert_eq!(w.blob_state(7000).await, "committed");
        // Still corrupt next time: reported as such, not as new, and kept.
        let again = run(&job).await;
        assert_eq!(again.count("verify_blobs", "newly_corrupt"), 0);
        assert_eq!(again.count("verify_blobs", "still_corrupt"), 1);
        assert_eq!(w.counters().await, counters);

        // Repair the bytes: the next verification clears the mark.
        std::fs::write(&path_a, &data_a).unwrap();
        let repaired = run(&job).await;
        assert_eq!(repaired.count("verify_blobs", "repaired"), 1);
        assert_eq!(w.blob_state(5000).await, "committed");

        // A missing object changes no state and deletes no metadata.
        let path_b = w.object_path(&key_b);
        make_writable(&path_b);
        std::fs::remove_file(&path_b).unwrap();
        let missing = run(&job).await;
        assert!(!missing.has_errors(), "{missing:?}");
        assert_eq!(missing.count("verify_blobs", "missing_objects"), 1);
        assert_eq!(w.blob_state(7000).await, "committed");
        assert_eq!(w.count("SELECT count(*) FROM file_versions").await, 2);
        assert_eq!(w.counters().await, counters);
    }

    // ---- usage drift ------------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn usage_drift_is_corrected_and_logged_with_ids_and_deltas() {
        let logs = capture_logs();
        let w = W::new().await;
        let project = w.project("Drift").await;
        let file = w.file(project, "drift.fig").await;
        w.push(file, &pattern(640, 1)).await;
        w.stored_upload(file, &pattern(128, 2)).await;
        assert_eq!(w.counters().await, (128, 640, 128, 640));
        sqlx::query("UPDATE organizations SET committed_bytes = committed_bytes + 7, reserved_bytes = reserved_bytes + 3 WHERE slug = 'acme'")
            .execute(&w.env.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE workspaces SET committed_bytes = committed_bytes + 7, reserved_bytes = reserved_bytes + 3 WHERE id = $1")
            .bind(w.ws)
            .execute(&w.env.pool)
            .await
            .unwrap();
        let job = w.job().await;

        let dry = run_with(&job, true).await;
        assert_eq!(dry.count("fix_usage", "orgs_with_drift"), 1);
        assert_eq!(w.counters().await, (131, 647, 131, 647));

        let fixed = run(&job).await;
        assert_eq!(fixed.count("fix_usage", "orgs_corrected"), 1);
        assert_eq!(w.counters().await, (128, 640, 128, 640));
        let text = logs.text();
        let line = text
            .lines()
            .find(|l| l.contains("usage counters differed"))
            .expect("a drift warning");
        assert!(line.contains(&w.org.to_string()), "{line}");
        assert!(line.contains("reserved_delta=-3"), "{line}");
        assert!(line.contains("committed_delta=-7"), "{line}");
        // Right counters are left alone.
        let quiet = run(&job).await;
        assert_eq!(quiet.count("fix_usage", "orgs_corrected"), 0);
    }

    // ---- run lock, dry run, failures, shutdown -------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_advisory_lock_admits_one_run_at_a_time() {
        let w = W::new().await;
        let job = w.job().await;
        let (_keep, stop) = watch::channel(false);
        let options = RunOptions {
            dry_run: false,
            trigger: Trigger::Scheduled,
        };
        let held = dbm::try_lock(&w.env.pool).await.unwrap().unwrap();
        assert!(dbm::try_lock(&w.env.pool).await.unwrap().is_none());
        assert_eq!(
            job.run_once(options, &stop).await.unwrap(),
            RunOutcome::Skipped
        );
        assert_eq!(w.count("SELECT count(*) FROM maintenance_runs").await, 0);
        held.release().await.unwrap();
        assert!(matches!(
            job.run_once(options, &stop).await.unwrap(),
            RunOutcome::Completed(_)
        ));
        assert_eq!(w.count("SELECT count(*) FROM maintenance_runs").await, 1);

        // The lock is freed when the holder is dropped (crash, cancellation).
        drop(dbm::try_lock(&w.env.pool).await.unwrap().unwrap());
        let mut freed = false;
        for _ in 0..100 {
            if dbm::try_lock(&w.env.pool).await.unwrap().is_some() {
                freed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(freed, "the lock was not freed after the holder was dropped");

        // Two runs started together: never both at once, never none.
        let job = Arc::new(job);
        let (a, b) = tokio::join!(
            {
                let job = job.clone();
                tokio::spawn(async move {
                    let (_keep, stop) = watch::channel(false);
                    job.run_once(options, &stop).await.unwrap()
                })
            },
            {
                let job = job.clone();
                tokio::spawn(async move {
                    let (_keep, stop) = watch::channel(false);
                    job.run_once(options, &stop).await.unwrap()
                })
            }
        );
        let done = [a.unwrap(), b.unwrap()]
            .iter()
            .filter(|o| matches!(o, RunOutcome::Completed(_)))
            .count();
        assert!(done >= 1);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_dry_run_changes_nothing() {
        let mut w = W::new().await;
        let project = w.project("Dry").await;
        let file = w.file(project, "dry.fig").await;
        w.push(file, &pattern(100, 1)).await;
        w.push(file, &pattern(200, 2)).await;
        let doomed = w.file(project, "doomed.fig").await;
        w.push(doomed, &pattern(300, 3)).await;
        w.stored_upload(file, &pattern(50, 4)).await;
        assert!(
            w.delete(&format!("/api/v1/files/{doomed}"))
                .await
                .status
                .is_success()
        );
        w.age(60).await;
        let before = snapshot(&w).await;
        let counters = w.counters().await;
        let objects = files_below(&w.objects_dir());
        let states: Vec<(String, String)> =
            sqlx::query_as("SELECT id::text, state FROM blobs ORDER BY id")
                .fetch_all(&w.env.pool)
                .await
                .unwrap();
        let job = w.job().await;
        let dry = run_with(&job, true).await;
        assert!(!dry.has_errors(), "{dry:?}");
        assert!(dry.dry_run);
        assert_eq!(dry.count("purge_trash", "files_due"), 1);
        assert_eq!(dry.count("expire_uploads", "would_expire"), 1);
        assert_eq!(dry.count("trim_versions", "files_with_old_versions"), 1);
        assert_eq!(snapshot(&w).await, before);
        assert_eq!(w.counters().await, counters);
        assert_eq!(files_below(&w.objects_dir()), objects);
        let after: Vec<(String, String)> =
            sqlx::query_as("SELECT id::text, state FROM blobs ORDER BY id")
                .fetch_all(&w.env.pool)
                .await
                .unwrap();
        assert_eq!(after, states);
        // A real run afterwards does change things.
        let real = run(&job).await;
        assert!(real.count("purge_trash", "purged_files") >= 1);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_failing_step_is_reported_and_the_others_still_run() {
        let mut w = W::new().await;
        let project = w.project("Broken").await;
        let file = w.file(project, "x.fig").await;
        w.push(file, &pattern(100, 1)).await;
        assert!(
            w.delete(&format!("/api/v1/files/{file}"))
                .await
                .status
                .is_success()
        );
        w.age(31).await;
        // Break only the version retention scan.
        sqlx::query("ALTER TABLE file_versions RENAME COLUMN created_at TO created_at_x")
            .execute(&w.env.pool)
            .await
            .unwrap();
        let job = w.job().await;
        let report = run(&job).await;
        assert!(report.has_errors());
        assert_eq!(
            report.steps["trim_versions"].error,
            Some("DATABASE_ERROR"),
            "{report:?}"
        );
        for step in [
            "expire_uploads",
            "purge_trash",
            "collect_blobs",
            "fix_usage",
        ] {
            assert_eq!(report.steps[step].error, None, "{step}");
        }
        assert_eq!(report.count("purge_trash", "purged_files"), 1);
        assert_eq!(report.count("collect_blobs", "blobs_deleted"), 1);
        // The operator sees the error code.
        let status = w
            .send(Req::new("GET", "/api/v1/admin/maintenance").cookie_only(&w.op))
            .await;
        assert_eq!(status.status, StatusCode::OK, "{}", status.text);
        assert_eq!(
            status.body["last_run"]["error_codes"],
            json!(["DATABASE_ERROR"])
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_stopped_or_out_of_time_run_does_no_work() {
        let mut w = W::new().await;
        let project = w.project("Stop").await;
        let file = w.file(project, "stop.fig").await;
        w.push(file, &pattern(100, 1)).await;
        assert!(
            w.delete(&format!("/api/v1/files/{file}"))
                .await
                .status
                .is_success()
        );
        w.age(31).await;
        // Shutdown already requested: every step is skipped.
        let job = w.job().await;
        let (tx, stop) = watch::channel(false);
        tx.send(true).unwrap();
        let options = RunOptions {
            dry_run: false,
            trigger: Trigger::Scheduled,
        };
        let RunOutcome::Completed(report) = job.run_once(options, &stop).await.unwrap() else {
            panic!("skipped");
        };
        assert!(
            report
                .steps
                .values()
                .all(|s| s.counts.contains_key("skipped_shutdown"))
        );
        assert_eq!(w.count("SELECT count(*) FROM files").await, 1);
        // A zero time budget: the steps start but handle no item.
        let mut limited = settings(1 << 30);
        limited.max_run = Duration::ZERO;
        let job = w.job_with(limited).await;
        let report = run(&job).await;
        assert!(!report.has_errors(), "{report:?}");
        assert_eq!(report.count("purge_trash", "purged_files"), 0);
        assert_eq!(w.count("SELECT count(*) FROM files").await, 1);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_loop_runs_and_stops_on_shutdown() {
        let w = W::new().await;
        let job = Arc::new(w.job().await);
        let (tx, stop) = watch::channel(false);
        let handle = tokio::spawn(job.run_loop(Duration::from_millis(50), stop));
        let mut recorded = 0;
        for _ in 0..100 {
            recorded = w.count("SELECT count(*) FROM maintenance_runs").await;
            if recorded > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(recorded > 0, "the loop never finished a run");
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("the loop did not stop")
            .unwrap();
        let trigger: String =
            sqlx::query_scalar("SELECT triggered_by FROM maintenance_runs ORDER BY id LIMIT 1")
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
        assert_eq!(trigger, "scheduled");
    }

    // ---- GET /admin/maintenance ---------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_maintenance_status_is_for_the_operator_only() {
        let w = W::new().await;
        let url = "/api/v1/admin/maintenance";
        // Anonymous callers: 401.
        let anonymous = w.send(Req::new("GET", url)).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "{}",
            anonymous.text
        );
        // Organization owners and members are not the operator: 403.
        let owner = w.send(Req::new("GET", url).cookie_only(&w.owner.1)).await;
        assert_eq!(owner.status, StatusCode::FORBIDDEN, "{}", owner.text);
        assert_eq!(owner.code(), "FORBIDDEN");
        let member_id = w.env.user("member").await;
        let added = w
            .post(
                &format!("/api/v1/organizations/{}/members", w.org),
                json!({"user_id": member_id.to_string(), "role": "member"}),
            )
            .await;
        assert_eq!(added.status, StatusCode::CREATED, "{}", added.text);
        let member = w.env.login_ok("member").await;
        let denied = w.send(Req::new("GET", url).cookie_only(&member)).await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.text);

        // The operator: no run yet, then the report of the last one.
        let none = w.send(Req::new("GET", url).cookie_only(&w.op)).await;
        assert_eq!(none.status, StatusCode::OK, "{}", none.text);
        assert_eq!(none.body, json!({"last_run": null}));
        let project = w.project("Visible").await;
        let file = w.file(project, "v.fig").await;
        w.push(file, &pattern(10, 1)).await;
        let job = w.job().await;
        run(&job).await;
        let seen = w.send(Req::new("GET", url).cookie_only(&w.op)).await;
        assert_eq!(seen.status, StatusCode::OK, "{}", seen.text);
        let last = &seen.body["last_run"];
        assert_eq!(last["trigger"], "scheduled");
        assert_eq!(last["error_codes"], json!([]));
        assert!(last["duration_ms"].as_i64().unwrap() >= 0);
        assert!(last["started_at"].as_str().unwrap().contains('T'));
        for step in [
            "expire_uploads",
            "purge_trash",
            "trim_versions",
            "collect_blobs",
            "reconcile_storage",
            "verify_blobs",
            "fix_usage",
        ] {
            assert!(last["steps"][step]["counts"].is_object(), "{step}");
            assert!(last["steps"][step]["error_code"].is_null(), "{step}");
        }
        assert_eq!(last["steps"]["verify_blobs"]["counts"]["verified"], 1);
        // Counts only: no tenant ids or names.
        for secret in [w.org.to_string(), project.to_string(), file.to_string()] {
            assert!(!seen.text.contains(&secret), "{}", seen.text);
        }
        assert!(!seen.text.contains("v.fig") && !seen.text.contains("Visible"));
        // The status route only reads: a POST is not allowed.
        let post = w
            .send(Req::new("POST", url).session(&w.op).json(json!({})))
            .await;
        assert_eq!(post.status, StatusCode::METHOD_NOT_ALLOWED, "{}", post.text);
    }

    // ---- store binding, quarantine, brake ----------------------------------------------------------

    fn quarantine_dir(w: &W) -> PathBuf {
        w.env.storage_dir.join("quarantine")
    }

    fn day_of(w: &W) -> String {
        let t = w.env.clock.now();
        format!("{:04}{:02}{:02}", t.year(), u8::from(t.month()), t.day())
    }

    /// A published object that no blob row knows (the crash window after publishing).
    async fn stray_object(w: &W, seed: u8) -> craftcloud_storage::ObjectKey {
        let store = w.store().await;
        let data = pattern(40, seed);
        let sha: [u8; 32] = Sha256::digest(&data).into();
        let mut temp = store.begin_write(1024).await.unwrap();
        temp.write(&data).await.unwrap();
        store.finalize(temp, 40, &sha).await.unwrap()
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_first_run_binds_database_and_store_and_keeps_the_binding() {
        let w = W::new().await;
        let job = w.job().await;
        assert_eq!(w.count("SELECT count(*) FROM storage_binding").await, 0);
        // A dry run never binds.
        run_with(&job, true).await;
        assert_eq!(w.count("SELECT count(*) FROM storage_binding").await, 0);
        assert!(!w.env.storage_dir.join("craftcloud-store.json").exists());

        let first = run(&job).await;
        assert!(!first.has_errors(), "{first:?}");
        let bound: Uuid = sqlx::query_scalar("SELECT store_id FROM storage_binding")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        let marker = w.store().await.read_marker().await.unwrap();
        assert_eq!(marker, Some(bound));
        let text =
            std::fs::read_to_string(w.env.storage_dir.join("craftcloud-store.json")).unwrap();
        assert!(text.contains(&bound.to_string()), "{text}");
        // Read-only marker.
        assert!(
            std::fs::metadata(w.env.storage_dir.join("craftcloud-store.json"))
                .unwrap()
                .permissions()
                .readonly()
        );
        let second = run(&job).await;
        assert!(!second.has_errors(), "{second:?}");
        let again: Uuid = sqlx::query_scalar("SELECT store_id FROM storage_binding")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(again, bound);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_store_that_does_not_belong_to_the_database_is_never_touched() {
        let mut w = W::new().await;
        let project = w.project("Bound").await;
        let file = w.file(project, "b.fig").await;
        w.push(file, &pattern(90, 1)).await;
        let stray = stray_object(&w, 5).await;
        let job = w.job().await;
        run(&job).await;
        let marker = w.store().await.read_marker().await.unwrap().unwrap();
        w.age(2).await;
        let untouched = |w: &W| files_below(&w.objects_dir()) == 2 && !quarantine_dir(w).exists();
        let assert_mismatch = |report: &RunReport| {
            for step in ["collect_blobs", "reconcile_storage", "verify_blobs"] {
                assert_eq!(
                    report.steps[step].error,
                    Some("STORE_BINDING_MISMATCH"),
                    "{step}: {report:?}"
                );
            }
            assert!(report.has_errors());
        };

        // 1. A fresh database against the bound store directory.
        let fresh = TestEnv::new().await;
        let other = Maintenance::new(
            fresh.pool.clone(),
            Some(w.store().await),
            fresh.clock.clone(),
            settings(1 << 30),
        );
        fresh.clock.advance(Duration::from_secs(2 * DAY));
        let report = run(&other).await;
        assert_mismatch(&report);
        assert!(untouched(&w));
        assert_eq!(
            fresh.count("SELECT count(*) FROM storage_binding").await,
            0,
            "no automatic repair"
        );

        // 2. The database was unbound (or restored without the row).
        sqlx::query("DELETE FROM storage_binding")
            .execute(&w.env.pool)
            .await
            .unwrap();
        assert_mismatch(&run(&job).await);
        assert!(untouched(&w));
        assert_eq!(w.count("SELECT count(*) FROM storage_binding").await, 0);

        // 3. The database names another store.
        sqlx::query(
            "INSERT INTO storage_binding (id, store_id, bound_at) VALUES (true, $1, now())",
        )
        .bind(Uuid::new_v4())
        .execute(&w.env.pool)
        .await
        .unwrap();
        assert_mismatch(&run(&job).await);
        assert!(untouched(&w));

        // 4. The store directory lost its marker (a fresh or replaced directory).
        sqlx::query("UPDATE storage_binding SET store_id = $1")
            .bind(marker)
            .execute(&w.env.pool)
            .await
            .unwrap();
        let marker_path = w.env.storage_dir.join("craftcloud-store.json");
        make_writable(&marker_path);
        std::fs::remove_file(&marker_path).unwrap();
        assert_mismatch(&run(&job).await);
        assert!(untouched(&w));
        assert!(w.object_path(stray.as_str()).exists());
        // Everything else the job does still works (the counters step has no store).
        let report = run(&job).await;
        assert_eq!(report.steps["fix_usage"].error, None);

        // 5. A deliberate rebind makes the pair usable again; then the stray goes
        // to the quarantine like any stray.
        let rebound = craftcloud_api::maintenance::rebind_store(
            &w.env.pool,
            &w.store().await,
            w.env.clock.now(),
        )
        .await
        .unwrap();
        assert_eq!(rebound.old_database, Some(marker));
        assert_eq!(rebound.old_marker, None);
        let now_bound: Uuid = sqlx::query_scalar("SELECT store_id FROM storage_binding")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(now_bound, rebound.new);
        assert_eq!(
            w.store().await.read_marker().await.unwrap(),
            Some(rebound.new)
        );
        let ok = run(&job).await;
        assert!(!ok.has_errors(), "{ok:?}");
        assert_eq!(ok.count("reconcile_storage", "quarantined_objects"), 1);
        // Rebinding to an existing marker adopts the marker of the store.
        sqlx::query("UPDATE storage_binding SET store_id = $1")
            .bind(Uuid::new_v4())
            .execute(&w.env.pool)
            .await
            .unwrap();
        let adopted = craftcloud_api::maintenance::rebind_store(
            &w.env.pool,
            &w.store().await,
            w.env.clock.now(),
        )
        .await
        .unwrap();
        assert_eq!(adopted.new, rebound.new);
        assert_eq!(adopted.old_marker, Some(rebound.new));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn strays_are_quarantined_and_purged_after_the_quarantine_days() {
        let mut w = W::new().await;
        let project = w.project("Quarantine").await;
        let file = w.file(project, "q.fig").await;
        w.push(file, &pattern(90, 1)).await;
        let stray = stray_object(&w, 7).await;
        let job = w.job().await;
        run(&job).await;
        w.age(2).await;
        let day = day_of(&w);

        let moved = run(&job).await;
        assert!(!moved.has_errors(), "{moved:?}");
        assert_eq!(moved.count("reconcile_storage", "quarantined_objects"), 1);
        // Out of service, not deleted: the bytes are in the quarantine.
        assert!(!w.object_path(stray.as_str()).exists());
        let kept = quarantine_dir(&w).join(&day).join(stray.as_str());
        assert!(kept.exists());
        assert_eq!(std::fs::read(&kept).unwrap(), pattern(40, 7));
        assert_eq!(files_below(&w.objects_dir()), 1);

        // Still inside the quarantine period (30 days): kept.
        w.age(29).await;
        let early = run(&job).await;
        assert_eq!(early.count("reconcile_storage", "purged_quarantine"), 0);
        assert!(kept.exists());

        // A dry run counts, a real run deletes after the period.
        w.age(3).await;
        let dry = run_with(&job, true).await;
        assert_eq!(dry.count("reconcile_storage", "would_purge_quarantine"), 1);
        assert!(kept.exists());
        let late = run(&job).await;
        assert_eq!(late.count("reconcile_storage", "purged_quarantine"), 1);
        assert!(!kept.exists());
        assert!(!quarantine_dir(&w).join(&day).exists());
        // The committed blob's object was never touched.
        assert_eq!(files_below(&w.objects_dir()), 1);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_brake_stops_a_reconciliation_that_would_move_most_objects() {
        let mut w = W::new().await;
        let project = w.project("Brake").await;
        let file = w.file(project, "k.fig").await;
        w.push(file, &pattern(90, 1)).await;
        let job = w.job().await;
        run(&job).await;
        let mut strays = Vec::new();
        for n in 0..120_u8 {
            strays.push(stray_object(&w, n).await);
        }
        w.age(2).await;
        assert_eq!(files_below(&w.objects_dir()), 121);

        for dry in [true, false] {
            let report = run_with(&job, dry).await;
            assert_eq!(
                report.steps["reconcile_storage"].error,
                Some("RECONCILE_BRAKE"),
                "{report:?}"
            );
            assert_eq!(report.count("reconcile_storage", "unknown_objects"), 120);
            assert_eq!(report.count("reconcile_storage", "quarantined_objects"), 0);
            assert!(report.has_errors());
            assert_eq!(files_below(&w.objects_dir()), 121);
            assert!(!quarantine_dir(&w).exists());
        }

        // Fewer than 10 unknown objects: neither brake rule applies.
        let store = w.store().await;
        for key in strays.iter().skip(9) {
            store.delete(key).await.unwrap();
        }
        let report = run(&job).await;
        assert!(!report.has_errors(), "{report:?}");
        assert_eq!(report.count("reconcile_storage", "quarantined_objects"), 9);
        assert_eq!(files_below(&w.objects_dir()), 1);
    }

    // ---- failing items, clocks -----------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_purge_survives_an_upload_committed_into_another_file() {
        let mut w = W::new().await;
        let project = w.project("Copies").await;
        let a = w.file(project, "a.fig").await;
        let b = w.file(project, "b.fig").await;
        // The upload was created for a but became the first version of b. The
        // API no longer commits an upload into a file it was not created for
        // (AC-03), so the state is built through b and the session is re-pointed
        // at a, as data from before that rule can look.
        let upload = w.stored_upload(b, &pattern(77, 3)).await;
        let commit = w
            .send(
                Req::new("POST", &format!("/api/v1/files/{b}/versions"))
                    .session(&w.owner.1)
                    .header("If-None-Match", "*")
                    .json(json!({"upload_id": upload.to_string()})),
            )
            .await;
        assert_eq!(commit.status, StatusCode::CREATED, "{}", commit.text);
        sqlx::query("UPDATE upload_sessions SET file_id = $2 WHERE id = $1")
            .bind(upload)
            .bind(a)
            .execute(&w.env.pool)
            .await
            .unwrap();
        assert!(
            w.delete(&format!("/api/v1/files/{a}"))
                .await
                .status
                .is_success()
        );
        w.age(31).await;
        let job = w.job().await;
        let report = run(&job).await;
        assert!(!report.has_errors(), "{report:?}");
        assert_eq!(report.count("purge_trash", "purged_files"), 1);
        assert_eq!(w.count("SELECT count(*) FROM files").await, 1);
        // The version of b and its session survive; the session now belongs to b.
        assert_eq!(w.count("SELECT count(*) FROM file_versions").await, 1);
        let owner_file: Uuid = sqlx::query_scalar("SELECT file_id FROM upload_sessions")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(owner_file, b);
        assert_eq!(w.blob_state(77).await, "committed");
        assert_eq!(w.counters().await, (0, 77, 0, 77));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_failing_item_does_not_block_the_other_items() {
        let mut w = W::new().await;
        let project = w.project("Poison").await;
        let x = w.file(project, "x.fig").await;
        let y = w.file(project, "y.fig").await;
        w.push(x, &pattern(31, 1)).await;
        w.push(y, &pattern(32, 2)).await;
        assert!(
            w.delete(&format!("/api/v1/files/{x}"))
                .await
                .status
                .is_success()
        );
        w.age(1).await;
        assert!(
            w.delete(&format!("/api/v1/files/{y}"))
                .await
                .status
                .is_success()
        );
        w.age(31).await;
        // x (the older item, handled first) can never be deleted.
        sqlx::query(
            "CREATE FUNCTION poison() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'poisoned'; END $$",
        )
        .execute(&w.env.pool)
        .await
        .unwrap();
        sqlx::query(&format!(
            "CREATE TRIGGER poison BEFORE DELETE ON files FOR EACH ROW WHEN (OLD.id = '{x}') EXECUTE FUNCTION poison()"
        ))
        .execute(&w.env.pool)
        .await
        .unwrap();
        let job = w.job().await;
        for round in 0..2 {
            let report = run(&job).await;
            if round == 0 {
                assert!(report.has_errors());
                assert_eq!(report.steps["purge_trash"].error, Some("DATABASE_ERROR"));
                assert_eq!(report.count("purge_trash", "item_errors"), 1);
            } else {
                // The failed item is left alone for a day instead of failing every run.
                assert_eq!(report.count("purge_trash", "item_errors"), 0);
                assert_eq!(report.count("purge_trash", "excluded_items"), 1);
                assert_eq!(report.steps["purge_trash"].error, None);
            }
            // y was purged although x failed first; the later steps ran.
            assert_eq!(w.count("SELECT count(*) FROM files").await, 1);
            for step in ["expire_uploads", "trim_versions", "fix_usage"] {
                assert_eq!(report.steps[step].error, None, "{step}");
            }
        }
        assert_eq!(w.blob_state(31).await, "committed");
        assert_eq!(w.count("SELECT count(*) FROM blobs").await, 1);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_clock_that_differs_from_the_database_skips_the_destructive_steps() {
        let mut w = W::new().await;
        let project = w.project("Clock").await;
        let file = w.file(project, "c.fig").await;
        w.push(file, &pattern(60, 1)).await;
        assert!(
            w.delete(&format!("/api/v1/files/{file}"))
                .await
                .status
                .is_success()
        );
        // Within the tolerance: the clock check passes.
        let mut checked = settings(1 << 30);
        checked.max_clock_drift = Some(Duration::from_secs(300));
        let job = w.job_with(checked.clone()).await;
        let ok = run(&job).await;
        for (name, step) in &ok.steps {
            assert_ne!(step.error, Some("CLOCK_DRIFT"), "{name}");
        }
        // The application clock is a month ahead of the database.
        w.age(31).await;
        for dry in [true, false] {
            let report = run_with(&job, dry).await;
            for step in [
                "expire_uploads",
                "purge_trash",
                "trim_versions",
                "collect_blobs",
                "reconcile_storage",
            ] {
                assert_eq!(report.steps[step].error, Some("CLOCK_DRIFT"), "{step}");
                assert_eq!(
                    report.steps[step].counts.get("skipped_clock_drift"),
                    Some(&1)
                );
            }
            // Read-only checks still run.
            assert_eq!(report.steps["verify_blobs"].error, None);
            assert_eq!(report.steps["fix_usage"].error, None);
            assert!(report.has_errors());
            assert_eq!(w.count("SELECT count(*) FROM files").await, 1);
            assert_eq!(w.count("SELECT count(*) FROM blobs").await, 1);
        }
        // Without the check (as in the other tests) the purge happens.
        let unchecked = w.job().await;
        let purged = run(&unchecked).await;
        assert_eq!(purged.count("purge_trash", "purged_files"), 1);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn device_session_cleanup_uses_database_clock_and_deletes_history_in_batches() {
        let mut w = W::new().await;
        let now: time::OffsetDateTime = sqlx::query_scalar("SELECT now()")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        let fresh_id = Uuid::new_v4();
        let expired_id = Uuid::new_v4();
        let family_fresh = Uuid::new_v4();
        let family_expired = Uuid::new_v4();
        let expired_id_2 = Uuid::new_v4();
        let pool = w.env.pool.clone();
        let user_id = w.owner.0;
        let insert = |id: Uuid,
                      family: Uuid,
                      refresh: [u8; 32],
                      created: time::OffsetDateTime,
                      idle: time::OffsetDateTime,
                      absolute: time::OffsetDateTime,
                      access_expiry: Option<time::OffsetDateTime>,
                      access_hash: Option<Vec<u8>>,
                      revoked: Option<time::OffsetDateTime>,
                      reason: Option<&str>| {
            let pool = pool.clone();
            let reason = reason.map(str::to_owned);
            async move {
                sqlx::query("INSERT INTO device_sessions (id,user_id,name,client_kind,client_id,refresh_hash,refresh_family,access_hash,access_expires_at,created_at,last_used_at,idle_expires_at,absolute_expires_at,revoked_at,revoke_reason) VALUES ($1,$2,'Test device','cli','maintenance-test',$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)")
                .bind(id).bind(user_id).bind(refresh.as_slice()).bind(family).bind(access_hash).bind(access_expiry)
                .bind(created).bind(created).bind(idle).bind(absolute).bind(revoked).bind(reason)
                .execute(&pool).await.unwrap();
            }
        };
        insert(
            fresh_id,
            family_fresh,
            [1; 32],
            now - time::Duration::days(1),
            now + time::Duration::hours(1),
            now + time::Duration::days(89),
            Some(now + time::Duration::minutes(15)),
            Some(vec![2; 32]),
            None,
            None,
        )
        .await;
        insert(
            expired_id,
            family_expired,
            [3; 32],
            now - time::Duration::days(60),
            now - time::Duration::days(31),
            now - time::Duration::days(31),
            None,
            None,
            Some(now - time::Duration::days(31)),
            Some("security"),
        )
        .await;
        insert(
            expired_id_2,
            Uuid::new_v4(),
            [5; 32],
            now - time::Duration::days(60),
            now - time::Duration::days(31),
            now - time::Duration::days(31),
            None,
            None,
            Some(now - time::Duration::days(31)),
            Some("security"),
        )
        .await;
        sqlx::query("INSERT INTO device_refresh_history(device_session_id,refresh_hash,rotated_at) VALUES($1,$2,$3)")
            .bind(expired_id).bind([4_u8;32].as_slice()).bind(now - time::Duration::days(40))
            .execute(&w.env.pool).await.unwrap();

        sqlx::query("INSERT INTO outbound_emails(id,recipient,template,language,expires_at,status,created_at,available_at) VALUES($1,'old@example.test','test','en',now(),'failed',now()-interval '8 days',now()-interval '8 days')")
            .bind(Uuid::new_v4()).execute(&w.env.pool).await.unwrap();

        w.age(180).await;
        let mut checked = settings(1024);
        checked.max_clock_drift = Some(Duration::from_secs(300));
        checked.batch_size = 1;
        let job = w.job_with(checked).await;
        let report = run(&job).await;
        assert_eq!(report.count("purge_audit", "purged_device_sessions"), 2);
        assert_eq!(report.count("purge_audit", "purged_emails"), 1);
        assert_eq!(
            w.count(&format!(
                "SELECT count(*) FROM device_sessions WHERE id='{fresh_id}' AND revoked_at IS NULL"
            ))
            .await,
            1
        );
        assert_eq!(
            w.count(&format!(
                "SELECT count(*) FROM device_sessions WHERE id='{expired_id}'"
            ))
            .await,
            0
        );
        assert_eq!(
            w.count(&format!(
                "SELECT count(*) FROM device_sessions WHERE id='{expired_id_2}'"
            ))
            .await,
            0
        );
        assert_eq!(
            w.count(&format!(
                "SELECT count(*) FROM device_refresh_history WHERE device_session_id='{expired_id}'"
            ))
            .await,
            0
        );
    }

    // ---- epoch, small-store brake, poisoned head ------------------------------------------------------

    async fn marker_epoch(w: &W) -> u64 {
        w.store()
            .await
            .read_marker_info()
            .await
            .unwrap()
            .unwrap()
            .epoch
    }

    async fn db_epoch(w: &W) -> i64 {
        sqlx::query_scalar("SELECT epoch FROM storage_binding")
            .fetch_one(&w.env.pool)
            .await
            .unwrap()
    }

    /// A trashed file (old enough to purge after `age`) and a stray object.
    async fn bait(w: &mut W) -> Uuid {
        let project = w.project("Bait").await;
        let file = w.file(project, "bait.fig").await;
        w.push(file, &pattern(50, 1)).await;
        assert!(
            w.delete(&format!("/api/v1/files/{file}"))
                .await
                .status
                .is_success()
        );
        stray_object(w, 9).await;
        file
    }

    const DESTRUCTIVE: [&str; 5] = [
        "expire_uploads",
        "purge_trash",
        "trim_versions",
        "collect_blobs",
        "reconcile_storage",
    ];

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn every_destructive_run_raises_the_epoch_in_the_database_and_the_marker() {
        let w = W::new().await;
        let job = w.job().await;
        assert!(!run(&job).await.has_errors());
        let first = (db_epoch(&w).await, marker_epoch(&w).await);
        assert_eq!(first, (1, 1));
        assert!(!run(&job).await.has_errors());
        assert_eq!((db_epoch(&w).await, marker_epoch(&w).await), (2, 2));
        // A dry run changes neither.
        run_with(&job, true).await;
        assert_eq!((db_epoch(&w).await, marker_epoch(&w).await), (2, 2));
        // The marker stays read-only after the replacing write.
        assert!(
            std::fs::metadata(w.env.storage_dir.join("craftcloud-store.json"))
                .unwrap()
                .permissions()
                .readonly()
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_restored_older_database_blocks_every_destructive_step_until_accepted() {
        let mut w = W::new().await;
        let job = w.job().await;
        run(&job).await;
        run(&job).await;
        let file = bait(&mut w).await;
        w.age(31).await;
        // The database goes back to an older state: its epoch is behind the store's.
        sqlx::query("UPDATE storage_binding SET epoch = 0")
            .execute(&w.env.pool)
            .await
            .unwrap();
        let objects = files_below(&w.objects_dir());
        for dry in [true, false] {
            let report = run_with(&job, dry).await;
            for step in DESTRUCTIVE {
                assert_eq!(
                    report.steps[step].error,
                    Some("DATABASE_OLDER_THAN_STORE"),
                    "{step}: {report:?}"
                );
                assert_eq!(
                    report.steps[step].counts.get("skipped_epoch_conflict"),
                    Some(&1)
                );
            }
            // Reads and counter checks still run; nothing was deleted or moved.
            assert_eq!(report.steps["verify_blobs"].error, None);
            assert_eq!(report.steps["fix_usage"].error, None);
            assert_eq!(
                w.count(&format!("SELECT count(*) FROM files WHERE id = '{file}'"))
                    .await,
                1
            );
            assert_eq!(files_below(&w.objects_dir()), objects);
            assert!(!quarantine_dir(&w).exists());
            // The epochs are not touched either.
            assert_eq!((db_epoch(&w).await, marker_epoch(&w).await), (0, 2));
        }

        // The operator accepts the database: the epoch of the marker is adopted.
        let rebound = craftcloud_api::maintenance::rebind_store(
            &w.env.pool,
            &w.store().await,
            w.env.clock.now(),
        )
        .await
        .unwrap();
        assert_eq!(rebound.epoch, 2);
        assert_eq!(db_epoch(&w).await, 2);
        let ok = run(&job).await;
        assert!(!ok.has_errors(), "{ok:?}");
        assert_eq!(ok.count("purge_trash", "purged_files"), 1);
        assert_eq!((db_epoch(&w).await, marker_epoch(&w).await), (3, 3));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_marker_that_is_older_than_the_database_blocks_destructive_steps() {
        let mut w = W::new().await;
        let job = w.job().await;
        run(&job).await;
        bait(&mut w).await;
        w.age(31).await;
        // Two ahead is not a crash window (that is one): the marker was restored older.
        sqlx::query("UPDATE storage_binding SET epoch = epoch + 2")
            .execute(&w.env.pool)
            .await
            .unwrap();
        let report = run(&job).await;
        for step in DESTRUCTIVE {
            assert_eq!(
                report.steps[step].error,
                Some("MARKER_OLDER_THAN_DATABASE"),
                "{step}"
            );
        }
        assert_eq!(w.count("SELECT count(*) FROM files").await, 1);
        assert!(!quarantine_dir(&w).exists());
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_crash_between_the_database_and_the_marker_write_is_repaired() {
        let mut w = W::new().await;
        let job = w.job().await;
        run(&job).await;
        let file = bait(&mut w).await;
        w.age(31).await;
        // The database was raised, the process died before the marker followed.
        sqlx::query("UPDATE storage_binding SET epoch = epoch + 1")
            .execute(&w.env.pool)
            .await
            .unwrap();
        assert_eq!((db_epoch(&w).await, marker_epoch(&w).await), (2, 1));
        // A dry run does not repair.
        let dry = run_with(&job, true).await;
        assert!(!dry.has_errors(), "{dry:?}");
        assert_eq!((db_epoch(&w).await, marker_epoch(&w).await), (2, 1));
        // The next real run finishes the write and then does its work.
        let report = run(&job).await;
        assert!(!report.has_errors(), "{report:?}");
        assert_eq!(report.count("purge_trash", "purged_files"), 1);
        assert_eq!(
            w.count(&format!("SELECT count(*) FROM files WHERE id = '{file}'"))
                .await,
            0
        );
        assert_eq!((db_epoch(&w).await, marker_epoch(&w).await), (3, 3));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_small_store_with_a_wrong_database_hits_the_brake() {
        let logs = capture_logs();
        let mut w = W::new().await;
        let project = w.project("Small").await;
        let file = w.file(project, "s.fig").await;
        w.push(file, &pattern(90, 1)).await;
        let job = w.job().await;
        run(&job).await;
        let mut strays = Vec::new();
        for n in 0..12_u8 {
            strays.push(stray_object(&w, n).await);
        }
        w.age(2).await;
        // 12 of 13 old objects are unknown: far over half, and at least 10.
        let report = run(&job).await;
        assert_eq!(
            report.steps["reconcile_storage"].error,
            Some("RECONCILE_BRAKE"),
            "{report:?}"
        );
        assert_eq!(report.count("reconcile_storage", "unknown_objects"), 12);
        assert_eq!(files_below(&w.objects_dir()), 13);
        assert!(!quarantine_dir(&w).exists());
        // Fewer than 10 unknown objects: a few strays are moved, with a warning.
        let store = w.store().await;
        for key in strays.iter().skip(5) {
            store.delete(key).await.unwrap();
        }
        let moved = run(&job).await;
        assert!(!moved.has_errors(), "{moved:?}");
        assert_eq!(moved.count("reconcile_storage", "quarantined_objects"), 5);
        let text = logs.text();
        assert!(
            text.lines().any(
                |l| l.contains("moving objects that the database does not know")
                    && l.contains("count=5")
            ),
            "{text}"
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn poisoned_items_at_the_head_do_not_starve_the_batch() {
        let mut w = W::new().await;
        let project = w.project("Starve").await;
        let mut ids = Vec::new();
        // Three poisoned files are the oldest in the trash, two good ones follow.
        for n in 0..5 {
            let file = w.file(project, &format!("f{n}.fig")).await;
            assert!(
                w.delete(&format!("/api/v1/files/{file}"))
                    .await
                    .status
                    .is_success()
            );
            w.env.clock.advance(Duration::from_secs(1));
            ids.push(file);
        }
        let poisoned: Vec<String> = ids.iter().take(3).map(|id| format!("'{id}'")).collect();
        sqlx::query(
            "CREATE FUNCTION poison() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'poisoned'; END $$",
        )
        .execute(&w.env.pool)
        .await
        .unwrap();
        sqlx::query(&format!(
            "CREATE TRIGGER poison BEFORE DELETE ON files FOR EACH ROW WHEN (OLD.id IN ({})) EXECUTE FUNCTION poison()",
            poisoned.join(", ")
        ))
        .execute(&w.env.pool)
        .await
        .unwrap();
        w.age(31).await;
        let mut small = settings(1 << 30);
        small.batch_size = 2;
        let job = w.job_with(small).await;

        // Batch of two: the two oldest fail and are set aside.
        let first = run(&job).await;
        assert_eq!(first.count("purge_trash", "item_errors"), 2);
        assert_eq!(w.count("SELECT count(*) FROM files").await, 5);
        // The next batch skips them: the third poisoned file fails, a good one goes.
        let second = run(&job).await;
        assert_eq!(second.count("purge_trash", "item_errors"), 1);
        assert_eq!(second.count("purge_trash", "excluded_items"), 2);
        assert_eq!(w.count("SELECT count(*) FROM files").await, 4);
        // Now only good files are reachable: the last one goes, nothing fails.
        let third = run(&job).await;
        assert_eq!(third.count("purge_trash", "item_errors"), 0);
        assert_eq!(third.count("purge_trash", "excluded_items"), 3);
        assert_eq!(w.count("SELECT count(*) FROM files").await, 3);
        // A day later the poisoned files are tried again.
        w.age(2).await;
        let later = run(&job).await;
        assert_eq!(later.count("purge_trash", "excluded_items"), 0);
        assert!(later.count("purge_trash", "item_errors") >= 1);
        assert_eq!(w.count("SELECT count(*) FROM files").await, 3);
    }
}
