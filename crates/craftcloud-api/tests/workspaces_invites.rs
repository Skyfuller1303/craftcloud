//! HTTP-level tests of workspaces, workspace members and invitations
//! (CC-F03, CC-F04, CC-F05). All tests need PostgreSQL (`cargo xtask db-test`).

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use crate::common::{
        COOKIE_NAME, PASSWORD, Req, Resp, Session, TestEnv, call, capture_logs, session_from,
    };

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

        async fn org(&self, slug: &str, owner: Uuid) -> Uuid {
            let r = call(
                &self.env.app,
                Req::new("POST", "/api/v1/organizations")
                    .session(&self.op)
                    .json(json!({"name": format!("Org {slug}"), "slug": slug, "owner_user_id": owner.to_string()})),
            )
            .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            Uuid::parse_str(r.body["id"].as_str().unwrap()).unwrap()
        }

        async fn org_member(&self, actor: &Session, org: Uuid, user: Uuid, role: &str) {
            let r = call(
                &self.env.app,
                Req::new("POST", &format!("/api/v1/organizations/{org}/members"))
                    .session(actor)
                    .json(json!({"user_id": user.to_string(), "role": role})),
            )
            .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
        }

        async fn create_ws(&self, actor: &Session, org: Uuid, slug: &str) -> Resp {
            call(
                &self.env.app,
                Req::new("POST", &format!("/api/v1/organizations/{org}/workspaces"))
                    .session(actor)
                    .json(json!({"name": format!("WS {slug}"), "slug": slug})),
            )
            .await
        }

        async fn ws(&self, actor: &Session, org: Uuid, slug: &str) -> Uuid {
            let r = self.create_ws(actor, org, slug).await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            Uuid::parse_str(r.body["id"].as_str().unwrap()).unwrap()
        }

        async fn ws_add(&self, actor: &Session, ws: Uuid, user: Uuid, role: &str) -> Resp {
            call(
                &self.env.app,
                Req::new("POST", &format!("/api/v1/workspaces/{ws}/members"))
                    .session(actor)
                    .json(json!({"user_id": user.to_string(), "role": role})),
            )
            .await
        }

        async fn ws_role(&self, actor: &Session, ws: Uuid, user: Uuid, role: &str) -> Resp {
            call(
                &self.env.app,
                Req::new("PATCH", &format!("/api/v1/workspaces/{ws}/members/{user}"))
                    .session(actor)
                    .json(json!({"role": role})),
            )
            .await
        }

        async fn ws_remove(&self, actor: &Session, ws: Uuid, user: Uuid) -> Resp {
            call(
                &self.env.app,
                Req::new("DELETE", &format!("/api/v1/workspaces/{ws}/members/{user}"))
                    .session(actor),
            )
            .await
        }

        async fn get(&self, actor: &Session, path: &str) -> Resp {
            call(&self.env.app, Req::new("GET", path).cookie_only(actor)).await
        }

        async fn invite(&self, actor: &Session, org: Uuid, body: Value) -> Resp {
            call(
                &self.env.app,
                Req::new("POST", &format!("/api/v1/organizations/{org}/invitations"))
                    .session(actor)
                    .json(body),
            )
            .await
        }

        async fn accept(&self, session: Option<&Session>, body: Value) -> Resp {
            let req = Req::new("POST", "/api/v1/invitations/accept").json(body);
            call(
                &self.env.app,
                match session {
                    Some(s) => req.session(s),
                    None => req,
                },
            )
            .await
        }

        async fn sql_i64(&self, sql: &str) -> i64 {
            self.env.count(sql).await
        }
    }

    fn token_of(resp: &Resp) -> String {
        resp.body["invite_url"]
            .as_str()
            .unwrap()
            .split("#token=")
            .nth(1)
            .unwrap()
            .to_owned()
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

    /// Standard cast: organization `acme` (owner), org admin, a workspace with
    /// ws admin / editor / viewer, a plain org member without workspace role.
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
        let mut people = Vec::new();
        for (name, org_role) in [
            ("admin", "admin"),
            ("wsadmin", "member"),
            ("editor", "member"),
            ("viewer", "member"),
            ("plain", "member"),
        ] {
            let p = w.user(name).await;
            w.org_member(&owner.1, org, p.0, org_role).await;
            people.push(p);
        }
        let outsider = w.user("outsider").await;
        w.org("elsewhere", outsider.0).await;
        let ws = w.ws(&owner.1, org, "main").await;
        for (p, role) in [
            (&people[1], "admin"),
            (&people[2], "editor"),
            (&people[3], "viewer"),
        ] {
            assert_eq!(
                w.ws_add(&owner.1, ws, p.0, role).await.status,
                StatusCode::CREATED
            );
        }
        let mut it = people.into_iter();
        Cast {
            org,
            ws,
            owner,
            admin: it.next().unwrap(),
            wsadmin: it.next().unwrap(),
            editor: it.next().unwrap(),
            viewer: it.next().unwrap(),
            plain: it.next().unwrap(),
            outsider,
        }
    }

    // ---- workspaces -------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn workspace_visibility_listing_creation_and_rename() {
        let w = W::new().await;
        let c = cast(&w).await;
        let url = format!("/api/v1/workspaces/{}", c.ws);

        // Who can read: owner, org admin and every workspace member; nobody else.
        for (who, role) in [
            (&c.owner.1, None),
            (&c.admin.1, None),
            (&c.wsadmin.1, Some("admin")),
            (&c.editor.1, Some("editor")),
            (&c.viewer.1, Some("viewer")),
        ] {
            let r = w.get(who, &url).await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.text);
            assert_eq!(r.body["workspace_role"], json!(role));
            assert_eq!(r.etag(), "\"ws-v1\"");
        }
        let unknown = w
            .get(
                &c.owner.1,
                &format!("/api/v1/workspaces/{}", Uuid::new_v4()),
            )
            .await;
        hidden(&unknown);
        for who in [&c.plain.1, &c.outsider.1, &w.op] {
            let r = w.get(who, &url).await;
            hidden(&r);
            assert_eq!(r.body["error"]["message"], unknown.body["error"]["message"]);
        }
        hidden(&w.get(&c.owner.1, "/api/v1/workspaces/not-a-uuid").await);

        // Listing: managers see all, members only their workspaces, outsiders 404.
        let second = w.ws(&c.admin.1, c.org, "second").await;
        let list_url = format!("/api/v1/organizations/{}/workspaces", c.org);
        assert_eq!(
            w.get(&c.owner.1, &list_url).await.body["items"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            w.get(&c.admin.1, &list_url).await.body["items"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let editor_list = w.get(&c.editor.1, &list_url).await;
        let ids: Vec<_> = editor_list.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(ids, [c.ws.to_string()]);
        assert!(
            w.get(&c.plain.1, &list_url).await.body["items"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        hidden(&w.get(&c.outsider.1, &list_url).await);
        let _ = second;

        // Creation: org owner/admin only; slug and name validated.
        forbidden(&w.create_ws(&c.editor.1, c.org, "nope").await);
        forbidden(&w.create_ws(&c.plain.1, c.org, "nope").await);
        hidden(&w.create_ws(&c.outsider.1, c.org, "nope").await);
        let dup = w.create_ws(&c.owner.1, c.org, "main").await;
        assert_eq!(dup.body["error"]["details"]["fields"], json!(["slug"]));
        let bad = call(
            &w.env.app,
            Req::new(
                "POST",
                &format!("/api/v1/organizations/{}/workspaces", c.org),
            )
            .session(&c.owner.1)
            .json(json!({"name": "", "slug": "Bad Slug"})),
        )
        .await;
        assert_eq!(
            bad.body["error"]["details"]["fields"],
            json!(["name", "slug"])
        );
        assert_eq!(w.sql_i64("SELECT count(*) FROM workspaces").await, 2);

        // Rename with ETag: ws admin and org managers may; editors and viewers may not.
        let rename = |who: &Session, etag: Option<&str>, name: &str| {
            let req = Req::new("PATCH", &url)
                .session(who)
                .json(json!({"name": name}));
            match etag {
                Some(e) => req.if_match(e),
                None => req,
            }
        };
        forbidden(&call(&w.env.app, rename(&c.editor.1, Some("\"ws-v1\""), "X")).await);
        forbidden(&call(&w.env.app, rename(&c.viewer.1, Some("\"ws-v1\""), "X")).await);
        hidden(&call(&w.env.app, rename(&c.plain.1, Some("\"ws-v1\""), "X")).await);
        let missing = call(&w.env.app, rename(&c.wsadmin.1, None, "New")).await;
        assert_eq!(
            (missing.status, missing.code()),
            (StatusCode::PRECONDITION_REQUIRED, "PRECONDITION_REQUIRED")
        );
        let ok = call(
            &w.env.app,
            rename(&c.wsadmin.1, Some("\"ws-v1\""), "  Renamed  "),
        )
        .await;
        assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
        assert_eq!(
            (ok.body["name"].as_str(), ok.etag().as_str()),
            (Some("Renamed"), "\"ws-v2\"")
        );
        let stale = call(&w.env.app, rename(&c.admin.1, Some("\"ws-v1\""), "Again")).await;
        assert_status(&stale, StatusCode::PRECONDITION_FAILED, "VERSION_CONFLICT");
        assert_eq!(stale.etag(), "\"ws-v2\"");
        assert_eq!(
            call(
                &w.env.app,
                rename(&c.admin.1, Some("\"ws-v2\""), "By admin")
            )
            .await
            .status,
            StatusCode::OK
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn workspace_member_role_matrix_including_workspace_admin_limits() {
        let w = W::new().await;
        let c = cast(&w).await;
        let members_url = format!("/api/v1/workspaces/{}/members", c.ws);

        // Listing: members of the workspace and org managers; plain members 404.
        for who in [
            &c.owner.1,
            &c.admin.1,
            &c.wsadmin.1,
            &c.editor.1,
            &c.viewer.1,
        ] {
            let r = w.get(who, &members_url).await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.text);
            assert_eq!(r.body["items"].as_array().unwrap().len(), 3);
        }
        hidden(&w.get(&c.plain.1, &members_url).await);
        hidden(&w.get(&c.outsider.1, &members_url).await);

        // Editors/viewers cannot manage.
        forbidden(&w.ws_add(&c.editor.1, c.ws, c.plain.0, "viewer").await);
        forbidden(&w.ws_add(&c.viewer.1, c.ws, c.plain.0, "viewer").await);
        forbidden(&w.ws_role(&c.editor.1, c.ws, c.viewer.0, "editor").await);
        forbidden(&w.ws_remove(&c.viewer.1, c.ws, c.editor.0).await);
        forbidden(&w.ws_role(&c.editor.1, c.ws, Uuid::new_v4(), "editor").await);

        // Workspace admin: assigns at most editor, only to active org members.
        forbidden(&w.ws_add(&c.wsadmin.1, c.ws, c.plain.0, "admin").await);
        hidden(&w.ws_add(&c.wsadmin.1, c.ws, c.outsider.0, "viewer").await); // not an org member
        hidden(&w.ws_add(&c.wsadmin.1, c.ws, Uuid::new_v4(), "viewer").await); // unknown: same answer
        assert_eq!(
            w.ws_add(&c.wsadmin.1, c.ws, c.plain.0, "viewer")
                .await
                .status,
            StatusCode::CREATED
        );
        assert_eq!(
            w.ws_add(&c.wsadmin.1, c.ws, c.plain.0, "editor")
                .await
                .code(),
            "ALREADY_MEMBER"
        );
        // ... changes roles among editor/viewer ...
        let up = w.ws_role(&c.wsadmin.1, c.ws, c.plain.0, "editor").await;
        assert_eq!(
            (up.status, up.body["role"].as_str()),
            (StatusCode::OK, Some("editor"))
        );
        forbidden(&w.ws_role(&c.wsadmin.1, c.ws, c.plain.0, "admin").await);
        // ... and never touches (other) workspace admins or oneself.
        let admin2 = w.user("admin2").await;
        w.org_member(&c.owner.1, c.org, admin2.0, "member").await;
        assert_eq!(
            w.ws_add(&c.owner.1, c.ws, admin2.0, "admin").await.status,
            StatusCode::CREATED
        );
        forbidden(&w.ws_role(&c.wsadmin.1, c.ws, admin2.0, "viewer").await);
        forbidden(&w.ws_remove(&c.wsadmin.1, c.ws, admin2.0).await);
        forbidden(&w.ws_role(&c.wsadmin.1, c.ws, c.wsadmin.0, "editor").await);
        assert_eq!(
            w.get(&c.owner.1, &members_url).await.body["items"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        // Unknown target: managers 404, editors 403.
        hidden(
            &w.ws_role(&c.wsadmin.1, c.ws, Uuid::new_v4(), "viewer")
                .await,
        );
        hidden(&w.ws_remove(&c.wsadmin.1, c.ws, Uuid::new_v4()).await);
        // Removing editors/viewers works; leaving is allowed for everybody.
        assert_eq!(
            w.ws_remove(&c.wsadmin.1, c.ws, c.plain.0).await.status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            w.ws_remove(&c.editor.1, c.ws, c.editor.0).await.status,
            StatusCode::NO_CONTENT
        );
        hidden(
            &w.get(&c.editor.1, &format!("/api/v1/workspaces/{}", c.ws))
                .await,
        );

        // Org managers do everything, also on workspace admins.
        assert_eq!(
            w.ws_role(&c.admin.1, c.ws, admin2.0, "viewer").await.status,
            StatusCode::OK
        );
        assert_eq!(
            w.ws_role(&c.owner.1, c.ws, c.viewer.0, "admin")
                .await
                .status,
            StatusCode::OK
        );
        assert_eq!(
            w.ws_remove(&c.admin.1, c.ws, c.wsadmin.0).await.status,
            StatusCode::NO_CONTENT
        );
        // Validation.
        for bad in [
            json!({"user_id": "x", "role": "viewer"}),
            json!({"user_id": c.plain.0.to_string(), "role": "owner"}),
            json!({"user_id": c.plain.0.to_string()}),
        ] {
            let r = call(
                &w.env.app,
                Req::new("POST", &members_url).session(&c.owner.1).json(bad),
            )
            .await;
            assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.text);
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn removing_an_org_member_drops_their_workspace_roles_and_disabled_or_removed_users_lose_visibility()
     {
        let w = W::new().await;
        let c = cast(&w).await;
        assert_eq!(
            call(
                &w.env.app,
                Req::new(
                    "DELETE",
                    &format!("/api/v1/organizations/{}/members/{}", c.org, c.viewer.0)
                )
                .session(&c.owner.1)
            )
            .await
            .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            w.sql_i64(&format!(
                "SELECT count(*) FROM workspace_members WHERE user_id = '{}'",
                c.viewer.0
            ))
            .await,
            0
        );
        hidden(
            &w.get(&c.viewer.1, &format!("/api/v1/workspaces/{}", c.ws))
                .await,
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn workspaces_never_cross_organizations_and_swapped_ids_find_nothing() {
        let w = W::new().await;
        let (a_owner, b_owner) = (w.user("aowner").await, w.user("bowner").await);
        let (a_org, b_org) = (
            w.org("org-a", a_owner.0).await,
            w.org("org-b", b_owner.0).await,
        );
        let a_ws = w.ws(&a_owner.1, a_org, "a-main").await;
        let b_ws = w.ws(&b_owner.1, b_org, "b-main").await;
        let a_member = w.user("amember").await;
        w.org_member(&a_owner.1, a_org, a_member.0, "member").await;
        assert_eq!(
            w.ws_add(&a_owner.1, a_ws, a_member.0, "editor")
                .await
                .status,
            StatusCode::CREATED
        );

        let unknown = Uuid::new_v4();
        for ws in [b_ws, unknown] {
            let s = &a_owner.1;
            hidden(&w.get(s, &format!("/api/v1/workspaces/{ws}")).await);
            hidden(
                &call(
                    &w.env.app,
                    Req::new("PATCH", &format!("/api/v1/workspaces/{ws}"))
                        .session(s)
                        .if_match("\"ws-v1\"")
                        .json(json!({"name": "X"})),
                )
                .await,
            );
            hidden(&w.get(s, &format!("/api/v1/workspaces/{ws}/members")).await);
            hidden(&w.ws_add(s, ws, a_member.0, "viewer").await);
            hidden(&w.ws_role(s, ws, b_owner.0, "viewer").await);
            hidden(&w.ws_remove(s, ws, b_owner.0).await);
        }
        // Organization routes of the other tenant.
        hidden(&w.create_ws(&a_owner.1, b_org, "x").await);
        hidden(
            &w.get(
                &a_owner.1,
                &format!("/api/v1/organizations/{b_org}/workspaces"),
            )
            .await,
        );
        // IDOR: own workspace with a foreign user id, and foreign workspace with own user id.
        hidden(&w.ws_role(&a_owner.1, a_ws, b_owner.0, "viewer").await);
        hidden(&w.ws_remove(&a_owner.1, a_ws, b_owner.0).await);
        hidden(&w.ws_add(&a_owner.1, a_ws, b_owner.0, "viewer").await); // not an org member of A
        hidden(&w.ws_add(&b_owner.1, b_ws, a_member.0, "viewer").await);
        // Org-scoped listing with the other organization's id never shows foreign workspaces.
        let mine = w
            .get(
                &a_owner.1,
                &format!("/api/v1/organizations/{a_org}/workspaces"),
            )
            .await;
        assert_eq!(mine.body["items"].as_array().unwrap().len(), 1);
        // Nothing changed in B.
        assert_eq!(
            w.sql_i64(&format!(
                "SELECT count(*) FROM workspace_members WHERE workspace_id = '{b_ws}'"
            ))
            .await,
            0
        );
        assert_eq!(w.sql_i64("SELECT count(*) FROM workspaces").await, 2);
    }

    // ---- invitations: creating --------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn who_may_invite_what() {
        let w = W::new().await;
        let c = cast(&w).await;
        let new_invite = |email: &str, org_role: &str, ws: Option<(&str, Uuid)>| match ws {
            Some((role, id)) => {
                json!({"email": email, "org_role": org_role, "workspace_id": id.to_string(), "workspace_role": role})
            }
            None => json!({"email": email, "org_role": org_role}),
        };

        // Owner and org admin: admin or member, with any workspace role; never owner.
        let r = w
            .invite(
                &c.owner.1,
                c.org,
                new_invite("a1@example.test", "admin", Some(("admin", c.ws))),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
        assert!(
            r.body["invite_url"]
                .as_str()
                .unwrap()
                .starts_with("https://cloud.test/invite#token=")
        );
        assert_eq!(token_of(&r).len(), 43);
        assert_eq!(
            w.invite(
                &c.admin.1,
                c.org,
                new_invite("a2@example.test", "member", None)
            )
            .await
            .status,
            StatusCode::CREATED
        );
        forbidden(
            &w.invite(
                &c.owner.1,
                c.org,
                new_invite("a3@example.test", "owner", None),
            )
            .await,
        );
        forbidden(
            &w.invite(
                &c.admin.1,
                c.org,
                new_invite("a3@example.test", "owner", None),
            )
            .await,
        );

        // Workspace admin: member + own workspace + at most editor.
        assert_eq!(
            w.invite(
                &c.wsadmin.1,
                c.org,
                new_invite("w1@example.test", "member", Some(("editor", c.ws)))
            )
            .await
            .status,
            StatusCode::CREATED
        );
        assert_eq!(
            w.invite(
                &c.wsadmin.1,
                c.org,
                new_invite("w2@example.test", "member", Some(("viewer", c.ws)))
            )
            .await
            .status,
            StatusCode::CREATED
        );
        let other_ws = w.ws(&c.owner.1, c.org, "other").await;
        for denied in [
            new_invite("w3@example.test", "member", Some(("admin", c.ws))), // ws admin role
            new_invite("w3@example.test", "admin", Some(("editor", c.ws))), // org admin role
            new_invite("w3@example.test", "member", None),                  // org-only
            new_invite("w3@example.test", "member", Some(("editor", other_ws))), // not their workspace
            new_invite("w3@example.test", "owner", Some(("editor", c.ws))),
        ] {
            forbidden(&w.invite(&c.wsadmin.1, c.org, denied).await);
        }
        // Everybody else.
        for who in [&c.editor.1, &c.viewer.1, &c.plain.1] {
            forbidden(
                &w.invite(
                    who,
                    c.org,
                    new_invite("x@example.test", "member", Some(("viewer", c.ws))),
                )
                .await,
            );
        }
        hidden(
            &w.invite(
                &c.outsider.1,
                c.org,
                new_invite("x@example.test", "member", None),
            )
            .await,
        );
        hidden(
            &w.invite(&w.op, c.org, new_invite("x@example.test", "member", None))
                .await,
        );

        // Organization setting: workspace admins may be switched off (and on again).
        let org_url = format!("/api/v1/organizations/{}", c.org);
        let etag = w.get(&c.owner.1, &org_url).await.etag();
        forbidden(
            &call(
                &w.env.app,
                Req::new("PATCH", &org_url)
                    .session(&c.wsadmin.1)
                    .if_match(&etag)
                    .json(json!({"workspace_admin_invites_enabled": false})),
            )
            .await,
        );
        let off = call(
            &w.env.app,
            Req::new("PATCH", &org_url)
                .session(&c.admin.1)
                .if_match(&etag)
                .json(json!({"workspace_admin_invites_enabled": false})),
        )
        .await;
        assert_eq!(off.status, StatusCode::OK, "{}", off.text);
        assert_eq!(off.body["workspace_admin_invites_enabled"], json!(false));
        assert_eq!(off.body["name"], "Org acme", "name untouched");
        forbidden(
            &w.invite(
                &c.wsadmin.1,
                c.org,
                new_invite("w4@example.test", "member", Some(("viewer", c.ws))),
            )
            .await,
        );
        // Owners and admins are unaffected.
        assert_eq!(
            w.invite(
                &c.admin.1,
                c.org,
                new_invite("w4@example.test", "member", Some(("viewer", c.ws)))
            )
            .await
            .status,
            StatusCode::CREATED
        );
        let on = call(
            &w.env.app,
            Req::new("PATCH", &org_url)
                .session(&c.owner.1)
                .if_match(&off.etag())
                .json(json!({"workspace_admin_invites_enabled": true})),
        )
        .await;
        assert_eq!(on.body["workspace_admin_invites_enabled"], json!(true));
        let empty = call(
            &w.env.app,
            Req::new("PATCH", &org_url)
                .session(&c.owner.1)
                .if_match(&on.etag())
                .json(json!({})),
        )
        .await;
        assert_eq!(empty.status, StatusCode::UNPROCESSABLE_ENTITY);

        // Validation.
        for bad in [
            json!({"email": "nope", "org_role": "member"}),
            json!({"email": "ok@example.test", "org_role": "god"}),
            json!({"email": "ok@example.test", "org_role": "member", "workspace_id": c.ws.to_string()}),
            json!({"email": "ok@example.test", "org_role": "member", "workspace_role": "viewer"}),
            json!({"email": "ok@example.test", "org_role": "member", "workspace_id": "x", "workspace_role": "viewer"}),
        ] {
            assert_eq!(
                w.invite(&c.owner.1, c.org, bad).await.status,
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
        // A workspace of another organization is not found.
        let foreign_owner = w.user("fo").await;
        let foreign_org = w.org("foreign", foreign_owner.0).await;
        let foreign_ws = w.ws(&foreign_owner.1, foreign_org, "fws").await;
        hidden(
            &w.invite(
                &c.owner.1,
                c.org,
                new_invite("z@example.test", "member", Some(("viewer", foreign_ws))),
            )
            .await,
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn one_open_invitation_per_address_listing_revocation_and_expiry() {
        let w = W::new().await;
        let c = cast(&w).await;
        let body = |email: &str| json!({"email": email, "org_role": "member"});
        let first = w
            .invite(&c.owner.1, c.org, body("Carol@Example.Test"))
            .await;
        assert_eq!(first.status, StatusCode::CREATED);
        let first_id = first.body["invitation_id"].as_str().unwrap().to_owned();
        // Same address (normalized) again: refused until the first is revoked.
        assert_status(
            &w.invite(&c.admin.1, c.org, body("carol@example.test"))
                .await,
            StatusCode::CONFLICT,
            "INVITATION_PENDING",
        );

        // Listing: statuses, no token, owners and admins only.
        let list_url = format!("/api/v1/organizations/{}/invitations", c.org);
        let list = w.get(&c.owner.1, &list_url).await;
        assert_eq!(list.status, StatusCode::OK, "{}", list.text);
        let item = &list.body["items"][0];
        assert_eq!(
            (
                item["status"].as_str(),
                item["email"].as_str(),
                item["org_role"].as_str()
            ),
            (Some("pending"), Some("carol@example.test"), Some("member"))
        );
        assert!(
            !list.text.contains(&token_of(&first))
                && !list.text.contains("invite_url")
                && !list.text.contains("token_hash")
        );
        forbidden(&w.get(&c.wsadmin.1, &list_url).await);
        hidden(&w.get(&c.outsider.1, &list_url).await);

        // Revoke (owner/admin; foreign ids 404); then a new invitation is possible.
        let rev = |who: &Session, org: Uuid, id: &str| {
            let r = Req::new(
                "DELETE",
                &format!("/api/v1/organizations/{org}/invitations/{id}"),
            )
            .session(who);
            call(&w.env.app, r)
        };
        forbidden(&rev(&c.editor.1, c.org, &first_id).await);
        hidden(&rev(&c.outsider.1, c.org, &first_id).await);
        hidden(&rev(&c.owner.1, c.org, &Uuid::new_v4().to_string()).await);
        hidden(&rev(&c.owner.1, c.org, "nope").await);
        assert_eq!(
            rev(&c.admin.1, c.org, &first_id).await.status,
            StatusCode::NO_CONTENT
        );
        hidden(&rev(&c.admin.1, c.org, &first_id).await); // already revoked
        let revoked_status = w.get(&c.owner.1, &list_url).await.body["items"][0]["status"].clone();
        assert_eq!(revoked_status, "revoked");
        let second = w
            .invite(&c.owner.1, c.org, body("carol@example.test"))
            .await;
        assert_eq!(second.status, StatusCode::CREATED);

        // Expiry after 72 hours: status expired, token useless, a new invitation is allowed.
        w.env.clock.advance(Duration::from_secs(72 * 3600 - 5));
        let almost = w.accept(None, json!({"token": token_of(&second), "display_name": "Carol", "password": "carol-secret-password-1"})).await;
        assert_eq!(
            almost.status,
            StatusCode::OK,
            "still valid shortly before 72 h: {}",
            almost.text
        );

        // Sessions idle out after 8 h of the manual clock: sign in again.
        let owner = w.env.login_ok("owner").await;
        let third = w.invite(&owner, c.org, body("dave@example.test")).await;
        w.env.clock.advance(Duration::from_secs(72 * 3600 + 1));
        let owner = w.env.login_ok("owner").await;
        let expired = w.accept(None, json!({"token": token_of(&third), "display_name": "Dave", "password": "dave-secret-password-1"})).await;
        assert_status(&expired, StatusCode::BAD_REQUEST, "INVALID_INVITATION");
        // Order of the listing is by id (random), invitations created at the same
        // manual-clock instant tie on creation time: compare as sets.
        let listed = w.get(&owner, &list_url).await;
        let mut statuses: Vec<_> = listed.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["status"].as_str().unwrap().to_owned())
            .collect();
        statuses.sort();
        assert_eq!(statuses, ["accepted", "expired", "revoked"]);
        let dave: Vec<_> = listed.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["email"] == "dave@example.test")
            .map(|i| i["status"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(dave, ["expired"]);
        // The expired open invitation does not block a new one.
        assert_eq!(
            w.invite(&owner, c.org, body("dave@example.test"))
                .await
                .status,
            StatusCode::CREATED
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn concurrent_invitations_for_one_address_yield_exactly_one() {
        let w = W::new().await;
        let c = cast(&w).await;
        let mut handles = Vec::new();
        for _ in 0..8 {
            let (app, session, org) = (w.env.app.clone(), c.owner.1.clone(), c.org);
            handles.push(tokio::spawn(async move {
                call(
                    &app,
                    Req::new("POST", &format!("/api/v1/organizations/{org}/invitations"))
                        .session(&session)
                        .json(json!({"email": "race@example.test", "org_role": "member"})),
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
        assert_eq!(w.sql_i64("SELECT count(*) FROM invitations").await, 1);
    }

    // ---- invitations: accepting -------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn accepting_creates_the_account_membership_workspace_role_and_a_session() {
        let w = W::new().await;
        let c = cast(&w).await;
        let invite = w.invite(&c.owner.1, c.org, json!({"email": "Newbie@Example.test", "org_role": "member", "workspace_id": c.ws.to_string(), "workspace_role": "editor"})).await;
        let token = token_of(&invite);

        // Validation errors name fields only and do not consume the token.
        for (body, fields) in [
            (json!({"token": token}), json!(["display_name", "password"])),
            (
                json!({"token": token, "display_name": "N", "password": "short"}),
                json!(["password"]),
            ),
            (
                json!({"token": token, "display_name": "N", "password": "newbie"}),
                json!(["password"]),
            ),
            (
                json!({"token": token, "display_name": "  ", "password": "long-enough-password"}),
                json!(["display_name"]),
            ),
        ] {
            let r = w.accept(None, body).await;
            assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.text);
            assert_eq!(r.body["error"]["details"]["fields"], fields);
        }
        // A foreign Origin is refused (login-CSRF-like protection); nothing is created.
        let evil = call(&w.env.app, Req::new("POST", "/api/v1/invitations/accept").origin(Some("https://evil.example")).json(json!({"token": token, "display_name": "N", "password": "long-enough-password"}))).await;
        assert_status(&evil, StatusCode::FORBIDDEN, "CSRF_FAILED");
        assert_eq!(
            w.sql_i64("SELECT count(*) FROM users WHERE normalized_email = 'newbie@example.test'")
                .await,
            0
        );

        let accepted = w.accept(None, json!({"token": token, "display_name": " Newbie ", "password": "long-enough-password"})).await;
        assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.text);
        assert_eq!(accepted.body["account_created"], json!(true));
        assert_eq!(accepted.body["workspace_id"], json!(c.ws.to_string()));
        assert!(accepted.body["csrf_token"].is_string());
        let cookie = accepted.headers[axum::http::header::SET_COOKIE]
            .to_str()
            .unwrap();
        assert!(
            cookie.starts_with(&format!("{COOKIE_NAME}="))
                && cookie.contains("HttpOnly")
                && cookie.contains("Secure")
        );
        let session = session_from(&accepted);

        // The new account is logged in, is a member with the invited roles, and sees the workspace.
        let me = w.env.get_session(&session).await;
        assert_eq!(
            (
                me.status,
                me.body["email"].as_str(),
                me.body["display_name"].as_str()
            ),
            (StatusCode::OK, Some("newbie@example.test"), Some("Newbie"))
        );
        let org = w
            .get(&session, &format!("/api/v1/organizations/{}", c.org))
            .await;
        assert_eq!(
            (org.status, org.body["role"].as_str()),
            (StatusCode::OK, Some("member"))
        );
        let ws = w
            .get(&session, &format!("/api/v1/workspaces/{}", c.ws))
            .await;
        assert_eq!(
            (ws.status, ws.body["workspace_role"].as_str()),
            (StatusCode::OK, Some("editor"))
        );
        assert_eq!(
            call(
                &w.env.app,
                Req::new("POST", "/api/v1/auth/login").json(
                    json!({"email": "newbie@example.test", "password": "long-enough-password"})
                )
            )
            .await
            .status,
            StatusCode::OK,
            "password works for login"
        );
        // Outsiders and other workspaces stay invisible.
        let other = w.ws(&c.owner.1, c.org, "other").await;
        hidden(
            &w.get(&session, &format!("/api/v1/workspaces/{other}"))
                .await,
        );

        // Single use: replay, even with a fresh address, fails uniformly.
        let replay = w.accept(None, json!({"token": token, "display_name": "Again", "password": "long-enough-password"})).await;
        assert_status(&replay, StatusCode::BAD_REQUEST, "INVALID_INVITATION");
        let unknown = w.accept(None, json!({"token": "A".repeat(43), "display_name": "X", "password": "long-enough-password"})).await;
        let garbage = w.accept(None, json!({"token": "garbage", "display_name": "X", "password": "long-enough-password"})).await;
        for r in [&unknown, &garbage] {
            assert_status(r, StatusCode::BAD_REQUEST, "INVALID_INVITATION");
            assert_eq!(r.body["error"]["message"], replay.body["error"]["message"]);
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn invitations_for_existing_accounts_need_that_account_logged_in() {
        let w = W::new().await;
        let c = cast(&w).await;
        let target = w.user("target").await;
        let stranger = w.user("stranger").await;
        let body = |extra: Value| {
            let mut v = json!({"email": "target@example.test", "org_role": "member"});
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            v
        };
        let token = token_of(&w.invite(&c.owner.1, c.org, body(json!({}))).await);

        // Anonymous: login required (the token stays valid).
        assert_status(
            &w.accept(
                None,
                json!({"token": token, "display_name": "T", "password": "long-enough-password"}),
            )
            .await,
            StatusCode::UNAUTHORIZED,
            "LOGIN_REQUIRED",
        );
        // Another account: mismatch. A cookie without CSRF header: CSRF failure.
        assert_status(
            &w.accept(Some(&stranger.1), json!({"token": token})).await,
            StatusCode::FORBIDDEN,
            "INVITATION_EMAIL_MISMATCH",
        );
        let no_csrf = call(
            &w.env.app,
            Req::new("POST", "/api/v1/invitations/accept")
                .cookie_only(&target.1)
                .json(json!({"token": token})),
        )
        .await;
        assert_status(&no_csrf, StatusCode::FORBIDDEN, "CSRF_FAILED");
        assert_eq!(w.sql_i64("SELECT count(*) FROM organization_members WHERE role <> 'owner' AND user_id IN (SELECT id FROM users WHERE normalized_email = 'target@example.test')").await, 0);

        // The right account joins; no new account, no new session cookie.
        let ok = w.accept(Some(&target.1), json!({"token": token})).await;
        assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
        assert_eq!(
            (
                ok.body["account_created"].clone(),
                ok.body["user_id"].as_str()
            ),
            (json!(false), Some(target.0.to_string().as_str()))
        );
        assert!(!ok.headers.contains_key(axum::http::header::SET_COOKIE));
        assert_eq!(
            w.get(&target.1, &format!("/api/v1/organizations/{}", c.org))
                .await
                .status,
            StatusCode::OK
        );
        assert_status(
            &w.accept(Some(&target.1), json!({"token": token})).await,
            StatusCode::BAD_REQUEST,
            "INVALID_INVITATION",
        );

        // A logged-in stranger cannot use an invitation for an address without account either.
        let fresh = token_of(
            &w.invite(
                &c.owner.1,
                c.org,
                json!({"email": "nobody@example.test", "org_role": "member"}),
            )
            .await,
        );
        assert_status(
            &w.accept(
                Some(&stranger.1),
                json!({"token": fresh, "display_name": "N", "password": "long-enough-password"}),
            )
            .await,
            StatusCode::FORBIDDEN,
            "INVITATION_EMAIL_MISMATCH",
        );

        // A disabled target account: uniform invalid.
        let gone = w.user("gone").await;
        let t2 = token_of(
            &w.invite(
                &c.owner.1,
                c.org,
                json!({"email": "gone@example.test", "org_role": "member"}),
            )
            .await,
        );
        sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
            .bind(gone.0)
            .execute(&w.env.pool)
            .await
            .unwrap();
        assert_status(
            &w.accept(
                None,
                json!({"token": t2, "display_name": "G", "password": "long-enough-password"}),
            )
            .await,
            StatusCode::BAD_REQUEST,
            "INVALID_INVITATION",
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn existing_members_keep_their_roles_and_workspace_roles_are_never_lowered() {
        let w = W::new().await;
        let c = cast(&w).await;
        let accept_as = |who: &Session, token: String| {
            let who = who.clone();
            let w = &w;
            async move { w.accept(Some(&who), json!({"token": token})).await }
        };
        // Org-only invitation for an active member: 409, the invitation stays open and revocable.
        let org_only = w
            .invite(
                &c.owner.1,
                c.org,
                json!({"email": "plain@example.test", "org_role": "admin"}),
            )
            .await;
        assert_status(
            &accept_as(&c.plain.1, token_of(&org_only)).await,
            StatusCode::CONFLICT,
            "ALREADY_MEMBER",
        );
        let role: String =
            sqlx::query_scalar("SELECT role FROM organization_members WHERE user_id = $1")
                .bind(c.plain.0)
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
        assert_eq!(
            role, "member",
            "the organization role is never changed by an invitation"
        );
        let id = org_only.body["invitation_id"].as_str().unwrap();
        let rev = call(
            &w.env.app,
            Req::new(
                "DELETE",
                &format!("/api/v1/organizations/{}/invitations/{id}", c.org),
            )
            .session(&c.owner.1),
        )
        .await;
        assert_eq!(
            rev.status,
            StatusCode::NO_CONTENT,
            "still pending, so revocable"
        );

        // Workspace part for an existing member: granted when it raises the role ...
        let up = w.invite(&c.owner.1, c.org, json!({"email": "plain@example.test", "org_role": "member", "workspace_id": c.ws.to_string(), "workspace_role": "editor"})).await;
        let r = accept_as(&c.plain.1, token_of(&up)).await;
        assert_eq!(
            (r.status, r.body["workspace_role_applied"].clone()),
            (StatusCode::OK, json!(true)),
            "{}",
            r.text
        );
        // ... never lowered (editor invited as viewer, ws admin invited as editor).
        let down = w.invite(&c.owner.1, c.org, json!({"email": "plain@example.test", "org_role": "member", "workspace_id": c.ws.to_string(), "workspace_role": "viewer"})).await;
        let r = accept_as(&c.plain.1, token_of(&down)).await;
        assert_eq!(
            (r.status, r.body["workspace_role_applied"].clone()),
            (StatusCode::OK, json!(false))
        );
        let r = accept_as(&c.wsadmin.1, token_of(&w.invite(&c.owner.1, c.org, json!({"email": "wsadmin@example.test", "org_role": "member", "workspace_id": c.ws.to_string(), "workspace_role": "editor"})).await)).await;
        assert_eq!(r.body["workspace_role_applied"], json!(false));
        let roles: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT user_id, role FROM workspace_members WHERE user_id = ANY($1) ORDER BY role",
        )
        .bind(vec![c.plain.0, c.wsadmin.0])
        .fetch_all(&w.env.pool)
        .await
        .unwrap();
        assert!(
            roles.contains(&(c.plain.0, "editor".to_owned()))
                && roles.contains(&(c.wsadmin.0, "admin".to_owned())),
            "{roles:?}"
        );
        // An owner invited as member keeps owner.
        let owner_inv = w.invite(&c.admin.1, c.org, json!({"email": "owner@example.test", "org_role": "member", "workspace_id": c.ws.to_string(), "workspace_role": "viewer"})).await;
        assert_eq!(
            accept_as(&c.owner.1, token_of(&owner_inv)).await.status,
            StatusCode::OK
        );
        let owner_role: String =
            sqlx::query_scalar("SELECT role FROM organization_members WHERE user_id = $1")
                .bind(c.owner.0)
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
        assert_eq!(owner_role, "owner");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn guest_invitation_workspace_is_context_only_even_for_legacy_roles() {
        let w = W::new().await;
        let c = cast(&w).await;
        let invitation=w.invite(&c.owner.1,c.org,json!({"email":"legacy-guest@example.test","org_role":"guest","workspace_id":c.ws.to_string()})).await;
        assert_eq!(
            invitation.status,
            StatusCode::CREATED,
            "{}",
            invitation.text
        );
        let invitation_id =
            Uuid::parse_str(invitation.body["invitation_id"].as_str().unwrap()).unwrap();
        // Migration 0016 forbids a workspace role on guest invitations, so a row
        // from before that rule is modelled by lifting the check in this test
        // database only.
        sqlx::query("ALTER TABLE invitations DROP CONSTRAINT invitations_workspace_pair")
            .execute(&w.env.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE invitations SET workspace_role='editor' WHERE id=$1")
            .bind(invitation_id)
            .execute(&w.env.pool)
            .await
            .unwrap();
        let accepted=w.accept(None,json!({"token":token_of(&invitation),"display_name":"Legacy Guest","password":"legacy-guest-password-1"})).await;
        assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.text);
        assert_eq!(accepted.body["workspace_id"], c.ws.to_string());
        assert_eq!(accepted.body["workspace_role_applied"], false);
        let guest_id = Uuid::parse_str(accepted.body["user_id"].as_str().unwrap()).unwrap();
        let guest_session = session_from(&accepted);
        let role: String = sqlx::query_scalar(
            "SELECT role FROM organization_members WHERE org_id=$1 AND user_id=$2",
        )
        .bind(c.org)
        .bind(guest_id)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(role, "guest");
        let ws_role: Option<String> = sqlx::query_scalar(
            "SELECT role FROM workspace_members WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3",
        )
        .bind(c.org)
        .bind(c.ws)
        .bind(guest_id)
        .fetch_optional(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(ws_role, None);

        // Even a legacy database row cannot grant a guest workspace visibility.
        sqlx::query("INSERT INTO workspace_members(org_id,workspace_id,user_id,role) VALUES($1,$2,$3,'editor')").bind(c.org).bind(c.ws).bind(guest_id).execute(&w.env.pool).await.unwrap();
        hidden(
            &w.get(&guest_session, &format!("/api/v1/workspaces/{}", c.ws))
                .await,
        );
        hidden(&w.ws_role(&guest_session, c.ws, c.plain.0, "viewer").await);
        assert_eq!(
            w.ws_add(&guest_session, c.ws, c.plain.0, "viewer")
                .await
                .status,
            StatusCode::NOT_FOUND
        );
        sqlx::query(
            "DELETE FROM workspace_members WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3",
        )
        .bind(c.org)
        .bind(c.ws)
        .bind(guest_id)
        .execute(&w.env.pool)
        .await
        .unwrap();

        // A normal member invitation accepted by an existing guest never installs its workspace role.
        let regular=w.invite(&c.owner.1,c.org,json!({"email":"legacy-guest@example.test","org_role":"member","workspace_id":c.ws.to_string(),"workspace_role":"editor"})).await;
        assert_eq!(regular.status, StatusCode::CREATED, "{}", regular.text);
        let outcome = w
            .accept(Some(&guest_session), json!({"token":token_of(&regular)}))
            .await;
        assert_eq!(outcome.status, StatusCode::CONFLICT, "{}", outcome.text);
        let ws_role: Option<String> = sqlx::query_scalar(
            "SELECT role FROM workspace_members WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3",
        )
        .bind(c.org)
        .bind(c.ws)
        .bind(guest_id)
        .fetch_optional(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(ws_role, None);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn stale_invitations_of_demoted_or_disabled_inviters_and_changed_settings_are_refused() {
        let w = W::new().await;
        let c = cast(&w).await;
        let anon = |token: String, name: &str| json!({"token": token, "display_name": name, "password": "long-enough-password"});

        // Org admin demoted to member between invite and accept.
        let t1 = token_of(
            &w.invite(
                &c.admin.1,
                c.org,
                json!({"email": "s1@example.test", "org_role": "member"}),
            )
            .await,
        );
        assert_eq!(
            call(
                &w.env.app,
                Req::new(
                    "PATCH",
                    &format!("/api/v1/organizations/{}/members/{}", c.org, c.admin.0)
                )
                .session(&c.owner.1)
                .json(json!({"role": "member"}))
            )
            .await
            .status,
            StatusCode::OK
        );
        assert_status(
            &w.accept(None, anon(t1, "S1")).await,
            StatusCode::BAD_REQUEST,
            "INVALID_INVITATION",
        );
        assert_eq!(
            w.sql_i64("SELECT count(*) FROM users WHERE normalized_email = 's1@example.test'")
                .await,
            0
        );
        let status: String = sqlx::query_scalar("SELECT CASE WHEN invalidated_at IS NOT NULL THEN 'invalid' ELSE 'open' END FROM invitations WHERE normalized_email = 's1@example.test'").fetch_one(&w.env.pool).await.unwrap();
        assert_eq!(status, "invalid", "the stale invitation is marked invalid");

        // Workspace admin: setting switched off, or demoted, or removed from the workspace.
        let t2 = token_of(&w.invite(&c.wsadmin.1, c.org, json!({"email": "s2@example.test", "org_role": "member", "workspace_id": c.ws.to_string(), "workspace_role": "editor"})).await);
        let org_url = format!("/api/v1/organizations/{}", c.org);
        let etag = w.get(&c.owner.1, &org_url).await.etag();
        let off = call(
            &w.env.app,
            Req::new("PATCH", &org_url)
                .session(&c.owner.1)
                .if_match(&etag)
                .json(json!({"workspace_admin_invites_enabled": false})),
        )
        .await;
        assert_status(
            &w.accept(None, anon(t2, "S2")).await,
            StatusCode::BAD_REQUEST,
            "INVALID_INVITATION",
        );
        let on = call(
            &w.env.app,
            Req::new("PATCH", &org_url)
                .session(&c.owner.1)
                .if_match(&off.etag())
                .json(json!({"workspace_admin_invites_enabled": true})),
        )
        .await;
        assert_eq!(on.status, StatusCode::OK);
        let t3 = token_of(&w.invite(&c.wsadmin.1, c.org, json!({"email": "s3@example.test", "org_role": "member", "workspace_id": c.ws.to_string(), "workspace_role": "editor"})).await);
        assert_eq!(
            w.ws_role(&c.owner.1, c.ws, c.wsadmin.0, "editor")
                .await
                .status,
            StatusCode::OK
        );
        assert_status(
            &w.accept(None, anon(t3, "S3")).await,
            StatusCode::BAD_REQUEST,
            "INVALID_INVITATION",
        );

        // Inviter account disabled / removed from the organization.
        let t4 = token_of(
            &w.invite(
                &c.owner.1,
                c.org,
                json!({"email": "s4@example.test", "org_role": "member"}),
            )
            .await,
        );
        let owner2 = w.user("owner2").await;
        w.org_member(&c.owner.1, c.org, owner2.0, "owner").await;
        let t6 = token_of(
            &w.invite(
                &owner2.1,
                c.org,
                json!({"email": "s6@example.test", "org_role": "member"}),
            )
            .await,
        );
        sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
            .bind(owner2.0)
            .execute(&w.env.pool)
            .await
            .unwrap();
        assert_status(
            &w.accept(None, anon(t6, "S6")).await,
            StatusCode::BAD_REQUEST,
            "INVALID_INVITATION",
        );
        // The still-entitled owner's invitation works.
        assert_eq!(w.accept(None, anon(t4, "S4")).await.status, StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn concurrent_accepts_of_one_token_have_exactly_one_winner() {
        let w = W::new().await;
        let c = cast(&w).await;
        let token = token_of(&w.invite(&c.owner.1, c.org, json!({"email": "racer@example.test", "org_role": "member", "workspace_id": c.ws.to_string(), "workspace_role": "viewer"})).await);
        let mut handles = Vec::new();
        for i in 0..8 {
            let (app, token) = (w.env.app.clone(), token.clone());
            handles.push(tokio::spawn(async move {
                call(&app, Req::new("POST", "/api/v1/invitations/accept").json(json!({"token": token, "display_name": format!("Racer {i}"), "password": "long-enough-password"}))).await.status
            }));
        }
        let mut ok = 0;
        for h in handles {
            let status = h.await.unwrap();
            match status {
                StatusCode::OK => ok += 1,
                // Losers see the closed invitation, or (once the account exists) a login prompt.
                StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED => {}
                other => panic!("unexpected status {other}"),
            }
        }
        assert_eq!(ok, 1);
        assert_eq!(
            w.sql_i64("SELECT count(*) FROM users WHERE normalized_email = 'racer@example.test'")
                .await,
            1
        );
        assert_eq!(w.sql_i64("SELECT count(*) FROM organization_members m JOIN users u ON u.id = m.user_id WHERE u.normalized_email = 'racer@example.test'").await, 1);
        assert_eq!(w.sql_i64("SELECT count(*) FROM workspace_members wm JOIN users u ON u.id = wm.user_id WHERE u.normalized_email = 'racer@example.test'").await, 1);
        assert_eq!(
            w.sql_i64("SELECT count(*) FROM invitations WHERE accepted_at IS NOT NULL")
                .await,
            1
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_revoked_invitation_cannot_be_accepted() {
        let w = W::new().await;
        let c = cast(&w).await;
        let inv = w
            .invite(
                &c.owner.1,
                c.org,
                json!({"email": "revoked@example.test", "org_role": "member"}),
            )
            .await;
        let id = inv.body["invitation_id"].as_str().unwrap();
        let rev = call(
            &w.env.app,
            Req::new(
                "DELETE",
                &format!("/api/v1/organizations/{}/invitations/{id}", c.org),
            )
            .session(&c.owner.1),
        )
        .await;
        assert_eq!(rev.status, StatusCode::NO_CONTENT);
        let r = w.accept(None, json!({"token": token_of(&inv), "display_name": "R", "password": "long-enough-password"})).await;
        assert_status(&r, StatusCode::BAD_REQUEST, "INVALID_INVITATION");
        assert_eq!(
            w.sql_i64("SELECT count(*) FROM users WHERE normalized_email = 'revoked@example.test'")
                .await,
            0
        );
        // A workspace admin can revoke only their own invitations.
        let own = w.invite(&c.wsadmin.1, c.org, json!({"email": "o1@example.test", "org_role": "member", "workspace_id": c.ws.to_string(), "workspace_role": "viewer"})).await;
        let foreign = w.invite(&c.owner.1, c.org, json!({"email": "o2@example.test", "org_role": "member", "workspace_id": c.ws.to_string(), "workspace_role": "viewer"})).await;
        let del = |id: &Value| {
            Req::new(
                "DELETE",
                &format!(
                    "/api/v1/organizations/{}/invitations/{}",
                    c.org,
                    id.as_str().unwrap()
                ),
            )
            .session(&c.wsadmin.1)
        };
        forbidden(&call(&w.env.app, del(&foreign.body["invitation_id"])).await);
        assert_eq!(
            call(&w.env.app, del(&own.body["invitation_id"]))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
    }

    // ---- authentication, CSRF, rate limits, logging -------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn every_unsafe_workspace_and_invitation_route_needs_session_csrf_and_origin() {
        let w = W::new().await;
        let c = cast(&w).await;
        let inv = w
            .invite(
                &c.owner.1,
                c.org,
                json!({"email": "csrf@example.test", "org_role": "member"}),
            )
            .await;
        let inv_id = inv.body["invitation_id"].as_str().unwrap().to_owned();
        let routes: Vec<(&'static str, String, Option<Value>)> = vec![
            (
                "POST",
                format!("/api/v1/organizations/{}/workspaces", c.org),
                Some(json!({"name": "N", "slug": "n"})),
            ),
            (
                "PATCH",
                format!("/api/v1/workspaces/{}", c.ws),
                Some(json!({"name": "N"})),
            ),
            (
                "POST",
                format!("/api/v1/workspaces/{}/members", c.ws),
                Some(json!({"user_id": c.plain.0.to_string(), "role": "viewer"})),
            ),
            (
                "PATCH",
                format!("/api/v1/workspaces/{}/members/{}", c.ws, c.viewer.0),
                Some(json!({"role": "editor"})),
            ),
            (
                "DELETE",
                format!("/api/v1/workspaces/{}/members/{}", c.ws, c.viewer.0),
                None,
            ),
            (
                "POST",
                format!("/api/v1/organizations/{}/invitations", c.org),
                Some(json!({"email": "n@example.test", "org_role": "member"})),
            ),
            (
                "DELETE",
                format!("/api/v1/organizations/{}/invitations/{inv_id}", c.org),
                None,
            ),
            ("GET", format!("/api/v1/workspaces/{}", c.ws), None),
            ("GET", format!("/api/v1/workspaces/{}/members", c.ws), None),
            (
                "GET",
                format!("/api/v1/organizations/{}/workspaces", c.org),
                None,
            ),
            (
                "GET",
                format!("/api/v1/organizations/{}/invitations", c.org),
                None,
            ),
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
        assert_eq!(w.sql_i64("SELECT count(*) FROM workspaces").await, 1);
        assert_eq!(
            w.sql_i64("SELECT count(*) FROM invitations WHERE revoked_at IS NOT NULL")
                .await,
            0
        );
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
    async fn accepting_is_rate_limited_per_address_and_invitation_creation_per_user() {
        let mut settings = crate::common::settings();
        settings.ip_limit = 3;
        settings.invite_limit = 2;
        let env = TestEnv::with(settings, craftcloud_api::ApiConfig::default()).await;
        let op = env.login_ok("op").await;
        for i in 0..3 {
            let r = call(
                &env.app,
                Req::new("POST", "/api/v1/invitations/accept")
                    .json(json!({"token": format!("{i}")})),
            )
            .await;
            assert_status(&r, StatusCode::BAD_REQUEST, "INVALID_INVITATION");
        }
        let limited = call(
            &env.app,
            Req::new("POST", "/api/v1/invitations/accept").json(json!({"token": "x"})),
        )
        .await;
        assert_status(&limited, StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED");
        assert!(
            limited
                .headers
                .contains_key(axum::http::header::RETRY_AFTER)
        );

        // The operator owns an organization of their own and creates invitations at a limited rate.
        let org = {
            let r = call(
                &env.app,
                Req::new("POST", "/api/v1/organizations").session(&op).json(
                    json!({"name": "O", "slug": "o", "owner_user_id": env.operator_id.to_string()}),
                ),
            )
            .await;
            Uuid::parse_str(r.body["id"].as_str().unwrap()).unwrap()
        };
        let invite = |i: u32| {
            call(
                &env.app,
                Req::new("POST", &format!("/api/v1/organizations/{org}/invitations"))
                    .session(&op)
                    .json(json!({"email": format!("u{i}@example.test"), "org_role": "member"})),
            )
        };
        assert_eq!(invite(1).await.status, StatusCode::CREATED);
        assert_eq!(invite(2).await.status, StatusCode::CREATED);
        assert_status(
            &invite(3).await,
            StatusCode::TOO_MANY_REQUESTS,
            "RATE_LIMITED",
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn tokens_and_addresses_never_reach_the_logs() {
        let logs = capture_logs();
        let w = W::new().await;
        let c = cast(&w).await;
        let invite = w.invite(&c.owner.1, c.org, json!({"email": "logcheck@example.test", "org_role": "member", "workspace_id": c.ws.to_string(), "workspace_role": "viewer"})).await;
        let token = token_of(&invite);
        let bad = w.accept(None, json!({"token": "B".repeat(43), "display_name": "Zed", "password": "never-log-this-password"})).await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST);
        let accepted = w.accept(None, json!({"token": token, "display_name": "LogCheck", "password": "never-log-this-password-2"})).await;
        assert_eq!(accepted.status, StatusCode::OK);
        let session = session_from(&accepted);
        let id = invite.body["invitation_id"].as_str().unwrap();
        let _ = call(
            &w.env.app,
            Req::new(
                "DELETE",
                &format!("/api/v1/organizations/{}/invitations/{id}", c.org),
            )
            .session(&c.owner.1),
        )
        .await;

        let text = logs.text();
        // Positive capture: the audit trail contains the events with identifiers.
        for needle in [
            "craftcloud::audit",
            "invitation.create",
            "invitation.accept",
            &c.org.to_string(),
        ] {
            assert!(text.contains(needle), "log lacks {needle}");
        }
        for secret in [
            token.as_str(),
            &"B".repeat(43),
            "never-log-this-password",
            "never-log-this-password-2",
            "logcheck@example.test",
            "LogCheck",
            PASSWORD,
            session.token.as_str(),
            session.csrf.as_str(),
        ] {
            assert!(
                !text.contains(secret),
                "log leaks {}",
                &secret[..4.min(secret.len())]
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn invitation_created_accounts_record_provenance_and_the_operator_can_list_them() {
        use base64::Engine as _;
        let logs = capture_logs();
        let w = W::new().await;
        let c = cast(&w).await;
        let inv = w
            .invite(
                &c.admin.1,
                c.org,
                json!({"email": "squat@example.test", "org_role": "member"}),
            )
            .await;
        let invitation_id = inv.body["invitation_id"].as_str().unwrap().to_owned();
        let accepted = w
            .accept(
                None,
                json!({"token": token_of(&inv), "display_name": "Squat", "password": "long-enough-password"}),
            )
            .await;
        assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.text);
        let new_id = accepted.body["user_id"].as_str().unwrap().to_owned();

        // Stored provenance: the address is unverified, the invitation is recorded.
        let row: (bool, String) = sqlx::query_as(
            "SELECT email_verified_at IS NULL, created_via_invitation_id::text FROM users WHERE id = $1::uuid",
        )
        .bind(&new_id)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(row, (true, invitation_id.clone()));
        // Accounts created outside invitations have no origin.
        let no_origin: i64 = w
            .sql_i64("SELECT count(*) FROM users WHERE created_via_invitation_id IS NULL")
            .await;
        assert!(no_origin >= 7);
        assert!(logs.text().contains("account.created_via_invitation"));

        // Operator listing.
        let list = w.get(&w.op, "/api/v1/admin/users?limit=100").await;
        assert_eq!(list.status, StatusCode::OK, "{}", list.text);
        let items = list.body["items"].as_array().unwrap();
        let squat = items.iter().find(|i| i["id"] == new_id).unwrap();
        assert_eq!(squat["email"], "squat@example.test");
        assert_eq!(squat["email_verified"], json!(false));
        assert_eq!(squat["disabled"], json!(false));
        assert_eq!(
            squat["origin"],
            json!({
                "organization_id": c.org.to_string(),
                "invited_by_user_id": c.admin.0.to_string(),
                "invitation_id": invitation_id,
            })
        );
        let other = items
            .iter()
            .find(|i| i["id"] == c.owner.0.to_string())
            .unwrap();
        assert!(other["origin"].is_null());
        assert!(!list.text.contains("password") && !list.text.contains("token"));

        // Filter: verified accounts drop out of ?unverified=true.
        sqlx::query("UPDATE users SET email_verified_at = now() WHERE id = $1")
            .bind(c.owner.0)
            .execute(&w.env.pool)
            .await
            .unwrap();
        let unverified = w
            .get(&w.op, "/api/v1/admin/users?unverified=true&limit=100")
            .await;
        let ids: Vec<_> = unverified.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_owned())
            .collect();
        assert!(ids.contains(&new_id) && !ids.contains(&c.owner.0.to_string()));
        assert_eq!(
            w.get(&w.op, "/api/v1/admin/users?unverified=false&limit=100")
                .await
                .body["items"]
                .as_array()
                .unwrap()
                .len(),
            items.len()
        );
        assert_eq!(
            w.get(&w.op, "/api/v1/admin/users?unverified=maybe")
                .await
                .status,
            StatusCode::UNPROCESSABLE_ENTITY
        );

        // Paging: the cursor holds timestamp and id only and is bound to the filter.
        let page = w.get(&w.op, "/api/v1/admin/users?limit=2").await;
        let cursor = page.body["next_cursor"].as_str().unwrap().to_owned();
        let decoded = String::from_utf8(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(&cursor)
                .unwrap(),
        )
        .unwrap();
        assert!(
            !decoded.contains('@') && !decoded.contains("example"),
            "{decoded}"
        );
        let second = w
            .get(
                &w.op,
                &format!("/api/v1/admin/users?limit=2&cursor={cursor}"),
            )
            .await;
        assert_eq!(second.status, StatusCode::OK);
        assert_ne!(second.body["items"][0]["id"], page.body["items"][0]["id"]);
        let wrong = w
            .get(
                &w.op,
                &format!("/api/v1/admin/users?unverified=true&cursor={cursor}"),
            )
            .await;
        assert_status(&wrong, StatusCode::BAD_REQUEST, "INVALID_CURSOR");

        // Operator only: owners, admins and members get 403; anonymous 401.
        for who in [&c.owner.1, &c.admin.1, &c.wsadmin.1, &c.outsider.1] {
            forbidden(&w.get(who, "/api/v1/admin/users").await);
        }
        assert_status(
            &call(&w.env.app, Req::new("GET", "/api/v1/admin/users")).await,
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
        );
        // Remedy: disabling the squatted account stops its logins.
        let disable = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/admin/users/{new_id}/disable")).session(&w.op),
        )
        .await;
        assert_eq!(disable.status, StatusCode::OK, "{}", disable.text);
        assert_eq!(
            call(
                &w.env.app,
                Req::new("POST", "/api/v1/auth/login").json(
                    json!({"email": "squat@example.test", "password": "long-enough-password"})
                )
            )
            .await
            .status,
            StatusCode::UNAUTHORIZED
        );
    }
}
