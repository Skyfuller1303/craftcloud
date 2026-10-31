//! HTTP-level tests of projects and file metadata (CC-F06): role matrix,
//! name validation, tenant isolation (IDOR), trash and retention, pagination
//! stability, ETags, CSRF and logging. All tests need PostgreSQL
//! (`cargo xtask db-test`).

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::time::Duration;

    use axum::http::StatusCode;
    use craftcloud_api::Clock;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    use crate::common::{Req, Resp, Session, TestEnv, call, capture_logs};

    struct FakeFigcraft;
    impl craftcloud_figcraft::Backend for FakeFigcraft {
        fn publish(
            &self,
            _original: &[u8],
            _name: &str,
        ) -> Result<String, craftcloud_figcraft::Error> {
            Ok("remote-file-id".into())
        }
        fn create_link(
            &self,
            _file: &str,
            _access: craftcloud_figcraft::Access,
        ) -> Result<craftcloud_figcraft::Link, craftcloud_figcraft::Error> {
            Ok(craftcloud_figcraft::Link {
                id: "remote-link-id".into(),
                token: "opaque-secret".into(),
            })
        }
        fn revoke_link(&self, _file: &str, _link: &str) -> Result<(), craftcloud_figcraft::Error> {
            Ok(())
        }
        fn export(
            &self,
            _file: &str,
            original: &[u8],
        ) -> Result<craftcloud_figcraft::Export, craftcloud_figcraft::Error> {
            Ok(craftcloud_figcraft::Export {
                bytes: original.to_vec(),
            })
        }
        fn remove(&self, _file: &str) -> Result<(), craftcloud_figcraft::Error> {
            Ok(())
        }
    }

    struct GatedBackend {
        creates: std::sync::atomic::AtomicUsize,
        entered: Arc<std::sync::Barrier>,
        release: Arc<std::sync::Barrier>,
        revokes: std::sync::atomic::AtomicUsize,
    }
    impl craftcloud_figcraft::Backend for GatedBackend {
        fn publish(&self, _: &[u8], _: &str) -> Result<String, craftcloud_figcraft::Error> {
            Ok("race-file".into())
        }
        fn create_link(
            &self,
            _: &str,
            _: craftcloud_figcraft::Access,
        ) -> Result<craftcloud_figcraft::Link, craftcloud_figcraft::Error> {
            let n = self
                .creates
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            if n == 2 {
                self.entered.wait();
                self.release.wait();
            }
            Ok(craftcloud_figcraft::Link {
                id: format!("race-link-{n}"),
                token: format!("race-token-{n}"),
            })
        }
        fn revoke_link(&self, _: &str, _: &str) -> Result<(), craftcloud_figcraft::Error> {
            self.revokes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn export(
            &self,
            _: &str,
            original: &[u8],
        ) -> Result<craftcloud_figcraft::Export, craftcloud_figcraft::Error> {
            Ok(craftcloud_figcraft::Export {
                bytes: original.to_vec(),
            })
        }
        fn remove(&self, _: &str) -> Result<(), craftcloud_figcraft::Error> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct OrderedState {
        calls: std::sync::Mutex<Vec<&'static str>>,
        fail_publish: std::sync::atomic::AtomicBool,
        fail_export: std::sync::atomic::AtomicBool,
        fail_remove: std::sync::atomic::AtomicBool,
        remote_present: std::sync::atomic::AtomicBool,
        next_link: std::sync::atomic::AtomicUsize,
    }

    struct OrderedBackend(Arc<OrderedState>);

    impl craftcloud_figcraft::Backend for OrderedBackend {
        fn publish(&self, _: &[u8], _: &str) -> Result<String, craftcloud_figcraft::Error> {
            if self
                .0
                .fail_publish
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(craftcloud_figcraft::Error::RemoteUnavailable);
            }
            self.0
                .remote_present
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok("ordered-file".into())
        }
        fn create_link(
            &self,
            _: &str,
            _: craftcloud_figcraft::Access,
        ) -> Result<craftcloud_figcraft::Link, craftcloud_figcraft::Error> {
            let n = self
                .0
                .next_link
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(craftcloud_figcraft::Link {
                id: format!("ordered-link-{n}"),
                token: "ordered-token".into(),
            })
        }
        fn revoke_link(&self, _: &str, _: &str) -> Result<(), craftcloud_figcraft::Error> {
            self.0.calls.lock().unwrap().push("revoke");
            Ok(())
        }
        fn export(
            &self,
            _: &str,
            original: &[u8],
        ) -> Result<craftcloud_figcraft::Export, craftcloud_figcraft::Error> {
            self.0.calls.lock().unwrap().push("export");
            if self.0.fail_export.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(craftcloud_figcraft::Error::RemoteUnavailable);
            }
            Ok(craftcloud_figcraft::Export {
                bytes: original.to_vec(),
            })
        }
        fn remove(&self, _: &str) -> Result<(), craftcloud_figcraft::Error> {
            self.0.calls.lock().unwrap().push("remove");
            if self.0.fail_remove.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(craftcloud_figcraft::Error::RemoteUnavailable);
            }
            self.0
                .remote_present
                .store(false, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    type NativeSocket =
        tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

    fn native_join(join_url: &str, name: &str) -> (NativeSocket, figcraft_collab::Client) {
        use figcraft_collab::{
            Client,
            message::{self, ServerMsg},
        };
        use tungstenite::Message;
        let invite = url::Url::parse(join_url).unwrap();
        let query = invite
            .query_pairs()
            .into_owned()
            .collect::<std::collections::HashMap<_, _>>();
        let (mut socket, _) = tungstenite::connect(query.get("server").unwrap().as_str()).unwrap();
        if let tungstenite::stream::MaybeTlsStream::Plain(stream) = socket.get_mut() {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
        }
        let hello = message::encode(&Client::hello(
            query.get("file").unwrap(),
            query.get("token").unwrap(),
            name,
            "#336699",
        ))
        .unwrap();
        socket.send(Message::Binary(hello.into())).unwrap();
        let welcome = loop {
            match socket.read().unwrap() {
                Message::Binary(bytes) => {
                    let msg: ServerMsg = message::decode(&bytes, 64 * 1024 * 1024).unwrap();
                    match &msg {
                        ServerMsg::Welcome { .. } => break msg,
                        ServerMsg::Refused { .. } => panic!("native server refused issued link"),
                        _ => (),
                    }
                }
                Message::Ping(value) => socket.send(Message::Pong(value)).unwrap(),
                Message::Close(_) => panic!("native server closed join"),
                _ => (),
            }
        };
        (socket, Client::join(welcome).unwrap())
    }

    fn native_message(socket: &mut NativeSocket) -> figcraft_collab::message::ServerMsg {
        use figcraft_collab::message::{self, ServerMsg};
        use tungstenite::Message;
        loop {
            match socket.read().unwrap() {
                Message::Binary(bytes) => {
                    return message::decode::<ServerMsg>(&bytes, 64 * 1024 * 1024).unwrap();
                }
                Message::Ping(value) => socket.send(Message::Pong(value)).unwrap(),
                Message::Close(_) => panic!("native connection closed"),
                _ => (),
            }
        }
    }

    fn native_link_refused(join_url: &str) -> bool {
        use figcraft_collab::{
            Client,
            message::{self, ServerMsg},
        };
        use tungstenite::Message;
        let invite = url::Url::parse(join_url).unwrap();
        let query = invite
            .query_pairs()
            .into_owned()
            .collect::<std::collections::HashMap<_, _>>();
        let (mut socket, _) = tungstenite::connect(query.get("server").unwrap().as_str()).unwrap();
        let hello = message::encode(&Client::hello(
            query.get("file").unwrap(),
            query.get("token").unwrap(),
            "Revoked capability",
            "#336699",
        ))
        .unwrap();
        socket.send(Message::Binary(hello.into())).unwrap();
        loop {
            match socket.read().unwrap() {
                Message::Binary(bytes) => {
                    let response: ServerMsg = message::decode(&bytes, 64 * 1024 * 1024).unwrap();
                    return matches!(response, ServerMsg::Refused { .. });
                }
                Message::Ping(value) => socket.send(Message::Pong(value)).unwrap(),
                Message::Close(_) => return true,
                _ => {}
            }
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
            Self { env, op }
        }

        async fn user(&self, name: &str) -> (Uuid, Session) {
            let id = self.env.user(name).await;
            (id, self.env.login_ok(name).await)
        }

        async fn login_again(&self, name: &str) -> Session {
            self.env.login_ok(name).await
        }

        async fn org(&self, slug: &str, owner: Uuid) -> Uuid {
            let r = call(
                &self.env.app,
                Req::new("POST", "/api/v1/organizations")
                    .session(&self.op)
                    .json(json!({"name": format!("Org {slug}"), "slug": slug, "owner_user_id": owner.to_string()})),
            )
            .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            id_of(&r)
        }

        async fn org_member(&self, actor: &Session, org: Uuid, user: Uuid) {
            let r = call(
                &self.env.app,
                Req::new("POST", &format!("/api/v1/organizations/{org}/members"))
                    .session(actor)
                    .json(json!({"user_id": user.to_string(), "role": "member"})),
            )
            .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
        }

        async fn ws(&self, actor: &Session, org: Uuid, slug: &str) -> Uuid {
            let r = call(
                &self.env.app,
                Req::new("POST", &format!("/api/v1/organizations/{org}/workspaces"))
                    .session(actor)
                    .json(json!({"name": format!("WS {slug}"), "slug": slug})),
            )
            .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            id_of(&r)
        }

        async fn ws_add(&self, actor: &Session, ws: Uuid, user: Uuid, role: &str) {
            let r = call(
                &self.env.app,
                Req::new("POST", &format!("/api/v1/workspaces/{ws}/members"))
                    .session(actor)
                    .json(json!({"user_id": user.to_string(), "role": role})),
            )
            .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
        }

        async fn get(&self, actor: &Session, path: &str) -> Resp {
            call(&self.env.app, Req::new("GET", path).cookie_only(actor)).await
        }

        async fn post(&self, actor: &Session, path: &str, body: Option<Value>) -> Resp {
            let req = Req::new("POST", path).session(actor);
            call(
                &self.env.app,
                match body {
                    Some(b) => req.json(b),
                    None => req,
                },
            )
            .await
        }

        async fn patch(&self, actor: &Session, path: &str, etag: Option<&str>, name: &str) -> Resp {
            let req = Req::new("PATCH", path)
                .session(actor)
                .json(json!({"name": name}));
            call(
                &self.env.app,
                match etag {
                    Some(e) => req.if_match(e),
                    None => req,
                },
            )
            .await
        }

        async fn delete(&self, actor: &Session, path: &str) -> Resp {
            call(&self.env.app, Req::new("DELETE", path).session(actor)).await
        }

        async fn project(&self, actor: &Session, ws: Uuid, name: &str) -> Resp {
            self.post(
                actor,
                &format!("/api/v1/workspaces/{ws}/projects"),
                Some(json!({"name": name})),
            )
            .await
        }

        async fn project_id(&self, actor: &Session, ws: Uuid, name: &str) -> Uuid {
            let r = self.project(actor, ws, name).await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            id_of(&r)
        }

        async fn file(&self, actor: &Session, project: Uuid, name: &str) -> Resp {
            self.post(
                actor,
                &format!("/api/v1/projects/{project}/files"),
                Some(json!({"name": name, "app": "figcraft", "format": "fig"})),
            )
            .await
        }

        async fn file_id(&self, actor: &Session, project: Uuid, name: &str) -> Uuid {
            let r = self.file(actor, project, name).await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            id_of(&r)
        }

        async fn count(&self, sql: &str) -> i64 {
            self.env.count(sql).await
        }
    }

    fn id_of(r: &Resp) -> Uuid {
        Uuid::parse_str(r.body["id"].as_str().unwrap()).unwrap()
    }

    fn assert_status(resp: &Resp, status: StatusCode, code: &str) {
        assert_eq!((resp.status, resp.code()), (status, code), "{}", resp.text);
    }

    async fn upload_bytes(w: &W, actor: &Session, file: Uuid, bytes: &[u8]) {
        let sha256 = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let created = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{file}/uploads"))
                .session(actor)
                .json(json!({"size_bytes":bytes.len(),"sha256":sha256})),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        let upload = created.body["upload_id"].as_str().unwrap().to_owned();
        let stored = call(
            &w.env.app,
            Req::new("PUT", &format!("/api/v1/uploads/{upload}/content"))
                .session(actor)
                .body(axum::body::Body::from(bytes.to_vec())),
        )
        .await;
        assert_eq!(stored.status, StatusCode::OK, "{}", stored.text);
        let committed = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{file}/versions"))
                .session(actor)
                .header("If-None-Match", "*")
                .json(json!({"upload_id":upload})),
        )
        .await;
        assert_eq!(committed.status, StatusCode::CREATED, "{}", committed.text);
    }

    fn hidden(resp: &Resp) {
        assert_status(resp, StatusCode::NOT_FOUND, "NOT_FOUND");
    }

    fn forbidden(resp: &Resp) {
        assert_status(resp, StatusCode::FORBIDDEN, "FORBIDDEN");
    }

    fn ids(resp: &Resp) -> Vec<String> {
        resp.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_owned())
            .collect()
    }

    struct Cast {
        org: Uuid,
        ws: Uuid,
        owner: (Uuid, Session),
        admin: (Uuid, Session),
        wsadmin: (Uuid, Session),
        editor: (Uuid, Session),
        viewer: (Uuid, Session),
        plain: (Uuid, Session),
        outsider: (Uuid, Session),
    }

    async fn cast(w: &W) -> Cast {
        let owner = w.user("owner").await;
        let org = w.org("acme", owner.0).await;
        let admin = w.user("admin").await;
        let r = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/members"))
                .session(&owner.1)
                .json(json!({"user_id": admin.0.to_string(), "role": "admin"})),
        )
        .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
        let wsadmin = w.user("wsadmin").await;
        let editor = w.user("editor").await;
        let viewer = w.user("viewer").await;
        let plain = w.user("plain").await;
        for p in [&wsadmin, &editor, &viewer, &plain] {
            w.org_member(&owner.1, org, p.0).await;
        }
        let outsider = w.user("outsider").await;
        w.org("elsewhere", outsider.0).await;
        let ws = w.ws(&owner.1, org, "main").await;
        w.ws_add(&owner.1, ws, wsadmin.0, "admin").await;
        w.ws_add(&owner.1, ws, editor.0, "editor").await;
        w.ws_add(&owner.1, ws, viewer.0, "viewer").await;
        Cast {
            org,
            ws,
            owner,
            admin,
            wsadmin,
            editor,
            viewer,
            plain,
            outsider,
        }
    }

    // ---- role matrix ------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn project_role_matrix() {
        let w = W::new().await;
        let c = cast(&w).await;
        let projects_url = format!("/api/v1/workspaces/{}/projects", c.ws);

        // Create: org owner/admin, workspace admin and editor; not viewers; others do not see the workspace.
        forbidden(&w.project(&c.viewer.1, c.ws, "Nope").await);
        hidden(&w.project(&c.plain.1, c.ws, "Nope").await);
        hidden(&w.project(&c.outsider.1, c.ws, "Nope").await);
        hidden(&w.project(&w.op, c.ws, "Nope").await);
        let mut created = Vec::new();
        for (who, name) in [
            (&c.editor.1, "By editor"),
            (&c.wsadmin.1, "By wsadmin"),
            (&c.admin.1, "By admin"),
            (&c.owner.1, "By owner"),
        ] {
            let r = w.project(who, c.ws, name).await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            assert_eq!(r.etag(), "\"prj-v1\"");
            created.push(id_of(&r));
        }
        assert_eq!(w.count("SELECT count(*) FROM projects").await, 4);
        let project = created[0];
        let url = format!("/api/v1/projects/{project}");

        // Read and list: every workspace member and org managers.
        for who in [
            &c.viewer.1,
            &c.editor.1,
            &c.wsadmin.1,
            &c.admin.1,
            &c.owner.1,
        ] {
            let got = w.get(who, &url).await;
            assert_eq!(got.status, StatusCode::OK, "{}", got.text);
            assert_eq!(got.body["name"], "By editor");
            assert_eq!(
                w.get(who, &projects_url).await.body["items"]
                    .as_array()
                    .unwrap()
                    .len(),
                4
            );
        }
        for who in [&c.plain.1, &c.outsider.1, &w.op] {
            hidden(&w.get(who, &url).await);
            hidden(&w.get(who, &projects_url).await);
        }

        // Rename: same roles as create.
        forbidden(&w.patch(&c.viewer.1, &url, Some("\"prj-v1\""), "X").await);
        hidden(&w.patch(&c.plain.1, &url, Some("\"prj-v1\""), "X").await);
        let renamed = w
            .patch(&c.editor.1, &url, Some("\"prj-v1\""), "Renamed")
            .await;
        assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.text);
        assert_eq!(renamed.etag(), "\"prj-v2\"");

        // Delete and restore: org owner/admin and workspace admin only.
        forbidden(&w.delete(&c.editor.1, &url).await);
        forbidden(&w.delete(&c.viewer.1, &url).await);
        hidden(&w.delete(&c.plain.1, &url).await);
        hidden(&w.delete(&c.outsider.1, &url).await);
        assert_eq!(
            w.delete(&c.wsadmin.1, &url).await.status,
            StatusCode::NO_CONTENT
        );
        let restore_url = format!("{url}/restore");
        forbidden(&w.post(&c.editor.1, &restore_url, None).await);
        forbidden(&w.post(&c.viewer.1, &restore_url, None).await);
        hidden(&w.post(&c.plain.1, &restore_url, None).await);
        let restored = w.post(&c.wsadmin.1, &restore_url, None).await;
        assert_eq!(restored.status, StatusCode::OK, "{}", restored.text);
        assert!(restored.body["deleted_at"].is_null());
        for who in [&c.admin.1, &c.owner.1] {
            assert_eq!(w.delete(who, &url).await.status, StatusCode::NO_CONTENT);
            assert_eq!(w.post(who, &restore_url, None).await.status, StatusCode::OK);
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn file_role_matrix_and_metadata_defaults() {
        let w = W::new().await;
        let c = cast(&w).await;
        let project = w.project_id(&c.owner.1, c.ws, "Design").await;
        let files_url = format!("/api/v1/projects/{project}/files");

        forbidden(&w.file(&c.viewer.1, project, "a.fig").await);
        hidden(&w.file(&c.plain.1, project, "a.fig").await);
        hidden(&w.file(&c.outsider.1, project, "a.fig").await);
        let created = w.file(&c.editor.1, project, "Logo.fig").await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        assert_eq!(created.etag(), "\"file-v1\"");
        let file = id_of(&created);
        // Metadata defaults: no content yet, head version field present but null.
        assert_eq!(created.body["size_bytes"], 0);
        assert!(created.body["sha256"].is_null());
        assert!(
            created.body.get("head_version_id").is_some()
                && created.body["head_version_id"].is_null()
        );
        assert_eq!(created.body["app"], "figcraft");
        assert_eq!(created.body["format"], "fig");
        assert_eq!(created.body["project_id"], project.to_string());
        assert_eq!(created.body["workspace_id"], c.ws.to_string());
        assert_eq!(created.body["created_by"], c.editor.0.to_string());
        assert!(created.body["deleted_at"].is_null());
        for who in [&c.wsadmin.1, &c.admin.1, &c.owner.1] {
            let r = w
                .file(who, project, &format!("by-{}.fig", Uuid::new_v4()))
                .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
        }

        let url = format!("/api/v1/files/{file}");
        // Reads: members and managers; the viewer only reads.
        for who in [
            &c.viewer.1,
            &c.editor.1,
            &c.wsadmin.1,
            &c.admin.1,
            &c.owner.1,
        ] {
            assert_eq!(w.get(who, &url).await.status, StatusCode::OK);
            assert_eq!(
                w.get(who, &files_url).await.body["items"]
                    .as_array()
                    .unwrap()
                    .len(),
                4
            );
        }
        for who in [&c.plain.1, &c.outsider.1, &w.op] {
            hidden(&w.get(who, &url).await);
            hidden(&w.get(who, &files_url).await);
        }
        // Rename, delete and restore: editor and up.
        forbidden(
            &w.patch(&c.viewer.1, &url, Some("\"file-v1\""), "x.fig")
                .await,
        );
        hidden(
            &w.patch(&c.plain.1, &url, Some("\"file-v1\""), "x.fig")
                .await,
        );
        let renamed = w
            .patch(&c.editor.1, &url, Some("\"file-v1\""), "Logo v2.fig")
            .await;
        assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.text);
        assert_eq!(renamed.etag(), "\"file-v2\"");
        assert_eq!(renamed.body["updated_by"], c.editor.0.to_string());
        forbidden(&w.delete(&c.viewer.1, &url).await);
        hidden(&w.delete(&c.plain.1, &url).await);
        assert_eq!(
            w.delete(&c.editor.1, &url).await.status,
            StatusCode::NO_CONTENT
        );
        let restore_url = format!("{url}/restore");
        forbidden(&w.post(&c.viewer.1, &restore_url, None).await);
        hidden(&w.post(&c.outsider.1, &restore_url, None).await);
        let restored = w.post(&c.editor.1, &restore_url, None).await;
        assert_eq!(restored.status, StatusCode::OK, "{}", restored.text);
        // Org admins may also restore after an owner deleted.
        assert_eq!(
            w.delete(&c.owner.1, &url).await.status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            w.post(&c.admin.1, &restore_url, None).await.status,
            StatusCode::OK
        );
        // Deleting twice is idempotent and does not change the version.
        assert_eq!(
            w.delete(&c.editor.1, &url).await.status,
            StatusCode::NO_CONTENT
        );
        let version = w.get(&c.editor.1, &url).await.body["version"].clone();
        assert_eq!(
            w.delete(&c.editor.1, &url).await.status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(w.get(&c.editor.1, &url).await.body["version"], version);
        // Restoring a live file changes nothing.
        w.post(&c.editor.1, &restore_url, None).await;
        let again = w.post(&c.editor.1, &restore_url, None).await;
        assert_eq!(again.status, StatusCode::OK);
    }

    // ---- name validation -----------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn file_names_are_validated_normalized_and_unique_by_key() {
        let w = W::new().await;
        let c = cast(&w).await;
        let project = w.project_id(&c.owner.1, c.ws, "Names").await;
        let invalid = [
            "",
            "../etc/passwd",
            "..\\windows",
            "a/b",
            "a\\b",
            "nul\0byte",
            "tab\there",
            "a:b",
            "a*b",
            "a?b",
            "a\"b",
            "a<b",
            "a>b",
            "a|b",
            ".",
            "..",
            " leading",
            "trailing ",
            "dot.",
            "CON",
            "con.txt",
            "NUL.tar.gz",
            "Com1.fig",
            "lpt9",
            "CONIN$",
            "conout$.txt",
            "Clock$",
            "COM0",
            "lpt0.log",
            "bidi\u{202E}gnp.exe",
            "zero\u{200B}width",
            "nbsp\u{00A0}here",
            &"x".repeat(256),
            &"\u{00E9}".repeat(128),
        ];
        for name in invalid {
            let r = w.file(&c.owner.1, project, name).await;
            assert_eq!(
                r.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{name:?}: {}",
                r.text
            );
            assert_eq!(
                r.body["error"]["details"]["fields"],
                json!(["name"]),
                "{name:?}"
            );
        }
        assert_eq!(w.count("SELECT count(*) FROM files").await, 0);

        // Good names, including the longest allowed and non-ASCII ones.
        for name in [
            "日本語.fig",
            "emoji 🎨.fig",
            ".hidden",
            "console.txt",
            &"x".repeat(255),
        ] {
            let r = w.file(&c.owner.1, project, name).await;
            assert_eq!(r.status, StatusCode::CREATED, "{name:?}: {}", r.text);
            assert_eq!(r.body["name"], name);
        }

        // NFC and NFD are the same name: stored in NFC, second spelling collides.
        let nfd = "cafe\u{0301}.fig";
        let first = w.file(&c.owner.1, project, nfd).await;
        assert_eq!(first.status, StatusCode::CREATED, "{}", first.text);
        assert_eq!(first.body["name"], "caf\u{00E9}.fig");
        assert_status(
            &w.file(&c.owner.1, project, "caf\u{00E9}.fig").await,
            StatusCode::CONFLICT,
            "FILE_NAME_TAKEN",
        );
        // Case collision and the ß/SS fold.
        assert_status(
            &w.file(&c.owner.1, project, "CAF\u{00C9}.FIG").await,
            StatusCode::CONFLICT,
            "FILE_NAME_TAKEN",
        );
        w.file_id(&c.owner.1, project, "Stra\u{00DF}e.fig").await;
        assert_status(
            &w.file(&c.owner.1, project, "STRASSE.fig").await,
            StatusCode::CONFLICT,
            "FILE_NAME_TAKEN",
        );
        // Compatibility look-alikes collide too, while the typed name is kept.
        w.file_id(&c.owner.1, project, "Report.psd").await;
        assert_status(
            &w.file(&c.owner.1, project, "\u{FF32}eport.psd").await,
            StatusCode::CONFLICT,
            "FILE_NAME_TAKEN",
        );
        w.file_id(&c.owner.1, project, "office.txt").await;
        assert_status(
            &w.file(&c.owner.1, project, "o\u{FB03}ce.txt").await,
            StatusCode::CONFLICT,
            "FILE_NAME_TAKEN",
        );
        let wide = w.file(&c.owner.1, project, "\u{FF21}\u{FF22}.fig").await;
        assert_eq!(wide.status, StatusCode::CREATED, "{}", wide.text);
        assert_eq!(wide.body["name"], "\u{FF21}\u{FF22}.fig");
        // The longest expansion fits the key limits.
        let long = w.file(&c.owner.1, project, &"\u{FDFA}".repeat(85)).await;
        assert_eq!(long.status, StatusCode::CREATED, "{}", long.text);
        // The same name is fine in another project.
        let other = w.project_id(&c.owner.1, c.ws, "Other").await;
        w.file_id(&c.owner.1, other, "caf\u{00E9}.fig").await;

        // Rename collisions use the same key; renaming to the own name (any case) is fine.
        let a = w.file_id(&c.owner.1, project, "alpha.fig").await;
        let b = w.file_id(&c.owner.1, project, "beta.fig").await;
        let b_url = format!("/api/v1/files/{b}");
        assert_status(
            &w.patch(&c.owner.1, &b_url, Some("\"file-v1\""), "ALPHA.fig")
                .await,
            StatusCode::CONFLICT,
            "FILE_NAME_TAKEN",
        );
        let same = w
            .patch(&c.owner.1, &b_url, Some("\"file-v1\""), "BETA.fig")
            .await;
        assert_eq!(same.status, StatusCode::OK, "{}", same.text);
        let invalid_rename = w
            .patch(&c.owner.1, &b_url, Some("\"file-v2\""), "a/b")
            .await;
        assert_eq!(invalid_rename.status, StatusCode::UNPROCESSABLE_ENTITY);
        let _ = a;

        // App and format identifiers; unknown fields are rejected.
        for (app, format, fields) in [
            ("Figcraft", "fig", json!(["app"])),
            ("a b", "fig", json!(["app"])),
            ("figcraft", "FIG", json!(["format"])),
            ("figcraft", "a/b", json!(["format"])),
            ("", "", json!(["app", "format"])),
        ] {
            let r = w
                .post(
                    &c.owner.1,
                    &format!("/api/v1/projects/{project}/files"),
                    Some(json!({"name": "ok.fig", "app": app, "format": format})),
                )
                .await;
            assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{app}/{format}");
            assert_eq!(r.body["error"]["details"]["fields"], fields);
        }
        let extra = w
            .post(
                &c.owner.1,
                &format!("/api/v1/projects/{project}/files"),
                Some(json!({"name": "ok.fig", "app": "x", "format": "y", "size_bytes": 5})),
            )
            .await;
        assert_eq!(extra.status, StatusCode::UNPROCESSABLE_ENTITY);

        // Project names: display name rules plus the same uniqueness per workspace.
        for name in ["", "   ", "a\0b", "bidi\u{202E}x", &"x".repeat(101)] {
            let r = w.project(&c.owner.1, c.ws, name).await;
            assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{name:?}");
        }
        w.project_id(&c.owner.1, c.ws, "Caf\u{00E9} Projekt").await;
        assert_status(
            &w.project(&c.owner.1, c.ws, "CAFE\u{0301} PROJEKT").await,
            StatusCode::CONFLICT,
            "PROJECT_NAME_TAKEN",
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn restore_compares_the_stored_name_key() {
        let w = W::new().await;
        let c = cast(&w).await;
        let project = w.project_id(&c.owner.1, c.ws, "Keys").await;
        let first = w.file_id(&c.owner.1, project, "a.fig").await;
        let second = w.file_id(&c.owner.1, project, "b.fig").await;
        for f in [first, second] {
            w.delete(&c.owner.1, &format!("/api/v1/files/{f}")).await;
        }
        // Both trashed rows get the same stored key although their names differ.
        sqlx::query("UPDATE files SET name_key = 'shared-key' WHERE project_id = $1")
            .bind(project)
            .execute(&w.env.pool)
            .await
            .unwrap();
        let restored = w
            .post(&c.owner.1, &format!("/api/v1/files/{first}/restore"), None)
            .await;
        assert_eq!(restored.status, StatusCode::OK, "{}", restored.text);
        // The second one collides by its stored key, not by its name.
        assert_status(
            &w.post(&c.owner.1, &format!("/api/v1/files/{second}/restore"), None)
                .await,
            StatusCode::CONFLICT,
            "FILE_NAME_TAKEN",
        );
        // Same for projects.
        let p1 = w.project_id(&c.owner.1, c.ws, "P one").await;
        let p2 = w.project_id(&c.owner.1, c.ws, "P two").await;
        for p in [p1, p2] {
            w.delete(&c.owner.1, &format!("/api/v1/projects/{p}")).await;
        }
        sqlx::query("UPDATE projects SET name_key = 'shared-key' WHERE id = ANY($1)")
            .bind(vec![p1, p2])
            .execute(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(
            w.post(&c.owner.1, &format!("/api/v1/projects/{p1}/restore"), None)
                .await
                .status,
            StatusCode::OK
        );
        assert_status(
            &w.post(&c.owner.1, &format!("/api/v1/projects/{p2}/restore"), None)
                .await,
            StatusCode::CONFLICT,
            "PROJECT_NAME_TAKEN",
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_file_name_never_reaches_a_path_or_the_logs() {
        let logs = capture_logs();
        let w = W::new().await;
        let c = cast(&w).await;
        let project = w.project_id(&c.owner.1, c.ws, "Private project xq9").await;
        let r = w.file(&c.owner.1, project, "secret-name-zz7.fig").await;
        assert_eq!(r.status, StatusCode::CREATED);
        let id = id_of(&r);
        w.patch(
            &c.owner.1,
            &format!("/api/v1/files/{id}"),
            Some("\"file-v1\""),
            "renamed-secret-yy8.fig",
        )
        .await;
        w.delete(&c.owner.1, &format!("/api/v1/files/{id}")).await;
        w.post(&c.owner.1, &format!("/api/v1/files/{id}/restore"), None)
            .await;
        // Rejected names are not logged either.
        w.file(&c.owner.1, project, "../rejected-ww6").await;

        let text = logs.text();
        for needle in [
            "file.create",
            "file.update",
            "file.delete",
            "file.restore",
            "project.create",
        ] {
            assert!(text.contains(needle), "audit lacks {needle}");
        }
        assert!(text.contains(&id.to_string()), "audit lacks the file id");
        for secret in [
            "secret-name-zz7",
            "renamed-secret-yy8",
            "rejected-ww6",
            "Private project xq9",
        ] {
            assert!(!text.contains(secret), "log leaks a name");
        }
    }

    // ---- tenant isolation ------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn projects_and_files_never_cross_organizations_workspaces_or_kinds() {
        let w = W::new().await;
        let (a_owner, b_owner) = (w.user("aowner").await, w.user("bowner").await);
        let (a_org, b_org) = (
            w.org("org-a", a_owner.0).await,
            w.org("org-b", b_owner.0).await,
        );
        let a_ws = w.ws(&a_owner.1, a_org, "a-main").await;
        let a_ws2 = w.ws(&a_owner.1, a_org, "a-second").await;
        let b_ws = w.ws(&b_owner.1, b_org, "b-main").await;
        let a_project = w.project_id(&a_owner.1, a_ws, "A project").await;
        let a_file = w.file_id(&a_owner.1, a_project, "a-secret.fig").await;
        let b_project = w.project_id(&b_owner.1, b_ws, "B project").await;
        let b_file = w.file_id(&b_owner.1, b_project, "b-secret.fig").await;

        // A member of workspace 2 only (same organization) sees nothing of workspace 1.
        let member = w.user("member").await;
        w.org_member(&a_owner.1, a_org, member.0).await;
        w.ws_add(&a_owner.1, a_ws2, member.0, "admin").await;

        let unknown = Uuid::new_v4();
        let mut actors = vec![
            ("b owner", &b_owner.1),
            ("same-org other workspace", &member.1),
        ];
        actors.push(("operator", &w.op));
        for (label, who) in actors {
            for path in [
                format!("/api/v1/projects/{a_project}"),
                format!("/api/v1/projects/{a_project}/files"),
                format!("/api/v1/files/{a_file}"),
                format!("/api/v1/workspaces/{a_ws}/projects"),
            ] {
                let r = w.get(who, &path).await;
                hidden(&r);
                let baseline = w
                    .get(
                        who,
                        &path
                            .replace(&a_project.to_string(), &unknown.to_string())
                            .replace(&a_file.to_string(), &unknown.to_string())
                            .replace(&a_ws.to_string(), &unknown.to_string()),
                    )
                    .await;
                assert_eq!(
                    r.body["error"]["message"], baseline.body["error"]["message"],
                    "{label} {path}"
                );
            }
            hidden(
                &w.patch(
                    who,
                    &format!("/api/v1/projects/{a_project}"),
                    Some("\"prj-v1\""),
                    "X",
                )
                .await,
            );
            hidden(
                &w.patch(
                    who,
                    &format!("/api/v1/files/{a_file}"),
                    Some("\"file-v1\""),
                    "X.fig",
                )
                .await,
            );
            hidden(
                &w.delete(who, &format!("/api/v1/projects/{a_project}"))
                    .await,
            );
            hidden(&w.delete(who, &format!("/api/v1/files/{a_file}")).await);
            hidden(
                &w.post(who, &format!("/api/v1/projects/{a_project}/restore"), None)
                    .await,
            );
            hidden(
                &w.post(who, &format!("/api/v1/files/{a_file}/restore"), None)
                    .await,
            );
            hidden(&w.file(who, a_project, "planted.fig").await);
            hidden(&w.project(who, a_ws, "planted").await);
        }
        // Guessing: random ids and ids of the wrong kind answer 404.
        for path in [
            format!("/api/v1/files/{unknown}"),
            format!("/api/v1/projects/{unknown}"),
            // project id in a file route and the other way round
            format!("/api/v1/files/{a_project}"),
            format!("/api/v1/projects/{a_file}"),
            format!("/api/v1/projects/{a_ws}"),
            format!("/api/v1/files/{a_ws}"),
            "/api/v1/files/not-a-uuid".to_owned(),
            "/api/v1/projects/12345".to_owned(),
        ] {
            hidden(&w.get(&a_owner.1, &path).await);
        }
        // Swapped ids: B's own file id under A's project, and so on, find nothing.
        hidden(&w.get(&b_owner.1, &format!("/api/v1/files/{a_file}")).await);
        hidden(&w.get(&a_owner.1, &format!("/api/v1/files/{b_file}")).await);
        hidden(
            &w.get(&a_owner.1, &format!("/api/v1/projects/{b_project}/files"))
                .await,
        );
        hidden(&w.project(&a_owner.1, b_ws, "cross").await);
        hidden(&w.file(&a_owner.1, b_project, "cross.fig").await);

        // Nothing leaked or changed.
        assert_eq!(w.count("SELECT count(*) FROM files").await, 2);
        assert_eq!(w.count("SELECT count(*) FROM projects").await, 2);
        assert_eq!(
            w.count("SELECT count(*) FROM files WHERE deleted_at IS NOT NULL")
                .await,
            0
        );
        assert_eq!(
            w.count("SELECT count(*) FROM files WHERE version <> 1")
                .await,
            0
        );
        // Own data stays reachable.
        assert_eq!(
            w.get(&a_owner.1, &format!("/api/v1/files/{a_file}"))
                .await
                .status,
            StatusCode::OK
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn composite_foreign_keys_and_unique_indexes_hold_below_the_api() {
        let w = W::new().await;
        let c = cast(&w).await;
        let other_owner = w.user("owner2").await;
        let other_org = w.org("second", other_owner.0).await;
        let other_ws = w.ws(&other_owner.1, other_org, "s-main").await;
        let project = w.project_id(&c.owner.1, c.ws, "P").await;
        let now = "now()";
        // A project row pointing at a workspace of another organization.
        let bad_project = sqlx::query(&format!(
            "INSERT INTO projects (id, org_id, workspace_id, name, name_key, created_by_user_id, created_at, updated_by_user_id, updated_at) \
             VALUES (gen_random_uuid(), $1, $2, 'x', 'x', $3, {now}, $3, {now})"
        ))
        .bind(c.org)
        .bind(other_ws)
        .bind(c.owner.0)
        .execute(&w.env.pool)
        .await;
        assert!(bad_project.is_err(), "project must not mix organizations");
        // A file row whose org/workspace do not match its project.
        for (org, ws) in [(other_org, c.ws), (c.org, other_ws), (other_org, other_ws)] {
            let bad_file = sqlx::query(&format!(
                "INSERT INTO files (id, org_id, workspace_id, project_id, name, name_key, app, format, created_by_user_id, created_at, updated_by_user_id, updated_at) \
                 VALUES (gen_random_uuid(), $1, $2, $3, 'x', 'x', 'a', 'b', $4, {now}, $4, {now})"
            ))
            .bind(org)
            .bind(ws)
            .bind(project)
            .bind(c.owner.0)
            .execute(&w.env.pool)
            .await;
            assert!(bad_file.is_err(), "file must not mix tenants");
        }
        // Live names are unique per key; deleted rows do not count.
        let insert = |name: &'static str, deleted: bool| {
            let sql = format!(
                "INSERT INTO files (id, org_id, workspace_id, project_id, name, name_key, app, format, created_by_user_id, created_at, updated_by_user_id, updated_at, deleted_at, deleted_by_user_id) \
                 VALUES (gen_random_uuid(), $1, $2, $3, $5, $5, 'a', 'b', $4, {now}, $4, {now}, {}, {})",
                if deleted { now } else { "NULL" },
                if deleted { "$4" } else { "NULL" }
            );
            let pool = w.env.pool.clone();
            let (org, ws, owner) = (c.org, c.ws, c.owner.0);
            async move {
                sqlx::query(&sql)
                    .bind(org)
                    .bind(ws)
                    .bind(project)
                    .bind(owner)
                    .bind(name)
                    .execute(&pool)
                    .await
            }
        };
        assert!(insert("dup", false).await.is_ok());
        assert!(insert("dup", false).await.is_err());
        assert!(insert("dup", true).await.is_ok());
        // Constraint checks on values.
        for sql in [
            "UPDATE files SET app = 'Bad App'",
            "UPDATE files SET format = ''",
            "UPDATE files SET size_bytes = -1",
            "UPDATE files SET sha256 = '\\x00'::bytea",
            "UPDATE files SET deleted_at = now()",
        ] {
            assert!(
                sqlx::query(sql).execute(&w.env.pool).await.is_err(),
                "{sql}"
            );
        }
    }

    // ---- trash, restore, retention ----------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn trash_restore_and_retention_follow_the_clock() {
        let w = W::new().await;
        let c = cast(&w).await;
        let project = w.project_id(&c.owner.1, c.ws, "Trash").await;
        let files_url = format!("/api/v1/projects/{project}/files");
        let file = w.file_id(&c.editor.1, project, "report.fig").await;
        let url = format!("/api/v1/files/{file}");

        assert_eq!(
            w.delete(&c.editor.1, &url).await.status,
            StatusCode::NO_CONTENT
        );
        // Gone from the live list, present in the trash, still readable by id with deleted_at set.
        assert!(ids(&w.get(&c.viewer.1, &files_url).await).is_empty());
        let trash = w
            .get(&c.viewer.1, &format!("{files_url}?deleted=true"))
            .await;
        assert_eq!(ids(&trash), [file.to_string()]);
        assert!(trash.body["items"][0]["deleted_at"].is_string());
        let read = w.get(&c.viewer.1, &url).await;
        assert_eq!(read.status, StatusCode::OK);
        assert!(read.body["deleted_at"].is_string());
        // A deleted file cannot be renamed.
        assert_status(
            &w.patch(&c.editor.1, &url, Some(&read.etag()), "new.fig")
                .await,
            StatusCode::CONFLICT,
            "FILE_DELETED",
        );
        // The name is free again; restoring into the taken name conflicts.
        let replacement = w.file_id(&c.editor.1, project, "REPORT.fig").await;
        assert_status(
            &w.post(&c.editor.1, &format!("{url}/restore"), None).await,
            StatusCode::CONFLICT,
            "FILE_NAME_TAKEN",
        );
        assert_eq!(
            w.delete(&c.editor.1, &format!("/api/v1/files/{replacement}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );

        // Within the retention period (30 days by default) the file comes back with its id and name.
        w.env.clock.advance(Duration::from_secs(29 * 24 * 3600));
        let editor = w.login_again("editor").await;
        let restored = w.post(&editor, &format!("{url}/restore"), None).await;
        assert_eq!(restored.status, StatusCode::OK, "{}", restored.text);
        assert_eq!(restored.body["name"], "report.fig");
        assert_eq!(id_of(&restored), file);
        assert!(restored.body["deleted_at"].is_null());
        assert_eq!(ids(&w.get(&editor, &files_url).await), [file.to_string()]);

        // After the retention period restoring answers 410; the metadata is still readable (no purge yet).
        assert_eq!(w.delete(&editor, &url).await.status, StatusCode::NO_CONTENT);
        w.env.clock.advance(Duration::from_secs(30 * 24 * 3600 + 1));
        let editor = w.login_again("editor").await;
        assert_status(
            &w.post(&editor, &format!("{url}/restore"), None).await,
            StatusCode::GONE,
            "RETENTION_EXPIRED",
        );
        assert_eq!(w.get(&editor, &url).await.status, StatusCode::OK);
        // Just inside the retention period (timestamps are stored with microsecond precision).
        let edge = w.file_id(&editor, project, "edge.fig").await;
        let edge_url = format!("/api/v1/files/{edge}");
        assert_eq!(
            w.delete(&editor, &edge_url).await.status,
            StatusCode::NO_CONTENT
        );
        w.env.clock.advance(Duration::from_secs(30 * 24 * 3600 - 1));
        let editor = w.login_again("editor").await;
        assert_eq!(
            w.post(&editor, &format!("{edge_url}/restore"), None)
                .await
                .status,
            StatusCode::OK
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_project_in_the_trash_is_read_only_and_restores_with_its_files() {
        let w = W::new().await;
        let c = cast(&w).await;
        let project = w.project_id(&c.owner.1, c.ws, "Doomed").await;
        let file = w.file_id(&c.editor.1, project, "keep.fig").await;
        let project_url = format!("/api/v1/projects/{project}");
        assert_eq!(
            w.delete(&c.wsadmin.1, &project_url).await.status,
            StatusCode::NO_CONTENT
        );

        // Trash list of projects; live list is empty; the project and its files stay readable.
        let projects_url = format!("/api/v1/workspaces/{}/projects", c.ws);
        assert!(ids(&w.get(&c.viewer.1, &projects_url).await).is_empty());
        assert_eq!(
            ids(&w
                .get(&c.viewer.1, &format!("{projects_url}?deleted=true"))
                .await),
            [project.to_string()]
        );
        let read = w.get(&c.viewer.1, &project_url).await;
        assert!(read.body["deleted_at"].is_string());
        assert_eq!(
            w.get(&c.viewer.1, &format!("/api/v1/files/{file}"))
                .await
                .status,
            StatusCode::OK
        );
        assert_eq!(
            ids(&w.get(&c.viewer.1, &format!("{project_url}/files")).await),
            [file.to_string()]
        );
        // Writes are refused.
        assert_status(
            &w.file(&c.editor.1, project, "new.fig").await,
            StatusCode::CONFLICT,
            "PROJECT_DELETED",
        );
        assert_status(
            &w.patch(
                &c.editor.1,
                &format!("/api/v1/files/{file}"),
                Some("\"file-v1\""),
                "x.fig",
            )
            .await,
            StatusCode::CONFLICT,
            "PROJECT_DELETED",
        );
        assert_status(
            &w.delete(&c.editor.1, &format!("/api/v1/files/{file}"))
                .await,
            StatusCode::CONFLICT,
            "PROJECT_DELETED",
        );
        assert_status(
            &w.patch(&c.editor.1, &project_url, Some(&read.etag()), "Renamed")
                .await,
            StatusCode::CONFLICT,
            "PROJECT_DELETED",
        );
        // The name is free; restoring into a taken name conflicts; the other one restores.
        let usurper = w.project_id(&c.owner.1, c.ws, "DOOMED").await;
        assert_status(
            &w.post(&c.wsadmin.1, &format!("{project_url}/restore"), None)
                .await,
            StatusCode::CONFLICT,
            "PROJECT_NAME_TAKEN",
        );
        assert_eq!(
            w.delete(&c.owner.1, &format!("/api/v1/projects/{usurper}"))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        let restored = w
            .post(&c.wsadmin.1, &format!("{project_url}/restore"), None)
            .await;
        assert_eq!(restored.status, StatusCode::OK, "{}", restored.text);
        assert_eq!(
            w.get(&c.editor.1, &format!("/api/v1/files/{file}"))
                .await
                .body["deleted_at"],
            Value::Null
        );
        assert_eq!(
            w.file(&c.editor.1, project, "new.fig").await.status,
            StatusCode::CREATED
        );

        // Retention applies to projects too.
        assert_eq!(
            w.delete(&c.owner.1, &project_url).await.status,
            StatusCode::NO_CONTENT
        );
        w.env.clock.advance(Duration::from_secs(31 * 24 * 3600));
        let owner = w.login_again("owner").await;
        assert_status(
            &w.post(&owner, &format!("{project_url}/restore"), None)
                .await,
            StatusCode::GONE,
            "RETENTION_EXPIRED",
        );
    }

    // ---- pagination -------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn pagination_is_stable_while_rows_are_inserted() {
        let w = W::new().await;
        let c = cast(&w).await;
        let project = w.project_id(&c.owner.1, c.ws, "Paged").await;
        let mut original = Vec::new();
        for i in 0..7 {
            // Distinct creation times, so insertion order is the listing order.
            w.env.clock.advance(Duration::from_secs(1));
            original.push(
                w.file_id(&c.owner.1, project, &format!("file-{i}.fig"))
                    .await
                    .to_string(),
            );
        }
        let base = format!("/api/v1/projects/{project}/files?limit=3");
        let page1 = w.get(&c.owner.1, &base).await;
        assert_eq!(ids(&page1), original[..3]);
        let cursor1 = page1.body["next_cursor"].as_str().unwrap().to_owned();

        // New rows arrive between the pages (later creation time, so they sort last).
        w.env.clock.advance(Duration::from_secs(1));
        let inserted = w.file_id(&c.owner.1, project, "late.fig").await.to_string();
        let page2 = w.get(&c.owner.1, &format!("{base}&cursor={cursor1}")).await;
        assert_eq!(ids(&page2), original[3..6]);
        let cursor2 = page2.body["next_cursor"].as_str().unwrap().to_owned();
        let page3 = w.get(&c.owner.1, &format!("{base}&cursor={cursor2}")).await;
        assert_eq!(ids(&page3), [original[6].clone(), inserted.clone()]);
        assert!(page3.body["next_cursor"].is_null());
        let all: Vec<String> = [ids(&page1), ids(&page2), ids(&page3)].concat();
        assert_eq!(
            all.iter().collect::<BTreeSet<_>>().len(),
            all.len(),
            "no duplicates"
        );

        // A row that is deleted between pages does not shift the others.
        let again = w.get(&c.owner.1, &base).await;
        let cursor = again.body["next_cursor"].as_str().unwrap().to_owned();
        w.delete(&c.owner.1, &format!("/api/v1/files/{}", original[0]))
            .await;
        let rest = w.get(&c.owner.1, &format!("{base}&cursor={cursor}")).await;
        assert_eq!(ids(&rest), original[3..6]);

        // Cursors are bound to their listing: project, trash switch and kind.
        let decoded = {
            use base64::Engine as _;
            String::from_utf8(
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(&cursor1)
                    .unwrap(),
            )
            .unwrap()
        };
        assert!(
            !decoded.contains(".fig") && !decoded.contains("file-"),
            "{decoded}"
        );
        let other = w.project_id(&c.owner.1, c.ws, "Other").await;
        assert_status(
            &w.get(
                &c.owner.1,
                &format!("/api/v1/projects/{other}/files?cursor={cursor1}"),
            )
            .await,
            StatusCode::BAD_REQUEST,
            "INVALID_CURSOR",
        );
        assert_status(
            &w.get(
                &c.owner.1,
                &format!("/api/v1/projects/{project}/files?deleted=true&cursor={cursor1}"),
            )
            .await,
            StatusCode::BAD_REQUEST,
            "INVALID_CURSOR",
        );
        assert_status(
            &w.get(
                &c.owner.1,
                &format!("/api/v1/workspaces/{}/projects?cursor={cursor1}", c.ws),
            )
            .await,
            StatusCode::BAD_REQUEST,
            "INVALID_CURSOR",
        );
        // Limits and switches.
        for bad in ["limit=0", "limit=abc", "deleted=maybe", "unknown=1"] {
            let r = w
                .get(
                    &c.owner.1,
                    &format!("/api/v1/projects/{project}/files?{bad}"),
                )
                .await;
            assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        }
        let big = w
            .get(
                &c.owner.1,
                &format!("/api/v1/projects/{project}/files?limit=1000"),
            )
            .await;
        assert_eq!(big.status, StatusCode::OK);
        assert_eq!(ids(&big).len(), 7);

        // Projects paginate the same way.
        for i in 0..4 {
            w.env.clock.advance(Duration::from_secs(1));
            w.project_id(&c.owner.1, c.ws, &format!("Proj {i}")).await;
        }
        let first = w
            .get(
                &c.owner.1,
                &format!("/api/v1/workspaces/{}/projects?limit=2", c.ws),
            )
            .await;
        assert_eq!(ids(&first).len(), 2);
        let next = first.body["next_cursor"].as_str().unwrap();
        let second = w
            .get(
                &c.owner.1,
                &format!("/api/v1/workspaces/{}/projects?limit=2&cursor={next}", c.ws),
            )
            .await;
        assert_eq!(ids(&second).len(), 2);
        assert!(ids(&first).iter().all(|id| !ids(&second).contains(id)));
    }

    // ---- ETags ---------------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn etags_guard_renames_of_projects_and_files() {
        let w = W::new().await;
        let c = cast(&w).await;
        let project = w.project_id(&c.owner.1, c.ws, "Etag").await;
        let file = w.file_id(&c.owner.1, project, "etag.fig").await;
        for (url, prefix) in [
            (format!("/api/v1/projects/{project}"), "prj"),
            (format!("/api/v1/files/{file}"), "file"),
        ] {
            let name = |n: &str| {
                if prefix == "file" {
                    format!("{n}.fig")
                } else {
                    n.to_owned()
                }
            };
            let v1 = format!("\"{prefix}-v1\"");
            let v2 = format!("\"{prefix}-v2\"");
            let got = w.get(&c.owner.1, &url).await;
            assert_eq!(got.etag(), v1);
            // Missing precondition, malformed one, wrong prefix.
            assert_status(
                &w.patch(&c.owner.1, &url, None, &name("A")).await,
                StatusCode::PRECONDITION_REQUIRED,
                "PRECONDITION_REQUIRED",
            );
            for bad in ["garbage", "\"org-v1\"", "\"prj-v\"", "*"] {
                let r = w.patch(&c.owner.1, &url, Some(bad), &name("A")).await;
                assert_status(&r, StatusCode::PRECONDITION_FAILED, "VERSION_CONFLICT");
                assert_eq!(r.etag(), v1, "current ETag is returned");
            }
            let ok = w.patch(&c.owner.1, &url, Some(&v1), &name("First")).await;
            assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
            assert_eq!(ok.etag(), v2);
            // The stale writer loses and learns the current version.
            let stale = w.patch(&c.owner.1, &url, Some(&v1), &name("Second")).await;
            assert_status(&stale, StatusCode::PRECONDITION_FAILED, "VERSION_CONFLICT");
            assert_eq!(stale.etag(), v2);
            assert_eq!(w.get(&c.owner.1, &url).await.body["name"], name("First"));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn concurrent_creations_of_one_name_yield_exactly_one_file() {
        let w = W::new().await;
        let c = cast(&w).await;
        let project = w.project_id(&c.owner.1, c.ws, "Race").await;
        let mut handles = Vec::new();
        for i in 0..8 {
            let (app, session) = (w.env.app.clone(), c.owner.1.clone());
            let name = if i % 2 == 0 { "race.fig" } else { "RACE.FIG" };
            handles.push(tokio::spawn(async move {
                call(
                    &app,
                    Req::new("POST", &format!("/api/v1/projects/{project}/files"))
                        .session(&session)
                        .json(json!({"name": name, "app": "figcraft", "format": "fig"})),
                )
                .await
                .status
            }));
        }
        let mut statuses = Vec::new();
        for h in handles {
            statuses.push(h.await.unwrap().as_u16());
        }
        statuses.sort_unstable();
        assert_eq!(statuses, [201, 409, 409, 409, 409, 409, 409, 409]);
        assert_eq!(w.count("SELECT count(*) FROM files").await, 1);
    }

    // ---- members leaving, CSRF -------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn removing_a_member_keeps_their_files_and_cuts_their_access() {
        let w = W::new().await;
        let c = cast(&w).await;
        let project = w.project_id(&c.editor.1, c.ws, "By editor").await;
        let file = w.file_id(&c.editor.1, project, "mine.fig").await;
        let removed = call(
            &w.env.app,
            Req::new(
                "DELETE",
                &format!("/api/v1/workspaces/{}/members/{}", c.ws, c.editor.0),
            )
            .session(&c.owner.1),
        )
        .await;
        assert_eq!(removed.status, StatusCode::NO_CONTENT, "{}", removed.text);
        hidden(&w.get(&c.editor.1, &format!("/api/v1/files/{file}")).await);
        hidden(&w.file(&c.editor.1, project, "again.fig").await);
        let kept = w.get(&c.owner.1, &format!("/api/v1/files/{file}")).await;
        assert_eq!(kept.status, StatusCode::OK);
        assert_eq!(kept.body["created_by"], c.editor.0.to_string());
        // The whole organization membership can go too; the rows stay.
        let left = call(
            &w.env.app,
            Req::new(
                "DELETE",
                &format!("/api/v1/organizations/{}/members/{}", c.org, c.editor.0),
            )
            .session(&c.owner.1),
        )
        .await;
        assert_eq!(left.status, StatusCode::NO_CONTENT, "{}", left.text);
        assert_eq!(w.count("SELECT count(*) FROM files").await, 1);
        assert_eq!(w.count("SELECT count(*) FROM projects").await, 1);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn every_unsafe_project_and_file_route_needs_session_csrf_origin_and_a_fresh_session() {
        let w = W::new().await;
        let c = cast(&w).await;
        let project = w.project_id(&c.owner.1, c.ws, "Csrf").await;
        let file = w.file_id(&c.owner.1, project, "csrf.fig").await;
        let routes: Vec<(&'static str, String, Option<Value>)> = vec![
            (
                "POST",
                format!("/api/v1/workspaces/{}/projects", c.ws),
                Some(json!({"name": "N"})),
            ),
            (
                "PATCH",
                format!("/api/v1/projects/{project}"),
                Some(json!({"name": "N"})),
            ),
            ("DELETE", format!("/api/v1/projects/{project}"), None),
            ("POST", format!("/api/v1/projects/{project}/restore"), None),
            (
                "POST",
                format!("/api/v1/projects/{project}/files"),
                Some(json!({"name": "n.fig", "app": "a", "format": "b"})),
            ),
            (
                "PATCH",
                format!("/api/v1/files/{file}"),
                Some(json!({"name": "n.fig"})),
            ),
            ("DELETE", format!("/api/v1/files/{file}"), None),
            ("POST", format!("/api/v1/files/{file}/restore"), None),
            ("GET", format!("/api/v1/workspaces/{}/projects", c.ws), None),
            ("GET", format!("/api/v1/projects/{project}"), None),
            ("GET", format!("/api/v1/projects/{project}/files"), None),
            ("GET", format!("/api/v1/files/{file}"), None),
        ];
        let build = |m: &'static str, u: &str, b: &Option<Value>| {
            let req = Req::new(m, u);
            match b {
                Some(b) => req.json(b.clone()),
                None => req,
            }
        };
        for (m, u, b) in &routes {
            assert_status(
                &call(&w.env.app, build(m, u, b)).await,
                StatusCode::UNAUTHORIZED,
                "UNAUTHENTICATED",
            );
            if *m != "GET" {
                assert_status(
                    &call(&w.env.app, build(m, u, b).cookie_only(&c.owner.1)).await,
                    StatusCode::FORBIDDEN,
                    "CSRF_FAILED",
                );
                assert_status(
                    &call(
                        &w.env.app,
                        build(m, u, b)
                            .session(&c.owner.1)
                            .origin(Some("https://evil.example")),
                    )
                    .await,
                    StatusCode::FORBIDDEN,
                    "CSRF_FAILED",
                );
            }
        }
        assert_eq!(w.count("SELECT count(*) FROM projects").await, 1);
        assert_eq!(w.count("SELECT count(*) FROM files").await, 1);
        assert_eq!(
            w.count("SELECT count(*) FROM projects WHERE deleted_at IS NOT NULL")
                .await,
            0
        );
        // An expired session (idle timeout) is a 401 on every route.
        w.env.clock.advance(Duration::from_secs(8 * 3600 + 60));
        for (m, u, b) in &routes {
            assert_status(
                &call(&w.env.app, build(m, u, b).session(&c.owner.1)).await,
                StatusCode::UNAUTHORIZED,
                "UNAUTHENTICATED",
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn figcraft_open_reuses_join_url_with_remote_file_id() {
        let mut w = W::new().await;
        let backend_state = Arc::new(OrderedState::default());
        w.env
            .configure_figcraft(
                Arc::new(OrderedBackend(Arc::clone(&backend_state))),
                "ws://figcraft.test/figcraft",
            )
            .await;
        let c = cast(&w).await;
        let project = w.project_id(&c.owner.1, c.ws, "Live").await;
        let file = w.file_id(&c.owner.1, project, "design.figcraft").await;
        let source = b"figcraft-native-test";
        upload_bytes(&w, &c.owner.1, file, source).await;
        let path = format!("/api/v1/files/{file}/live");
        let first = call(&w.env.app, Req::new("POST", &path).session(&c.owner.1)).await;
        assert_eq!(first.status, StatusCode::OK, "{}", first.text);
        assert!(
            first.body["join_url"]
                .as_str()
                .unwrap()
                .contains("ordered-file")
        );
        let second = call(&w.env.app, Req::new("POST", &path).session(&c.owner.1)).await;
        assert_eq!(second.status, StatusCode::OK, "{}", second.text);
        assert_eq!(first.body["join_url"], second.body["join_url"]);
        let stopped = call(&w.env.app, Req::new("DELETE", &path).session(&c.owner.1)).await;
        assert_eq!(stopped.status, StatusCode::NO_CONTENT, "{}", stopped.text);
        let actor_kind:String=sqlx::query_scalar("SELECT v.actor_kind FROM files f JOIN file_versions v ON v.id=f.head_version_id WHERE f.id=$1").bind(file).fetch_one(&w.env.pool).await.unwrap();
        assert_eq!(actor_kind, "figcraft_live");

        sqlx::query("UPDATE figcraft_live_files SET state='publishing',figcraft_file_id=NULL,updated_at=$2 WHERE file_id=$1")
            .bind(file).bind(w.env.clock.now() - time::Duration::minutes(2)).execute(&w.env.pool).await.unwrap();
        let mut worker = craftcloud_api::FigcraftWorkerState::default();
        craftcloud_api::figcraft_worker_tick(Arc::clone(&w.env.services), &mut worker).await;
        let after_stale_cleanup: String =
            sqlx::query_scalar("SELECT state FROM figcraft_live_files WHERE file_id=$1")
                .bind(file)
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
        assert_eq!(
            after_stale_cleanup, "republish",
            "stale intent recovery must preserve historic link references"
        );

        backend_state
            .fail_publish
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let failed_retry = call(&w.env.app, Req::new("POST", &path).session(&c.owner.1)).await;
        assert_eq!(
            failed_retry.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            failed_retry.text
        );
        let after_failure: String =
            sqlx::query_scalar("SELECT state FROM figcraft_live_files WHERE file_id=$1")
                .bind(file)
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
        assert_eq!(
            after_failure, "republish",
            "failed retry must preserve the publishable row with historical links"
        );
        backend_state
            .fail_publish
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let retried = call(&w.env.app, Req::new("POST", &path).session(&c.owner.1)).await;
        assert_eq!(retried.status, StatusCode::OK, "{}", retried.text);
        let final_state: String =
            sqlx::query_scalar("SELECT state FROM figcraft_live_files WHERE file_id=$1")
                .bind(file)
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
        assert_eq!(final_state, "live");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn figcraft_live_blocks_file_and_project_trash() {
        let mut w = W::new().await;
        w.env
            .configure_figcraft(Arc::new(FakeFigcraft), "ws://figcraft.test")
            .await;
        let c = cast(&w).await;
        let project = w
            .project_id(&c.owner.1, c.ws, "Protected live project")
            .await;
        let file = w.file_id(&c.owner.1, project, "protected.figcraft").await;
        upload_bytes(&w, &c.owner.1, file, b"native fixture").await;
        let live = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{file}/live")).session(&c.owner.1),
        )
        .await;
        assert_eq!(live.status, StatusCode::OK, "{}", live.text);

        let file_trash = call(
            &w.env.app,
            Req::new("DELETE", &format!("/api/v1/files/{file}")).session(&c.owner.1),
        )
        .await;
        assert_status(&file_trash, StatusCode::CONFLICT, "FILE_IS_LIVE");
        let project_trash = call(
            &w.env.app,
            Req::new("DELETE", &format!("/api/v1/projects/{project}")).session(&c.owner.1),
        )
        .await;
        assert_status(&project_trash, StatusCode::CONFLICT, "FILE_IS_LIVE");
        let state: (String, bool, bool) = sqlx::query_as("SELECT l.state,f.deleted_at IS NULL,p.deleted_at IS NULL FROM figcraft_live_files l JOIN files f ON f.id=l.file_id JOIN projects p ON p.id=f.project_id WHERE l.file_id=$1")
            .bind(file).fetch_one(&w.env.pool).await.unwrap();
        assert_eq!(state, ("live".into(), true, true));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn figcraft_file_grant_revoke_keeps_other_file_capability_active() {
        let mut w = W::new().await;
        w.env
            .configure_figcraft(Arc::new(FakeFigcraft), "ws://figcraft.test")
            .await;
        let c = cast(&w).await;
        let project = w.project_id(&c.owner.1, c.ws, "Scoped revoke").await;
        let file_a = w.file_id(&c.owner.1, project, "a.figcraft").await;
        let file_b = w.file_id(&c.owner.1, project, "b.figcraft").await;
        for file in [file_a, file_b] {
            upload_bytes(&w, &c.owner.1, file, b"native source").await;
            let opened = call(
                &w.env.app,
                Req::new("POST", &format!("/api/v1/files/{file}/live")).session(&c.owner.1),
            )
            .await;
            assert_eq!(opened.status, StatusCode::OK, "{}", opened.text);
            let granted = call(
                &w.env.app,
                Req::new("POST", &format!("/api/v1/files/{file}/grants"))
                    .session(&c.owner.1)
                    .json(json!({"user_id":c.plain.0.to_string(),"role":"editor"})),
            )
            .await;
            assert_eq!(granted.status, StatusCode::CREATED, "{}", granted.text);
            let joined = call(
                &w.env.app,
                Req::new("POST", &format!("/api/v1/files/{file}/live")).session(&c.plain.1),
            )
            .await;
            assert_eq!(joined.status, StatusCode::OK, "{}", joined.text);
        }
        let grant_a: Uuid = sqlx::query_scalar("SELECT id FROM grants WHERE org_id=$1 AND file_id=$2 AND grantee_user_id=$3 AND revoked_at IS NULL")
            .bind(c.org).bind(file_a).bind(c.plain.0).fetch_one(&w.env.pool).await.unwrap();
        let revoked = call(
            &w.env.app,
            Req::new("DELETE", &format!("/api/v1/grants/{grant_a}")).session(&c.owner.1),
        )
        .await;
        assert_eq!(revoked.status, StatusCode::NO_CONTENT, "{}", revoked.text);
        let mut worker = craftcloud_api::FigcraftWorkerState::default();
        craftcloud_api::figcraft_worker_tick(Arc::clone(&w.env.services), &mut worker).await;
        let revoked_a: bool = sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM figcraft_links WHERE file_id=$1 AND user_id=$2 AND access='edit'")
            .bind(file_a).bind(c.plain.0).fetch_one(&w.env.pool).await.unwrap();
        let revoked_b: bool = sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM figcraft_links WHERE file_id=$1 AND user_id=$2 AND access='edit'")
            .bind(file_b).bind(c.plain.0).fetch_one(&w.env.pool).await.unwrap();
        assert!(revoked_a, "revoked file grant left its capability active");
        assert!(
            !revoked_b,
            "revoking one file grant must preserve the same user's other-file capability"
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn figcraft_project_and_workspace_revokes_stay_in_scope() {
        let mut w = W::new().await;
        w.env
            .configure_figcraft(Arc::new(FakeFigcraft), "ws://figcraft.test")
            .await;
        let c = cast(&w).await;
        let project_a = w.project_id(&c.owner.1, c.ws, "Project A").await;
        let project_b = w.project_id(&c.owner.1, c.ws, "Project B").await;
        let file_a = w.file_id(&c.owner.1, project_a, "a.figcraft").await;
        let file_b = w.file_id(&c.owner.1, project_b, "b.figcraft").await;
        for file in [file_a, file_b] {
            upload_bytes(&w, &c.owner.1, file, b"native source").await;
            let opened = call(
                &w.env.app,
                Req::new("POST", &format!("/api/v1/files/{file}/live")).session(&c.owner.1),
            )
            .await;
            assert_eq!(opened.status, StatusCode::OK, "{}", opened.text);
        }
        let project_grant = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/projects/{project_a}/grants"))
                .session(&c.owner.1)
                .json(json!({"user_id":c.plain.0.to_string(),"role":"editor"})),
        )
        .await;
        assert_eq!(
            project_grant.status,
            StatusCode::CREATED,
            "{}",
            project_grant.text
        );
        let file_grant = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{file_b}/grants"))
                .session(&c.owner.1)
                .json(json!({"user_id":c.plain.0.to_string(),"role":"editor"})),
        )
        .await;
        assert_eq!(
            file_grant.status,
            StatusCode::CREATED,
            "{}",
            file_grant.text
        );
        for file in [file_a, file_b] {
            let opened = call(
                &w.env.app,
                Req::new("POST", &format!("/api/v1/files/{file}/live")).session(&c.plain.1),
            )
            .await;
            assert_eq!(opened.status, StatusCode::OK, "{}", opened.text);
        }
        let project_grant_id = id_of(&project_grant);
        let revoked = call(
            &w.env.app,
            Req::new("DELETE", &format!("/api/v1/grants/{project_grant_id}")).session(&c.owner.1),
        )
        .await;
        assert_eq!(revoked.status, StatusCode::NO_CONTENT, "{}", revoked.text);
        let mut worker = craftcloud_api::FigcraftWorkerState::default();
        craftcloud_api::figcraft_worker_tick(Arc::clone(&w.env.services), &mut worker).await;
        let a_revoked: bool = sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM figcraft_links WHERE file_id=$1 AND user_id=$2 AND access='edit'").bind(file_a).bind(c.plain.0).fetch_one(&w.env.pool).await.unwrap();
        let b_revoked: bool = sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM figcraft_links WHERE file_id=$1 AND user_id=$2 AND access='edit'").bind(file_b).bind(c.plain.0).fetch_one(&w.env.pool).await.unwrap();
        assert!(a_revoked);
        assert!(
            !b_revoked,
            "project grant revoke must not affect another project in the organization"
        );

        let workspace_b = w.ws(&c.owner.1, c.org, "secondary").await;
        w.ws_add(&c.owner.1, workspace_b, c.editor.0, "editor")
            .await;
        let project_c = w.project_id(&c.owner.1, workspace_b, "Project C").await;
        let file_c = w.file_id(&c.owner.1, project_c, "c.figcraft").await;
        upload_bytes(&w, &c.owner.1, file_c, b"native source").await;
        for file in [file_a, file_c] {
            let opened = call(
                &w.env.app,
                Req::new("POST", &format!("/api/v1/files/{file}/live")).session(&c.owner.1),
            )
            .await;
            assert_eq!(opened.status, StatusCode::OK, "{}", opened.text);
            let joined = call(
                &w.env.app,
                Req::new("POST", &format!("/api/v1/files/{file}/live")).session(&c.editor.1),
            )
            .await;
            assert_eq!(joined.status, StatusCode::OK, "{}", joined.text);
        }
        let role = call(
            &w.env.app,
            Req::new(
                "PATCH",
                &format!("/api/v1/workspaces/{}/members/{}", c.ws, c.editor.0),
            )
            .session(&c.owner.1)
            .json(json!({"role":"viewer"})),
        )
        .await;
        assert_eq!(role.status, StatusCode::OK, "{}", role.text);
        worker.reconcile_now();
        craftcloud_api::figcraft_worker_tick(Arc::clone(&w.env.services), &mut worker).await;
        let main_revoked: bool = sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM figcraft_links WHERE file_id=$1 AND user_id=$2 AND access='edit'").bind(file_a).bind(c.editor.0).fetch_one(&w.env.pool).await.unwrap();
        let other_workspace_revoked: bool = sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM figcraft_links WHERE file_id=$1 AND user_id=$2 AND access='edit'").bind(file_c).bind(c.editor.0).fetch_one(&w.env.pool).await.unwrap();
        assert!(
            main_revoked,
            "workspace role change must revoke links in its workspace"
        );
        assert!(
            !other_workspace_revoked,
            "workspace role change must preserve link in another workspace"
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn figcraft_stop_orders_revoke_snapshot_remove_and_recovers_failures() {
        let mut w = W::new().await;
        let state = Arc::new(OrderedState::default());
        w.env
            .configure_figcraft(
                Arc::new(OrderedBackend(Arc::clone(&state))),
                "ws://figcraft.test",
            )
            .await;
        let c = cast(&w).await;
        let project = w.project_id(&c.owner.1, c.ws, "Stop ordering").await;
        let file = w.file_id(&c.owner.1, project, "stop.figcraft").await;
        upload_bytes(&w, &c.owner.1, file, b"native source").await;
        let opened = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{file}/live")).session(&c.owner.1),
        )
        .await;
        assert_eq!(opened.status, StatusCode::OK, "{}", opened.text);
        state.calls.lock().unwrap().clear();

        state
            .fail_export
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let stopped = call(
            &w.env.app,
            Req::new("DELETE", &format!("/api/v1/files/{file}/live")).session(&c.owner.1),
        )
        .await;
        assert_eq!(
            stopped.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            stopped.text
        );
        assert_eq!(*state.calls.lock().unwrap(), ["revoke", "export"]);
        let after_export_failure: (String, bool) = sqlx::query_as(
            "SELECT state,final_snapshot_done FROM figcraft_live_files WHERE file_id=$1",
        )
        .bind(file)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(after_export_failure, ("unpublishing".into(), false));
        assert!(
            state
                .remote_present
                .load(std::sync::atomic::Ordering::SeqCst),
            "failed final export must keep remote file"
        );

        state
            .fail_export
            .store(false, std::sync::atomic::Ordering::SeqCst);
        state
            .fail_remove
            .store(true, std::sync::atomic::Ordering::SeqCst);
        state.calls.lock().unwrap().clear();
        let mut worker = craftcloud_api::FigcraftWorkerState::default();
        craftcloud_api::figcraft_worker_tick(Arc::clone(&w.env.services), &mut worker).await;
        assert_eq!(
            *state.calls.lock().unwrap(),
            ["export"],
            "recovery must save final snapshot before removing"
        );
        let after_snapshot: (String, bool) = sqlx::query_as(
            "SELECT state,final_snapshot_done FROM figcraft_live_files WHERE file_id=$1",
        )
        .bind(file)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(after_snapshot, ("unpublishing".into(), true));
        assert!(
            state
                .remote_present
                .load(std::sync::atomic::Ordering::SeqCst)
        );

        state.calls.lock().unwrap().clear();
        craftcloud_api::figcraft_worker_tick(Arc::clone(&w.env.services), &mut worker).await;
        assert_eq!(*state.calls.lock().unwrap(), ["remove"]);
        let after_remove_failure: (String, bool) = sqlx::query_as(
            "SELECT state,final_snapshot_done FROM figcraft_live_files WHERE file_id=$1",
        )
        .bind(file)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(after_remove_failure, ("unpublishing".into(), true));
        assert!(
            state
                .remote_present
                .load(std::sync::atomic::Ordering::SeqCst)
        );

        state
            .fail_remove
            .store(false, std::sync::atomic::Ordering::SeqCst);
        state.calls.lock().unwrap().clear();
        craftcloud_api::figcraft_worker_tick(Arc::clone(&w.env.services), &mut worker).await;
        assert_eq!(*state.calls.lock().unwrap(), ["remove"]);
        assert!(
            !state
                .remote_present
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        let final_state: String =
            sqlx::query_scalar("SELECT state FROM figcraft_live_files WHERE file_id=$1")
                .bind(file)
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
        assert_eq!(final_state, "republish");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn figcraft_snapshot_quota_and_revoked_session_preserve_stored_blob() {
        let mut w = W::new().await;
        w.env
            .configure_figcraft(Arc::new(FakeFigcraft), "ws://figcraft.test")
            .await;
        let c = cast(&w).await;
        let project = w.project_id(&c.owner.1, c.ws, "Snapshot quota").await;
        let file = w.file_id(&c.owner.1, project, "quota.figcraft").await;
        let source = b"native source that fits quota";
        upload_bytes(&w, &c.owner.1, file, source).await;
        let opened = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{file}/live")).session(&c.owner.1),
        )
        .await;
        assert_eq!(opened.status, StatusCode::OK, "{}", opened.text);
        sqlx::query("UPDATE workspaces SET quota_bytes=committed_bytes WHERE id=$1")
            .bind(c.ws)
            .execute(&w.env.pool)
            .await
            .unwrap();
        let before: i64 = sqlx::query_scalar("SELECT count(*) FROM file_versions WHERE file_id=$1")
            .bind(file)
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        let exhausted = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{file}/live/snapshot")).session(&c.owner.1),
        )
        .await;
        assert_status(&exhausted, StatusCode::PAYLOAD_TOO_LARGE, "QUOTA_EXCEEDED");
        let after: i64 = sqlx::query_scalar("SELECT count(*) FROM file_versions WHERE file_id=$1")
            .bind(file)
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(after, before, "quota rejection must not create a version");
        let live_state: String =
            sqlx::query_scalar("SELECT state FROM figcraft_live_files WHERE file_id=$1")
                .bind(file)
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
        assert_eq!(
            live_state, "live",
            "quota rejection must leave remote collaboration active"
        );

        sqlx::query("UPDATE workspaces SET quota_bytes=NULL WHERE id=$1")
            .bind(c.ws)
            .execute(&w.env.pool)
            .await
            .unwrap();
        sqlx::query("CREATE FUNCTION ac06_revoke_session_after_upload_store() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.state='stored' AND OLD.state IS DISTINCT FROM NEW.state THEN UPDATE sessions SET revoked_at=NEW.updated_at WHERE user_id=NEW.created_by_user_id AND revoked_at IS NULL; END IF; RETURN NEW; END $$")
            .execute(&w.env.pool).await.unwrap();
        sqlx::query("CREATE TRIGGER ac06_revoke_session_after_upload_store AFTER UPDATE OF state ON upload_sessions FOR EACH ROW EXECUTE FUNCTION ac06_revoke_session_after_upload_store()")
            .execute(&w.env.pool).await.unwrap();
        let before: i64 = sqlx::query_scalar("SELECT count(*) FROM file_versions WHERE file_id=$1")
            .bind(file)
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        let revoked = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{file}/live/snapshot")).session(&c.owner.1),
        )
        .await;
        assert_status(&revoked, StatusCode::FORBIDDEN, "ACCESS_CHANGED");
        sqlx::query("DROP TRIGGER ac06_revoke_session_after_upload_store ON upload_sessions")
            .execute(&w.env.pool)
            .await
            .unwrap();
        sqlx::query("DROP FUNCTION ac06_revoke_session_after_upload_store()")
            .execute(&w.env.pool)
            .await
            .unwrap();
        let after: i64 = sqlx::query_scalar("SELECT count(*) FROM file_versions WHERE file_id=$1")
            .bind(file)
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(
            after, before,
            "revoked session must not commit a snapshot version"
        );
        let staged: (String, String) = sqlx::query_as("SELECT b.object_key,b.state FROM upload_sessions u JOIN blobs b ON b.id=u.blob_id WHERE u.file_id=$1 AND u.state='stored' ORDER BY u.created_at DESC LIMIT 1")
            .bind(file).fetch_one(&w.env.pool).await.unwrap();
        assert_eq!(staged.1, "staged");
        let object_path = w
            .env
            .storage_dir
            .join("objects")
            .join(&staged.0[..2])
            .join(&staged.0[2..4])
            .join(&staged.0);
        assert!(
            object_path.exists(),
            "DB-referenced staged blob was deleted after session revocation"
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn figcraft_link_creation_rechecks_demotion_after_remote_call() {
        let mut w = W::new().await;
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let backend = Arc::new(GatedBackend {
            creates: std::sync::atomic::AtomicUsize::new(0),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            revokes: std::sync::atomic::AtomicUsize::new(0),
        });
        w.env
            .configure_figcraft(backend.clone(), "ws://figcraft.test/figcraft")
            .await;
        let c = cast(&w).await;
        let project = w.project_id(&c.owner.1, c.ws, "Live race").await;
        let file = w.file_id(&c.owner.1, project, "race.figcraft").await;
        let source = b"native source";
        upload_bytes(&w, &c.owner.1, file, source).await;
        let owner_link = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{file}/live")).session(&c.owner.1),
        )
        .await;
        assert_eq!(owner_link.status, StatusCode::OK, "{}", owner_link.text);
        let grant = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{file}/grants"))
                .session(&c.owner.1)
                .json(json!({"user_id":c.plain.0.to_string(),"role":"editor"})),
        )
        .await;
        assert_eq!(grant.status, StatusCode::CREATED, "{}", grant.text);
        let grant_id = grant.body["id"].as_str().unwrap().to_owned();
        let app = w.env.app.clone();
        let editor = c.plain.1.clone();
        let open = tokio::spawn(async move {
            call(
                &app,
                Req::new("POST", &format!("/api/v1/files/{file}/live")).session(&editor),
            )
            .await
        });
        let entered_wait = Arc::clone(&entered);
        tokio::task::spawn_blocking(move || entered_wait.wait())
            .await
            .unwrap();
        let changed = call(
            &w.env.app,
            Req::new("PATCH", &format!("/api/v1/grants/{grant_id}"))
                .session(&c.owner.1)
                .if_match("\"grant-v1\"")
                .json(json!({"role":"viewer"})),
        )
        .await;
        assert_eq!(changed.status, StatusCode::OK, "{}", changed.text);
        let release_wait = Arc::clone(&release);
        tokio::task::spawn_blocking(move || release_wait.wait())
            .await
            .unwrap();
        let result = open.await.unwrap();
        assert_eq!(
            (result.status, result.code()),
            (StatusCode::FORBIDDEN, "ACCESS_CHANGED"),
            "{}",
            result.text
        );
        assert_eq!(
            backend.revokes.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "unpersisted remote capability must be revoked"
        );
        let active:i64=sqlx::query_scalar("SELECT count(*) FROM figcraft_links WHERE file_id=$1 AND user_id=$2 AND revoked_at IS NULL").bind(file).bind(c.plain.0).fetch_one(&w.env.pool).await.unwrap();
        assert_eq!(active, 0);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL and CC_TEST_FIGCRAFT_SERVER"]
    async fn figcraft_grant_revoke_disconnects_native_websocket() {
        use craftcloud_figcraft::Client;
        use std::{
            net::{TcpListener, TcpStream},
            process::{Child, Command, Stdio},
            time::Duration,
        };
        use tokio::time::timeout;

        let Ok(server_exe) = std::env::var("CC_TEST_FIGCRAFT_SERVER") else {
            return;
        };
        struct ChildGuard(Child);
        impl ChildGuard {
            fn stop(&mut self) {
                if self.0.try_wait().ok().flatten().is_none() {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
        }
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                self.stop();
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        drop(listener);
        let data = std::env::temp_dir().join(format!("craftcloud-figcraft-api-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&data).unwrap();
        let child = Command::new(&server_exe)
            .args(["--listen", &host, "--public-url"])
            .arg(format!("ws://{host}/figcraft/"))
            .arg("--data")
            .arg(&data)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut _child = ChildGuard(child);
        let mut ready = false;
        for _ in 0..100 {
            if TcpStream::connect(&host).is_ok() {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ready, "FigCraft server did not start");
        let key = std::fs::read_to_string(data.join("server.key"))
            .unwrap()
            .trim()
            .to_owned();

        let mut w = W::new().await;
        let internal = format!("ws://{host}/figcraft/");
        w.env
            .configure_figcraft(Arc::new(Client::new(&internal, key).unwrap()), &internal)
            .await;
        let c = cast(&w).await;
        let project = w.project_id(&c.owner.1, c.ws, "Native live").await;
        let file = w.file_id(&c.owner.1, project, "native.figcraft").await;
        let mut document = figcraft_doc::Document::new("Native API E2E");
        let page = document.first_page().unwrap();
        let shape = document
            .create(
                page,
                figcraft_doc::NodeKind::Rectangle,
                "Initial live shape",
                12.0,
                18.0,
                120.0,
                80.0,
            )
            .unwrap();
        let png = vec![
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1,
            8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 8, 215, 99, 248, 207,
            192, 240, 31, 0, 5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66,
            96, 130,
        ];
        let image_hash = figcraft_format::image_hash(&png);
        document.images.insert(
            image_hash.clone(),
            figcraft_doc::ImageData {
                bytes: Arc::new(png.clone()),
                width: 1,
                height: 1,
            },
        );
        document
            .node_mut(shape)
            .unwrap()
            .fills
            .push(figcraft_doc::color::Paint::image(
                image_hash.clone(),
                figcraft_doc::color::ImageScaleMode::Fit,
            ));
        let original_version = figcraft_format::Version::new(
            "source-1",
            "Original checkpoint",
            "AC-06 archive fixture",
            1,
            Arc::new(document.clone()),
        );
        let source = figcraft_format::save_bytes_with_history(
            &document,
            Some(&png),
            &[original_version],
            &[],
        )
        .unwrap();
        upload_bytes(&w, &c.owner.1, file, &source).await;

        let added = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{file}/grants"))
                .session(&c.owner.1)
                .json(json!({"user_id":c.plain.0.to_string(),"role":"editor"})),
        )
        .await;
        assert_eq!(added.status, StatusCode::CREATED, "{}", added.text);
        let editor_grant = added.body["id"].as_str().unwrap().to_owned();
        let viewer_added = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{file}/grants"))
                .session(&c.owner.1)
                .json(json!({"user_id":c.viewer.0.to_string(),"role":"viewer"})),
        )
        .await;
        assert_eq!(
            viewer_added.status,
            StatusCode::CREATED,
            "{}",
            viewer_added.text
        );
        let joined = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{file}/live")).session(&c.plain.1),
        )
        .await;
        assert_eq!(joined.status, StatusCode::OK, "{}", joined.text);
        let issued_url = url::Url::parse(joined.body["join_url"].as_str().unwrap()).unwrap();
        let issued_server = issued_url
            .query_pairs()
            .find(|(name, _)| name == "server")
            .map(|(_, value)| value.into_owned());
        assert_eq!(issued_server.as_deref(), Some(internal.as_str()));
        let (mut editor_socket, mut editor) = native_join(
            joined.body["join_url"].as_str().unwrap(),
            "Native API editor",
        );
        let viewer_join = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{file}/live")).session(&c.viewer.1),
        )
        .await;
        assert_eq!(viewer_join.status, StatusCode::OK, "{}", viewer_join.text);
        let (mut viewer_socket, mut viewer) = native_join(
            viewer_join.body["join_url"].as_str().unwrap(),
            "Native API viewer",
        );
        let shape = editor
            .document()
            .nodes()
            .find(|n| n.name == "Initial live shape")
            .unwrap()
            .id;
        let mut changed = editor.document().clone();
        changed.node_mut(shape).unwrap().name = "Edited by native editor".into();
        let batch = editor.edit(&changed).unwrap().unwrap();
        editor_socket
            .send(tungstenite::Message::Binary(
                figcraft_collab::message::encode(&batch).unwrap().into(),
            ))
            .unwrap();
        loop {
            if let msg @ figcraft_collab::message::ServerMsg::Changes(_) =
                native_message(&mut editor_socket)
            {
                editor.receive(msg, &figcraft_collab::NoRecompute).unwrap();
                break;
            }
        }
        let mut forbidden = viewer.document().clone();
        forbidden.node_mut(shape).unwrap().name = "Viewer forbidden edit".into();
        let batch = viewer.edit(&forbidden).unwrap().unwrap();
        viewer_socket
            .send(tungstenite::Message::Binary(
                figcraft_collab::message::encode(&batch).unwrap().into(),
            ))
            .unwrap();
        loop {
            let message = native_message(&mut viewer_socket);
            match message {
                msg @ figcraft_collab::message::ServerMsg::Changes(_) => {
                    viewer.receive(msg, &figcraft_collab::NoRecompute).unwrap();
                }
                figcraft_collab::message::ServerMsg::Reject { .. } => break,
                _ => {}
            }
        }

        let snapshot = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{file}/live/snapshot")).session(&c.owner.1),
        )
        .await;
        assert_eq!(snapshot.status, StatusCode::CREATED, "{}", snapshot.text);
        assert_eq!(snapshot.body["actor_kind"], "figcraft_live");
        let version = snapshot.body["id"]
            .as_str()
            .or_else(|| snapshot.body["version_id"].as_str())
            .expect("snapshot version id");
        let content = call(
            &w.env.app,
            Req::new(
                "GET",
                &format!("/api/v1/files/{file}/versions/{version}/content"),
            )
            .session(&c.owner.1),
        )
        .await;
        assert_eq!(content.status, StatusCode::OK, "{}", content.text);
        let native_bytes = content.bytes;
        assert_eq!(
            snapshot.body["size_bytes"].as_u64(),
            Some(native_bytes.len() as u64)
        );
        let native_sha = Sha256::digest(&native_bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        assert_eq!(
            snapshot.body["sha256"].as_str(),
            Some(native_sha.as_str()),
            "download bytes must match the committed version digest"
        );
        let loaded = figcraft_format::load_bytes_with_versions(&native_bytes).unwrap();
        assert!(
            loaded.issues.is_empty(),
            "snapshot archive has load issues: {:?}",
            loaded.issues
        );
        assert!(
            loaded
                .versions
                .iter()
                .any(|v| v.name == "Original checkpoint"),
            "snapshot lost source history"
        );
        assert_eq!(
            loaded.doc.node(shape).unwrap().name,
            "Edited by native editor"
        );
        assert_eq!(
            loaded.doc.images.get(&image_hash).unwrap().bytes.as_slice(),
            png.as_slice()
        );
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&native_bytes)).unwrap();
        let mut thumbnail = Vec::new();
        std::io::Read::read_to_end(
            &mut archive.by_name("thumbnail.png").unwrap(),
            &mut thumbnail,
        )
        .unwrap();
        assert_eq!(thumbnail, png, "snapshot lost original thumbnail");
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(craftcloud_api::run_figcraft_worker(
            Arc::clone(&w.env.services),
            stop_rx,
        ));
        let revoke_started = std::time::Instant::now();
        let revoked = call(
            &w.env.app,
            Req::new("DELETE", &format!("/api/v1/grants/{editor_grant}")).session(&c.owner.1),
        )
        .await;
        assert_eq!(revoked.status, StatusCode::NO_CONTENT, "{}", revoked.text);
        let elapsed = timeout(
            Duration::from_secs(8),
            tokio::task::spawn_blocking(move || {
                loop {
                    match editor_socket.read() {
                        Err(tungstenite::Error::Io(error))
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                            ) =>
                        {
                            panic!("native socket timed out instead of being disconnected");
                        }
                        Err(_) | Ok(tungstenite::Message::Close(_)) => {
                            return revoke_started.elapsed();
                        }
                        Ok(_) => {}
                    }
                }
            }),
        )
        .await
        .expect("grant revoke did not close native WebSocket")
        .unwrap();
        assert!(
            elapsed < Duration::from_secs(8),
            "disconnect latency exceeded bound: {elapsed:?}"
        );
        println!(
            "AC-06 native grant revoke to WebSocket disconnect: {} ms",
            elapsed.as_millis()
        );
        assert!(
            native_link_refused(joined.body["join_url"].as_str().unwrap()),
            "revoked native capability still joined"
        );
        let denied = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{file}/live")).session(&c.plain.1),
        )
        .await;
        assert!(
            matches!(denied.status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN),
            "revoked grant still authorizes API link: {} {}",
            denied.status,
            denied.text
        );
        drop(viewer_socket);
        let _ = stop_tx.send(true);
        let _ = worker.await;
        _child.stop();
        let restarted = Command::new(&server_exe)
            .args(["--listen", &host, "--public-url"])
            .arg(format!("ws://{host}/figcraft/"))
            .arg("--data")
            .arg(&data)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut _restarted = ChildGuard(restarted);
        let mut ready = false;
        for _ in 0..100 {
            if TcpStream::connect(&host).is_ok() {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ready, "FigCraft server did not restart");
        let (persisted_socket, persisted_client) = native_join(
            viewer_join.body["join_url"].as_str().unwrap(),
            "After restart",
        );
        assert_eq!(
            persisted_client.document().node(shape).unwrap().name,
            "Edited by native editor"
        );
        drop(persisted_socket);
        drop(_restarted);
        drop(_child);
        let _ = std::fs::remove_dir_all(data);
    }
}
