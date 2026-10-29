//! HTTP-level tests of projects and file metadata (CC-F06): role matrix,
//! name validation, tenant isolation (IDOR), trash and retention, pagination
//! stability, ETags, CSRF and logging. All tests need PostgreSQL
//! (`cargo xtask db-test`).

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use crate::common::{Req, Resp, Session, TestEnv, call, capture_logs};

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
}
