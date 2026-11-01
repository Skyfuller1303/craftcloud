//! HTTP-level tests of file versions, compare-and-set commits, restores and
//! downloads (CC-F08): two writers on one base, preconditions, idempotent
//! retries, revoked editors, quota counters, tenant isolation, download
//! headers, ranges and integrity. All tests need PostgreSQL
//! (`cargo xtask db-test`).

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::StatusCode;
    use base64::Engine as _;
    use bytes::Bytes;
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    use crate::common::{Req, Resp, Session, TestEnv, call, call_stream};

    // ---- helpers ---------------------------------------------------------------------

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

    fn id_of(r: &Resp) -> Uuid {
        Uuid::parse_str(r.body["id"].as_str().unwrap()).unwrap()
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

    fn tag(id: Uuid) -> String {
        format!("\"version:{id}\"")
    }

    struct W {
        env: TestEnv,
        op: Session,
    }

    impl W {
        async fn new() -> Self {
            let env = TestEnv::new().await;
            let op = env.login_ok("op").await;
            Self { env, op }
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
            self.file_of_app(actor, project, name, "figcraft").await
        }

        /// A file of an app whose files support edit leases (FigCraft files do not).
        async fn file_of_app(&self, actor: &Session, project: Uuid, name: &str, app: &str) -> Uuid {
            let r = self
                .json(
                    "POST",
                    &format!("/api/v1/projects/{project}/files"),
                    actor,
                    json!({"name": name, "app": app, "format": "fig"}),
                )
                .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            id_of(&r)
        }

        /// Creates an upload, sends the content, returns the stored upload id.
        async fn stored_upload(&self, who: &Session, file: Uuid, data: &[u8]) -> Uuid {
            let created = self
                .json(
                    "POST",
                    &format!("/api/v1/files/{file}/uploads"),
                    who,
                    json!({"size_bytes": data.len(), "sha256": sha_hex(data)}),
                )
                .await;
            assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
            let id = Uuid::parse_str(created.body["upload_id"].as_str().unwrap()).unwrap();
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

        /// POST /files/{file}/versions with the given precondition header.
        async fn commit(
            &self,
            who: &Session,
            file: Uuid,
            upload: Uuid,
            header: Option<(&str, String)>,
        ) -> Resp {
            let mut req = Req::new("POST", &format!("/api/v1/files/{file}/versions"))
                .session(who)
                .json(json!({"upload_id": upload.to_string()}));
            if let Some((name, value)) = header {
                req = req.header(name, &value);
            }
            self.send(req).await
        }

        async fn first_commit(&self, who: &Session, file: Uuid, upload: Uuid) -> Resp {
            self.commit(who, file, upload, Some(("If-None-Match", "*".to_owned())))
                .await
        }

        async fn next_commit(&self, who: &Session, file: Uuid, upload: Uuid, head: Uuid) -> Resp {
            self.commit(who, file, upload, Some(("If-Match", tag(head))))
                .await
        }

        /// Uploads and commits `data` as the next version; returns the version id.
        async fn push(&self, who: &Session, file: Uuid, data: &[u8]) -> Uuid {
            let head = self.head(who, file).await;
            let upload = self.stored_upload(who, file, data).await;
            let r = match head {
                None => self.first_commit(who, file, upload).await,
                Some(head) => self.next_commit(who, file, upload, head).await,
            };
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            id_of(&r)
        }

        async fn head(&self, who: &Session, file: Uuid) -> Option<Uuid> {
            let r = self.get(who, &format!("/api/v1/files/{file}")).await;
            r.body["head_version_id"]
                .as_str()
                .map(|s| Uuid::parse_str(s).unwrap())
        }

        async fn download(&self, who: &Session, path: &str, range: Option<&str>) -> Resp {
            let mut req = Req::new("GET", path).cookie_only(who);
            if let Some(range) = range {
                req = req.header("Range", range);
            }
            self.send(req).await
        }

        async fn count(&self, sql: &str) -> i64 {
            self.env.count(sql).await
        }

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

        fn object_path(&self, key: &str) -> PathBuf {
            self.env
                .storage_dir
                .join("objects")
                .join(&key[0..2])
                .join(&key[2..4])
                .join(key)
        }
    }

    async fn archive_version(
        w: &W,
        version_id: Uuid,
        archive: &craftcloud_storage::ArchiveStore,
        crypto: &craftcloud_storage::ArchiveCrypto,
    ) -> (Uuid, String, String, Uuid) {
        use craftcloud_storage::{LocalStore, ObjectKey, ObjectStore};

        let (blob_id, local_key, size, sha256, quota_group_id): (Uuid, String, i64, Vec<u8>, Uuid) =
            sqlx::query_as(
                "SELECT b.id, b.object_key, b.size_bytes, b.sha256, b.quota_group_id \
                 FROM file_versions v JOIN blobs b ON b.id=v.blob_id WHERE v.id=$1",
            )
            .bind(version_id)
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        let local = LocalStore::open(&w.env.storage_dir).await.unwrap();
        let local_key = ObjectKey::parse(&local_key).unwrap();
        let digest: [u8; 32] = sha256.as_slice().try_into().unwrap();
        let copy = craftcloud_api::archive_transport::upload_verified(
            &local,
            archive,
            crypto,
            blob_id,
            &local_key,
            size as u64,
            &digest,
        )
        .await
        .unwrap();
        sqlx::query(
            "UPDATE blobs SET tier_state='archived', archive_object_key=$2, archive_wrapped_key=$3 \
             WHERE id=$1",
        )
        .bind(blob_id)
        .bind(copy.object_key.as_str())
        .bind(copy.envelope.to_bytes().as_slice())
        .execute(&w.env.pool)
        .await
        .unwrap();
        local.delete(&local_key).await.unwrap();
        (
            blob_id,
            local_key.to_string(),
            copy.object_key.to_string(),
            quota_group_id,
        )
    }

    struct Cast {
        org: Uuid,
        ws: Uuid,
        project: Uuid,
        file: Uuid,
        owner: (Uuid, Session),
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
        let (wsadmin, editor, editor2, viewer, plain) = (
            w.user("wsadmin").await,
            w.user("editor").await,
            w.user("editor2").await,
            w.user("viewer").await,
            w.user("plain").await,
        );
        for p in [&wsadmin, &editor, &editor2, &viewer, &plain] {
            w.org_member(&owner.1, org, p.0).await;
        }
        let outsider = w.user("outsider").await;
        w.org("elsewhere", outsider.0).await;
        let ws = w.ws(&owner.1, org, "main").await;
        w.ws_add(&owner.1, ws, wsadmin.0, "admin").await;
        w.ws_add(&owner.1, ws, editor.0, "editor").await;
        w.ws_add(&owner.1, ws, editor2.0, "editor").await;
        w.ws_add(&owner.1, ws, viewer.0, "viewer").await;
        let project = w.project(&owner.1, ws, "Design").await;
        let file = w.file(&owner.1, project, "design.fig").await;
        Cast {
            org,
            ws,
            project,
            file,
            owner,
            wsadmin,
            editor,
            editor2,
            viewer,
            plain,
            outsider,
        }
    }

    // ---- commits and the two-writer race ---------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn two_writers_on_one_base_yield_one_version_and_one_412_and_the_loser_can_save_a_copy() {
        let w = W::new().await;
        let c = cast(&w).await;
        let url = format!("/api/v1/files/{}/versions", c.file);

        // The first version needs If-None-Match: *.
        let first_data = pattern(1000, 1);
        let up1 = w.stored_upload(&c.editor.1, c.file, &first_data).await;
        assert_status(
            &w.commit(&c.editor.1, c.file, up1, None).await,
            StatusCode::PRECONDITION_REQUIRED,
            "PRECONDITION_REQUIRED",
        );
        // Both headers, or a header that is not a precondition, are errors.
        let both = w
            .send(
                Req::new("POST", &url)
                    .session(&c.editor.1)
                    .header("If-Match", &tag(Uuid::new_v4()))
                    .header("If-None-Match", "*")
                    .json(json!({"upload_id": up1.to_string()})),
            )
            .await;
        assert_eq!(both.status, StatusCode::UNPROCESSABLE_ENTITY);
        let weird = w
            .commit(
                &c.editor.1,
                c.file,
                up1,
                Some(("If-None-Match", "\"abc\"".to_owned())),
            )
            .await;
        assert_eq!(weird.status, StatusCode::UNPROCESSABLE_ENTITY);
        // If-Match on a file without version never matches.
        let no_head = w
            .next_commit(&c.editor.1, c.file, up1, Uuid::new_v4())
            .await;
        assert_status(
            &no_head,
            StatusCode::PRECONDITION_FAILED,
            "VERSION_CONFLICT",
        );
        assert!(no_head.body["error"]["details"]["current_version_id"].is_null());

        let v1 = w.first_commit(&c.editor.1, c.file, up1).await;
        assert_eq!(v1.status, StatusCode::CREATED, "{}", v1.text);
        let v1_id = id_of(&v1);
        assert_eq!(v1.body["number"], 1);
        assert_eq!(v1.body["sha256"], sha_hex(&first_data));
        assert!(v1.body["base_version_id"].is_null());
        assert_eq!(v1.etag(), tag(v1_id));

        // The file metadata now shows the head; the metadata ETag moved on (file-v1 -> file-v2).
        let file = w
            .get(&c.editor.1, &format!("/api/v1/files/{}", c.file))
            .await;
        assert_eq!(file.body["head_version_id"], v1_id.to_string());
        assert_eq!(file.body["size_bytes"], 1000);
        assert_eq!(file.body["sha256"], sha_hex(&first_data));
        assert_eq!(file.etag(), "\"file-v2\"");

        // A second writer who still thinks there is no version loses.
        let up_b = w
            .stored_upload(&c.editor2.1, c.file, &pattern(500, 2))
            .await;
        let lost = w.first_commit(&c.editor2.1, c.file, up_b).await;
        assert_status(&lost, StatusCode::PRECONDITION_FAILED, "VERSION_CONFLICT");
        assert_eq!(
            lost.body["error"]["details"]["current_version_id"],
            v1_id.to_string()
        );
        assert_eq!(lost.etag(), tag(v1_id));

        // Two writers on the same base v1: exactly one 201, one 412.
        let up_c = w.stored_upload(&c.editor.1, c.file, &pattern(600, 3)).await;
        let up_d = w
            .stored_upload(&c.editor2.1, c.file, &pattern(700, 4))
            .await;
        let c_commit = w.next_commit(&c.editor.1, c.file, up_c, v1_id).await;
        assert_eq!(c_commit.status, StatusCode::CREATED, "{}", c_commit.text);
        assert_eq!(c_commit.body["number"], 2);
        assert_eq!(c_commit.body["base_version_id"], v1_id.to_string());
        let v2_id = id_of(&c_commit);
        let d_commit = w.next_commit(&c.editor2.1, c.file, up_d, v1_id).await;
        assert_status(
            &d_commit,
            StatusCode::PRECONDITION_FAILED,
            "VERSION_CONFLICT",
        );
        assert_eq!(
            d_commit.body["error"]["details"]["current_version_id"],
            v2_id.to_string()
        );

        // The loser's upload is still stored and keeps its reservation ...
        let state: String = sqlx::query_scalar("SELECT state FROM upload_sessions WHERE id = $1")
            .bind(up_d)
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(state, "stored");
        // ... and becomes a new file in the same workspace without uploading again.
        let copy = w.file(&c.editor2.1, c.project, "design (copy).fig").await;
        hidden(&w.first_commit(&c.editor2.1, copy, up_d).await);
        let copy_upload = w.stored_upload(&c.editor2.1, copy, &pattern(700, 24)).await;
        let saved = w.first_commit(&c.editor2.1, copy, copy_upload).await;
        assert_eq!(saved.status, StatusCode::CREATED, "{}", saved.text);
        assert_eq!(saved.body["file_id"], copy.to_string());
        assert_eq!(saved.body["size_bytes"], 700);
        // The loser's upload cannot be moved to another file, so it stays unused.
        let untouched: String =
            sqlx::query_scalar("SELECT state FROM upload_sessions WHERE id = $1")
                .bind(up_d)
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
        assert_eq!(
            untouched, "stored",
            "the cross-file attempt changed nothing"
        );

        // Rebased on the new head the other lost upload goes through.
        let rebased = w.next_commit(&c.editor2.1, c.file, up_b, v2_id).await;
        assert_eq!(rebased.status, StatusCode::CREATED, "{}", rebased.text);
        assert_eq!(rebased.body["number"], 3);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn lease_blocks_version_commit_and_restore_but_allows_reads() {
        let w = W::new().await;
        let c = cast(&w).await;
        let file = w
            .file_of_app(&c.owner.1, c.project, "lease.pcf", "photocraft")
            .await;
        let v1 = w.push(&c.editor.1, file, &pattern(128, 1)).await;
        let v2 = w.push(&c.editor.1, file, &pattern(256, 2)).await;
        let upload = w.stored_upload(&c.editor.1, file, &pattern(512, 3)).await;
        let lease = w
            .send(
                Req::new("POST", &format!("/api/v1/files/{}/lock", file))
                    .session(&c.editor2.1)
                    .json(json!({"client_id":"version-owner"})),
            )
            .await;
        assert_eq!(lease.status, StatusCode::OK, "{}", lease.text);
        let commit = w.next_commit(&c.editor.1, file, upload, v2).await;
        assert_status(&commit, StatusCode::LOCKED, "FILE_LOCKED");
        let restore = w
            .send(
                Req::new(
                    "POST",
                    &format!("/api/v1/files/{}/versions/{v1}/restore", file),
                )
                .session(&c.editor.1)
                .header("If-Match", &tag(v2)),
            )
            .await;
        assert_status(&restore, StatusCode::LOCKED, "FILE_LOCKED");
        assert_eq!(
            w.get(&c.editor.1, &format!("/api/v1/files/{}/versions", file))
                .await
                .status,
            StatusCode::OK
        );
        assert_eq!(
            w.download(
                &c.editor.1,
                &format!("/api/v1/files/{}/content", file),
                None
            )
            .await
            .status,
            StatusCode::OK
        );
        assert_eq!(
            w.delete(&c.editor2.1, &format!("/api/v1/files/{}/lock", file))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        let restored = w
            .send(
                Req::new(
                    "POST",
                    &format!("/api/v1/files/{}/versions/{v1}/restore", file),
                )
                .session(&c.editor.1)
                .header("If-Match", &tag(v2)),
            )
            .await;
        assert_eq!(restored.status, StatusCode::CREATED, "{}", restored.text);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn file_editor_grant_can_commit_but_revocation_before_commit_is_rechecked() {
        let w = W::new().await;
        let c = cast(&w).await;
        let grant = w
            .json(
                "POST",
                &format!("/api/v1/files/{}/grants", c.file),
                &c.owner.1,
                json!({"user_id":c.plain.0.to_string(),"role":"editor"}),
            )
            .await;
        assert_eq!(grant.status, StatusCode::CREATED, "{}", grant.text);
        let data = pattern(256, 73);
        let upload = w.stored_upload(&c.plain.1, c.file, &data).await;
        let committed = w.first_commit(&c.plain.1, c.file, upload).await;
        assert_eq!(committed.status, StatusCode::CREATED, "{}", committed.text);

        let pending = w.stored_upload(&c.plain.1, c.file, &data).await;
        let grant_id = id_of(&grant);
        assert_eq!(
            w.delete(&c.owner.1, &format!("/api/v1/grants/{grant_id}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        hidden(&w.first_commit(&c.plain.1, c.file, pending).await);
        let state: String = sqlx::query_scalar("SELECT state FROM upload_sessions WHERE id=$1")
            .bind(pending)
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(
            state, "stored",
            "revoked grant leaves upload recoverable but commits no version"
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn an_upload_is_bound_to_its_file_even_with_a_foreign_target_lease() {
        let w = W::new().await;
        let c = cast(&w).await;
        let other_file = w
            .file_of_app(&c.editor.1, c.project, "other.fig", "photocraft")
            .await;
        let upload = w
            .stored_upload(&c.editor.1, c.file, &pattern(128, 42))
            .await;
        let lease = w
            .send(
                Req::new("POST", &format!("/api/v1/files/{other_file}/lock"))
                    .session(&c.editor2.1)
                    .json(json!({"client_id":"editor-two"})),
            )
            .await;
        assert_eq!(lease.status, StatusCode::OK, "{}", lease.text);
        forbidden(&w.first_commit(&c.editor2.1, c.file, upload).await);
        let untouched: String = sqlx::query_scalar("SELECT state FROM upload_sessions WHERE id=$1")
            .bind(upload)
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(
            untouched, "stored",
            "another editor cannot commit the creator's upload"
        );
        hidden(&w.first_commit(&c.editor.1, other_file, upload).await);
        let state: String = sqlx::query_scalar("SELECT state FROM upload_sessions WHERE id=$1")
            .bind(upload)
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(state, "stored", "cross-file attempt changes nothing");
        let committed = w.first_commit(&c.editor.1, c.file, upload).await;
        assert_eq!(committed.status, StatusCode::CREATED, "{}", committed.text);
        assert_eq!(committed.body["file_id"], c.file.to_string());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn parallel_commits_on_the_same_base_have_exactly_one_winner() {
        let w = W::new().await;
        let c = cast(&w).await;
        let base = w.push(&c.editor.1, c.file, &pattern(100, 1)).await;
        let mut uploads = Vec::new();
        for i in 0..6_u8 {
            uploads.push(
                w.stored_upload(&c.editor.1, c.file, &pattern(200, i + 10))
                    .await,
            );
        }
        let mut handles = Vec::new();
        for upload in uploads {
            let (app, session, file) = (w.env.app.clone(), c.editor.1.clone(), c.file);
            handles.push(tokio::spawn(async move {
                call(
                    &app,
                    Req::new("POST", &format!("/api/v1/files/{file}/versions"))
                        .session(&session)
                        .header("If-Match", &tag(base))
                        .json(json!({"upload_id": upload.to_string()})),
                )
                .await
                .status
            }));
        }
        let mut created = 0;
        let mut conflicts = 0;
        for h in handles {
            match h.await.unwrap() {
                StatusCode::CREATED => created += 1,
                StatusCode::PRECONDITION_FAILED => conflicts += 1,
                other => panic!("unexpected {other}"),
            }
        }
        assert_eq!((created, conflicts), (1, 5));
        assert_eq!(w.count("SELECT count(*) FROM file_versions").await, 2);
        // Only the winner moved its bytes to committed; the five losers still hold reservations.
        let (org_reserved, org_committed, _, ws_committed) = w.counters().await;
        assert_eq!(
            (org_reserved, org_committed, ws_committed),
            (1000, 300, 300)
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_retried_commit_returns_the_same_version_and_counts_bytes_once() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(4000, 5);
        let upload = w.stored_upload(&c.editor.1, c.file, &data).await;
        assert_eq!(w.counters().await, (4000, 0, 4000, 0));
        let first = w.first_commit(&c.editor.1, c.file, upload).await;
        assert_eq!(first.status, StatusCode::CREATED);
        assert_eq!(
            w.counters().await,
            (0, 4000, 0, 4000),
            "reserved moved to committed"
        );
        // The answer got lost: the client retries with the same, with no, or with outdated preconditions.
        for header in [
            Some(("If-None-Match", "*".to_owned())),
            None,
            Some(("If-Match", tag(Uuid::new_v4()))),
        ] {
            let again = w.commit(&c.editor.1, c.file, upload, header).await;
            assert_eq!(again.status, StatusCode::OK, "{}", again.text);
            assert_eq!(again.body["id"], first.body["id"]);
        }
        assert_eq!(w.counters().await, (0, 4000, 0, 4000), "counted once");
        assert_eq!(w.count("SELECT count(*) FROM file_versions").await, 1);
        // Somebody else cannot replay it, a wrong Idempotency-Key is refused.
        forbidden(
            &w.commit(
                &c.editor2.1,
                c.file,
                upload,
                Some(("If-None-Match", "*".to_owned())),
            )
            .await,
        );
        hidden(
            &w.commit(
                &c.plain.1,
                c.file,
                upload,
                Some(("If-None-Match", "*".to_owned())),
            )
            .await,
        );
        let keyed = w
            .send(
                Req::new("POST", &format!("/api/v1/files/{}/versions", c.file))
                    .session(&c.editor.1)
                    .header("If-None-Match", "*")
                    .header("Idempotency-Key", "not-the-key")
                    .json(json!({"upload_id": upload.to_string()})),
            )
            .await;
        assert_status(&keyed, StatusCode::CONFLICT, "IDEMPOTENCY_KEY_REUSED");
        // With the key the upload was created with, the replay works.
        let created = w
            .send(
                Req::new("POST", &format!("/api/v1/files/{}/uploads", c.file))
                    .session(&c.editor.1)
                    .header("Idempotency-Key", "k-1")
                    .json(json!({"size_bytes": 3, "sha256": sha_hex(b"abc")})),
            )
            .await;
        let up = Uuid::parse_str(created.body["upload_id"].as_str().unwrap()).unwrap();
        assert_eq!(
            w.send(
                Req::new("PUT", &format!("/api/v1/uploads/{up}/content"))
                    .session(&c.editor.1)
                    .body(Body::from(b"abc".to_vec()))
            )
            .await
            .status,
            StatusCode::OK
        );
        let head = w.head(&c.editor.1, c.file).await.unwrap();
        let ok = w
            .send(
                Req::new("POST", &format!("/api/v1/files/{}/versions", c.file))
                    .session(&c.editor.1)
                    .header("If-Match", &tag(head))
                    .header("Idempotency-Key", "k-1")
                    .json(json!({"upload_id": up.to_string()})),
            )
            .await;
        assert_eq!(ok.status, StatusCode::CREATED, "{}", ok.text);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn quota_counters_follow_commit_restore_and_recompute() {
        let w = W::new().await;
        let c = cast(&w).await;
        let v1 = w.push(&c.editor.1, c.file, &pattern(1000, 1)).await;
        let _v2 = w.push(&c.editor.1, c.file, &pattern(2500, 2)).await;
        assert_eq!(w.counters().await, (0, 3500, 0, 3500));
        // Restoring v1 creates a version without new bytes.
        let head = w.head(&c.editor.1, c.file).await.unwrap();
        let restored = w
            .send(
                Req::new(
                    "POST",
                    &format!("/api/v1/files/{}/versions/{v1}/restore", c.file),
                )
                .session(&c.editor.1)
                .header("If-Match", &tag(head)),
            )
            .await;
        assert_eq!(restored.status, StatusCode::CREATED, "{}", restored.text);
        assert_eq!(restored.body["restored_from_version_id"], v1.to_string());
        assert_eq!(restored.body["number"], 3);
        assert_eq!(w.counters().await, (0, 3500, 0, 3500));
        assert_eq!(
            w.count("SELECT count(*) FROM blobs").await,
            2,
            "the restore shares the blob"
        );
        // Reconciliation recomputes the same numbers from the rows.
        sqlx::query(
            "UPDATE organizations SET committed_bytes = 1, reserved_bytes = 9 WHERE id = $1",
        )
        .bind(c.org)
        .execute(&w.env.pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE workspaces SET committed_bytes = 2, reserved_bytes = 8 WHERE org_id = $1",
        )
        .bind(c.org)
        .execute(&w.env.pool)
        .await
        .unwrap();
        let mut tx = craftcloud_db::members::OrgTx::begin(&w.env.pool, c.org)
            .await
            .unwrap()
            .unwrap();
        tx.recompute_usage().await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(w.counters().await, (0, 3500, 0, 3500));
        // Usage endpoint.
        let usage = w
            .get(
                &c.owner.1,
                &format!("/api/v1/organizations/{}/usage", c.org),
            )
            .await;
        assert_eq!(usage.body["organization"]["committed_bytes"], 3500);
        assert_eq!(usage.body["organization"]["reserved_bytes"], 0);
        // A quota lowered after the reservation does not cancel it, and the commit still works.
        let up = w.stored_upload(&c.editor.1, c.file, &pattern(100, 3)).await;
        let put = w
            .json(
                "PUT",
                &format!("/api/v1/admin/organizations/{}/quota", c.org),
                &w.op,
                json!({"quota_bytes": 10}),
            )
            .await;
        assert_eq!(put.status, StatusCode::OK);
        let head = w.head(&c.editor.1, c.file).await.unwrap();
        assert_eq!(
            w.next_commit(&c.editor.1, c.file, up, head).await.status,
            StatusCode::CREATED
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn revoked_editors_and_deleted_files_cannot_commit_and_keep_a_releasable_upload() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(800, 6);
        // Editor demoted to viewer between upload and commit.
        let up = w.stored_upload(&c.editor.1, c.file, &data).await;
        assert_eq!(
            w.json(
                "PATCH",
                &format!("/api/v1/workspaces/{}/members/{}", c.ws, c.editor.0),
                &c.owner.1,
                json!({"role": "viewer"})
            )
            .await
            .status,
            StatusCode::OK
        );
        forbidden(&w.first_commit(&c.editor.1, c.file, up).await);
        // Nothing changed: the session is stored, the reservation stays, and the creator can still abort it.
        assert_eq!(w.counters().await, (800, 0, 800, 0));
        assert_eq!(w.count("SELECT count(*) FROM file_versions").await, 0);
        assert_eq!(
            w.delete(&c.editor.1, &format!("/api/v1/uploads/{up}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        // The published object is still on disk, so its bytes stay reserved until the garbage collection.
        assert_eq!(w.counters().await, (800, 0, 800, 0));
        // Removed from the workspace: the workspace is invisible (404).
        w.json(
            "PATCH",
            &format!("/api/v1/workspaces/{}/members/{}", c.ws, c.editor.0),
            &c.owner.1,
            json!({"role": "editor"}),
        )
        .await;
        let up = w.stored_upload(&c.editor.1, c.file, &data).await;
        assert_eq!(
            w.delete(
                &c.owner.1,
                &format!("/api/v1/workspaces/{}/members/{}", c.ws, c.editor.0)
            )
            .await
            .status,
            StatusCode::NO_CONTENT
        );
        hidden(&w.first_commit(&c.editor.1, c.file, up).await);

        // File deleted between upload and commit.
        let up2 = w.stored_upload(&c.editor2.1, c.file, &data).await;
        assert_eq!(
            w.delete(&c.owner.1, &format!("/api/v1/files/{}", c.file))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_status(
            &w.first_commit(&c.editor2.1, c.file, up2).await,
            StatusCode::CONFLICT,
            "FILE_DELETED",
        );
        assert_eq!(w.count("SELECT count(*) FROM file_versions").await, 0);
        // After the restore of the file the same upload commits.
        assert_eq!(
            w.send(
                Req::new("POST", &format!("/api/v1/files/{}/restore", c.file)).session(&c.owner.1)
            )
            .await
            .status,
            StatusCode::OK
        );
        assert_eq!(
            w.first_commit(&c.editor2.1, c.file, up2).await.status,
            StatusCode::CREATED
        );
        // Project deleted.
        let file2 = w.file(&c.owner.1, c.project, "second.fig").await;
        let up3 = w.stored_upload(&c.editor2.1, file2, &data).await;
        assert_eq!(
            w.delete(&c.owner.1, &format!("/api/v1/projects/{}", c.project))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_status(
            &w.first_commit(&c.editor2.1, file2, up3).await,
            StatusCode::CONFLICT,
            "PROJECT_DELETED",
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn only_stored_unexpired_uploads_of_the_same_workspace_can_be_committed() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(50, 7);
        // Created but not filled.
        let created = w
            .json(
                "POST",
                &format!("/api/v1/files/{}/uploads", c.file),
                &c.editor.1,
                json!({"size_bytes": 50, "sha256": sha_hex(&data)}),
            )
            .await;
        let open = Uuid::parse_str(created.body["upload_id"].as_str().unwrap()).unwrap();
        assert_status(
            &w.first_commit(&c.editor.1, c.file, open).await,
            StatusCode::CONFLICT,
            "UPLOAD_INCOMPLETE",
        );
        // Aborted.
        assert_eq!(
            w.delete(&c.editor.1, &format!("/api/v1/uploads/{open}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_status(
            &w.first_commit(&c.editor.1, c.file, open).await,
            StatusCode::CONFLICT,
            "UPLOAD_CLOSED",
        );
        // Unknown, malformed.
        hidden(&w.first_commit(&c.editor.1, c.file, Uuid::new_v4()).await);
        let bad = w
            .json(
                "POST",
                &format!("/api/v1/files/{}/versions", c.file),
                &c.editor.1,
                json!({"upload_id": "x"}),
            )
            .await;
        assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
        // An upload of another workspace of the same organization.
        let other_ws = w.ws(&c.owner.1, c.org, "other").await;
        w.ws_add(&c.owner.1, other_ws, c.editor.0, "editor").await;
        let other_project = w.project(&c.owner.1, other_ws, "Other").await;
        let other_file = w.file(&c.owner.1, other_project, "o.fig").await;
        let other_upload = w.stored_upload(&c.editor.1, other_file, &data).await;
        hidden(&w.first_commit(&c.editor.1, c.file, other_upload).await);
        // Expired (the session timeout is 24 h).
        let up = w.stored_upload(&c.editor.1, c.file, &data).await;
        let reserved_before = w.counters().await.0;
        w.env.clock.advance(Duration::from_secs(25 * 3600));
        let editor = w.env.login_ok("editor").await;
        let late = w.first_commit(&editor, c.file, up).await;
        assert_status(&late, StatusCode::GONE, "UPLOAD_EXPIRED");
        assert_eq!(
            w.counters().await.0,
            reserved_before,
            "an expired stored upload keeps counting until its blob is collected"
        );
    }

    // ---- reading versions ------------------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn versions_list_newest_first_with_stable_paging_and_clean_isolation() {
        let w = W::new().await;
        let c = cast(&w).await;
        let mut ids = Vec::new();
        for i in 0..5_u8 {
            ids.push(
                w.push(&c.editor.1, c.file, &pattern(100 + usize::from(i), i))
                    .await,
            );
        }
        let url = format!("/api/v1/files/{}/versions", c.file);
        // Viewers read, plain members and outsiders do not see the workspace.
        for who in [&c.viewer.1, &c.editor.1, &c.wsadmin.1, &c.owner.1] {
            assert_eq!(w.get(who, &url).await.status, StatusCode::OK);
        }
        hidden(&w.get(&c.plain.1, &url).await);
        hidden(&w.get(&c.outsider.1, &url).await);
        let first = w.get(&c.viewer.1, &format!("{url}?limit=2")).await;
        let numbers: Vec<i64> = first.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["number"].as_i64().unwrap())
            .collect();
        assert_eq!(numbers, [5, 4]);
        let cursor = first.body["next_cursor"].as_str().unwrap().to_owned();
        // A new version arriving between the pages does not disturb them.
        w.push(&c.editor.1, c.file, &pattern(10, 99)).await;
        let second = w
            .get(&c.viewer.1, &format!("{url}?limit=2&cursor={cursor}"))
            .await;
        let numbers: Vec<i64> = second.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["number"].as_i64().unwrap())
            .collect();
        assert_eq!(numbers, [3, 2]);
        let third = w
            .get(
                &c.viewer.1,
                &format!(
                    "{url}?limit=5&cursor={}",
                    second.body["next_cursor"].as_str().unwrap()
                ),
            )
            .await;
        assert_eq!(third.body["items"].as_array().unwrap().len(), 1);
        assert!(third.body["next_cursor"].is_null());
        // The cursor is bound to this file.
        let other = w.file(&c.owner.1, c.project, "other.fig").await;
        assert_status(
            &w.get(
                &c.owner.1,
                &format!("/api/v1/files/{other}/versions?cursor={cursor}"),
            )
            .await,
            StatusCode::BAD_REQUEST,
            "INVALID_CURSOR",
        );
        // Single versions.
        let one = w.get(&c.viewer.1, &format!("{url}/{}", ids[2])).await;
        assert_eq!(one.status, StatusCode::OK);
        assert_eq!(one.body["number"], 3);
        assert_eq!(one.etag(), tag(ids[2]));
        hidden(&w.get(&c.plain.1, &format!("{url}/{}", ids[2])).await);
        hidden(
            &w.get(&c.viewer.1, &format!("{url}/{}", Uuid::new_v4()))
                .await,
        );
        hidden(&w.get(&c.viewer.1, &format!("{url}/not-a-uuid")).await);
        // A version of another file is not found under this one.
        hidden(
            &w.get(
                &c.owner.1,
                &format!("/api/v1/files/{other}/versions/{}", ids[2]),
            )
            .await,
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn versions_of_another_organization_are_invisible_and_cannot_be_committed_into() {
        let w = W::new().await;
        let c = cast(&w).await;
        let b_owner = w.user("bowner").await;
        let b_org = w.org("org-b", b_owner.0).await;
        let b_ws = w.ws(&b_owner.1, b_org, "b-main").await;
        let b_project = w.project(&b_owner.1, b_ws, "B").await;
        let b_file = w.file(&b_owner.1, b_project, "b.fig").await;
        let a_version = w.push(&c.editor.1, c.file, &pattern(64, 1)).await;
        let b_version = w.push(&b_owner.1, b_file, &pattern(64, 2)).await;
        let a_upload = w.stored_upload(&c.editor.1, c.file, &pattern(64, 3)).await;
        let unknown = Uuid::new_v4();
        let baseline = w
            .get(
                &b_owner.1,
                &format!("/api/v1/files/{}/versions/{unknown}", c.file),
            )
            .await;
        hidden(&baseline);
        for path in [
            format!("/api/v1/files/{}/versions", c.file),
            format!("/api/v1/files/{}/versions/{a_version}", c.file),
            format!("/api/v1/files/{}/content", c.file),
            format!("/api/v1/files/{}/versions/{a_version}/content", c.file),
        ] {
            let r = w.get(&b_owner.1, &path).await;
            hidden(&r);
            assert_eq!(
                r.body["error"]["message"],
                baseline.body["error"]["message"]
            );
        }
        hidden(
            &w.get(
                &c.owner.1,
                &format!("/api/v1/files/{b_file}/versions/{b_version}"),
            )
            .await,
        );
        // Restoring and committing across tenants.
        hidden(
            &w.send(
                Req::new(
                    "POST",
                    &format!("/api/v1/files/{}/versions/{a_version}/restore", c.file),
                )
                .session(&b_owner.1)
                .header("If-Match", &tag(a_version)),
            )
            .await,
        );
        hidden(&w.first_commit(&b_owner.1, c.file, a_upload).await);
        hidden(&w.first_commit(&b_owner.1, b_file, a_upload).await);
        hidden(&w.first_commit(&c.editor.1, b_file, a_upload).await);
        assert_eq!(w.count("SELECT count(*) FROM file_versions").await, 2);
    }

    // ---- restore -------------------------------------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn restoring_an_old_version_creates_a_new_head_with_the_old_content() {
        let w = W::new().await;
        let c = cast(&w).await;
        let old = pattern(1200, 1);
        let v1 = w.push(&c.editor.1, c.file, &old).await;
        let v2 = w.push(&c.editor.1, c.file, &pattern(300, 2)).await;
        let restore_url = format!("/api/v1/files/{}/versions/{v1}/restore", c.file);
        let post = |who: &Session, header: Option<(&str, String)>| {
            let mut req = Req::new("POST", &restore_url).session(who);
            if let Some((n, v)) = header {
                req = req.header(n, &v);
            }
            w.send(req)
        };
        assert_status(
            &post(&c.editor.1, None).await,
            StatusCode::PRECONDITION_REQUIRED,
            "PRECONDITION_REQUIRED",
        );
        assert_eq!(
            post(&c.editor.1, Some(("If-None-Match", "*".to_owned())))
                .await
                .status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let stale = post(&c.editor.1, Some(("If-Match", tag(v1)))).await;
        assert_status(&stale, StatusCode::PRECONDITION_FAILED, "VERSION_CONFLICT");
        assert_eq!(
            stale.body["error"]["details"]["current_version_id"],
            v2.to_string()
        );
        forbidden(&post(&c.viewer.1, Some(("If-Match", tag(v2)))).await);
        hidden(&post(&c.plain.1, Some(("If-Match", tag(v2)))).await);
        let done = post(&c.editor.1, Some(("If-Match", tag(v2)))).await;
        assert_eq!(done.status, StatusCode::CREATED, "{}", done.text);
        assert_eq!(done.body["number"], 3);
        assert_eq!(done.body["size_bytes"], 1200);
        assert_eq!(done.body["sha256"], sha_hex(&old));
        assert_eq!(done.body["base_version_id"], v2.to_string());
        // The head is the restored content; the old versions stay.
        let head = w
            .download(
                &c.viewer.1,
                &format!("/api/v1/files/{}/content", c.file),
                None,
            )
            .await;
        assert_eq!(head.status, StatusCode::OK);
        assert_eq!(w.count("SELECT count(*) FROM file_versions").await, 3);
        // Unknown versions.
        let head_id = id_of(&done);
        let missing = w
            .send(
                Req::new(
                    "POST",
                    &format!(
                        "/api/v1/files/{}/versions/{}/restore",
                        c.file,
                        Uuid::new_v4()
                    ),
                )
                .session(&c.editor.1)
                .header("If-Match", &tag(head_id)),
            )
            .await;
        hidden(&missing);
    }

    // ---- downloads ------------------------------------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn downloads_carry_the_safe_headers_and_the_exact_bytes() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(150_000, 8);
        let v1 = w.push(&c.editor.1, c.file, &data).await;
        let older = pattern(2000, 9);
        // A hostile display name straight into the row (the API would refuse most of it).
        sqlx::query("UPDATE files SET name = $2 WHERE id = $1")
            .bind(c.file)
            .bind("evil\"; \r\nSet-Cookie: x=y; name=caf\u{e9} \u{65e5}\u{672c}.fig")
            .execute(&w.env.pool)
            .await
            .unwrap();
        let _v2 = w.push(&c.editor.1, c.file, &older).await;
        let url = format!("/api/v1/files/{}/content", c.file);
        let r = w.download(&c.viewer.1, &url, None).await;
        assert_eq!(r.status, StatusCode::OK);
        let h = &r.headers;
        let get = |name: &str| h.get(name).map(|v| v.to_str().unwrap().to_owned());
        assert_eq!(
            get("content-type").as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(get("x-content-type-options").as_deref(), Some("nosniff"));
        assert_eq!(get("content-security-policy").as_deref(), Some("sandbox"));
        assert_eq!(get("cache-control").as_deref(), Some("no-store"));
        assert_eq!(get("accept-ranges").as_deref(), Some("bytes"));
        assert_eq!(get("content-length").as_deref(), Some("2000"));
        let digest = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&older));
        assert_eq!(
            get("repr-digest").as_deref(),
            Some(format!("sha-256=:{digest}:").as_str())
        );
        let disposition = get("content-disposition").unwrap();
        assert!(
            disposition.starts_with("attachment; filename=\""),
            "{disposition}"
        );
        assert!(!disposition.contains('\n') && !disposition.contains('\r'));
        assert_eq!(
            disposition.split('"').count(),
            3,
            "exactly one quoted string: {disposition}"
        );
        assert!(disposition.contains("filename*=UTF-8''"));
        assert!(disposition.contains("caf%C3%A9"));
        assert_eq!(r.bytes.len(), 2000, "(lossy text of the binary body)");
        // The head's ETag; the first version by id.
        let head = w.head(&c.viewer.1, c.file).await.unwrap();
        assert_eq!(get("etag"), Some(tag(head)));
        let first = w
            .download(
                &c.viewer.1,
                &format!("/api/v1/files/{}/versions/{v1}/content", c.file),
                None,
            )
            .await;
        assert_eq!(first.status, StatusCode::OK);
        assert_eq!(
            first.headers.get("etag").unwrap().to_str().unwrap(),
            tag(v1)
        );
        assert_eq!(first.headers.get("content-length").unwrap(), "150000");

        // Access: members read, others do not exist for it.
        for who in [&c.viewer.1, &c.editor.1, &c.wsadmin.1, &c.owner.1] {
            assert_eq!(w.download(who, &url, None).await.status, StatusCode::OK);
        }
        hidden(&w.download(&c.plain.1, &url, None).await);
        hidden(&w.download(&c.outsider.1, &url, None).await);
        assert_eq!(
            w.send(Req::new("GET", &url)).await.status,
            StatusCode::UNAUTHORIZED
        );
        // Unknown version, unknown file, a file without version, a trashed file.
        hidden(
            &w.download(
                &c.viewer.1,
                &format!(
                    "/api/v1/files/{}/versions/{}/content",
                    c.file,
                    Uuid::new_v4()
                ),
                None,
            )
            .await,
        );
        let empty_file = w.file(&c.owner.1, c.project, "none.fig").await;
        hidden(
            &w.download(
                &c.viewer.1,
                &format!("/api/v1/files/{empty_file}/content"),
                None,
            )
            .await,
        );
        assert_eq!(
            w.delete(&c.owner.1, &format!("/api/v1/files/{}", c.file))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_status(
            &w.download(&c.viewer.1, &url, None).await,
            StatusCode::CONFLICT,
            "FILE_DELETED",
        );
        assert_status(
            &w.download(
                &c.viewer.1,
                &format!("/api/v1/files/{}/versions/{v1}/content", c.file),
                None,
            )
            .await,
            StatusCode::CONFLICT,
            "FILE_DELETED",
        );
        hidden(&w.download(&c.plain.1, &url, None).await);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn archived_download_range_restore_and_auth_fail_closed() {
        use craftcloud_storage::{ObjectKey, ObjectStore};

        let mut w = W::new().await;
        let c = cast(&w).await;
        let archive_data = pattern(150_000, 83);
        let first = w.push(&c.editor.1, c.file, &archive_data).await;
        let head_data = pattern(512, 91);
        let head = w.push(&c.editor.1, c.file, &head_data).await;
        let archive_pool = w
            .env
            .pool_with_max_connections(1, Duration::from_secs(5))
            .await;
        let (archive, crypto) = w.env.configure_archive_with_pool(archive_pool).await;
        let (blob_id, local_key, archive_key, quota_group_id) =
            archive_version(&w, first, &archive, &crypto).await;
        let first_url = format!("/api/v1/files/{}/versions/{first}/content", c.file);

        let full = w.download(&c.viewer.1, &first_url, None).await;
        assert_eq!(full.status, StatusCode::OK, "{}", full.text);
        assert_eq!(full.bytes, archive_data);
        assert_eq!(
            full.headers["repr-digest"],
            format!(
                "sha-256=:{}:",
                base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&archive_data))
            )
        );

        let start = 65_530usize;
        let end = 65_561usize;
        let range = w
            .download(
                &c.viewer.1,
                &first_url,
                Some(&format!("bytes={start}-{}", end - 1)),
            )
            .await;
        assert_eq!(range.status, StatusCode::PARTIAL_CONTENT, "{}", range.text);
        assert_eq!(range.bytes, archive_data[start..end]);
        assert_eq!(
            range.headers["content-range"].to_str().unwrap(),
            format!("bytes {start}-{}/{}", end - 1, archive_data.len())
        );

        let current_url = format!("/api/v1/files/{}/content", c.file);
        let current = w.download(&c.viewer.1, &current_url, None).await;
        assert_eq!(current.status, StatusCode::OK);
        assert_eq!(current.bytes, head_data);

        let before = w.counters().await;
        let blobs_before = w.count("SELECT count(*) FROM blobs").await;
        let local_before = {
            let local = craftcloud_storage::LocalStore::open(&w.env.storage_dir)
                .await
                .unwrap();
            local.list_objects(None, 20).await.unwrap().len()
        };
        let denied_restore = w
            .send(
                Req::new(
                    "POST",
                    &format!("/api/v1/files/{}/versions/{first}/restore", c.file),
                )
                .session(&c.viewer.1)
                .header("If-Match", &tag(head)),
            )
            .await;
        forbidden(&denied_restore);
        assert_eq!(w.count("SELECT count(*) FROM blobs").await, blobs_before);
        let stale = tokio::time::timeout(
            Duration::from_secs(5),
            w.send(
                Req::new(
                    "POST",
                    &format!("/api/v1/files/{}/versions/{first}/restore", c.file),
                )
                .session(&c.editor.1)
                .header("If-Match", &tag(Uuid::new_v4())),
            ),
        )
        .await
        .expect("restore source lookup must reuse the open OrgTx connection");
        assert_status(&stale, StatusCode::PRECONDITION_FAILED, "VERSION_CONFLICT");
        assert_eq!(w.count("SELECT count(*) FROM blobs").await, blobs_before);
        let local_after = {
            let local = craftcloud_storage::LocalStore::open(&w.env.storage_dir)
                .await
                .unwrap();
            local.list_objects(None, 20).await.unwrap().len()
        };
        assert_eq!(local_after, local_before, "failed CAS copy is cleaned up");

        let restore = tokio::time::timeout(
            Duration::from_secs(5),
            w.send(
                Req::new(
                    "POST",
                    &format!("/api/v1/files/{}/versions/{first}/restore", c.file),
                )
                .session(&c.editor.1)
                .header("If-Match", &tag(head)),
            ),
        )
        .await
        .expect("archived restore must release its first DB connection before archive I/O");
        assert_eq!(restore.status, StatusCode::CREATED, "{}", restore.text);
        let restored = id_of(&restore);
        assert_eq!(
            w.counters().await,
            before,
            "restore copy shares logical quota"
        );
        let (new_key, new_group, new_tier): (String, Uuid, String) = sqlx::query_as(
            "SELECT b.object_key, b.quota_group_id, b.tier_state FROM file_versions v \
             JOIN blobs b ON b.id=v.blob_id WHERE v.id=$1",
        )
        .bind(restored)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_ne!(new_key, local_key);
        assert_eq!(new_group, quota_group_id);
        assert_eq!(new_tier, "local");
        let source_tier: String = sqlx::query_scalar("SELECT tier_state FROM blobs WHERE id=$1")
            .bind(blob_id)
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(source_tier, "archived");
        let restored_bytes = w.download(&c.viewer.1, &current_url, None).await;
        assert_eq!(restored_bytes.status, StatusCode::OK);
        assert_eq!(restored_bytes.bytes, archive_data);

        let archive_object = ObjectKey::parse(&archive_key).unwrap();
        archive.delete(&archive_object).await.unwrap();
        let missing_archive = w.download(&c.viewer.1, &first_url, None).await;
        assert_status(
            &missing_archive,
            StatusCode::SERVICE_UNAVAILABLE,
            "ARCHIVE_UNAVAILABLE",
        );
        assert_eq!(missing_archive.headers["retry-after"], "60");
        assert_eq!(
            w.download(&c.viewer.1, &current_url, None).await.status,
            StatusCode::OK
        );
        hidden(&w.download(&c.outsider.1, &first_url, None).await);
        assert_eq!(
            w.download(&c.plain.1, &first_url, None).await.status,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn single_byte_ranges_and_their_edge_cases() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(100_000, 11);
        w.push(&c.editor.1, c.file, &data).await;
        let url = format!("/api/v1/files/{}/content", c.file);
        let range = |spec: &'static str| w.download(&c.viewer.1, &url, Some(spec));
        let body = |r: &Resp| r.bytes.len();
        let part = range("bytes=0-9").await;
        assert_eq!(part.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            part.headers.get("content-range").unwrap(),
            "bytes 0-9/100000"
        );
        assert_eq!(part.headers.get("content-length").unwrap(), "10");
        assert_eq!(body(&part), 10);
        for (spec, content_range, len) in [
            ("bytes=99990-", "bytes 99990-99999/100000", "10"),
            ("bytes=-5", "bytes 99995-99999/100000", "5"),
            ("bytes=99999-99999", "bytes 99999-99999/100000", "1"),
            ("bytes=99990-200000", "bytes 99990-99999/100000", "10"),
            ("bytes=-200000", "bytes 0-99999/100000", "100000"),
            ("bytes=65536-131071", "bytes 65536-99999/100000", "34464"),
        ] {
            let r = range(spec).await;
            assert_eq!(r.status, StatusCode::PARTIAL_CONTENT, "{spec}");
            assert_eq!(
                r.headers.get("content-range").unwrap(),
                content_range,
                "{spec}"
            );
            assert_eq!(r.headers.get("content-length").unwrap(), len, "{spec}");
        }
        for spec in [
            "bytes=100000-",
            "bytes=100000-100010",
            "bytes=-0",
            "bytes=99999999999-",
        ] {
            let r = range(spec).await;
            assert_status(
                &r,
                StatusCode::RANGE_NOT_SATISFIABLE,
                "RANGE_NOT_SATISFIABLE",
            );
            assert_eq!(
                r.headers.get("content-range").unwrap(),
                "bytes */100000",
                "{spec}"
            );
        }
        // Several ranges, unknown units and garbage are ignored: the whole content.
        for spec in ["bytes=0-1,5-6", "items=0-5", "bytes=9-3", "bytes=x-y"] {
            let r = range(spec).await;
            assert_eq!(r.status, StatusCode::OK, "{spec}");
            assert_eq!(r.headers.get("content-length").unwrap(), "100000");
        }
        // An empty version: whole content is empty, every range is unsatisfiable.
        let empty = w.file(&c.owner.1, c.project, "empty.fig").await;
        w.push(&c.editor.1, empty, b"").await;
        let empty_url = format!("/api/v1/files/{empty}/content");
        let whole = w.download(&c.viewer.1, &empty_url, None).await;
        assert_eq!((whole.status, whole.bytes.len()), (StatusCode::OK, 0));
        assert_eq!(whole.headers.get("content-length").unwrap(), "0");
        for spec in ["bytes=0-0", "bytes=0-", "bytes=-1"] {
            assert_status(
                &w.download(&c.viewer.1, &empty_url, Some(spec)).await,
                StatusCode::RANGE_NOT_SATISFIABLE,
                "RANGE_NOT_SATISFIABLE",
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_corrupted_object_aborts_the_download_and_marks_the_blob() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(300_000, 12);
        let v1 = w.push(&c.editor.1, c.file, &data).await;
        let key: String = sqlx::query_scalar("SELECT object_key FROM blobs")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        let path = w.object_path(&key);
        // Same length, one flipped byte in the middle.
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions).unwrap();
        let mut tampered = data.clone();
        tampered[150_000] ^= 0xff;
        std::fs::write(&path, &tampered).unwrap();

        let url = format!("/api/v1/files/{}/content", c.file);
        let (status, headers, mut body) =
            call_stream(&w.env.app, Req::new("GET", &url).cookie_only(&c.viewer.1)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get("content-length").unwrap(), "300000");
        let mut received = 0_usize;
        let mut ended_cleanly = false;
        loop {
            match body.frame().await {
                Some(Ok(frame)) => received += frame.into_data().map(|d| d.len()).unwrap_or(0),
                Some(Err(_)) => break,
                None => {
                    ended_cleanly = true;
                    break;
                }
            }
        }
        assert!(
            !ended_cleanly,
            "the body must end with an error, never cleanly"
        );
        assert!(
            received < 300_000,
            "the last chunk is withheld ({received})"
        );
        // The blob is marked; later downloads and restores refuse.
        for _ in 0..100 {
            let state: String = sqlx::query_scalar("SELECT state FROM blobs")
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
            if state == "corrupt" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let state: String = sqlx::query_scalar("SELECT state FROM blobs")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(state, "corrupt");
        assert_status(
            &w.download(&c.viewer.1, &url, None).await,
            StatusCode::INTERNAL_SERVER_ERROR,
            "BLOB_CORRUPT",
        );
        assert_status(
            &w.download(&c.viewer.1, &url, Some("bytes=0-9")).await,
            StatusCode::INTERNAL_SERVER_ERROR,
            "BLOB_CORRUPT",
        );
        let restore = w
            .send(
                Req::new(
                    "POST",
                    &format!("/api/v1/files/{}/versions/{v1}/restore", c.file),
                )
                .session(&c.editor.1)
                .header("If-Match", &tag(v1)),
            )
            .await;
        assert_status(&restore, StatusCode::INTERNAL_SERVER_ERROR, "BLOB_CORRUPT");
        // The bytes still count against the quota (the data is still there).
        assert_eq!(w.counters().await.1, 300_000);

        // A truncated object is caught before any byte is sent.
        let file2 = w.file(&c.owner.1, c.project, "second.fig").await;
        w.push(&c.editor.1, file2, &data).await;
        let key2: String =
            sqlx::query_scalar("SELECT object_key FROM blobs WHERE state = 'committed'")
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
        let path2 = w.object_path(&key2);
        let mut permissions = std::fs::metadata(&path2).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&path2, permissions).unwrap();
        std::fs::write(&path2, &data[..1000]).unwrap();
        assert_status(
            &w.download(&c.viewer.1, &format!("/api/v1/files/{file2}/content"), None)
                .await,
            StatusCode::INTERNAL_SERVER_ERROR,
            "BLOB_CORRUPT",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_64_mib_file_round_trips_with_a_matching_checksum() {
        let mut s = crate::common::settings();
        s.max_file_size_bytes = 128 * 1024 * 1024;
        let env = TestEnv::with(s, craftcloud_api::ApiConfig::default()).await;
        let op = env.login_ok("op").await;
        let w = W { env, op };
        let c = cast(&w).await;
        let block = Bytes::from(pattern(1024 * 1024, 13));
        let mut hasher = Sha256::new();
        for _ in 0..64 {
            hasher.update(&block);
        }
        let sha: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let size = 64_u64 * 1024 * 1024;
        let created = w
            .json(
                "POST",
                &format!("/api/v1/files/{}/uploads", c.file),
                &c.editor.1,
                json!({"size_bytes": size, "sha256": sha}),
            )
            .await;
        let upload = Uuid::parse_str(created.body["upload_id"].as_str().unwrap()).unwrap();
        let put = w
            .send(
                Req::new("PUT", &format!("/api/v1/uploads/{upload}/content"))
                    .session(&c.editor.1)
                    .body(Body::new(PatternBody { block, frames: 64 })),
            )
            .await;
        assert_eq!(put.status, StatusCode::OK, "{}", put.text);
        assert_eq!(
            w.first_commit(&c.editor.1, c.file, upload).await.status,
            StatusCode::CREATED
        );

        // Phase-02 gate: the download's checksum equals the committed upload's.
        let started = std::time::Instant::now();
        let (status, headers, mut body) = call_stream(
            &w.env.app,
            Req::new("GET", &format!("/api/v1/files/{}/content", c.file)).cookie_only(&c.viewer.1),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let mut hasher = Sha256::new();
        let (mut total, mut largest) = (0_u64, 0_usize);
        while let Some(frame) = body.frame().await {
            let data = frame.unwrap().into_data().unwrap();
            largest = largest.max(data.len());
            total += data.len() as u64;
            hasher.update(&data);
        }
        let got: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!((total, got.as_str()), (size, sha.as_str()));
        assert!(largest <= 64 * 1024, "bounded chunks, largest {largest}");
        let digest = base64::engine::general_purpose::STANDARD.encode(hex_to_bytes(&sha));
        assert_eq!(
            headers.get("repr-digest").unwrap().to_str().unwrap(),
            format!("sha-256=:{digest}:")
        );
        eprintln!("downloaded 64 MiB in {:?}", started.elapsed());
    }

    fn hex_to_bytes(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    // ---- review follow-ups -----------------------------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn if_range_honors_a_range_only_for_the_exact_current_etag() {
        let w = W::new().await;
        let c = cast(&w).await;
        let v1 = w.push(&c.editor.1, c.file, &pattern(1000, 1)).await;
        let url = format!("/api/v1/files/{}/content", c.file);
        let ranged = |if_range: Option<String>| {
            let mut req = Req::new("GET", &url)
                .cookie_only(&c.viewer.1)
                .header("Range", "bytes=0-9");
            if let Some(value) = if_range {
                req = req.header("If-Range", &value);
            }
            w.send(req)
        };
        assert_eq!(ranged(None).await.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            ranged(Some(tag(v1))).await.status,
            StatusCode::PARTIAL_CONTENT
        );
        // The head moves between the client's two requests: the old tag now gets the whole new content.
        let v2 = w.push(&c.editor.1, c.file, &pattern(2000, 2)).await;
        let whole = ranged(Some(tag(v1))).await;
        assert_eq!(whole.status, StatusCode::OK);
        assert_eq!(whole.bytes.len(), 2000);
        assert_eq!(
            whole.headers.get("etag").unwrap().to_str().unwrap(),
            tag(v2)
        );
        assert_eq!(
            ranged(Some(tag(v2))).await.status,
            StatusCode::PARTIAL_CONTENT
        );
        // Weak tags, bare ids, dates and garbage never honor the range.
        for bad in [
            format!("W/{}", tag(v2)),
            v2.to_string(),
            "Wed, 21 Oct 2026 07:28:00 GMT".to_owned(),
            "*".to_owned(),
            String::new(),
        ] {
            let r = ranged(Some(bad.clone())).await;
            assert_eq!((r.status, r.bytes.len()), (StatusCode::OK, 2000), "{bad:?}");
        }
        // An unsatisfiable range is also ignored when If-Range does not match.
        let r = w
            .send(
                Req::new("GET", &url)
                    .cookie_only(&c.viewer.1)
                    .header("Range", "bytes=99999-")
                    .header("If-Range", &tag(v1)),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_missing_object_is_a_storage_outage_not_corruption() {
        let w = W::new().await;
        let c = cast(&w).await;
        w.push(&c.editor.1, c.file, &pattern(5000, 3)).await;
        let key: String = sqlx::query_scalar("SELECT object_key FROM blobs")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        let path = w.object_path(&key);
        let away = path.with_extension("away");
        std::fs::rename(&path, &away).unwrap();
        let url = format!("/api/v1/files/{}/content", c.file);
        for range in [None, Some("bytes=0-9")] {
            assert_status(
                &w.download(&c.viewer.1, &url, range).await,
                StatusCode::SERVICE_UNAVAILABLE,
                "STORAGE_UNAVAILABLE",
            );
        }
        let state: String = sqlx::query_scalar("SELECT state FROM blobs")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(state, "committed", "nothing was marked");
        // The object is back (the mount returned): downloads work again.
        std::fs::rename(&away, &path).unwrap();
        assert_eq!(
            w.download(&c.viewer.1, &url, None).await.status,
            StatusCode::OK
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn star_lists_and_weak_tags_are_not_preconditions() {
        let w = W::new().await;
        let c = cast(&w).await;
        let v1 = w.push(&c.editor.1, c.file, &pattern(100, 1)).await;
        let up = w.stored_upload(&c.editor.1, c.file, &pattern(100, 2)).await;
        let bad = [
            "*".to_owned(),
            format!("{}, {}", tag(v1), tag(Uuid::new_v4())),
            format!("W/{}", tag(v1)),
            String::new(),
        ];
        for value in &bad {
            let commit = w
                .commit(&c.editor.1, c.file, up, Some(("If-Match", value.clone())))
                .await;
            assert_status(
                &commit,
                StatusCode::UNPROCESSABLE_ENTITY,
                "VALIDATION_ERROR",
            );
            let restore = w
                .send(
                    Req::new(
                        "POST",
                        &format!("/api/v1/files/{}/versions/{v1}/restore", c.file),
                    )
                    .session(&c.editor.1)
                    .header("If-Match", value),
                )
                .await;
            assert_status(
                &restore,
                StatusCode::UNPROCESSABLE_ENTITY,
                "VALIDATION_ERROR",
            );
        }
        // Nothing happened; a proper tag still works, a foreign quoted tag is an ordinary mismatch.
        assert_status(
            &w.commit(
                &c.editor.1,
                c.file,
                up,
                Some(("If-Match", "\"file-v3\"".to_owned())),
            )
            .await,
            StatusCode::PRECONDITION_FAILED,
            "VERSION_CONFLICT",
        );
        assert_eq!(w.count("SELECT count(*) FROM file_versions").await, 1);
        assert_eq!(
            w.next_commit(&c.editor.1, c.file, up, v1).await.status,
            StatusCode::CREATED
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_commit_itself_refuses_an_upload_from_another_workspace() {
        use craftcloud_core::policy::{
            Action, Actor, Decision, OrgMembership, OrgRole, Resource, can,
        };
        let w = W::new().await;
        let c = cast(&w).await;
        // A file in a second workspace of the same organization and an upload stored in the first.
        let other_ws = w.ws(&c.owner.1, c.org, "second").await;
        let other_project = w.project(&c.owner.1, other_ws, "Other").await;
        let other_file = w.file(&c.owner.1, other_project, "other.fig").await;
        let upload = w.stored_upload(&c.owner.1, c.file, &pattern(64, 5)).await;
        let actor = Actor {
            user_id: c.owner.0,
            instance_operator: false,
            membership: Some(OrgMembership {
                org_id: c.org,
                role: OrgRole::Owner,
            }),
            workspace: None,
        };
        let Decision::Allow(permit) = can(
            &actor,
            Action::VersionCommit,
            &Resource::Upload {
                org_id: c.org,
                workspace_id: c.ws,
                creator: c.owner.0,
            },
        ) else {
            panic!("owner may commit");
        };
        let scope = craftcloud_db::OrgScope::from_permit(&permit).unwrap();
        let mut tx = craftcloud_db::members::OrgTx::begin(&w.env.pool, c.org)
            .await
            .unwrap()
            .unwrap();
        let outcome = tx
            .version_commit(
                &scope,
                other_file,
                upload,
                c.owner.0,
                Some(craftcloud_db::versions::Precondition::NoVersionYet),
                time::OffsetDateTime::now_utc(),
            )
            .await
            .unwrap();
        assert!(
            matches!(outcome, craftcloud_db::versions::CommitOutcome::Gone),
            "{outcome:?}"
        );
        tx.commit().await.unwrap();
        assert_eq!(w.count("SELECT count(*) FROM file_versions").await, 0);
        assert_eq!(
            w.counters().await,
            (64, 0, 64, 0),
            "the reservation is untouched"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_download_client_that_stops_reading_is_dropped_and_releases_the_file() {
        let mut s = crate::common::settings();
        s.download_idle_timeout = Duration::from_millis(200);
        let env = TestEnv::with(s, craftcloud_api::ApiConfig::default()).await;
        let op = env.login_ok("op").await;
        let w = W { env, op };
        let c = cast(&w).await;
        let data = pattern(2 * 1024 * 1024, 14);
        w.push(&c.editor.1, c.file, &data).await;
        let key: String = sqlx::query_scalar("SELECT object_key FROM blobs")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        let (status, _headers, mut body) = call_stream(
            &w.env.app,
            Req::new("GET", &format!("/api/v1/files/{}/content", c.file)).cookie_only(&c.viewer.1),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // Take one frame, then stop reading for longer than the idle timeout.
        let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert!(!first.is_empty());
        tokio::time::sleep(Duration::from_millis(900)).await;
        // The streaming task gave up and closed its file handle: the object can be removed
        // (on Windows an open handle would forbid it).
        let path = w.object_path(&key);
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions).unwrap();
        std::fs::remove_file(&path).unwrap();
        // What the client still gets is cut short: it never receives the complete content.
        let mut received = first.len();
        while let Some(Ok(frame)) = body.frame().await {
            received += frame.into_data().map(|d| d.len()).unwrap_or(0);
        }
        assert!(received < data.len(), "{received}");
        // The blob was not marked: this is not evidence of corruption.
        let state: String = sqlx::query_scalar("SELECT state FROM blobs")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(state, "committed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_download_below_the_minimum_rate_is_cancelled_after_the_grace_period() {
        let w = W::new().await;
        let c = cast(&w).await;
        let data = pattern(2 * 1024 * 1024, 15);
        w.push(&c.editor.1, c.file, &data).await;
        let (status, _headers, mut body) = call_stream(
            &w.env.app,
            Req::new("GET", &format!("/api/v1/files/{}/content", c.file)).cookie_only(&c.viewer.1),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
        // Five minutes pass with almost nothing delivered.
        w.env.clock.advance(Duration::from_secs(301));
        let mut received = first.len();
        let mut failed = false;
        loop {
            match body.frame().await {
                Some(Ok(frame)) => received += frame.into_data().map(|d| d.len()).unwrap_or(0),
                Some(Err(_)) => {
                    failed = true;
                    break;
                }
                None => break,
            }
        }
        assert!(failed, "the slow download must end with an error");
        assert!(received < data.len());
        let state: String = sqlx::query_scalar("SELECT state FROM blobs")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(state, "committed");
    }

    // ---- csrf and authentication --------------------------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn every_version_route_needs_a_session_and_unsafe_ones_csrf_and_origin() {
        let w = W::new().await;
        let c = cast(&w).await;
        let v1 = w.push(&c.editor.1, c.file, &pattern(10, 1)).await;
        let up = w.stored_upload(&c.editor.1, c.file, &pattern(10, 2)).await;
        let routes: Vec<(&'static str, String)> = vec![
            ("POST", format!("/api/v1/files/{}/versions", c.file)),
            (
                "POST",
                format!("/api/v1/files/{}/versions/{v1}/restore", c.file),
            ),
            ("GET", format!("/api/v1/files/{}/versions", c.file)),
            ("GET", format!("/api/v1/files/{}/versions/{v1}", c.file)),
            ("GET", format!("/api/v1/files/{}/content", c.file)),
            (
                "GET",
                format!("/api/v1/files/{}/versions/{v1}/content", c.file),
            ),
        ];
        let build = |m: &'static str, u: &str| {
            let req = Req::new(m, u).header("If-Match", &tag(v1));
            if m == "POST" && u.ends_with("/versions") {
                req.json(json!({"upload_id": up.to_string()}))
            } else {
                req
            }
        };
        for (m, u) in &routes {
            assert_status(
                &w.send(build(m, u)).await,
                StatusCode::UNAUTHORIZED,
                "UNAUTHENTICATED",
            );
            if *m == "POST" {
                assert_status(
                    &w.send(build(m, u).cookie_only(&c.editor.1)).await,
                    StatusCode::FORBIDDEN,
                    "CSRF_FAILED",
                );
                assert_status(
                    &w.send(
                        build(m, u)
                            .session(&c.editor.1)
                            .origin(Some("https://evil.example")),
                    )
                    .await,
                    StatusCode::FORBIDDEN,
                    "CSRF_FAILED",
                );
            }
        }
        assert_eq!(w.count("SELECT count(*) FROM file_versions").await, 1);
        w.env.clock.advance(Duration::from_secs(8 * 3600 + 60));
        for (m, u) in &routes {
            assert_status(
                &w.send(build(m, u).session(&c.editor.1)).await,
                StatusCode::UNAUTHORIZED,
                "UNAUTHENTICATED",
            );
        }
    }

    // ---- no existence oracle for foreign ids (SR-03) -----------------------------------------------

    /// Everything a client can compare: status, error body without the request id, headers
    /// without the per-request ones.
    fn shape(resp: &Resp) -> (StatusCode, Value, Vec<(String, String)>) {
        let mut body = resp.body.clone();
        if let Some(error) = body.get_mut("error").and_then(Value::as_object_mut) {
            error.remove("request_id");
        }
        let mut headers: Vec<(String, String)> = resp
            .headers
            .iter()
            .filter(|(name, _)| {
                !matches!(name.as_str(), "x-request-id" | "date" | "content-length")
            })
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or("").to_owned(),
                )
            })
            .collect();
        headers.sort();
        (resp.status, body, headers)
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn foreign_and_unknown_objects_answer_identically_on_every_route() {
        let w = W::new().await;
        let c = cast(&w).await;
        // The caller: an outsider with an organization and an upload of their own.
        let mine_org = w.org("outsiders", c.outsider.0).await;
        let mine_ws = w.ws(&c.outsider.1, mine_org, "out-main").await;
        let mine_project = w.project(&c.outsider.1, mine_ws, "Mine").await;
        let mine_file = w.file(&c.outsider.1, mine_project, "mine.fig").await;
        let mine = w
            .stored_upload(&c.outsider.1, mine_file, &pattern(64, 1))
            .await;
        // The foreign objects: a file with a version, a stored upload, a member.
        let version = w.push(&c.owner.1, c.file, &pattern(128, 2)).await;
        let other = w.file(&c.owner.1, c.project, "other.fig").await;
        let upload = w.stored_upload(&c.owner.1, other, &pattern(256, 3)).await;
        let foreign = [
            ("{org}", c.org),
            ("{ws}", c.ws),
            ("{project}", c.project),
            ("{file}", c.file),
            ("{version}", version),
            ("{upload}", upload),
            ("{user}", c.editor.0),
            ("{inv}", Uuid::new_v4()),
            ("{mine}", mine),
        ];
        let unknown: Vec<(&str, Uuid)> = foreign
            .iter()
            .map(|(name, id)| {
                (
                    *name,
                    if *name == "{mine}" || *name == "{inv}" {
                        *id
                    } else {
                        Uuid::new_v4()
                    },
                )
            })
            .collect();
        let fill = |text: &str, ids: &[(&str, Uuid)]| {
            ids.iter().fold(text.to_owned(), |text, (name, id)| {
                text.replace(name, &id.to_string())
            })
        };

        let none = Value::Null;
        let routes: Vec<(&str, &str, Vec<Value>)> = vec![
            ("GET", "/api/v1/organizations/{org}", vec![none.clone()]),
            (
                "PATCH",
                "/api/v1/organizations/{org}",
                vec![json!({"name": "New"}), json!({"name": ""}), json!({})],
            ),
            (
                "GET",
                "/api/v1/organizations/{org}/members",
                vec![none.clone()],
            ),
            (
                "POST",
                "/api/v1/organizations/{org}/members",
                vec![json!({"user_id": "{user}", "role": "member"})],
            ),
            (
                "PATCH",
                "/api/v1/organizations/{org}/members/{user}",
                vec![json!({"role": "admin"})],
            ),
            (
                "DELETE",
                "/api/v1/organizations/{org}/members/{user}",
                vec![none.clone()],
            ),
            (
                "GET",
                "/api/v1/organizations/{org}/workspaces",
                vec![none.clone()],
            ),
            (
                "POST",
                "/api/v1/organizations/{org}/workspaces",
                vec![
                    json!({"name": "W", "slug": "w-new"}),
                    json!({"name": "", "slug": ""}),
                ],
            ),
            (
                "GET",
                "/api/v1/organizations/{org}/invitations",
                vec![none.clone()],
            ),
            (
                "POST",
                "/api/v1/organizations/{org}/invitations",
                vec![json!({"email": "x@example.test", "org_role": "member"})],
            ),
            (
                "DELETE",
                "/api/v1/organizations/{org}/invitations/{inv}",
                vec![none.clone()],
            ),
            (
                "GET",
                "/api/v1/organizations/{org}/usage",
                vec![none.clone()],
            ),
            (
                "GET",
                "/api/v1/organizations/{org}/audit",
                vec![none.clone()],
            ),
            (
                "GET",
                "/api/v1/organizations/{org}/audit/export",
                vec![none.clone()],
            ),
            ("GET", "/api/v1/workspaces/{ws}", vec![none.clone()]),
            (
                "PATCH",
                "/api/v1/workspaces/{ws}",
                vec![json!({"name": "New"}), json!({"name": ""})],
            ),
            ("GET", "/api/v1/workspaces/{ws}/members", vec![none.clone()]),
            (
                "POST",
                "/api/v1/workspaces/{ws}/members",
                vec![json!({"user_id": "{user}", "role": "viewer"})],
            ),
            (
                "PATCH",
                "/api/v1/workspaces/{ws}/members/{user}",
                vec![json!({"role": "editor"})],
            ),
            (
                "DELETE",
                "/api/v1/workspaces/{ws}/members/{user}",
                vec![none.clone()],
            ),
            (
                "PUT",
                "/api/v1/workspaces/{ws}/quota",
                vec![json!({"quota_bytes": 5}), json!({"quota_bytes": "x"})],
            ),
            (
                "GET",
                "/api/v1/workspaces/{ws}/projects",
                vec![none.clone()],
            ),
            (
                "POST",
                "/api/v1/workspaces/{ws}/projects",
                vec![json!({"name": "P"}), json!({"name": ""})],
            ),
            ("GET", "/api/v1/projects/{project}", vec![none.clone()]),
            (
                "PATCH",
                "/api/v1/projects/{project}",
                vec![json!({"name": "Q"}), json!({"name": ""})],
            ),
            (
                "POST",
                "/api/v1/projects/{project}/restore",
                vec![none.clone()],
            ),
            (
                "GET",
                "/api/v1/projects/{project}/files",
                vec![none.clone()],
            ),
            (
                "POST",
                "/api/v1/projects/{project}/files",
                vec![
                    json!({"name": "a.fig", "app": "figcraft", "format": "fig"}),
                    json!({"name": "", "app": "x", "format": "y"}),
                ],
            ),
            ("GET", "/api/v1/files/{file}", vec![none.clone()]),
            (
                "PATCH",
                "/api/v1/files/{file}",
                vec![json!({"name": "b.fig"}), json!({"name": ""})],
            ),
            ("POST", "/api/v1/files/{file}/restore", vec![none.clone()]),
            (
                "POST",
                "/api/v1/files/{file}/uploads",
                vec![
                    json!({"size_bytes": 1, "sha256": "0".repeat(64)}),
                    json!({"size_bytes": 1, "sha256": "bad"}),
                ],
            ),
            ("GET", "/api/v1/files/{file}/versions", vec![none.clone()]),
            (
                "POST",
                "/api/v1/files/{file}/versions",
                vec![
                    json!({"upload_id": "invalid"}),
                    json!({"upload_id": "{upload}"}),
                    json!({"upload_id": "{mine}"}),
                ],
            ),
            (
                "GET",
                "/api/v1/files/{file}/versions/{version}",
                vec![none.clone()],
            ),
            (
                "POST",
                "/api/v1/files/{file}/versions/{version}/restore",
                vec![none.clone()],
            ),
            ("GET", "/api/v1/files/{file}/content", vec![none.clone()]),
            (
                "GET",
                "/api/v1/files/{file}/versions/{version}/content",
                vec![none.clone()],
            ),
            ("GET", "/api/v1/uploads/{upload}", vec![none.clone()]),
            ("DELETE", "/api/v1/uploads/{upload}", vec![none.clone()]),
            (
                "PUT",
                "/api/v1/uploads/{upload}/content",
                vec![none.clone()],
            ),
        ];
        let header_sets: [Option<(&str, &str)>; 3] = [
            None,
            Some(("If-Match", "\"x\"")),
            Some(("If-None-Match", "*")),
        ];
        let mut compared = 0;
        for (method, template, bodies) in &routes {
            for body in bodies {
                for extra in header_sets {
                    let mut shapes = Vec::new();
                    for ids in [&foreign[..], &unknown[..]] {
                        let mut req = Req::new(method, &fill(template, ids)).session(&c.outsider.1);
                        if let Some((name, value)) = extra {
                            req = req.header(name, value);
                        }
                        if *method == "PUT" && template.ends_with("/content") {
                            req = req.body(Body::from(vec![1_u8, 2, 3]));
                        } else if !body.is_null() {
                            req = req
                                .json(serde_json::from_str(&fill(&body.to_string(), ids)).unwrap());
                        }
                        let resp = w.send(req).await;
                        assert!(
                            !resp.status.is_success(),
                            "{method} {template} {body} succeeded: {}",
                            resp.text
                        );
                        shapes.push(shape(&resp));
                    }
                    assert_eq!(
                        shapes[0], shapes[1],
                        "{method} {template} {body} {extra:?}: foreign and unknown ids differ"
                    );
                    compared += 1;
                }
            }
        }
        assert!(compared > 100, "only {compared} comparisons");
        // The outsider changed nothing of the foreign tenant.
        assert_eq!(w.head(&c.owner.1, c.file).await, Some(version));
    }
}
