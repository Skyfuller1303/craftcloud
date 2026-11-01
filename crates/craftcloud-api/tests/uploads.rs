//! HTTP-level tests of upload sessions, streamed content, quota reservation
//! and usage (CC-F07, CC-F10): role matrix, idempotency, quota boundaries and
//! races, caps, size and hash verification, disk full, concurrency, revoked
//! editors, deleted files, expiry, abort, tenant isolation and CSRF. All tests
//! need PostgreSQL (`cargo xtask db-test`).

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::StatusCode;
    use bytes::Bytes;
    use craftcloud_api::{ApiConfig, Clock, Services, router_with_shared};
    use craftcloud_core::Capabilities;
    use craftcloud_storage::FaultKind;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use tokio::sync::mpsc;
    use uuid::Uuid;

    use crate::common::{Req, Resp, Session, TestEnv, call, capture_logs, settings};

    // ---- bodies -----------------------------------------------------------------

    /// A body fed from a channel, so that a test controls when bytes arrive.
    struct ChannelBody {
        rx: mpsc::Receiver<Bytes>,
    }

    impl http_body::Body for ChannelBody {
        type Data = Bytes;
        type Error = std::convert::Infallible;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
            self.rx
                .poll_recv(cx)
                .map(|item| item.map(|data| Ok(http_body::Frame::data(data))))
        }
    }

    /// `frames` copies of one block, produced lazily (the test never holds
    /// the whole body in memory).
    struct PatternBody {
        block: Bytes,
        frames: u64,
    }

    impl http_body::Body for PatternBody {
        type Data = Bytes;
        type Error = std::convert::Infallible;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
            if self.frames == 0 {
                return Poll::Ready(None);
            }
            self.frames -= 1;
            Poll::Ready(Some(Ok(http_body::Frame::data(self.block.clone()))))
        }
    }

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

    // ---- world ---------------------------------------------------------------------

    struct W {
        env: TestEnv,
        op: Session,
    }

    impl W {
        async fn new() -> Self {
            Self::from(TestEnv::new().await).await
        }

        async fn from(env: TestEnv) -> Self {
            let op = env.login_ok("op").await;
            Self { env, op }
        }

        async fn user(&self, name: &str) -> (Uuid, Session) {
            let id = self.env.user(name).await;
            (id, self.env.login_ok(name).await)
        }

        async fn relogin(&self, name: &str) -> Session {
            self.env.login_ok(name).await
        }

        async fn send(&self, req: Req) -> Resp {
            call(&self.env.app, req).await
        }

        async fn json(&self, method: &'static str, path: &str, who: &Session, body: Value) -> Resp {
            self.send(Req::new(method, path).session(who).json(body))
                .await
        }

        async fn get(&self, who: &Session, path: &str) -> Resp {
            self.send(Req::new("GET", path).cookie_only(who)).await
        }

        async fn delete(&self, who: &Session, path: &str) -> Resp {
            self.send(Req::new("DELETE", path).session(who)).await
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

        async fn org_member(&self, actor: &Session, org: Uuid, user: Uuid, role: &str) {
            let r = self
                .json(
                    "POST",
                    &format!("/api/v1/organizations/{org}/members"),
                    actor,
                    json!({"user_id": user.to_string(), "role": role}),
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

        async fn file_in(&self, actor: &Session, ws: Uuid, name: &str) -> Uuid {
            let p = self
                .json(
                    "POST",
                    &format!("/api/v1/workspaces/{ws}/projects"),
                    actor,
                    json!({"name": format!("P {name}")}),
                )
                .await;
            assert_eq!(p.status, StatusCode::CREATED, "{}", p.text);
            let f = self
                .json(
                    "POST",
                    &format!("/api/v1/projects/{}/files", id_of(&p)),
                    actor,
                    json!({"name": name, "app": "figcraft", "format": "fig"}),
                )
                .await;
            assert_eq!(f.status, StatusCode::CREATED, "{}", f.text);
            id_of(&f)
        }

        async fn create(&self, who: &Session, file: Uuid, data: &[u8]) -> Resp {
            self.create_raw(who, file, data.len() as u64, &sha_hex(data), None)
                .await
        }

        async fn create_raw(
            &self,
            who: &Session,
            file: Uuid,
            size: u64,
            sha: &str,
            key: Option<&str>,
        ) -> Resp {
            let mut req = Req::new("POST", &format!("/api/v1/files/{file}/uploads"))
                .session(who)
                .json(json!({"size_bytes": size, "sha256": sha}));
            if let Some(key) = key {
                req = req.header("Idempotency-Key", key);
            }
            self.send(req).await
        }

        async fn upload_id(&self, who: &Session, file: Uuid, data: &[u8]) -> Uuid {
            let r = self.create(who, file, data).await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            id_of_key(&r, "upload_id")
        }

        async fn put(&self, who: &Session, upload: Uuid, data: Vec<u8>) -> Resp {
            self.send(
                Req::new("PUT", &format!("/api/v1/uploads/{upload}/content"))
                    .session(who)
                    .body(Body::from(data)),
            )
            .await
        }

        async fn state(&self, upload: Uuid) -> String {
            sqlx::query_scalar("SELECT state FROM upload_sessions WHERE id = $1")
                .bind(upload)
                .fetch_one(&self.env.pool)
                .await
                .unwrap()
        }

        async fn wait_for(&self, upload: Uuid, state: &str) {
            for _ in 0..500 {
                if self.state(upload).await == state {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("upload never reached {state}");
        }

        async fn count(&self, sql: &str) -> i64 {
            self.env.count(sql).await
        }

        /// Reserved bytes of the (only) organization counters, as stored.
        async fn reserved(&self) -> (i64, i64) {
            let org: i64 = self
                .count("SELECT COALESCE(sum(reserved_bytes), 0)::bigint FROM organizations")
                .await;
            let ws: i64 = self
                .count("SELECT COALESCE(sum(reserved_bytes), 0)::bigint FROM workspaces")
                .await;
            (org, ws)
        }

        fn stored_objects(&self) -> usize {
            count_files(&self.env.storage_dir.join("objects"))
        }

        fn temp_files(&self) -> usize {
            count_files(&self.env.storage_dir.join("tmp"))
        }

        async fn settled_no_temp(&self) {
            for _ in 0..300 {
                if self.temp_files() == 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("temporary files remain");
        }

        async fn ws_quota(&self, ws: Uuid, quota: Option<u64>, who: &Session) -> Resp {
            self.json(
                "PUT",
                &format!("/api/v1/workspaces/{ws}/quota"),
                who,
                json!({"quota_bytes": quota}),
            )
            .await
        }

        async fn set_org_quota(&self, org: Uuid, quota: Option<u64>) {
            let r = self
                .json(
                    "PUT",
                    &format!("/api/v1/admin/organizations/{org}/quota"),
                    &self.op,
                    json!({"quota_bytes": quota}),
                )
                .await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        }
    }

    fn count_files(dir: &Path) -> usize {
        let mut total = 0;
        if let Ok(read) = std::fs::read_dir(dir) {
            for entry in read.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    total += count_files(&path);
                } else {
                    total += 1;
                }
            }
        }
        total
    }

    fn id_of(r: &Resp) -> Uuid {
        id_of_key(r, "id")
    }

    fn id_of_key(r: &Resp, key: &str) -> Uuid {
        Uuid::parse_str(r.body[key].as_str().unwrap()).unwrap()
    }

    fn assert_status(resp: &Resp, status: StatusCode, code: &str) {
        assert_eq!((resp.status, resp.code()), (status, code), "{}", resp.text);
    }

    fn hidden(resp: &Resp) {
        assert_status(resp, StatusCode::NOT_FOUND, "NOT_FOUND");
    }

    fn forbidden(resp: &Resp) {
        assert_status(resp, StatusCode::FORBIDDEN, "FORBIDDEN");
    }

    struct Cast {
        org: Uuid,
        ws: Uuid,
        file: Uuid,
        owner: (Uuid, Session),
        admin: (Uuid, Session),
        wsadmin: (Uuid, Session),
        editor: (Uuid, Session),
        editor2: (Uuid, Session),
        viewer: (Uuid, Session),
        plain: (Uuid, Session),
        outsider: (Uuid, Session),
    }

    async fn cast(w: &W) -> Cast {
        let owner = w.user("owner").await;
        let org = w.org("acme", owner.0).await;
        let admin = w.user("admin").await;
        w.org_member(&owner.1, org, admin.0, "admin").await;
        let (wsadmin, editor, editor2, viewer, plain) = (
            w.user("wsadmin").await,
            w.user("editor").await,
            w.user("editor2").await,
            w.user("viewer").await,
            w.user("plain").await,
        );
        for p in [&wsadmin, &editor, &editor2, &viewer, &plain] {
            w.org_member(&owner.1, org, p.0, "member").await;
        }
        let outsider = w.user("outsider").await;
        w.org("elsewhere", outsider.0).await;
        let ws = w.ws(&owner.1, org, "main").await;
        w.ws_add(&owner.1, ws, wsadmin.0, "admin").await;
        w.ws_add(&owner.1, ws, editor.0, "editor").await;
        w.ws_add(&owner.1, ws, editor2.0, "editor").await;
        w.ws_add(&owner.1, ws, viewer.0, "viewer").await;
        let file = w.file_in(&owner.1, ws, "design.fig").await;
        Cast {
            org,
            ws,
            file,
            owner,
            admin,
            wsadmin,
            editor,
            editor2,
            viewer,
            plain,
            outsider,
        }
    }

    // ---- roles ---------------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn upload_role_matrix_for_all_four_routes() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(1000, 1);

        // Create: editor and better; viewers 403; others do not see the workspace.
        forbidden(&w.create(&c.viewer.1, c.file, &data).await);
        hidden(&w.create(&c.plain.1, c.file, &data).await);
        hidden(&w.create(&c.outsider.1, c.file, &data).await);
        hidden(&w.create(&w.op, c.file, &data).await);
        let mut ids = Vec::new();
        for who in [&c.editor.1, &c.wsadmin.1, &c.admin.1, &c.owner.1] {
            let r = w.create(who, c.file, &data).await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            assert_eq!(r.body["state"], "created");
            assert_eq!(r.body["max_bytes"], 1000);
            ids.push(id_of_key(&r, "upload_id"));
        }
        assert_eq!(w.reserved().await, (4000, 4000));

        // Read and abort: the creator and managers of the workspace.
        let editors_upload = ids[0];
        let url = format!("/api/v1/uploads/{editors_upload}");
        for who in [&c.editor.1, &c.wsadmin.1, &c.admin.1, &c.owner.1] {
            let r = w.get(who, &url).await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.text);
            assert_eq!(r.body["sha256"], sha_hex(&data));
        }
        forbidden(&w.get(&c.editor2.1, &url).await);
        forbidden(&w.get(&c.viewer.1, &url).await);
        hidden(&w.get(&c.plain.1, &url).await);
        hidden(&w.get(&c.outsider.1, &url).await);
        forbidden(&w.delete(&c.editor2.1, &url).await);
        forbidden(&w.delete(&c.viewer.1, &url).await);
        hidden(&w.delete(&c.plain.1, &url).await);

        // Content: only the creator. Even a workspace admin or the owner cannot write for somebody else.
        forbidden(&w.put(&c.wsadmin.1, editors_upload, data.clone()).await);
        forbidden(&w.put(&c.owner.1, editors_upload, data.clone()).await);
        forbidden(&w.put(&c.editor2.1, editors_upload, data.clone()).await);
        hidden(&w.put(&c.plain.1, editors_upload, data.clone()).await);
        hidden(&w.put(&c.outsider.1, editors_upload, data.clone()).await);
        let stored = w.put(&c.editor.1, editors_upload, data.clone()).await;
        assert_eq!(stored.status, StatusCode::OK, "{}", stored.text);
        assert_eq!(stored.body["state"], "stored");

        // A workspace admin aborts somebody else's session.
        assert_eq!(
            w.delete(&c.wsadmin.1, &format!("/api/v1/uploads/{}", ids[3]))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(w.state(ids[3]).await, "aborted");
    }

    // ---- the happy path ----------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_verified_upload_leaves_a_staged_blob_and_keeps_its_reservation() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(300_000, 2);
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        assert_eq!(w.state(id).await, "created");
        let r = w.put(&c.editor.1, id, data.clone()).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        assert_eq!(r.body["state"], "stored");
        // One staged blob with the verified size and hash, one object, no leftovers.
        let blob: (String, i64, String, String) = sqlx::query_as(
            "SELECT state, size_bytes, encode(sha256, 'hex'), object_key FROM blobs",
        )
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!((blob.0.as_str(), blob.1), ("staged", 300_000));
        assert_eq!(blob.2, sha_hex(&data));
        assert_eq!(blob.3.len(), 32);
        assert_eq!(w.stored_objects(), 1);
        assert_eq!(w.temp_files(), 0);
        // The reservation stays until a version commit; the usage shows it.
        assert_eq!(w.reserved().await, (300_000, 300_000));
        let usage = w
            .get(
                &c.owner.1,
                &format!("/api/v1/organizations/{}/usage", c.org),
            )
            .await;
        assert_eq!(usage.status, StatusCode::OK, "{}", usage.text);
        assert_eq!(usage.body["organization"]["reserved_bytes"], 300_000);
        assert_eq!(usage.body["organization"]["committed_bytes"], 0);
        assert_eq!(usage.body["workspaces"][0]["reserved_bytes"], 300_000);
        // The session is closed for content: no second PUT, no resume.
        assert_status(
            &w.put(&c.editor.1, id, data.clone()).await,
            StatusCode::CONFLICT,
            "UPLOAD_CLOSED",
        );
        assert_eq!(w.stored_objects(), 1);
        // Empty content is a valid upload, too.
        let empty = w.upload_id(&c.editor.1, c.file, b"").await;
        assert_eq!(
            w.put(&c.editor.1, empty, Vec::new()).await.status,
            StatusCode::OK
        );
    }

    // ---- idempotency -----------------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn idempotency_keys_replay_the_same_session_and_reject_other_requests() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(500, 3);
        let sha = sha_hex(&data);
        let first = w
            .create_raw(&c.editor.1, c.file, 500, &sha, Some("key-1"))
            .await;
        assert_eq!(first.status, StatusCode::CREATED, "{}", first.text);
        let replay = w
            .create_raw(&c.editor.1, c.file, 500, &sha, Some("key-1"))
            .await;
        assert_eq!(replay.status, StatusCode::OK, "{}", replay.text);
        assert_eq!(replay.body["upload_id"], first.body["upload_id"]);
        assert_eq!(w.count("SELECT count(*) FROM upload_sessions").await, 1);
        assert_eq!(w.reserved().await, (500, 500), "reserved once");
        // Another payload under the same key.
        assert_status(
            &w.create_raw(&c.editor.1, c.file, 501, &sha, Some("key-1"))
                .await,
            StatusCode::CONFLICT,
            "IDEMPOTENCY_KEY_REUSED",
        );
        assert_status(
            &w.create_raw(&c.editor.1, c.file, 500, &sha_hex(b"other"), Some("key-1"))
                .await,
            StatusCode::CONFLICT,
            "IDEMPOTENCY_KEY_REUSED",
        );
        // Keys are per user.
        let other = w
            .create_raw(&c.editor2.1, c.file, 500, &sha, Some("key-1"))
            .await;
        assert_eq!(other.status, StatusCode::CREATED, "{}", other.text);
        assert_ne!(other.body["upload_id"], first.body["upload_id"]);
        // No key: every request is a new session.
        w.create(&c.editor.1, c.file, &data).await;
        w.create(&c.editor.1, c.file, &data).await;
        assert_eq!(w.count("SELECT count(*) FROM upload_sessions").await, 4);
        // Malformed keys.
        for bad in ["", "has space", "ü", &"k".repeat(129)] {
            let r = w
                .create_raw(&c.editor.1, c.file, 500, &sha, Some(bad))
                .await;
            assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad:?}");
        }
    }

    // ---- validation and size cap --------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn declarations_are_validated_and_capped_by_the_maximum_file_size() {
        let mut s = settings();
        s.max_file_size_bytes = 1000;
        let w = W::from(TestEnv::with(s, craftcloud_api::ApiConfig::default()).await).await;
        let c = cast(&w).await;
        let sha = sha_hex(b"x");
        // Exactly the maximum is fine, one more is not.
        assert_eq!(
            w.create_raw(&c.editor.1, c.file, 1000, &sha, None)
                .await
                .status,
            StatusCode::CREATED
        );
        assert_status(
            &w.create_raw(&c.editor.1, c.file, 1001, &sha, None).await,
            StatusCode::PAYLOAD_TOO_LARGE,
            "PAYLOAD_TOO_LARGE",
        );
        assert_status(
            &w.create_raw(&c.editor.1, c.file, u64::MAX, &sha, None)
                .await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "VALIDATION_ERROR",
        );
        for bad in ["", "xyz", &"g".repeat(64), &"a".repeat(63), &"a".repeat(65)] {
            let r = w.create_raw(&c.editor.1, c.file, 10, bad, None).await;
            assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad:?}");
            assert_eq!(r.body["error"]["details"]["fields"], json!(["sha256"]));
        }
        // Upper-case hex is accepted and normalized.
        let upper = sha.to_uppercase();
        let ok = w.create_raw(&c.editor.1, c.file, 10, &upper, None).await;
        assert_eq!(ok.status, StatusCode::CREATED);
        assert_eq!(ok.body["sha256"], sha);
        // Unknown fields and a negative size are rejected.
        let extra = w
            .json(
                "POST",
                &format!("/api/v1/files/{}/uploads", c.file),
                &c.editor.1,
                json!({"size_bytes": 1, "sha256": sha, "name": "x"}),
            )
            .await;
        assert_eq!(extra.status, StatusCode::UNPROCESSABLE_ENTITY);
        let negative = w
            .json(
                "POST",
                &format!("/api/v1/files/{}/uploads", c.file),
                &c.editor.1,
                json!({"size_bytes": -1, "sha256": sha}),
            )
            .await;
        assert_eq!(negative.status, StatusCode::UNPROCESSABLE_ENTITY);
        // Files in the trash take no uploads.
        let doomed = w.file_in(&c.owner.1, c.ws, "doomed.fig").await;
        assert_eq!(
            w.delete(&c.owner.1, &format!("/api/v1/files/{doomed}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_status(
            &w.create(&c.editor.1, doomed, b"x").await,
            StatusCode::CONFLICT,
            "FILE_DELETED",
        );
        // Unknown ids.
        hidden(&w.create(&c.editor.1, Uuid::new_v4(), b"x").await);
    }

    // ---- quota -------------------------------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn quotas_block_at_the_exact_boundary_and_lowering_never_deletes() {
        let w = W::new().await;
        let c = cast(&w).await;
        // Who sets quotas.
        forbidden(
            &w.json(
                "PUT",
                &format!("/api/v1/admin/organizations/{}/quota", c.org),
                &c.owner.1,
                json!({"quota_bytes": 5}),
            )
            .await,
        );
        hidden(
            &w.json(
                "PUT",
                &format!("/api/v1/admin/organizations/{}/quota", Uuid::new_v4()),
                &w.op,
                json!({"quota_bytes": 5}),
            )
            .await,
        );
        for who in [&c.editor.1, &c.wsadmin.1, &c.viewer.1] {
            forbidden(&w.ws_quota(c.ws, Some(5), who).await);
        }
        hidden(&w.ws_quota(c.ws, Some(5), &c.plain.1).await);
        hidden(&w.ws_quota(c.ws, Some(5), &c.outsider.1).await);
        // The key is required, values must be sane.
        for bad in [
            json!({}),
            json!({"quota_bytes": -1}),
            json!({"quota_bytes": "5"}),
            json!({"quota_bytes": 1.5}),
            json!({"quota_bytes": u64::MAX}),
        ] {
            let r = w
                .json(
                    "PUT",
                    &format!("/api/v1/admin/organizations/{}/quota", c.org),
                    &w.op,
                    bad.clone(),
                )
                .await;
            assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        }

        w.set_org_quota(c.org, Some(1000)).await;
        // A workspace quota above the organization quota is refused.
        assert_eq!(
            w.ws_quota(c.ws, Some(1001), &c.admin.1).await.status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            w.ws_quota(c.ws, Some(700), &c.admin.1).await.status,
            StatusCode::OK
        );

        // The workspace limit (700) binds first: 600 + 100 fits exactly, 1 more does not.
        let a = w.create(&c.editor.1, c.file, &pattern(600, 1)).await;
        assert_eq!(a.status, StatusCode::CREATED, "{}", a.text);
        assert_eq!(
            w.create(&c.editor.1, c.file, &pattern(100, 2)).await.status,
            StatusCode::CREATED
        );
        assert_status(
            &w.create(&c.editor.1, c.file, &pattern(1, 3)).await,
            StatusCode::PAYLOAD_TOO_LARGE,
            "QUOTA_EXCEEDED",
        );
        // A zero-byte file still fits nowhere beyond the limit only if sizes add up; 0 bytes adds nothing.
        assert_eq!(
            w.create(&c.editor.1, c.file, b"").await.status,
            StatusCode::CREATED
        );
        assert_eq!(w.reserved().await, (700, 700));
        // Aborting frees the space for exactly that much.
        let id = id_of_key(&a, "upload_id");
        assert_eq!(
            w.delete(&c.editor.1, &format!("/api/v1/uploads/{id}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(w.reserved().await, (100, 100));
        assert_eq!(
            w.create(&c.editor.1, c.file, &pattern(600, 4)).await.status,
            StatusCode::CREATED
        );
        assert_status(
            &w.create(&c.editor.1, c.file, &pattern(1, 5)).await,
            StatusCode::PAYLOAD_TOO_LARGE,
            "QUOTA_EXCEEDED",
        );

        // Lifting the workspace limit makes the organization limit (1000) bind.
        assert_eq!(
            w.ws_quota(c.ws, None, &c.owner.1).await.status,
            StatusCode::OK
        );
        assert_eq!(
            w.create(&c.editor.1, c.file, &pattern(300, 6)).await.status,
            StatusCode::CREATED
        );
        assert_status(
            &w.create(&c.editor.1, c.file, &pattern(1, 7)).await,
            StatusCode::PAYLOAD_TOO_LARGE,
            "QUOTA_EXCEEDED",
        );

        // Lowering below the current usage deletes nothing and only blocks new writes.
        w.set_org_quota(c.org, Some(10)).await;
        assert_eq!(w.reserved().await, (1000, 1000));
        assert_eq!(
            w.count("SELECT count(*) FROM upload_sessions WHERE state = 'created'")
                .await,
            4
        );
        assert_status(
            &w.create(&c.editor.1, c.file, b"x").await,
            StatusCode::PAYLOAD_TOO_LARGE,
            "QUOTA_EXCEEDED",
        );
        // Unlimited again.
        w.set_org_quota(c.org, None).await;
        assert_eq!(
            w.create(&c.editor.1, c.file, &pattern(5000, 8))
                .await
                .status,
            StatusCode::CREATED
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn usage_listing_is_for_owners_and_admins_and_paginated() {
        let w = W::new().await;
        let c = cast(&w).await;
        for i in 0..4 {
            w.ws(&c.owner.1, c.org, &format!("extra-{i}")).await;
        }
        let url = format!("/api/v1/organizations/{}/usage", c.org);
        for who in [&c.owner.1, &c.admin.1] {
            assert_eq!(w.get(who, &url).await.status, StatusCode::OK);
        }
        for who in [&c.wsadmin.1, &c.editor.1, &c.viewer.1, &c.plain.1] {
            forbidden(&w.get(who, &url).await);
        }
        hidden(&w.get(&c.outsider.1, &url).await);
        let first = w.get(&c.owner.1, &format!("{url}?limit=2")).await;
        assert_eq!(first.body["workspaces"].as_array().unwrap().len(), 2);
        let cursor = first.body["next_cursor"].as_str().unwrap();
        let second = w
            .get(&c.owner.1, &format!("{url}?limit=10&cursor={cursor}"))
            .await;
        assert_eq!(second.body["workspaces"].as_array().unwrap().len(), 3);
        assert!(second.body["next_cursor"].is_null());
        // The cursor belongs to this listing.
        let other_org = w.user("owner2").await;
        let org2 = w.org("second", other_org.0).await;
        assert_status(
            &w.get(
                &other_org.1,
                &format!("/api/v1/organizations/{org2}/usage?cursor={cursor}"),
            )
            .await,
            StatusCode::BAD_REQUEST,
            "INVALID_CURSOR",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn parallel_reservations_can_never_exceed_the_quota() {
        let mut s = settings();
        s.uploads_per_user = 100;
        s.uploads_per_org = 100;
        let w = W::from(TestEnv::with(s, craftcloud_api::ApiConfig::default()).await).await;
        let c = cast(&w).await;
        w.set_org_quota(c.org, Some(1000)).await;
        let mut handles = Vec::new();
        for i in 0..30_usize {
            let (app, session, file) = (w.env.app.clone(), c.editor.1.clone(), c.file);
            handles.push(tokio::spawn(async move {
                let data = pattern(100, i as u8);
                let response = call(
                    &app,
                    Req::new("POST", &format!("/api/v1/files/{file}/uploads"))
                        .session(&session)
                        .json(json!({"size_bytes": 100, "sha256": sha_hex(&data)})),
                )
                .await;
                (response.status, response.text)
            }));
        }
        let mut created = 0;
        let mut refused = 0;
        for h in handles {
            let (status, body) = h.await.unwrap();
            match status {
                StatusCode::CREATED => created += 1,
                StatusCode::PAYLOAD_TOO_LARGE => refused += 1,
                other => panic!("unexpected {other}: {body}"),
            }
        }
        assert_eq!((created, refused), (10, 20));
        assert_eq!(w.reserved().await, (1000, 1000));
        assert_eq!(w.count("SELECT count(*) FROM upload_sessions").await, 10);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn open_upload_caps_apply_per_user_and_per_organization() {
        let mut s = settings();
        s.uploads_per_user = 2;
        s.uploads_per_org = 3;
        let w = W::from(TestEnv::with(s, craftcloud_api::ApiConfig::default()).await).await;
        let c = cast(&w).await;
        let first = w.create(&c.editor.1, c.file, b"1").await;
        let second = w.create(&c.editor.1, c.file, b"2").await;
        assert_eq!(
            (first.status, second.status),
            (StatusCode::CREATED, StatusCode::CREATED)
        );
        assert_status(
            &w.create(&c.editor.1, c.file, b"3").await,
            StatusCode::CONFLICT,
            "UPLOAD_LIMIT",
        );
        // Another user is not affected by that cap, but the organization cap applies.
        assert_eq!(
            w.create(&c.editor2.1, c.file, b"4").await.status,
            StatusCode::CREATED
        );
        assert_status(
            &w.create(&c.owner.1, c.file, b"5").await,
            StatusCode::CONFLICT,
            "UPLOAD_LIMIT",
        );
        // Finishing or aborting frees a slot; a stored session no longer counts as open.
        let id = id_of_key(&first, "upload_id");
        assert_eq!(
            w.put(&c.editor.1, id, b"1".to_vec()).await.status,
            StatusCode::OK
        );
        assert_eq!(
            w.create(&c.owner.1, c.file, b"5").await.status,
            StatusCode::CREATED
        );
        let id2 = id_of_key(&second, "upload_id");
        assert_eq!(
            w.delete(&c.editor.1, &format!("/api/v1/uploads/{id2}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            w.create(&c.editor.1, c.file, b"6").await.status,
            StatusCode::CREATED
        );
    }

    // ---- content verification -----------------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn bad_content_fails_the_session_releases_the_reservation_and_stores_nothing() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(100_000, 9);
        // Too many bytes: 413, stopped at the cap.
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        let mut longer = data.clone();
        longer.extend_from_slice(&[0; 10]);
        assert_status(
            &w.put(&c.editor.1, id, longer).await,
            StatusCode::PAYLOAD_TOO_LARGE,
            "PAYLOAD_TOO_LARGE",
        );
        assert_eq!(w.state(id).await, "failed");
        // Too few bytes: 422 LENGTH_MISMATCH.
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        assert_status(
            &w.put(&c.editor.1, id, data[..99_999].to_vec()).await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "LENGTH_MISMATCH",
        );
        assert_eq!(w.state(id).await, "failed");
        // Right length, wrong bytes: 422 CHECKSUM_MISMATCH.
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        let mut wrong = data.clone();
        wrong[500] ^= 1;
        assert_status(
            &w.put(&c.editor.1, id, wrong).await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "CHECKSUM_MISMATCH",
        );
        assert_eq!(w.state(id).await, "failed");
        // Nothing stored, nothing reserved, no blob, no temporary file.
        w.settled_no_temp().await;
        assert_eq!(w.stored_objects(), 0);
        assert_eq!(w.count("SELECT count(*) FROM blobs").await, 0);
        assert_eq!(w.reserved().await, (0, 0));
        // A failed session cannot be resumed; a new one works.
        assert_status(
            &w.put(&c.editor.1, id, data.clone()).await,
            StatusCode::CONFLICT,
            "UPLOAD_CLOSED",
        );
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        assert_eq!(
            w.put(&c.editor.1, id, data.clone()).await.status,
            StatusCode::OK
        );
        assert_eq!(w.stored_objects(), 1);
        // A declared Content-Length above the declared size is refused before reading.
        let id = w.upload_id(&c.editor.1, c.file, b"abc").await;
        let early = w
            .send(
                Req::new("PUT", &format!("/api/v1/uploads/{id}/content"))
                    .session(&c.editor.1)
                    .header("Content-Length", "4")
                    .body(Body::from(b"abc".to_vec())),
            )
            .await;
        assert_status(&early, StatusCode::PAYLOAD_TOO_LARGE, "PAYLOAD_TOO_LARGE");
        assert_eq!(w.state(id).await, "failed");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_full_disk_answers_507_and_leaves_no_blob_no_object_and_no_reservation() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(300_000, 4);
        for offset in [0_u64, 70_000, 299_999] {
            w.env.faults.fail_write_at(offset, FaultKind::Full);
            let id = w.upload_id(&c.editor.1, c.file, &data).await;
            let r = w.put(&c.editor.1, id, data.clone()).await;
            assert_status(&r, StatusCode::INSUFFICIENT_STORAGE, "INSUFFICIENT_STORAGE");
            assert_eq!(w.state(id).await, "failed");
            w.settled_no_temp().await;
            assert_eq!(w.stored_objects(), 0, "offset {offset}");
            assert_eq!(w.count("SELECT count(*) FROM blobs").await, 0);
            assert_eq!(w.reserved().await, (0, 0), "offset {offset}");
        }
        // A generic I/O failure is a 500 with the same cleanup; storage works again afterwards.
        w.env.faults.fail_write_at(10, FaultKind::Io);
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        assert_eq!(
            w.put(&c.editor.1, id, data.clone()).await.status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(w.reserved().await, (0, 0));
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        assert_eq!(
            w.put(&c.editor.1, id, data.clone()).await.status,
            StatusCode::OK
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn missing_storage_is_503_and_reserves_nothing() {
        let w = W::from(TestEnv::without_storage().await).await;
        let c = cast(&w).await;
        assert_status(
            &w.create(&c.editor.1, c.file, b"x").await,
            StatusCode::SERVICE_UNAVAILABLE,
            "STORAGE_UNAVAILABLE",
        );
        assert_eq!(w.reserved().await, (0, 0));
        // Authorization still comes first.
        forbidden(&w.create(&c.viewer.1, c.file, b"x").await);
        hidden(&w.create(&c.outsider.1, c.file, b"x").await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn file_grant_editor_can_upload_and_revocation_during_stream_is_rechecked() {
        let w = W::new().await;
        let c = cast(&w).await;
        let grant = w
            .json(
                "POST",
                &format!("/api/v1/files/{}/grants", c.file),
                &c.owner.1,
                json!({"user_id": c.plain.0.to_string(), "role": "editor"}),
            )
            .await;
        assert_eq!(grant.status, StatusCode::CREATED, "{}", grant.text);
        let grant_id = Uuid::parse_str(grant.body["id"].as_str().unwrap()).unwrap();

        let data = pattern(100_000, 37);
        let completed = w.upload_id(&c.plain.1, c.file, &data).await;
        assert_eq!(
            w.put(&c.plain.1, completed, data.clone()).await.status,
            StatusCode::OK
        );
        let state: String = sqlx::query_scalar("SELECT state FROM upload_sessions WHERE id=$1")
            .bind(completed)
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(
            state, "stored",
            "an editor grant authorizes content finalization"
        );

        let raced = w.upload_id(&c.plain.1, c.file, &data).await;
        let (tx, put) = start_put(&w, &c.plain.1, raced);
        tx.send(Bytes::from(data[..50_000].to_vec())).await.unwrap();
        w.wait_for(raced, "receiving").await;
        assert_eq!(
            w.delete(&c.owner.1, &format!("/api/v1/grants/{grant_id}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        tx.send(Bytes::from(data[50_000..].to_vec())).await.unwrap();
        drop(tx);
        let rejected = put.await.unwrap();
        hidden(&rejected);
        w.wait_for(raced, "failed").await;
        w.settled_no_temp().await;
        assert_eq!(
            w.stored_objects(),
            1,
            "only the upload completed before revoke remains stored"
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn file_grantee_can_read_and_abort_own_upload_until_grant_is_revoked() {
        let w = W::new().await;
        let c = cast(&w).await;
        let grant = w
            .json(
                "POST",
                &format!("/api/v1/files/{}/grants", c.file),
                &c.owner.1,
                json!({"user_id": c.plain.0.to_string(), "role": "editor"}),
            )
            .await;
        assert_eq!(grant.status, StatusCode::CREATED, "{}", grant.text);
        let grant_id = id_of(&grant);

        let bytes = pattern(32, 13);
        let first = w.upload_id(&c.plain.1, c.file, &bytes).await;
        let seen = w.get(&c.plain.1, &format!("/api/v1/uploads/{first}")).await;
        assert_eq!(seen.status, StatusCode::OK, "{}", seen.text);
        assert_eq!(seen.body["upload_id"], first.to_string());
        assert_eq!(
            w.delete(&c.plain.1, &format!("/api/v1/uploads/{first}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            w.reserved().await,
            (0, 0),
            "abort releases both quota reservations"
        );

        let second = w.upload_id(&c.plain.1, c.file, &bytes).await;
        let demoted = w
            .send(
                Req::new("PATCH", &format!("/api/v1/grants/{grant_id}"))
                    .session(&c.owner.1)
                    .if_match("\"grant-v1\"")
                    .json(json!({"role":"viewer"})),
            )
            .await;
        assert_eq!(demoted.status, StatusCode::OK, "{}", demoted.text);
        assert_eq!(
            w.get(&c.plain.1, &format!("/api/v1/uploads/{second}"))
                .await
                .status,
            StatusCode::OK
        );
        assert_eq!(
            w.delete(&c.plain.1, &format!("/api/v1/uploads/{second}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );

        let promoted = w
            .send(
                Req::new("PATCH", &format!("/api/v1/grants/{grant_id}"))
                    .session(&c.owner.1)
                    .if_match("\"grant-v2\"")
                    .json(json!({"role":"editor"})),
            )
            .await;
        assert_eq!(promoted.status, StatusCode::OK, "{}", promoted.text);
        let third = w.upload_id(&c.plain.1, c.file, &bytes).await;
        assert_eq!(
            w.delete(&c.owner.1, &format!("/api/v1/grants/{grant_id}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        hidden(&w.get(&c.plain.1, &format!("/api/v1/uploads/{third}")).await);
        hidden(
            &w.delete(&c.plain.1, &format!("/api/v1/uploads/{third}"))
                .await,
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn organization_write_routes_reuse_the_transaction_connection() {
        let w = W::new().await;
        let c = cast(&w).await;
        sqlx::query("UPDATE files SET app='photocraft',format='native' WHERE id=$1")
            .bind(c.file)
            .execute(&w.env.pool)
            .await
            .unwrap();
        let pool = w
            .env
            .pool_with_max_connections(1, Duration::from_secs(2))
            .await;
        let config = ApiConfig::default();
        let store = craftcloud_storage::LocalStore::open(&w.env.storage_dir)
            .await
            .unwrap();
        let services = Services::new(pool, None, settings())
            .with_storage(store)
            .with_clock(w.env.clock.clone())
            .with_metrics(config.metrics.clone());
        let app = router_with_shared(
            Capabilities::development_defaults(),
            &config,
            Some(std::sync::Arc::new(services)),
        );

        let created = call(
            &app,
            Req::new("POST", &format!("/api/v1/files/{}/grants", c.file))
                .session(&c.owner.1)
                .json(json!({"user_id":c.plain.0.to_string(),"role":"editor"})),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        let listed = call(
            &app,
            Req::new("GET", &format!("/api/v1/files/{}/grants", c.file)).cookie_only(&c.owner.1),
        )
        .await;
        assert_eq!(listed.status, StatusCode::OK, "{}", listed.text);
        assert_eq!(listed.body["items"].as_array().unwrap().len(), 1);

        let upload = w.upload_id(&c.plain.1, c.file, b"single-pool upload").await;
        let completed = call(
            &app,
            Req::new("PUT", &format!("/api/v1/uploads/{upload}/content"))
                .session(&c.plain.1)
                .body(Body::from(b"single-pool upload".to_vec())),
        )
        .await;
        assert_eq!(completed.status, StatusCode::OK, "{}", completed.text);

        let lock = call(
            &app,
            Req::new("POST", &format!("/api/v1/files/{}/lock", c.file))
                .session(&c.owner.1)
                .json(json!({"client_id":"single-pool"})),
        )
        .await;
        assert_eq!(lock.status, StatusCode::OK, "{}", lock.text);

        let renamed = call(
            &app,
            Req::new("PATCH", &format!("/api/v1/files/{}", c.file))
                .session(&c.owner.1)
                .if_match("\"file-v1\"")
                .json(json!({"name":"single-pool.fig"})),
        )
        .await;
        assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.text);
        assert_eq!(
            call(
                &app,
                Req::new("DELETE", &format!("/api/v1/files/{}/lock", c.file)).session(&c.owner.1),
            )
            .await
            .status,
            StatusCode::NO_CONTENT
        );
    }

    // ---- concurrency and races ---------------------------------------------------------------------------------

    /// Starts a PUT whose body the test feeds; returns the sender and the task.
    fn start_put(
        w: &W,
        who: &Session,
        upload: Uuid,
    ) -> (mpsc::Sender<Bytes>, tokio::task::JoinHandle<Resp>) {
        let (tx, rx) = mpsc::channel(8);
        let (app, session) = (w.env.app.clone(), who.clone());
        let handle = tokio::spawn(async move {
            call(
                &app,
                Req::new("PUT", &format!("/api/v1/uploads/{upload}/content"))
                    .session(&session)
                    .body(Body::new(ChannelBody { rx })),
            )
            .await
        });
        (tx, handle)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn only_one_put_at_a_time_and_no_abort_while_receiving() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(200_000, 5);
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        let (tx, first) = start_put(&w, &c.editor.1, id);
        tx.send(Bytes::from(data[..100_000].to_vec()))
            .await
            .unwrap();
        w.wait_for(id, "receiving").await;
        // A second PUT, an abort and a read while the first one is running.
        assert_status(
            &w.put(&c.editor.1, id, data.clone()).await,
            StatusCode::CONFLICT,
            "UPLOAD_IN_PROGRESS",
        );
        assert_status(
            &w.delete(&c.editor.1, &format!("/api/v1/uploads/{id}"))
                .await,
            StatusCode::CONFLICT,
            "UPLOAD_IN_PROGRESS",
        );
        let seen = w.get(&c.editor.1, &format!("/api/v1/uploads/{id}")).await;
        assert_eq!(seen.body["state"], "receiving");
        // The running upload is unharmed and finishes.
        tx.send(Bytes::from(data[100_000..].to_vec()))
            .await
            .unwrap();
        drop(tx);
        let done = first.await.unwrap();
        assert_eq!(done.status, StatusCode::OK, "{}", done.text);
        assert_eq!(w.state(id).await, "stored");
        assert_eq!(w.stored_objects(), 1);
        assert_eq!(w.count("SELECT count(*) FROM blobs").await, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn parallel_puts_of_one_session_have_exactly_one_winner() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(50_000, 6);
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        let mut handles = Vec::new();
        for _ in 0..6 {
            let (app, session, body) = (w.env.app.clone(), c.editor.1.clone(), data.clone());
            handles.push(tokio::spawn(async move {
                call(
                    &app,
                    Req::new("PUT", &format!("/api/v1/uploads/{id}/content"))
                        .session(&session)
                        .body(Body::from(body)),
                )
                .await
                .status
            }));
        }
        let mut ok = 0;
        for h in handles {
            match h.await.unwrap() {
                StatusCode::OK => ok += 1,
                StatusCode::CONFLICT => {}
                other => panic!("unexpected {other}"),
            }
        }
        assert_eq!(ok, 1);
        assert_eq!(w.count("SELECT count(*) FROM blobs").await, 1);
        assert_eq!(w.stored_objects(), 1);
        assert_eq!(w.reserved().await, (50_000, 50_000));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn an_editor_demoted_during_the_upload_stores_nothing() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(120_000, 7);
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        let (tx, put) = start_put(&w, &c.editor.1, id);
        tx.send(Bytes::from(data[..60_000].to_vec())).await.unwrap();
        w.wait_for(id, "receiving").await;
        // The owner demotes the editor to viewer while the bytes are arriving.
        let demote = w
            .json(
                "PATCH",
                &format!("/api/v1/workspaces/{}/members/{}", c.ws, c.editor.0),
                &c.owner.1,
                json!({"role": "viewer"}),
            )
            .await;
        assert_eq!(demote.status, StatusCode::OK, "{}", demote.text);
        tx.send(Bytes::from(data[60_000..].to_vec())).await.unwrap();
        drop(tx);
        let r = put.await.unwrap();
        forbidden(&r);
        assert_eq!(w.state(id).await, "failed");
        assert_eq!(
            w.stored_objects(),
            0,
            "the published object is deleted again"
        );
        assert_eq!(w.count("SELECT count(*) FROM blobs").await, 0);
        assert_eq!(w.reserved().await, (0, 0));
        w.settled_no_temp().await;

        // The same when the editor is removed from the workspace altogether (the workspace is then invisible: 404).
        let editor2_upload = w.upload_id(&c.editor2.1, c.file, &data).await;
        let (tx, put) = start_put(&w, &c.editor2.1, editor2_upload);
        tx.send(Bytes::from(data[..1000].to_vec())).await.unwrap();
        w.wait_for(editor2_upload, "receiving").await;
        let removed = w
            .delete(
                &c.owner.1,
                &format!("/api/v1/workspaces/{}/members/{}", c.ws, c.editor2.0),
            )
            .await;
        assert_eq!(removed.status, StatusCode::NO_CONTENT, "{}", removed.text);
        tx.send(Bytes::from(data[1000..].to_vec())).await.unwrap();
        drop(tx);
        hidden(&put.await.unwrap());
        assert_eq!(w.stored_objects(), 0);
        assert_eq!(w.reserved().await, (0, 0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_file_or_project_deleted_during_the_upload_stores_nothing() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(80_000, 8);
        // File deleted.
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        let (tx, put) = start_put(&w, &c.editor.1, id);
        tx.send(Bytes::from(data[..10_000].to_vec())).await.unwrap();
        w.wait_for(id, "receiving").await;
        assert_eq!(
            w.delete(&c.owner.1, &format!("/api/v1/files/{}", c.file))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        tx.send(Bytes::from(data[10_000..].to_vec())).await.unwrap();
        drop(tx);
        assert_status(&put.await.unwrap(), StatusCode::CONFLICT, "FILE_DELETED");
        assert_eq!(w.state(id).await, "failed");
        assert_eq!(w.stored_objects(), 0);
        assert_eq!(w.reserved().await, (0, 0));
        // Project deleted.
        let file2 = w.file_in(&c.owner.1, c.ws, "second.fig").await;
        let project: Uuid = sqlx::query_scalar("SELECT project_id FROM files WHERE id = $1")
            .bind(file2)
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        let id = w.upload_id(&c.editor.1, file2, &data).await;
        let (tx, put) = start_put(&w, &c.editor.1, id);
        tx.send(Bytes::from(data[..10_000].to_vec())).await.unwrap();
        w.wait_for(id, "receiving").await;
        assert_eq!(
            w.delete(&c.owner.1, &format!("/api/v1/projects/{project}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        tx.send(Bytes::from(data[10_000..].to_vec())).await.unwrap();
        drop(tx);
        assert_status(&put.await.unwrap(), StatusCode::CONFLICT, "FILE_DELETED");
        assert_eq!(w.stored_objects(), 0);
        assert_eq!(w.reserved().await, (0, 0));
        // Deleting before the content arrives is caught at the start of the PUT.
        let file3 = w.file_in(&c.owner.1, c.ws, "third.fig").await;
        let id = w.upload_id(&c.editor.1, file3, &data).await;
        assert_eq!(
            w.delete(&c.owner.1, &format!("/api/v1/files/{file3}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_status(
            &w.put(&c.editor.1, id, data.clone()).await,
            StatusCode::CONFLICT,
            "FILE_DELETED",
        );
        assert_eq!(w.reserved().await, (0, 0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_client_that_disconnects_mid_upload_releases_everything() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(100_000, 10);
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        let (tx, put) = start_put(&w, &c.editor.1, id);
        tx.send(Bytes::from(data[..50_000].to_vec())).await.unwrap();
        w.wait_for(id, "receiving").await;
        // The request future is dropped (as when the connection closes).
        put.abort();
        let _ = put.await;
        w.wait_for(id, "failed").await;
        w.settled_no_temp().await;
        assert_eq!(w.stored_objects(), 0);
        assert_eq!(w.reserved().await, (0, 0));
        // A body that ends with an error is a 400 and cleans up the same way.
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        let (tx, put) = start_put(&w, &c.editor.1, id);
        tx.send(Bytes::from(data[..100].to_vec())).await.unwrap();
        w.wait_for(id, "receiving").await;
        drop(tx); // ends the body early: a short upload
        assert_status(
            &put.await.unwrap(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "LENGTH_MISMATCH",
        );
        assert_eq!(w.reserved().await, (0, 0));
    }

    // ---- expiry and abort -------------------------------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn sessions_expire_with_the_clock_and_release_their_reservation() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(1000, 11);
        let open = w.upload_id(&c.editor.1, c.file, &data).await;
        let stored = w.upload_id(&c.editor2.1, c.file, &data).await;
        assert_eq!(
            w.put(&c.editor2.1, stored, data.clone()).await.status,
            StatusCode::OK
        );
        assert_eq!(w.reserved().await, (2000, 2000));

        // Just before the 24 h end the session is still usable.
        w.env.clock.advance(Duration::from_secs(24 * 3600 - 10));
        let editor = w.relogin("editor").await;
        let shown = w.get(&editor, &format!("/api/v1/uploads/{open}")).await;
        assert_eq!(shown.body["state"], "created");
        w.env.clock.advance(Duration::from_secs(11));
        let editor = w.relogin("editor").await;
        // Reading closes it lazily; the reservation of the open session is released. The stored
        // one keeps counting (its blob is orphaned but the object is still on disk).
        let shown = w.get(&editor, &format!("/api/v1/uploads/{open}")).await;
        assert_eq!(shown.body["state"], "expired");
        assert_status(
            &w.put(&editor, open, data.clone()).await,
            StatusCode::GONE,
            "UPLOAD_EXPIRED",
        );
        // Expiry closes every due session of the organization at once.
        assert_eq!(w.state(stored).await, "expired");
        let closed = craftcloud_db::uploads::expire_due_all(&w.env.pool, w.env.clock.now(), 10)
            .await
            .unwrap();
        assert_eq!(closed, 0);
        assert_eq!(w.reserved().await, (1000, 1000));
        let blob_state: String = sqlx::query_scalar("SELECT state FROM blobs")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(blob_state, "orphaned");
        let orphans = craftcloud_db::uploads::orphaned_blobs(&w.env.pool, 10)
            .await
            .unwrap();
        assert_eq!(orphans.len(), 1);
        // The batch function (the cleanup job) expires due sessions of all organizations.
        let batch = w.upload_id(&editor, c.file, &data).await;
        w.env.clock.advance(Duration::from_secs(25 * 3600));
        let closed = craftcloud_db::uploads::expire_due_all(&w.env.pool, w.env.clock.now(), 10)
            .await
            .unwrap();
        assert_eq!(closed, 1);
        assert_eq!(w.state(batch).await, "expired");
        let editor = w.relogin("editor").await;
        // Creating a new upload also sweeps due sessions of the organization.
        let a = w.upload_id(&editor, c.file, &data).await;
        w.env.clock.advance(Duration::from_secs(25 * 3600));
        let editor = w.relogin("editor").await;
        let _b = w.upload_id(&editor, c.file, &data).await;
        assert_eq!(w.state(a).await, "expired");
        assert_eq!(w.reserved().await, (2000, 2000));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_session_that_expires_during_the_upload_stores_nothing() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(50_000, 12);
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        let (tx, put) = start_put(&w, &c.editor.1, id);
        tx.send(Bytes::from(data[..1000].to_vec())).await.unwrap();
        w.wait_for(id, "receiving").await;
        w.env.clock.advance(Duration::from_secs(25 * 3600));
        tx.send(Bytes::from(data[1000..].to_vec())).await.unwrap();
        drop(tx);
        // The session cookie is older than 8 hours of manual time too, so the
        // request is unauthenticated only for later requests; this one started before.
        let r = put.await.unwrap();
        assert_status(&r, StatusCode::GONE, "UPLOAD_EXPIRED");
        assert_eq!(w.stored_objects(), 0);
        assert_eq!(w.count("SELECT count(*) FROM blobs").await, 0);
        assert_eq!(w.reserved().await, (0, 0));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn aborting_releases_a_created_reservation_and_keeps_a_stored_blob_counted() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(4000, 13);
        let open = w.upload_id(&c.editor.1, c.file, &data).await;
        let stored = w.upload_id(&c.editor.1, c.file, &data).await;
        w.put(&c.editor.1, stored, data.clone()).await;
        assert_eq!(w.reserved().await, (8000, 8000));
        for id in [open, stored] {
            assert_eq!(
                w.delete(&c.editor.1, &format!("/api/v1/uploads/{id}"))
                    .await
                    .status,
                StatusCode::NO_CONTENT
            );
            assert_eq!(w.state(id).await, "aborted");
            // Aborting again changes nothing.
            assert_eq!(
                w.delete(&c.editor.1, &format!("/api/v1/uploads/{id}"))
                    .await
                    .status,
                StatusCode::NO_CONTENT
            );
        }
        // Only the announced upload is released; the published object still occupies storage.
        assert_eq!(w.reserved().await, (4000, 4000));
        let blob_state: String = sqlx::query_scalar("SELECT state FROM blobs")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(blob_state, "orphaned");
        assert_status(
            &w.put(&c.editor.1, open, data.clone()).await,
            StatusCode::CONFLICT,
            "UPLOAD_CLOSED",
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn usage_counters_can_be_recomputed_from_the_rows() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(2500, 14);
        w.upload_id(&c.editor.1, c.file, &data).await;
        let stored = w.upload_id(&c.editor.1, c.file, &data).await;
        w.put(&c.editor.1, stored, data.clone()).await;
        let aborted = w.upload_id(&c.editor.1, c.file, &data).await;
        w.delete(&c.editor.1, &format!("/api/v1/uploads/{aborted}"))
            .await;
        assert_eq!(w.reserved().await, (5000, 5000));
        // Corrupt the counters, then reconcile.
        sqlx::query("UPDATE organizations SET reserved_bytes = 1, committed_bytes = 7")
            .execute(&w.env.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE workspaces SET reserved_bytes = 3")
            .execute(&w.env.pool)
            .await
            .unwrap();
        let mut tx = craftcloud_db::members::OrgTx::begin(&w.env.pool, c.org)
            .await
            .unwrap()
            .unwrap();
        tx.recompute_usage().await.unwrap();
        tx.commit().await.unwrap();
        let org: (i64, i64) = sqlx::query_as(
            "SELECT reserved_bytes, committed_bytes FROM organizations WHERE id = $1",
        )
        .bind(c.org)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(org, (5000, 0));
        let ws: i64 = sqlx::query_scalar("SELECT reserved_bytes FROM workspaces WHERE id = $1")
            .bind(c.ws)
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(ws, 5000);
    }

    // ---- tenant isolation and csrf ---------------------------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn upload_ids_of_another_organization_find_nothing() {
        let w = W::new().await;
        let c = cast(&w).await;
        let b_owner = w.user("bowner").await;
        let b_org = w.org("org-b", b_owner.0).await;
        let b_ws = w.ws(&b_owner.1, b_org, "b-main").await;
        let b_file = w.file_in(&b_owner.1, b_ws, "b.fig").await;
        let data = pattern(100, 15);
        let a_upload = w.upload_id(&c.editor.1, c.file, &data).await;
        let b_upload = w.upload_id(&b_owner.1, b_file, &data).await;
        let unknown = Uuid::new_v4();
        let url = |id: Uuid| format!("/api/v1/uploads/{id}");

        let baseline = w.get(&b_owner.1, &url(unknown)).await;
        hidden(&baseline);
        for id in [a_upload, unknown] {
            let r = w.get(&b_owner.1, &url(id)).await;
            hidden(&r);
            assert_eq!(
                r.body["error"]["message"],
                baseline.body["error"]["message"]
            );
            hidden(&w.delete(&b_owner.1, &url(id)).await);
            hidden(&w.put(&b_owner.1, id, data.clone()).await);
        }
        // Creating an upload for a foreign file, and swapped ids.
        hidden(&w.create(&b_owner.1, c.file, &data).await);
        hidden(&w.create(&c.owner.1, b_file, &data).await);
        hidden(&w.get(&c.owner.1, &url(b_upload)).await);
        // Wrong kinds of ids.
        hidden(&w.get(&c.owner.1, &url(c.file)).await);
        hidden(&w.create(&c.owner.1, a_upload, &data).await);
        hidden(&w.get(&c.owner.1, "/api/v1/uploads/not-a-uuid").await);
        // Quota and usage of the other tenant.
        hidden(
            &w.json(
                "PUT",
                &format!("/api/v1/workspaces/{b_ws}/quota"),
                &c.owner.1,
                json!({"quota_bytes": 1}),
            )
            .await,
        );
        hidden(
            &w.get(&c.owner.1, &format!("/api/v1/organizations/{b_org}/usage"))
                .await,
        );
        // Nothing changed on either side.
        assert_eq!(w.state(a_upload).await, "created");
        assert_eq!(w.state(b_upload).await, "created");
        assert_eq!(w.count("SELECT count(*) FROM upload_sessions").await, 2);
        assert_eq!(w.stored_objects(), 0);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn every_unsafe_upload_and_quota_route_needs_session_csrf_origin_and_a_fresh_session() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(10, 16);
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        let routes: Vec<(&'static str, String)> = vec![
            ("POST", format!("/api/v1/files/{}/uploads", c.file)),
            ("DELETE", format!("/api/v1/uploads/{id}")),
            ("PUT", format!("/api/v1/uploads/{id}/content")),
            ("PUT", format!("/api/v1/workspaces/{}/quota", c.ws)),
            (
                "PUT",
                format!("/api/v1/admin/organizations/{}/quota", c.org),
            ),
            ("GET", format!("/api/v1/uploads/{id}")),
            ("GET", format!("/api/v1/organizations/{}/usage", c.org)),
        ];
        let build = |m: &'static str, u: &str| {
            let req = Req::new(m, u);
            match (m, u) {
                ("PUT", u) if u.ends_with("/content") => req.body(Body::from(data.clone())),
                ("PUT", _) => req.json(json!({"quota_bytes": 5})),
                ("POST", _) => req.json(json!({"size_bytes": 10, "sha256": sha_hex(&data)})),
                _ => req,
            }
        };
        for (m, u) in &routes {
            assert_status(
                &w.send(build(m, u)).await,
                StatusCode::UNAUTHORIZED,
                "UNAUTHENTICATED",
            );
            if *m != "GET" {
                assert_status(
                    &w.send(build(m, u).cookie_only(&c.owner.1)).await,
                    StatusCode::FORBIDDEN,
                    "CSRF_FAILED",
                );
                assert_status(
                    &w.send(
                        build(m, u)
                            .session(&c.owner.1)
                            .origin(Some("https://evil.example")),
                    )
                    .await,
                    StatusCode::FORBIDDEN,
                    "CSRF_FAILED",
                );
            }
        }
        assert_eq!(w.state(id).await, "created");
        assert_eq!(w.count("SELECT count(*) FROM upload_sessions").await, 1);
        assert_eq!(w.stored_objects(), 0);
        assert_eq!(
            w.count("SELECT count(*) FROM organizations WHERE quota_bytes IS NOT NULL")
                .await,
            0
        );
        w.env.clock.advance(Duration::from_secs(8 * 3600 + 60));
        for (m, u) in &routes {
            assert_status(
                &w.send(build(m, u).session(&c.owner.1)).await,
                StatusCode::UNAUTHORIZED,
                "UNAUTHENTICATED",
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn audit_lines_carry_ids_and_never_names_or_hashes() {
        let logs = capture_logs();
        let w = W::new().await;
        let c = cast(&w).await;
        let file = w
            .file_in(&c.owner.1, c.ws, "secret-upload-name-qq4.fig")
            .await;
        let data = pattern(100, 17);
        let id = w.upload_id(&c.editor.1, file, &data).await;
        w.put(&c.editor.1, id, data.clone()).await;
        let text = logs.text();
        for needle in ["upload.create", "upload.content"] {
            assert!(text.contains(needle), "audit lacks {needle}");
        }
        assert!(text.contains(&id.to_string()));
        assert!(!text.contains("secret-upload-name-qq4"));
        assert!(!text.contains(&sha_hex(&data)));
    }

    // ---- slow and long streams, cancellation, cross-organization ---------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_stream_that_outlives_the_session_is_cancelled_and_releases_everything() {
        let mut s = settings();
        s.uploads_per_user = 1;
        let w = W::from(TestEnv::with(s, craftcloud_api::ApiConfig::default()).await).await;
        let c = cast(&w).await;
        let data = pattern(60_000, 20);
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        let (tx, put) = start_put(&w, &c.editor.1, id);
        tx.send(Bytes::from(data[..10_000].to_vec())).await.unwrap();
        w.wait_for(id, "receiving").await;
        // While the stream is alive the slot and the quota stay taken.
        assert_status(
            &w.create(&c.editor.1, c.file, b"x").await,
            StatusCode::CONFLICT,
            "UPLOAD_LIMIT",
        );
        assert_eq!(w.reserved().await, (60_000, 60_000));
        // The session's time runs out while bytes keep arriving: the next piece ends it.
        w.env.clock.advance(Duration::from_secs(24 * 3600 + 1));
        tx.send(Bytes::from(data[10_000..20_000].to_vec()))
            .await
            .unwrap();
        let r = put.await.unwrap();
        assert_status(&r, StatusCode::GONE, "UPLOAD_EXPIRED");
        drop(tx);
        w.settled_no_temp().await;
        assert_eq!(w.stored_objects(), 0);
        assert_eq!(w.count("SELECT count(*) FROM blobs").await, 0);
        assert_eq!(w.reserved().await, (0, 0));
        // The state was closed exactly once: the reservation did not go negative (CHECK) and is zero.
        assert!(matches!(w.state(id).await.as_str(), "failed" | "expired"));
        // The slot is free again.
        let editor = w.relogin("editor").await;
        assert_eq!(
            w.create(&editor, c.file, b"x").await.status,
            StatusCode::CREATED
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_body_below_the_minimum_rate_is_cancelled_but_a_fast_one_is_not() {
        let w = W::new().await;
        let c = cast(&w).await;
        // Slow: 200 bytes in five minutes.
        let data = pattern(100_000, 21);
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        let (tx, put) = start_put(&w, &c.editor.1, id);
        tx.send(Bytes::from(data[..100].to_vec())).await.unwrap();
        w.wait_for(id, "receiving").await;
        w.env.clock.advance(Duration::from_secs(301));
        tx.send(Bytes::from(data[100..200].to_vec())).await.unwrap();
        assert_status(
            &put.await.unwrap(),
            StatusCode::REQUEST_TIMEOUT,
            "REQUEST_TIMEOUT",
        );
        drop(tx);
        assert_eq!(w.state(id).await, "failed");
        w.settled_no_temp().await;
        assert_eq!(w.stored_objects(), 0);
        assert_eq!(w.reserved().await, (0, 0));

        // Fast: 5 MiB at once, then the rest after the grace period still meets 16 KiB/s on average.
        let big = pattern(6 * 1024 * 1024, 22);
        let id = w.upload_id(&c.editor.1, c.file, &big).await;
        let (tx, put) = start_put(&w, &c.editor.1, id);
        tx.send(Bytes::from(big[..5 * 1024 * 1024].to_vec()))
            .await
            .unwrap();
        w.wait_for(id, "receiving").await;
        w.env.clock.advance(Duration::from_secs(301));
        tx.send(Bytes::from(big[5 * 1024 * 1024..].to_vec()))
            .await
            .unwrap();
        drop(tx);
        let done = put.await.unwrap();
        assert_eq!(done.status, StatusCode::OK, "{}", done.text);
        assert_eq!(w.state(id).await, "stored");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn cancelling_the_request_after_the_object_is_published_leaves_nothing_unaccounted() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(40_000, 23);

        for demote in [true, false] {
            let id = w.upload_id(&c.editor.1, c.file, &data).await;
            let (tx, put) = start_put(&w, &c.editor.1, id);
            tx.send(Bytes::from(data[..1000].to_vec())).await.unwrap();
            w.wait_for(id, "receiving").await;
            // Hold the organization lock so that the finishing step stalls right after publishing.
            let lock = craftcloud_db::members::OrgTx::begin(&w.env.pool, c.org)
                .await
                .unwrap()
                .unwrap();
            tx.send(Bytes::from(data[1000..].to_vec())).await.unwrap();
            drop(tx);
            for _ in 0..500 {
                if w.stored_objects() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(
                w.stored_objects(),
                1,
                "the object is published and waits for the lock"
            );
            // The client goes away right now.
            put.abort();
            let _ = put.await;
            if demote {
                // ... and meanwhile the editor loses the right to write.
                sqlx::query("UPDATE workspace_members SET role = 'viewer' WHERE user_id = $1")
                    .bind(c.editor.0)
                    .execute(&w.env.pool)
                    .await
                    .unwrap();
            }
            lock.commit().await.unwrap();
            let want = if demote { "failed" } else { "stored" };
            w.wait_for(id, want).await;
            if demote {
                // The session is marked failed first, the object is deleted right after.
                for _ in 0..500 {
                    if w.stored_objects() == 0 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert_eq!(w.stored_objects(), 0, "no object without a blob row");
                assert_eq!(w.count("SELECT count(*) FROM blobs").await, 0);
                assert_eq!(w.reserved().await, (0, 0));
                sqlx::query("UPDATE workspace_members SET role = 'editor' WHERE user_id = $1")
                    .bind(c.editor.0)
                    .execute(&w.env.pool)
                    .await
                    .unwrap();
            } else {
                // The upload completed on its own: object, blob and reservation agree.
                assert_eq!(w.stored_objects(), 1);
                assert_eq!(
                    w.count("SELECT count(*) FROM blobs WHERE state = 'staged'")
                        .await,
                    1
                );
                assert_eq!(w.reserved().await, (40_000, 40_000));
            }
            w.settled_no_temp().await;
        }
    }

    /// A second organization in which the editor of `cast` is a member and editor.
    async fn second_org_file(w: &W, c: &Cast) -> Uuid {
        let b_owner = w.user("bowner").await;
        let b_org = w.org("org-b", b_owner.0).await;
        w.org_member(&b_owner.1, b_org, c.editor.0, "member").await;
        let b_ws = w.ws(&b_owner.1, b_org, "b-main").await;
        w.ws_add(&b_owner.1, b_ws, c.editor.0, "editor").await;
        w.file_in(&b_owner.1, b_ws, "b.fig").await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn idempotency_keys_and_the_per_user_cap_hold_across_organizations() {
        let mut s = settings();
        s.uploads_per_user = 3;
        let w = W::from(TestEnv::with(s, craftcloud_api::ApiConfig::default()).await).await;
        let c = cast(&w).await;
        let b_file = second_org_file(&w, &c).await;
        let data = pattern(50, 24);
        let sha = sha_hex(&data);
        // The same key in another organization is a different request.
        let first = w
            .create_raw(&c.editor.1, c.file, 50, &sha, Some("shared-key"))
            .await;
        assert_eq!(first.status, StatusCode::CREATED, "{}", first.text);
        assert_status(
            &w.create_raw(&c.editor.1, b_file, 50, &sha, Some("shared-key"))
                .await,
            StatusCode::CONFLICT,
            "IDEMPOTENCY_KEY_REUSED",
        );
        // Concurrent first uses of one key in two organizations: one wins, the other is a conflict, never a 500.
        let mut handles = Vec::new();
        for (i, file) in [c.file, b_file].into_iter().enumerate() {
            let (app, session, sha) = (w.env.app.clone(), c.editor.1.clone(), sha.clone());
            handles.push(tokio::spawn(async move {
                let _ = i;
                call(
                    &app,
                    Req::new("POST", &format!("/api/v1/files/{file}/uploads"))
                        .session(&session)
                        .header("Idempotency-Key", "race-key")
                        .json(json!({"size_bytes": 50, "sha256": sha})),
                )
                .await
                .status
            }));
        }
        let mut statuses = Vec::new();
        for h in handles {
            statuses.push(h.await.unwrap());
        }
        statuses.sort_by_key(|s| s.as_u16());
        assert_eq!(statuses, [StatusCode::CREATED, StatusCode::CONFLICT]);

        // The per-user cap counts across organizations, also under parallel creation.
        let before = w
            .count("SELECT count(*) FROM upload_sessions WHERE state = 'created'")
            .await;
        assert_eq!(before, 2);
        let mut handles = Vec::new();
        for i in 0..8 {
            let file = if i % 2 == 0 { c.file } else { b_file };
            let (app, session, sha) = (w.env.app.clone(), c.editor.1.clone(), sha.clone());
            handles.push(tokio::spawn(async move {
                call(
                    &app,
                    Req::new("POST", &format!("/api/v1/files/{file}/uploads"))
                        .session(&session)
                        .json(json!({"size_bytes": 50, "sha256": sha})),
                )
                .await
                .status
            }));
        }
        let mut created = 0;
        for h in handles {
            match h.await.unwrap() {
                StatusCode::CREATED => created += 1,
                StatusCode::CONFLICT => {}
                other => panic!("unexpected {other}"),
            }
        }
        assert_eq!(created, 1, "3 open uploads in total for this user");
        assert_eq!(
            w.count("SELECT count(*) FROM upload_sessions WHERE state = 'created'")
                .await,
            3
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_effective_workspace_limit_is_the_smaller_of_workspace_and_organization_quota() {
        let w = W::new().await;
        let c = cast(&w).await;
        w.set_org_quota(c.org, Some(1000)).await;
        assert_eq!(
            w.ws_quota(c.ws, Some(800), &c.owner.1).await.status,
            StatusCode::OK
        );
        // Lowering the organization quota leaves the workspace quota alone ...
        w.set_org_quota(c.org, Some(500)).await;
        let usage = w
            .get(
                &c.owner.1,
                &format!("/api/v1/organizations/{}/usage", c.org),
            )
            .await;
        let entry = &usage.body["workspaces"][0];
        assert_eq!(entry["quota_bytes"], 800);
        // ... but the effective limit is the smaller one, and creation obeys it.
        assert_eq!(entry["effective_limit_bytes"], 500);
        assert_eq!(
            w.create(&c.editor.1, c.file, &pattern(500, 1)).await.status,
            StatusCode::CREATED
        );
        assert_status(
            &w.create(&c.editor.1, c.file, &pattern(1, 2)).await,
            StatusCode::PAYLOAD_TOO_LARGE,
            "QUOTA_EXCEEDED",
        );
        // Without an organization quota the workspace quota is the limit; without both there is none.
        w.set_org_quota(c.org, None).await;
        let usage = w
            .get(
                &c.owner.1,
                &format!("/api/v1/organizations/{}/usage", c.org),
            )
            .await;
        assert_eq!(usage.body["workspaces"][0]["effective_limit_bytes"], 800);
        assert_eq!(
            w.ws_quota(c.ws, None, &c.owner.1).await.status,
            StatusCode::OK
        );
        let usage = w
            .get(
                &c.owner.1,
                &format!("/api/v1/organizations/{}/usage", c.org),
            )
            .await;
        assert!(usage.body["workspaces"][0]["effective_limit_bytes"].is_null());
    }

    // ---- large ------------------------------------------------------------------------------------------------------------

    /// Streams `mib` MiB through the content route without holding it in memory.
    async fn stream_upload(mib: u64) {
        let mut s = settings();
        s.max_file_size_bytes = 2 * 1024 * 1024 * 1024;
        let w = W::from(TestEnv::with(s, craftcloud_api::ApiConfig::default()).await).await;
        let c = cast(&w).await;
        let block = Bytes::from(pattern(1024 * 1024, 18));
        let mut hasher = Sha256::new();
        for _ in 0..mib {
            hasher.update(&block);
        }
        let sha: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let size = mib * 1024 * 1024;
        let created = w.create_raw(&c.editor.1, c.file, size, &sha, None).await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        let id = id_of_key(&created, "upload_id");
        let started = std::time::Instant::now();
        let r = w
            .send(
                Req::new("PUT", &format!("/api/v1/uploads/{id}/content"))
                    .session(&c.editor.1)
                    .body(Body::new(PatternBody { block, frames: mib })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        eprintln!(
            "streamed {mib} MiB over the content route in {:?}",
            started.elapsed()
        );
        assert_eq!(w.stored_objects(), 1);
        let stored: i64 = w.count("SELECT size_bytes FROM blobs").await;
        assert_eq!(stored as u64, size);
        assert_eq!(w.temp_files(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_64_mib_upload_streams_over_http_with_bounded_buffers() {
        stream_upload(64).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "large: streams 1 GiB; run manually with a database (CC_TEST_DATABASE_URL)"]
    async fn large_one_gibibyte_upload_over_http() {
        // `cargo xtask db-test` runs ignored tests; this one only on request.
        if std::env::var_os("CC_LARGE_TESTS").is_none() {
            eprintln!("skipped: set CC_LARGE_TESTS=1 to stream 1 GiB");
            return;
        }
        stream_upload(1024).await;
    }

    // ---- account disable and session revoke during the upload (SR-02) ----------------------------

    /// Starts a PUT, runs `interfere`, then sends the rest of the body; the upload must end
    /// unauthenticated with nothing stored and the reservation released.
    async fn assert_nothing_stored_after(
        w: &W,
        who: &Session,
        upload: Uuid,
        data: &[u8],
        interfere: impl std::future::Future<Output = ()>,
    ) {
        let (tx, put) = start_put(w, who, upload);
        tx.send(Bytes::from(data[..data.len() / 2].to_vec()))
            .await
            .unwrap();
        w.wait_for(upload, "receiving").await;
        interfere.await;
        tx.send(Bytes::from(data[data.len() / 2..].to_vec()))
            .await
            .unwrap();
        drop(tx);
        assert_status(
            &put.await.unwrap(),
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
        );
        assert_eq!(w.state(upload).await, "failed");
        assert_eq!(
            w.stored_objects(),
            0,
            "the published object is deleted again"
        );
        assert_eq!(w.count("SELECT count(*) FROM blobs").await, 0);
        assert_eq!(w.reserved().await, (0, 0));
        w.settled_no_temp().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn an_account_disabled_during_the_upload_stores_nothing() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(120_000, 21);
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        let members = w
            .count("SELECT count(*) FROM organization_members WHERE status = 'active'")
            .await;
        let disable = async {
            let r = w
                .send(
                    Req::new(
                        "POST",
                        &format!("/api/v1/admin/users/{}/disable", c.editor.0),
                    )
                    .session(&w.op),
                )
                .await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        };
        assert_nothing_stored_after(&w, &c.editor.1, id, &data, disable).await;
        // The rejection is recorded as an aggregated denied event of the actor.
        let mut denied = 0;
        for _ in 0..200 {
            denied = w
                .count(
                    "SELECT COALESCE(sum(event_count), 0)::bigint FROM audit_events                      WHERE action = 'upload.content' AND result = 'denied'",
                )
                .await;
            if denied > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(denied, 1);
        // The membership roles are untouched: only the account state stopped the upload.
        assert_eq!(
            w.count("SELECT count(*) FROM organization_members WHERE status = 'active'")
                .await,
            members
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_session_revoked_during_the_upload_stores_nothing() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(120_000, 22);
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        let logout = async {
            sqlx::query("UPDATE organizations SET require_two_factor=true WHERE id=$1")
                .bind(c.org)
                .execute(&w.env.pool)
                .await
                .unwrap();
            let r = w
                .send(Req::new("POST", "/api/v1/auth/logout").session(&c.editor.1))
                .await;
            assert!(r.status.is_success(), "{}", r.text);
        };
        assert_nothing_stored_after(&w, &c.editor.1, id, &data, logout).await;
        // Another login of the same user is unaffected and can upload.
        sqlx::query("UPDATE organizations SET require_two_factor=false WHERE id=$1")
            .bind(c.org)
            .execute(&w.env.pool)
            .await
            .unwrap();
        let again = w.relogin("editor").await;
        let fresh = pattern(2000, 23);
        let next = w.upload_id(&again, c.file, &fresh).await;
        assert_eq!(w.put(&again, next, fresh).await.status, StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_session_that_expires_absolutely_during_the_upload_stores_nothing() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(120_000, 24);
        let id = w.upload_id(&c.editor.1, c.file, &data).await;
        // The login ends (8 h absolute) well before the 24 h upload session.
        let expire = async {
            sqlx::query(
                "UPDATE sessions SET created_at = created_at - interval '2 hours', \n                 absolute_expires_at = created_at - interval '1 hour'",
            )
                .execute(&w.env.pool)
                .await
                .unwrap();
        };
        assert_nothing_stored_after(&w, &c.editor.1, id, &data, expire).await;
    }

    // ---- no existence oracle through the storage state (SR-03) ----------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn missing_storage_does_not_reveal_foreign_uploads() {
        let w = W::from(TestEnv::without_storage().await).await;
        let c = cast(&w).await;
        // A session row of this organization (creating one through the API needs storage).
        let project: Uuid = sqlx::query_scalar("SELECT project_id FROM files WHERE id = $1")
            .bind(c.file)
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        let foreign = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO upload_sessions (id, org_id, workspace_id, project_id, file_id,                  created_by_user_id, declared_size, declared_sha256, request_fingerprint,                  expires_at, created_at, updated_at)              VALUES ($1, $2, $3, $4, $5, $6, 1, $7, 'x', now() + interval '1 day', now(), now())",
        )
        .bind(foreign)
        .bind(c.org)
        .bind(c.ws)
        .bind(project)
        .bind(c.file)
        .bind(c.editor.0)
        .bind(vec![0_u8; 32])
        .execute(&w.env.pool)
        .await
        .unwrap();
        let unknown = Uuid::new_v4();
        let a = w.put(&c.outsider.1, foreign, vec![1]).await;
        let b = w.put(&c.outsider.1, unknown, vec![1]).await;
        hidden(&a);
        hidden(&b);
        assert_eq!(a.body["error"]["message"], b.body["error"]["message"]);
        // Authorized callers still learn that storage is missing.
        assert_status(
            &w.put(&c.editor.1, foreign, vec![1]).await,
            StatusCode::SERVICE_UNAVAILABLE,
            "STORAGE_UNAVAILABLE",
        );
        assert_status(
            &w.put(&c.viewer.1, foreign, vec![1]).await,
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
        );
    }
}
