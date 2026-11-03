//! HTTP-level tests of the audit log (CC-F11): one event per committed change,
//! fail-closed writes, replays, aggregated denials and login failures, access
//! rules, filters, paging, export, download policy and retention. All tests
//! need PostgreSQL (`cargo xtask db-test`).

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::StatusCode;
    use craftcloud_api::maintenance::{
        Maintenance, MaintenanceSettings, RunOptions, RunOutcome, Trigger,
    };
    use craftcloud_api::{ApiConfig, AuthSettings};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use tokio::sync::watch;
    use uuid::Uuid;

    use crate::common::{PASSWORD, Req, Resp, Session, TestEnv, call, call_stream, settings};

    // ---- helpers ---------------------------------------------------------------------

    fn id_of(r: &Resp) -> Uuid {
        Uuid::parse_str(r.body["id"].as_str().unwrap()).unwrap()
    }

    fn sha_hex(data: &[u8]) -> String {
        Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

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

        async fn add_member(&self, actor: &Session, org: Uuid, user: Uuid, role: &str) -> Resp {
            self.json(
                "POST",
                &format!("/api/v1/organizations/{org}/members"),
                actor,
                json!({"user_id": user.to_string(), "role": role}),
            )
            .await
        }

        async fn ws(&self, actor: &Session, org: Uuid, slug: &str) -> Resp {
            self.json(
                "POST",
                &format!("/api/v1/organizations/{org}/workspaces"),
                actor,
                json!({"name": format!("WS {slug}"), "slug": slug}),
            )
            .await
        }

        async fn events(&self) -> BTreeMap<String, i64> {
            let rows: Vec<(String, i64)> = sqlx::query_as(
                "SELECT action, count(*) FROM audit_events WHERE result = 'success' GROUP BY action",
            )
            .fetch_all(&self.env.pool)
            .await
            .unwrap();
            rows.into_iter().collect()
        }

        async fn count(&self, sql: &str) -> i64 {
            self.env.count(sql).await
        }

        /// Events outside a transaction are written by a task of their own
        /// after the response: waits until the query counts `want`.
        async fn wait_count(&self, sql: &str, want: i64) {
            let mut last = -1;
            for _ in 0..100 {
                last = self.count(sql).await;
                if last == want {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("{sql}: expected {want}, found {last}");
        }

        async fn audit(&self, who: &Session, org: Uuid, query: &str) -> Resp {
            self.get(who, &format!("/api/v1/organizations/{org}/audit{query}"))
                .await
        }

        /// Uploads and commits `data` as the next version of `file`.
        async fn push(&self, who: &Session, file: Uuid, data: &[u8]) -> Uuid {
            let head = self.get(who, &format!("/api/v1/files/{file}")).await;
            let created = self
                .json(
                    "POST",
                    &format!("/api/v1/files/{file}/uploads"),
                    who,
                    json!({"size_bytes": data.len(), "sha256": sha_hex(data)}),
                )
                .await;
            assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
            let upload = Uuid::parse_str(created.body["upload_id"].as_str().unwrap()).unwrap();
            let put = self
                .send(
                    Req::new("PUT", &format!("/api/v1/uploads/{upload}/content"))
                        .session(who)
                        .body(Body::from(data.to_vec())),
                )
                .await;
            assert_eq!(put.status, StatusCode::OK, "{}", put.text);
            let mut commit = Req::new("POST", &format!("/api/v1/files/{file}/versions"))
                .session(who)
                .json(json!({"upload_id": upload.to_string()}));
            commit = match head.body["head_version_id"].as_str() {
                None => commit.header("If-None-Match", "*"),
                Some(head) => commit.header("If-Match", &format!("\"version:{head}\"")),
            };
            let r = self.send(commit).await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            id_of(&r)
        }
    }

    /// Org `acme` with owner, an org admin (bob) and a plain member (carol).
    struct Cast {
        org: Uuid,
        owner: (Uuid, Session),
        bob: (Uuid, Session),
        carol: (Uuid, Session),
    }

    async fn cast(w: &W) -> Cast {
        let owner = w.user("owner").await;
        let org = w.org("acme", owner.0).await;
        let bob = w.user("bob").await;
        let carol = w.user("carol").await;
        assert_eq!(
            w.add_member(&owner.1, org, bob.0, "admin").await.status,
            StatusCode::CREATED
        );
        assert_eq!(
            w.add_member(&owner.1, org, carol.0, "member").await.status,
            StatusCode::CREATED
        );
        Cast {
            org,
            owner,
            bob,
            carol,
        }
    }

    // ---- one event per change ----------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn every_sensitive_change_records_exactly_one_event() {
        let w = W::new().await;
        let c = cast(&w).await;
        let (org, owner, bob) = (c.org, &c.owner.1, &c.bob.1);

        // Organization settings and roles.
        let got = w.get(owner, &format!("/api/v1/organizations/{org}")).await;
        let patched = w
            .send(
                Req::new("PATCH", &format!("/api/v1/organizations/{org}"))
                    .session(owner)
                    .if_match(&got.etag())
                    .json(json!({"name": "Acme Inc"})),
            )
            .await;
        assert_eq!(patched.status, StatusCode::OK, "{}", patched.text);
        let role_url = format!("/api/v1/organizations/{org}/members/{}", c.carol.0);
        for (role, changed) in [("admin", true), ("admin", false), ("member", true)] {
            let r = w
                .json("PATCH", &role_url, owner, json!({"role": role}))
                .await;
            assert_eq!(r.status, StatusCode::OK, "{} (changed: {changed})", r.text);
        }

        // Workspace and its members.
        let ws = w.ws(bob, org, "main").await;
        assert_eq!(ws.status, StatusCode::CREATED, "{}", ws.text);
        let ws_id = id_of(&ws);
        let renamed = w
            .send(
                Req::new("PATCH", &format!("/api/v1/workspaces/{ws_id}"))
                    .session(bob)
                    .if_match(&ws.etag())
                    .json(json!({"name": "Main"})),
            )
            .await;
        assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.text);
        let base = format!("/api/v1/workspaces/{ws_id}/members");
        assert_eq!(
            w.json(
                "POST",
                &base,
                bob,
                json!({"user_id": c.carol.0.to_string(), "role": "editor"})
            )
            .await
            .status,
            StatusCode::CREATED
        );
        assert_eq!(
            w.json(
                "PATCH",
                &format!("{base}/{}", c.carol.0),
                bob,
                json!({"role": "viewer"})
            )
            .await
            .status,
            StatusCode::OK
        );
        assert_eq!(
            w.json(
                "PATCH",
                &format!("{base}/{}", c.carol.0),
                bob,
                json!({"role": "editor"})
            )
            .await
            .status,
            StatusCode::OK
        );
        assert_eq!(
            w.send(Req::new("DELETE", &format!("{base}/{}", c.carol.0)).session(bob))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        // Quotas.
        assert_eq!(
            w.json(
                "PUT",
                &format!("/api/v1/workspaces/{ws_id}/quota"),
                owner,
                json!({"quota_bytes": 1_000_000})
            )
            .await
            .status,
            StatusCode::OK
        );
        assert_eq!(
            w.json(
                "PUT",
                &format!("/api/v1/admin/organizations/{org}/quota"),
                &w.op,
                json!({"quota_bytes": 5_000_000})
            )
            .await
            .status,
            StatusCode::OK
        );

        // Projects and files.
        let project = w
            .json(
                "POST",
                &format!("/api/v1/workspaces/{ws_id}/projects"),
                bob,
                json!({"name": "Logo"}),
            )
            .await;
        assert_eq!(project.status, StatusCode::CREATED, "{}", project.text);
        let project_id = id_of(&project);
        let p_url = format!("/api/v1/projects/{project_id}");
        let r = w
            .send(
                Req::new("PATCH", &p_url)
                    .session(bob)
                    .if_match(&project.etag())
                    .json(json!({"name": "Logo 2"})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        let file = w
            .json(
                "POST",
                &format!("{p_url}/files"),
                bob,
                json!({"name": "a.fig", "app": "figcraft", "format": "fig"}),
            )
            .await;
        assert_eq!(file.status, StatusCode::CREATED, "{}", file.text);
        let file_id = id_of(&file);
        let f_url = format!("/api/v1/files/{file_id}");
        let r = w
            .send(
                Req::new("PATCH", &f_url)
                    .session(bob)
                    .if_match(&file.etag())
                    .json(json!({"name": "b.fig"})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        let v1 = w.push(bob, file_id, b"first version").await;
        let _v2 = w.push(bob, file_id, b"second version").await;
        let head = w.get(bob, &f_url).await.body["head_version_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let restored = w
            .send(
                Req::new("POST", &format!("{f_url}/versions/{v1}/restore"))
                    .session(bob)
                    .header("If-Match", &format!("\"version:{head}\"")),
            )
            .await;
        assert_eq!(restored.status, StatusCode::CREATED, "{}", restored.text);
        // A full download is recorded, a continuation range is not.
        let full = w.get(bob, &format!("{f_url}/content")).await;
        assert_eq!(full.status, StatusCode::OK);
        let tail = w
            .send(
                Req::new("GET", &format!("{f_url}/content"))
                    .cookie_only(bob)
                    .header("Range", "bytes=3-"),
            )
            .await;
        assert_eq!(tail.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            w.send(Req::new("DELETE", &f_url).session(bob)).await.status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            w.send(Req::new("POST", &format!("{f_url}/restore")).session(bob))
                .await
                .status,
            StatusCode::OK
        );
        assert_eq!(
            w.send(Req::new("DELETE", &p_url).session(bob)).await.status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            w.send(Req::new("POST", &format!("{p_url}/restore")).session(bob))
                .await
                .status,
            StatusCode::OK
        );

        // Invitations: create two, revoke one, accept one (creates the account).
        let inv_url = format!("/api/v1/organizations/{org}/invitations");
        let first = w
            .json(
                "POST",
                &inv_url,
                owner,
                json!({"email": "dave@example.test", "org_role": "member"}),
            )
            .await;
        assert_eq!(first.status, StatusCode::CREATED, "{}", first.text);
        let second = w
            .json(
                "POST",
                &inv_url,
                owner,
                json!({"email": "erin@example.test", "org_role": "member"}),
            )
            .await;
        assert_eq!(second.status, StatusCode::CREATED, "{}", second.text);
        let revoked = w
            .send(
                Req::new(
                    "DELETE",
                    &format!(
                        "{inv_url}/{}",
                        second.body["invitation_id"].as_str().unwrap()
                    ),
                )
                .session(owner),
            )
            .await;
        assert_eq!(revoked.status, StatusCode::NO_CONTENT, "{}", revoked.text);
        let token = first.body["invite_url"]
            .as_str()
            .unwrap()
            .split("#token=")
            .nth(1)
            .unwrap()
            .to_owned();
        let accept = |token: &str| {
            Req::new("POST", "/api/v1/invitations/accept").json(
                json!({"token": token, "display_name": "Dave", "password": "dave-secret-pass-123"}),
            )
        };
        assert_eq!(w.send(accept(&token)).await.status, StatusCode::OK);
        // A replay of the same link is refused and records nothing.
        assert_ne!(w.send(accept(&token)).await.status, StatusCode::OK);

        // Account security.
        assert_eq!(
            w.send(
                Req::new("POST", "/api/v1/auth/password/change")
                    .session(bob)
                    .json(json!({"current_password": PASSWORD, "new_password": "brand-new-password-1"}))
            )
            .await
            .status,
            StatusCode::NO_CONTENT
        );
        let issue = |s: &Session| {
            Req::new(
                "POST",
                &format!("/api/v1/admin/users/{}/password-reset", c.carol.0),
            )
            .session(s)
        };
        let issued = w.send(issue(&w.op)).await;
        assert_eq!(issued.status, StatusCode::CREATED, "{}", issued.text);
        let reset_token = issued.body["reset_url"]
            .as_str()
            .unwrap()
            .split("#token=")
            .nth(1)
            .unwrap()
            .to_owned();
        let reset = || {
            Req::new("POST", "/api/v1/auth/password-reset")
                .json(json!({"token": reset_token, "new_password": "another-new-password-1"}))
        };
        assert_eq!(w.send(reset()).await.status, StatusCode::NO_CONTENT);
        // Replaying the reset token records nothing.
        assert_ne!(w.send(reset()).await.status, StatusCode::NO_CONTENT);
        // A second link, voided again.
        assert_eq!(w.send(issue(&w.op)).await.status, StatusCode::CREATED);
        assert_eq!(
            w.send(
                Req::new(
                    "DELETE",
                    &format!("/api/v1/admin/users/{}/password-reset", c.carol.0)
                )
                .session(&w.op)
            )
            .await
            .status,
            StatusCode::NO_CONTENT
        );

        // Account state.
        let disable = |what: &str| {
            Req::new("POST", &format!("/api/v1/admin/users/{}/{what}", c.carol.0)).session(&w.op)
        };
        assert_eq!(w.send(disable("disable")).await.status, StatusCode::OK);
        assert_eq!(w.send(disable("disable")).await.status, StatusCode::OK);
        assert_eq!(
            w.send(disable("enable")).await.status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            w.send(Req::new("POST", "/api/v1/auth/logout-all").session(owner))
                .await
                .status,
            StatusCode::NO_CONTENT
        );

        // Member removal last.
        assert_eq!(
            w.send(
                Req::new(
                    "DELETE",
                    &format!("/api/v1/organizations/{org}/members/{}", c.carol.0)
                )
                .session(&c.owner.1)
            )
            .await
            .status,
            StatusCode::UNAUTHORIZED,
            "logout-all ended the owner's session"
        );
        let owner2 = w.env.login_ok("owner").await;
        assert_eq!(
            w.send(
                Req::new(
                    "DELETE",
                    &format!("/api/v1/organizations/{org}/members/{}", c.carol.0)
                )
                .session(&owner2)
            )
            .await
            .status,
            StatusCode::NO_CONTENT
        );

        let expected: BTreeMap<String, i64> = [
            ("auth.login", 6),
            ("auth.logout_all", 1),
            ("auth.password_change", 1),
            ("file.create", 1),
            ("file.delete", 1),
            ("file.download", 1),
            ("file.restore", 1),
            ("file.update", 1),
            ("invitation.accept", 1),
            ("invitation.create", 2),
            ("invitation.revoke", 1),
            ("member.add", 2),
            ("member.change_role", 2),
            ("member.remove", 1),
            ("organization.create", 1),
            ("organization.quota", 1),
            ("organization.update", 1),
            ("password_reset.issue", 2),
            ("password_reset.use", 1),
            ("password_reset.void", 1),
            ("project.create", 1),
            ("project.delete", 1),
            ("project.restore", 1),
            ("project.update", 1),
            ("user.disable", 1),
            ("user.enable", 1),
            ("version.commit", 2),
            ("version.restore", 1),
            ("workspace.create", 1),
            ("workspace.member.add", 1),
            ("workspace.member.change_role", 2),
            ("workspace.member.remove", 1),
            ("workspace.quota", 1),
            ("workspace.update", 1),
        ]
        .into_iter()
        .map(|(a, n)| (a.to_owned(), n))
        .collect();
        w.wait_count(
            "SELECT count(*) FROM audit_events WHERE action = 'file.download'",
            1,
        )
        .await;
        assert_eq!(w.events().await, expected);

        // Every event of a request carries that request's id and the actor.
        let (actor, request_id): (Option<Uuid>, Option<String>) = sqlx::query_as(
            "SELECT actor_id, request_id FROM audit_events WHERE action = 'project.create'",
        )
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(actor, Some(c.bob.0));
        assert!(request_id.is_some_and(|id| !id.is_empty()));
        let (org_id, ws_col, target_type): (Option<Uuid>, Option<Uuid>, Option<String>) =
            sqlx::query_as(
                "SELECT org_id, workspace_id, target_type FROM audit_events WHERE action = 'file.update'",
            )
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(
            (org_id, ws_col, target_type.as_deref()),
            (Some(org), Some(ws_id), Some("file"))
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_request_id_of_the_response_is_stored_with_the_event() {
        let w = W::new().await;
        let owner = w.user("owner").await;
        let r = w
            .send(
                Req::new("POST", "/api/v1/organizations")
                    .session(&w.op)
                    .header("x-request-id", "trace-abc.123")
                    .json(json!({"name": "O", "slug": "o", "owner_user_id": owner.0.to_string()})),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED);
        let stored: String = sqlx::query_scalar(
            "SELECT request_id FROM audit_events WHERE action = 'organization.create'",
        )
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(stored, "trace-abc.123");
        assert_eq!(r.headers["x-request-id"], "trace-abc.123");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_failing_audit_insert_fails_the_change_closed() {
        let w = W::new().await;
        let c = cast(&w).await;
        let eve = w.user("eve").await;
        sqlx::query(
            "CREATE FUNCTION fail_audit() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'no'; END; $$ LANGUAGE plpgsql",
        )
        .execute(&w.env.pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER fail_member_add BEFORE INSERT ON audit_events \
             FOR EACH ROW WHEN (NEW.action = 'member.add') EXECUTE FUNCTION fail_audit()",
        )
        .execute(&w.env.pool)
        .await
        .unwrap();
        let before = w.count("SELECT count(*) FROM organization_members").await;
        let r = w.add_member(&c.owner.1, c.org, eve.0, "member").await;
        assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR, "{}", r.text);
        assert_eq!(
            w.count("SELECT count(*) FROM organization_members").await,
            before,
            "the membership was rolled back with the failed event"
        );
        // The lock is released and other changes keep working.
        let ws = w.ws(&c.owner.1, c.org, "ok").await;
        assert_eq!(ws.status, StatusCode::CREATED, "{}", ws.text);
        // Login (event in the session transaction) fails closed as well.
        sqlx::query("DROP TRIGGER fail_member_add ON audit_events")
            .execute(&w.env.pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER fail_login BEFORE INSERT ON audit_events \
             FOR EACH ROW WHEN (NEW.action = 'auth.login' AND NEW.result = 'success') EXECUTE FUNCTION fail_audit()",
        )
        .execute(&w.env.pool)
        .await
        .unwrap();
        let sessions = w.count("SELECT count(*) FROM sessions").await;
        let login = w.env.login("bob").await;
        assert_eq!(
            login.status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{}",
            login.text
        );
        assert_eq!(w.count("SELECT count(*) FROM sessions").await, sessions);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn concurrent_identical_requests_record_one_event() {
        let w = W::new().await;
        let c = cast(&w).await;
        let eve = w.user("eve").await;
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let app = w.env.app.clone();
            let owner = c.owner.1.clone();
            let (org, user) = (c.org, eve.0);
            tasks.push(tokio::spawn(async move {
                call(
                    &app,
                    Req::new("POST", &format!("/api/v1/organizations/{org}/members"))
                        .session(&owner)
                        .json(json!({"user_id": user.to_string(), "role": "member"})),
                )
                .await
                .status
            }));
        }
        let mut statuses = Vec::new();
        for task in tasks {
            statuses.push(task.await.unwrap());
        }
        assert_eq!(
            statuses
                .iter()
                .filter(|s| **s == StatusCode::CREATED)
                .count(),
            1,
            "{statuses:?}"
        );
        // cast() added bob and carol; eve is the third.
        assert_eq!(w.events().await.get("member.add"), Some(&3));
    }

    // ---- access ------------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn only_owners_and_admins_read_the_audit_log_of_their_organization() {
        let w = W::new().await;
        let c = cast(&w).await;
        let other_owner = w.user("zed").await;
        let other = w.org("other", other_owner.0).await;
        let outsider = w.user("mallory").await;
        let url = |org: Uuid| format!("/api/v1/organizations/{org}/audit");

        for session in [&c.owner.1, &c.bob.1] {
            let r = w.get(session, &url(c.org)).await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.text);
            assert!(r.body["items"].as_array().unwrap().len() >= 3);
        }
        // A plain member: 403; a stranger and the operator without membership: 404.
        assert_eq!(
            w.get(&c.carol.1, &url(c.org)).await.status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            w.get(&outsider.1, &url(c.org)).await.status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            w.get(&w.op, &url(c.org)).await.status,
            StatusCode::NOT_FOUND
        );
        // Another organization's owner reads only their own log.
        assert_eq!(
            w.get(&other_owner.1, &url(c.org)).await.status,
            StatusCode::NOT_FOUND
        );
        let own = w.get(&other_owner.1, &url(other)).await;
        assert_eq!(own.status, StatusCode::OK);
        for item in own.body["items"].as_array().unwrap() {
            assert_eq!(item["org_id"], other.to_string());
        }
        assert!(
            !own.text.contains(&c.org.to_string()),
            "no id of the other organization leaks"
        );
        // The same for the export.
        let export = |org: Uuid| format!("/api/v1/organizations/{org}/audit/export");
        assert_eq!(
            w.get(&c.carol.1, &export(c.org)).await.status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            w.get(&other_owner.1, &export(c.org)).await.status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            w.get(&c.owner.1, &export(c.org)).await.status,
            StatusCode::OK
        );
        // Unauthenticated and malformed ids.
        let anonymous = w.send(Req::new("GET", &url(c.org))).await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            w.get(&c.owner.1, "/api/v1/organizations/not-a-uuid/audit")
                .await
                .status,
            StatusCode::NOT_FOUND
        );
        // The instance-level log is the operator's alone.
        assert_eq!(
            w.get(&c.owner.1, "/api/v1/admin/audit").await.status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            w.get(&c.owner.1, "/api/v1/admin/audit/export").await.status,
            StatusCode::FORBIDDEN
        );
        let instance = w.get(&w.op, "/api/v1/admin/audit").await;
        assert_eq!(instance.status, StatusCode::OK, "{}", instance.text);
        for item in instance.body["items"].as_array().unwrap() {
            assert!(item["org_id"].is_null(), "only instance-level events");
        }
        // Events are read-only over HTTP.
        let post = w
            .send(
                Req::new("POST", &url(c.org))
                    .session(&c.owner.1)
                    .json(json!({})),
            )
            .await;
        assert_eq!(post.status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn denials_and_login_failures_are_aggregated_and_hidden_ones_are_not_recorded() {
        let w = W::new().await;
        let c = cast(&w).await;
        let outsider = w.user("mallory").await;
        // A plain member tries an admin action three times: one row, count 3.
        // (Best-effort writes may be dropped under pressure: wait for each row update.)
        for i in 0..3_i64 {
            let r = w
                .json(
                    "POST",
                    &format!("/api/v1/organizations/{}/workspaces", c.org),
                    &c.carol.1,
                    json!({"name": "X", "slug": "x"}),
                )
                .await;
            assert_eq!(r.status, StatusCode::FORBIDDEN);
            w.wait_count(
                "SELECT COALESCE(sum(event_count), 0)::bigint FROM audit_events WHERE result = 'denied'",
                i + 1,
            )
            .await;
        }
        // A stranger probing the organization gets 404 and leaves no trace in it.
        for _ in 0..3 {
            let r = w
                .get(
                    &outsider.1,
                    &format!("/api/v1/organizations/{}/audit", c.org),
                )
                .await;
            assert_eq!(r.status, StatusCode::NOT_FOUND);
        }
        w.wait_count(
            "SELECT count(*) FROM audit_events WHERE result = 'denied'",
            1,
        )
        .await;
        let denied: Vec<(String, Option<Uuid>, i32, String)> = sqlx::query_as(
            "SELECT action, actor_id, event_count, result FROM audit_events WHERE result = 'denied' AND org_id = $1",
        )
        .bind(c.org)
        .fetch_all(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(
            denied,
            vec![(
                "workspace.create".to_owned(),
                Some(c.carol.0),
                3,
                "denied".to_owned()
            )]
        );
        let seen = w.audit(&c.owner.1, c.org, "?result=denied").await;
        assert_eq!(seen.body["items"][0]["count"], 3);

        // Failed logins of a known account: one row per window with a counter;
        // unknown addresses share one actor-less row. No address is stored.
        let failed_total = "SELECT COALESCE(sum(event_count), 0)::bigint FROM audit_events WHERE action = 'auth.login' AND result = 'failed'";
        let mut attempts = Vec::new();
        attempts.extend(std::iter::repeat_n("carol@example.test", 4));
        attempts.extend(["nobody-1@example.test", "nobody-2@example.test"]);
        for (i, who) in attempts.into_iter().enumerate() {
            let r = call(
                &w.env.app,
                Req::new("POST", "/api/v1/auth/login")
                    .json(json!({"email": who, "password": "wrong-password-123"})),
            )
            .await;
            assert_eq!(r.status, StatusCode::UNAUTHORIZED);
            w.wait_count(failed_total, i64::try_from(i).unwrap() + 1)
                .await;
        }
        w.wait_count(
            "SELECT count(*) FROM audit_events WHERE action = 'auth.login' AND result = 'failed'",
            2,
        )
        .await;
        w.wait_count(
            "SELECT COALESCE(sum(event_count), 0)::bigint FROM audit_events WHERE action = 'auth.login' AND result = 'failed'",
            6,
        )
        .await;
        // Nobody is authenticated, so there is no actor; the attacked account is the target.
        let failed: Vec<(Option<Uuid>, Option<Uuid>, i32)> = sqlx::query_as(
            "SELECT actor_id, target_id, event_count FROM audit_events \
             WHERE action = 'auth.login' AND result = 'failed' ORDER BY event_count DESC",
        )
        .fetch_all(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(failed, vec![(None, Some(c.carol.0), 4), (None, None, 2)]);
        let dump: String = sqlx::query_scalar(
            "SELECT COALESCE(string_agg(row_to_json(a)::text, ' '), '') FROM audit_events a",
        )
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert!(!dump.contains("nobody"), "unknown addresses are not stored");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn no_personal_data_or_secret_reaches_the_table() {
        let w = W::new().await;
        let c = cast(&w).await;
        let inv = w
            .json(
                "POST",
                &format!("/api/v1/organizations/{}/invitations", c.org),
                &c.owner.1,
                json!({"email": "secret.person@example.test", "org_role": "member"}),
            )
            .await;
        assert_eq!(inv.status, StatusCode::CREATED);
        let token = inv.body["invite_url"]
            .as_str()
            .unwrap()
            .split("#token=")
            .nth(1)
            .unwrap()
            .to_owned();
        let accepted = w
            .send(Req::new("POST", "/api/v1/invitations/accept").json(
                json!({"token": token, "display_name": "Secret Person", "password": "very-secret-password-9"}),
            ))
            .await;
        assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.text);
        let session_cookie = accepted.headers["set-cookie"].to_str().unwrap().to_owned();
        let session_token = session_cookie.split(['=', ';']).nth(1).unwrap().to_owned();
        let issued = w
            .send(
                Req::new(
                    "POST",
                    &format!("/api/v1/admin/users/{}/password-reset", c.carol.0),
                )
                .session(&w.op),
            )
            .await;
        let reset_token = issued.body["reset_url"]
            .as_str()
            .unwrap()
            .split("#token=")
            .nth(1)
            .unwrap()
            .to_owned();
        let ws = w.ws(&c.owner.1, c.org, "private-name").await;
        assert_eq!(ws.status, StatusCode::CREATED);

        let dump: String =
            sqlx::query_scalar("SELECT string_agg(row_to_json(a)::text, ' ') FROM audit_events a")
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
        for needle in [
            "@",
            "example.test",
            "Secret Person",
            "very-secret-password",
            PASSWORD,
            token.as_str(),
            session_token.as_str(),
            reset_token.as_str(),
            "private-name",
            "Org acme",
        ] {
            assert!(
                !dump.contains(needle),
                "{needle:?} found in the audit table"
            );
        }
        // The API shows the same data.
        let listing = w.audit(&c.owner.1, c.org, "?limit=100").await;
        for needle in ["@", "example.test", token.as_str(), reset_token.as_str()] {
            assert!(!listing.text.contains(needle), "{needle:?} in the listing");
        }
    }

    // ---- listing, filters, paging, export ------------------------------------------------

    async fn seed(w: &W, org: Uuid, n: i32, at: &str, action: &str, workspace: Option<Uuid>) {
        sqlx::query(
            "INSERT INTO audit_events (id, org_id, workspace_id, action, result, created_at, last_at) \
             SELECT gen_random_uuid(), $1, $5, $2, 'success', $3::timestamptz, $3::timestamptz \
             FROM generate_series(1, $4)",
        )
        .bind(org)
        .bind(action)
        .bind(at)
        .bind(n)
        .bind(workspace)
        .execute(&w.env.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn listing_pages_are_stable_and_filters_validate() {
        let w = W::new().await;
        let c = cast(&w).await;
        let ws = Uuid::new_v4();
        seed(
            &w,
            c.org,
            23,
            "2020-01-01T00:00:00Z",
            "file.update",
            Some(ws),
        )
        .await;
        seed(&w, c.org, 5, "2020-01-01T00:00:00Z", "project.update", None).await;
        let base = format!("/api/v1/organizations/{}/audit", c.org);

        // Walk the old equal-timestamp events with the filter, three pages of 10.
        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let mut query =
                "?limit=10&to=2020-01-02T00:00:00Z&from=2019-12-31T00:00:00Z".to_owned();
            if let Some(cursor) = &cursor {
                query.push_str(&format!("&cursor={cursor}"));
            }
            let page = w.get(&c.owner.1, &format!("{base}{query}")).await;
            assert_eq!(page.status, StatusCode::OK, "{}", page.text);
            for item in page.body["items"].as_array().unwrap() {
                seen.push(item["id"].as_str().unwrap().to_owned());
            }
            pages += 1;
            match page.body["next_cursor"].as_str() {
                Some(next) => cursor = Some(next.to_owned()),
                None => break,
            }
        }
        assert_eq!((pages, seen.len()), (3, 28));
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 28, "no event twice, none skipped");

        // Filters.
        let (wr, cr, br) = (&w, &c, &base);
        let by = move |q: String| async move {
            wr.get(&cr.owner.1, &format!("{br}?limit=100&{q}")).await
        };
        let r = by("action=file".to_owned()).await;
        assert_eq!(r.body["items"].as_array().unwrap().len(), 23);
        let r = by(format!("workspace={ws}")).await;
        assert_eq!(r.body["items"].as_array().unwrap().len(), 23);
        let r = by(format!("actor={}", c.owner.0)).await;
        assert!(
            r.body["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|i| i["actor_id"] == c.owner.0.to_string())
        );
        let r = by("result=denied".to_owned()).await;
        assert!(r.body["items"].as_array().unwrap().is_empty());
        let r = by("action=member.add".to_owned()).await;
        assert_eq!(r.body["items"].as_array().unwrap().len(), 2);
        let target = r.body["items"][0]["target"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let r = by(format!("target={target}&action=member")).await;
        assert_eq!(r.body["items"].as_array().unwrap().len(), 1);
        // Newest first.
        let all = by(String::new()).await;
        let times: Vec<&str> = all.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["created_at"].as_str().unwrap())
            .collect();
        let mut sorted = times.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(times, sorted);

        // Invalid parameters.
        for bad in [
            "action=A%25",
            "action=member%20add",
            "actor=nope",
            "target=1",
            "workspace=x",
            "result=ok",
            "from=yesterday",
            "to=2020",
            "from=2021-01-01T00:00:00Z&to=2020-01-01T00:00:00Z",
            "unknown=1",
        ] {
            let r = by(bad.to_owned()).await;
            assert_eq!(
                r.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{bad}: {}",
                r.text
            );
        }
        for bad in ["limit=0", "limit=x"] {
            let r = by(bad.to_owned()).await;
            assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        }
        let r = by("cursor=garbage".to_owned()).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert_eq!(r.code(), "INVALID_CURSOR");
        // A cursor of another organization's listing is refused.
        let other_owner = w.user("zed").await;
        let other = w.org("other", other_owner.0).await;
        let first = w.get(&c.owner.1, &format!("{base}?limit=1")).await;
        let cursor = first.body["next_cursor"].as_str().unwrap();
        let foreign = w
            .get(
                &other_owner.1,
                &format!("/api/v1/organizations/{other}/audit?cursor={cursor}"),
            )
            .await;
        assert_eq!(foreign.status, StatusCode::BAD_REQUEST);
    }

    async fn collect_lines(resp: Body) -> Vec<String> {
        let bytes = resp.collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn export_streams_ndjson_in_pages_and_truncates_at_the_limit() {
        let w = W::new().await;
        let c = cast(&w).await;
        seed(
            &w,
            c.org,
            1_200,
            "2020-01-01T00:00:00Z",
            "file.update",
            None,
        )
        .await;
        let url = format!("/api/v1/organizations/{}/audit/export", c.org);

        let (status, headers, body) = call_stream(
            &w.env.app,
            Req::new("GET", &format!("{url}?action=file")).cookie_only(&c.owner.1),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], "application/x-ndjson");
        assert!(
            headers["content-disposition"]
                .to_str()
                .unwrap()
                .starts_with("attachment")
        );
        let lines = collect_lines(body).await;
        assert_eq!(lines.len(), 1_200, "more than one database page");
        let mut ids = Vec::new();
        for line in &lines {
            let value: Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["action"], "file.update");
            ids.push(value["id"].as_str().unwrap().to_owned());
        }
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 1_200);
        // The export is itself recorded.
        w.wait_count(
            "SELECT count(*) FROM audit_events WHERE action = 'audit.export'",
            1,
        )
        .await;

        // A `limit` is not accepted; the page size is the server's.
        let bad = w.get(&c.owner.1, &format!("{url}?limit=5")).await;
        assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);

        // Above 100000 events the export ends with a continuation line.
        seed(
            &w,
            c.org,
            100_000,
            "2019-01-01T00:00:00Z",
            "file.update",
            None,
        )
        .await;
        // 101 200 matching events: the first request ends after 100 000 with a
        // continuation line, the second returns the remaining 1 200.
        let filtered = format!("{url}?action=file");
        let (_, _, body) = call_stream(
            &w.env.app,
            Req::new("GET", &filtered).cookie_only(&c.owner.1),
        )
        .await;
        let lines = collect_lines(body).await;
        assert_eq!(lines.len(), 100_001);
        let last: Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["truncated"], true);
        let cursor = last["next_cursor"].as_str().unwrap().to_owned();
        let (_, _, body) = call_stream(
            &w.env.app,
            Req::new("GET", &format!("{filtered}&cursor={cursor}")).cookie_only(&c.owner.1),
        )
        .await;
        let rest = collect_lines(body).await;
        assert_eq!(rest.len(), 1_200);
        assert!(
            serde_json::from_str::<Value>(rest.last().unwrap()).unwrap()["truncated"].is_null()
        );
        // Every export is itself recorded.
        w.wait_count(
            "SELECT count(*) FROM audit_events WHERE action = 'audit.export'",
            3,
        )
        .await;
    }

    // ---- downloads, retention ------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn downloads_follow_the_configured_policy() {
        for record in [true, false] {
            let env = TestEnv::with(
                AuthSettings {
                    audit_downloads: record,
                    ..settings()
                },
                ApiConfig::default(),
            )
            .await;
            let w = W::from(env).await;
            let c = cast(&w).await;
            let ws = id_of(&w.ws(&c.owner.1, c.org, "main").await);
            let project = w
                .json(
                    "POST",
                    &format!("/api/v1/workspaces/{ws}/projects"),
                    &c.owner.1,
                    json!({"name": "P"}),
                )
                .await;
            let file = w
                .json(
                    "POST",
                    &format!("/api/v1/projects/{}/files", id_of(&project)),
                    &c.owner.1,
                    json!({"name": "a.fig", "app": "figcraft", "format": "fig"}),
                )
                .await;
            let file_id = id_of(&file);
            w.push(&c.owner.1, file_id, b"hello").await;
            let r = w
                .get(&c.owner.1, &format!("/api/v1/files/{file_id}/content"))
                .await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.text);
            if record {
                w.wait_count(
                    "SELECT count(*) FROM audit_events WHERE action = 'file.download'",
                    1,
                )
                .await;
            } else {
                tokio::time::sleep(Duration::from_millis(300)).await;
                assert_eq!(
                    w.count("SELECT count(*) FROM audit_events WHERE action = 'file.download'")
                        .await,
                    0
                );
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn maintenance_purges_expired_audit_events_in_batches() {
        let w = W::new().await;
        let c = cast(&w).await;
        seed(&w, c.org, 30, "2018-01-01T00:00:00Z", "file.update", None).await;
        let mut settings = MaintenanceSettings::new(
            Duration::from_secs(86_400 * 30),
            Duration::from_secs(86_400 * 30),
            Duration::from_secs(86_400),
            1024 * 1024,
        );
        settings.max_clock_drift = None;
        settings.batch_size = 10;
        settings.audit_retention_days = 365;
        let job = Maintenance::new(w.env.pool.clone(), None, w.env.clock.clone(), settings);
        let (_keep, stop) = watch::channel(false);
        let dry = job
            .run_once(
                RunOptions {
                    dry_run: true,
                    trigger: Trigger::Cli,
                },
                &stop,
            )
            .await
            .unwrap();
        let RunOutcome::Completed(dry) = dry else {
            panic!("skipped")
        };
        assert_eq!(dry.count("purge_audit", "events_due"), 30);
        assert_eq!(
            w.count("SELECT count(*) FROM audit_events WHERE created_at < '2019-01-01'")
                .await,
            30
        );
        let done = job
            .run_once(
                RunOptions {
                    dry_run: false,
                    trigger: Trigger::Scheduled,
                },
                &stop,
            )
            .await
            .unwrap();
        let RunOutcome::Completed(done) = done else {
            panic!("skipped")
        };
        assert_eq!(done.count("purge_audit", "purged_events"), 30);
        assert!(!done.has_errors());
        assert_eq!(
            w.count("SELECT count(*) FROM audit_events WHERE created_at < '2019-01-01'")
                .await,
            0
        );
        assert!(
            w.count("SELECT count(*) FROM audit_events").await >= 5,
            "recent events stay"
        );
    }

    // ---- review follow-ups ---------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn every_range_read_is_recorded_folded_per_person_and_version_and_head_is_not() {
        let w = W::new().await;
        let c = cast(&w).await;
        let ws = id_of(&w.ws(&c.owner.1, c.org, "main").await);
        let project = w
            .json(
                "POST",
                &format!("/api/v1/workspaces/{ws}/projects"),
                &c.owner.1,
                json!({"name": "P"}),
            )
            .await;
        let file = w
            .json(
                "POST",
                &format!("/api/v1/projects/{}/files", id_of(&project)),
                &c.owner.1,
                json!({"name": "a.fig", "app": "figcraft", "format": "fig"}),
            )
            .await;
        let file_id = id_of(&file);
        w.push(&c.owner.1, file_id, &[7_u8; 100]).await;
        let url = format!("/api/v1/files/{file_id}/content");
        let ranged = |range: &'static str| {
            Req::new("GET", &url)
                .cookie_only(&c.owner.1)
                .header("Range", range)
        };

        // A read that does not start at byte 0 is a download too.
        assert_eq!(
            w.send(ranged("bytes=1-")).await.status,
            StatusCode::PARTIAL_CONTENT
        );
        w.wait_count(
            "SELECT count(*) FROM audit_events WHERE action = 'file.download'",
            1,
        )
        .await;
        // Many tiny reads fold into the same row with a counter.
        for i in 0..1000 {
            assert_eq!(
                w.send(ranged("bytes=0-0")).await.status,
                StatusCode::PARTIAL_CONTENT
            );
            if i % 20 == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        w.wait_count(
            "SELECT COALESCE(sum(event_count), 0)::bigint FROM audit_events WHERE action = 'file.download'",
            1001,
        )
        .await;
        // The database-clock window can roll over during the 1,000 requests.
        let (rows, windows): (i64, i64) = sqlx::query_as(
            "SELECT count(*), count(DISTINCT floor(extract(epoch FROM created_at) / $1::bigint)) \
             FROM audit_events WHERE action = 'file.download'",
        )
        .bind(craftcloud_db::audit::AGGREGATION_WINDOW_SECS)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(
            rows, windows,
            "downloads fold once per database-clock window"
        );
        // HEAD hands out no content and records nothing.
        let head = w.send(Req::new("HEAD", &url).cookie_only(&c.owner.1)).await;
        assert_eq!(head.status, StatusCode::OK);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            w.count("SELECT COALESCE(sum(event_count), 0)::bigint FROM audit_events WHERE action = 'file.download'")
                .await,
            1001
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn setting_a_quota_to_its_current_value_records_nothing() {
        let w = W::new().await;
        let c = cast(&w).await;
        let ws = id_of(&w.ws(&c.owner.1, c.org, "main").await);
        let ws_url = format!("/api/v1/workspaces/{ws}/quota");
        let org_url = format!("/api/v1/admin/organizations/{}/quota", c.org);
        for (url, who) in [(&ws_url, &c.owner.1), (&org_url, &w.op)] {
            for _ in 0..2 {
                let r = w.json("PUT", url, who, json!({"quota_bytes": 4096})).await;
                assert_eq!(r.status, StatusCode::OK, "{}", r.text);
            }
        }
        let events = w.events().await;
        assert_eq!(events.get("workspace.quota"), Some(&1));
        assert_eq!(events.get("organization.quota"), Some(&1));
        let amount: i64 = sqlx::query_scalar(
            "SELECT (details->>'quota_bytes')::bigint FROM audit_events WHERE action = 'workspace.quota'",
        )
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(amount, 4096);
        // A real change (to unlimited) is recorded again.
        let r = w
            .json("PUT", &ws_url, &c.owner.1, json!({"quota_bytes": null}))
            .await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(w.events().await.get("workspace.quota"), Some(&2));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_setting_flag_follows_the_actual_change() {
        let w = W::new().await;
        let c = cast(&w).await;
        let url = format!("/api/v1/organizations/{}", c.org);
        let first = w.get(&c.owner.1, &url).await;
        let initial = first.body["workspace_admin_invites_enabled"]
            .as_bool()
            .unwrap();
        let mut etag = first.etag();
        for (value, changed) in [(initial, false), (!initial, true), (!initial, false)] {
            let r = w
                .send(
                    Req::new("PATCH", &url)
                        .session(&c.owner.1)
                        .if_match(&etag)
                        .json(json!({"workspace_admin_invites_enabled": value})),
                )
                .await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.text);
            etag = r.etag();
            let flag: bool = sqlx::query_scalar(
                "SELECT (details->>'invites_setting_changed')::boolean FROM audit_events \
                 WHERE action = 'organization.update' ORDER BY created_at DESC, id DESC LIMIT 1",
            )
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
            assert_eq!(flag, changed, "value {value}");
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn an_invitation_that_dies_at_acceptance_is_recorded_once() {
        let w = W::new().await;
        let c = cast(&w).await;
        let invited = w
            .json(
                "POST",
                &format!("/api/v1/organizations/{}/invitations", c.org),
                &c.bob.1,
                json!({"email": "late@example.test", "org_role": "member"}),
            )
            .await;
        assert_eq!(invited.status, StatusCode::CREATED, "{}", invited.text);
        let token = invited.body["invite_url"]
            .as_str()
            .unwrap()
            .split("#token=")
            .nth(1)
            .unwrap()
            .to_owned();
        // The inviter loses the right to invite before the link is used.
        let demoted = w
            .json(
                "PATCH",
                &format!("/api/v1/organizations/{}/members/{}", c.org, c.bob.0),
                &c.owner.1,
                json!({"role": "member"}),
            )
            .await;
        assert_eq!(demoted.status, StatusCode::OK);
        let accept = || {
            Req::new("POST", "/api/v1/invitations/accept").json(
                json!({"token": token, "display_name": "Late", "password": "late-secret-pass-123"}),
            )
        };
        assert_eq!(w.send(accept()).await.status, StatusCode::BAD_REQUEST);
        assert_eq!(w.send(accept()).await.status, StatusCode::BAD_REQUEST);
        let rows: Vec<(Option<Uuid>, String)> = sqlx::query_as(
            "SELECT actor_id, target_type FROM audit_events WHERE action = 'invitation.invalidate'",
        )
        .fetch_all(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(rows, vec![(None, "invitation".to_owned())]);
        assert_eq!(w.events().await.get("invitation.accept"), None);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn ending_another_own_session_is_recorded_a_logout_is_not() {
        let w = W::new().await;
        let c = cast(&w).await;
        let first = c.carol.1.clone();
        let second = w.env.login_ok("carol").await;
        let listed = w.get(&second, "/api/v1/auth/sessions").await;
        let sessions = listed.body["sessions"].as_array().unwrap();
        let other = sessions
            .iter()
            .find(|s| s["current"] == false)
            .and_then(|s| s["id"].as_str())
            .unwrap()
            .to_owned();
        let current = sessions
            .iter()
            .find(|s| s["current"] == true)
            .and_then(|s| s["id"].as_str())
            .unwrap()
            .to_owned();
        let revoke = |id: &str, who: &Session| {
            Req::new("DELETE", &format!("/api/v1/auth/sessions/{id}")).session(who)
        };
        assert_eq!(
            w.send(revoke(&other, &second)).await.status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(w.events().await.get("auth.session_revoke"), Some(&1));
        // Ending the session in use is a logout; so is POST /auth/logout.
        assert_eq!(
            w.send(revoke(&current, &second)).await.status,
            StatusCode::NO_CONTENT
        );
        let third = w.env.login_ok("carol").await;
        assert_eq!(
            w.send(Req::new("POST", "/api/v1/auth/logout").session(&third))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(w.events().await.get("auth.session_revoke"), Some(&1));
        let _ = first;
        // An unknown or foreign id answers 404 and records nothing.
        let r = w
            .send(revoke(
                &Uuid::new_v4().to_string(),
                &w.env.login_ok("carol").await,
            ))
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        assert_eq!(w.events().await.get("auth.session_revoke"), Some(&1));
    }

    // ---- second review ---------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn probing_many_objects_stays_one_denied_row() {
        let w = W::new().await;
        let c = cast(&w).await;
        // A plain member loops over random user ids.
        // (Denials are written best-effort and dropped under pressure, so each
        // one waits for the previous row update before the next request.)
        for i in 0..20_i64 {
            let r = w
                .json(
                    "PATCH",
                    &format!("/api/v1/organizations/{}/members/{}", c.org, Uuid::new_v4()),
                    &c.carol.1,
                    json!({"role": "admin"}),
                )
                .await;
            assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.text);
            w.wait_count(
                "SELECT COALESCE(sum(event_count), 0)::bigint FROM audit_events WHERE result = 'denied' AND action = 'member.change_role'",
                i + 1,
            )
            .await;
        }
        // A non-operator loops over random organization ids on an operator route.
        for i in 0..20_i64 {
            let r = w
                .json(
                    "PUT",
                    &format!("/api/v1/admin/organizations/{}/quota", Uuid::new_v4()),
                    &c.owner.1,
                    json!({"quota_bytes": 1}),
                )
                .await;
            assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.text);
            w.wait_count(
                "SELECT COALESCE(sum(event_count), 0)::bigint FROM audit_events WHERE result = 'denied' AND action = 'organization.quota'",
                i + 1,
            )
            .await;
        }
        w.wait_count(
            "SELECT COALESCE(sum(event_count), 0)::bigint FROM audit_events WHERE result = 'denied' AND action = 'member.change_role'",
            20,
        )
        .await;
        w.wait_count(
            "SELECT COALESCE(sum(event_count), 0)::bigint FROM audit_events WHERE result = 'denied' AND action = 'organization.quota'",
            20,
        )
        .await;
        assert_eq!(
            w.count("SELECT count(*) FROM audit_events WHERE result = 'denied' AND action = 'member.change_role'")
                .await,
            1
        );
        assert_eq!(
            w.count("SELECT count(*) FROM audit_events WHERE result = 'denied' AND action = 'organization.quota'")
                .await,
            1
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn denial_pressure_drops_denials_but_not_downloads() {
        let w = W::new().await;
        let c = cast(&w).await;
        let ws = id_of(&w.ws(&c.owner.1, c.org, "main").await);
        let project = w
            .json(
                "POST",
                &format!("/api/v1/workspaces/{ws}/projects"),
                &c.owner.1,
                json!({"name": "P"}),
            )
            .await;
        let file = w
            .json(
                "POST",
                &format!("/api/v1/projects/{}/files", id_of(&project)),
                &c.owner.1,
                json!({"name": "a.fig", "app": "figcraft", "format": "fig"}),
            )
            .await;
        let file_id = id_of(&file);
        w.push(&c.owner.1, file_id, b"hello").await;

        // The lossy budget is used up: denials and failed logins are dropped ...
        let held = w
            .env
            .audit_lossy
            .clone()
            .acquire_many_owned(u32::try_from(w.env.audit_lossy.available_permits()).unwrap())
            .await
            .unwrap();
        let denied = || async {
            w.json(
                "POST",
                &format!("/api/v1/organizations/{}/workspaces", c.org),
                &c.carol.1,
                json!({"name": "X", "slug": "x"}),
            )
            .await
        };
        assert_eq!(denied().await.status, StatusCode::FORBIDDEN);
        let bad_login = call(
            &w.env.app,
            Req::new("POST", "/api/v1/auth/login")
                .json(json!({"email": "carol@example.test", "password": "wrong-password-123"})),
        )
        .await;
        assert_eq!(bad_login.status, StatusCode::UNAUTHORIZED);
        // ... while a download still gets its event, from the other budget.
        let r = w
            .get(&c.owner.1, &format!("/api/v1/files/{file_id}/content"))
            .await;
        assert_eq!(r.status, StatusCode::OK);
        w.wait_count(
            "SELECT count(*) FROM audit_events WHERE action = 'file.download'",
            1,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            w.count("SELECT count(*) FROM audit_events WHERE result IN ('denied', 'failed')")
                .await,
            0,
            "dropped under pressure"
        );

        // Once there is room again, denials are recorded.
        drop(held);
        assert_eq!(denied().await.status, StatusCode::FORBIDDEN);
        w.wait_count(
            "SELECT count(*) FROM audit_events WHERE result = 'denied'",
            1,
        )
        .await;
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_rename_to_the_same_name_is_not_a_name_change() {
        let w = W::new().await;
        let c = cast(&w).await;
        let url = format!("/api/v1/organizations/{}", c.org);
        let first = w.get(&c.owner.1, &url).await;
        let mut etag = first.etag();
        for (name, changed) in [("Org acme", false), ("Acme Inc", true), ("Acme Inc", false)] {
            let r = w
                .send(
                    Req::new("PATCH", &url)
                        .session(&c.owner.1)
                        .if_match(&etag)
                        .json(json!({"name": name})),
                )
                .await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.text);
            etag = r.etag();
            let flag: bool = sqlx::query_scalar(
                "SELECT (details->>'name_changed')::boolean FROM audit_events \
                 WHERE action = 'organization.update' ORDER BY created_at DESC, id DESC LIMIT 1",
            )
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
            assert_eq!(flag, changed, "name {name}");
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_audit_table_has_no_foreign_keys() {
        let w = W::new().await;
        let n: i64 = w
            .count(
                "SELECT count(*) FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid \
                 WHERE t.relname = 'audit_events' AND c.contype = 'f'",
            )
            .await;
        assert_eq!(n, 0);
    }
}
