//! AC-08b HTTP tests: self-service profile (`PATCH /auth/profile`) and the
//! organization overview (`GET /organizations/{id}/overview`). Run with
//! `cargo xtask db-test`.

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use crate::common::{Req, Resp, Session, TestEnv, call};
    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use uuid::Uuid;

    fn uuid(value: &Value, key: &str) -> Uuid {
        Uuid::parse_str(value[key].as_str().unwrap()).unwrap()
    }

    async fn patch(env: &TestEnv, session: &Session, body: Value) -> Resp {
        call(
            &env.app,
            Req::new("PATCH", "/api/v1/auth/profile")
                .session(session)
                .json(body),
        )
        .await
    }

    async fn stored_name(env: &TestEnv, id: Uuid) -> String {
        sqlx::query_scalar("SELECT display_name FROM users WHERE id=$1")
            .bind(id)
            .fetch_one(&env.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn profile_update_trims_returns_the_account_and_audits_without_the_name() {
        let env = TestEnv::new().await;
        let id = env.user("prof-ok").await;
        let session = env.login_ok("prof-ok").await;
        let response = patch(&env, &session, json!({"display_name": "  Ada Neu  "})).await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.text);
        assert_eq!(response.body["display_name"], "Ada Neu");
        assert_eq!(response.body["email"], "prof-ok@example.test");
        assert_eq!(response.body["user_id"], id.to_string());
        assert_eq!(response.body["csrf_token"], session.csrf);
        assert_eq!(stored_name(&env, id).await, "Ada Neu");
        let current = env.get_session(&session).await;
        assert_eq!(current.body["display_name"], "Ada Neu");

        let events: Vec<(Option<Uuid>, Option<Uuid>, String, String)> = sqlx::query_as(
            "SELECT actor_id, target_id, result, details::text FROM audit_events WHERE action='auth.profile_updated'",
        )
        .fetch_all(&env.pool)
        .await
        .unwrap();
        assert_eq!(events.len(), 1);
        let (actor, target, result, details) = &events[0];
        assert_eq!(*actor, Some(id));
        assert_eq!(*target, Some(id));
        assert_eq!(result, "success");
        assert!(!details.contains("Ada"), "{details}");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn profile_update_rejects_invalid_names_and_unknown_fields() {
        let env = TestEnv::new().await;
        let id = env.user("prof-bad").await;
        let session = env.login_ok("prof-bad").await;
        let too_long = "x".repeat(101);
        for bad in [
            json!({"display_name": ""}),
            json!({"display_name": "   "}),
            json!({"display_name": too_long}),
            json!({"display_name": "a\nb"}),
            json!({"display_name": "a\u{0}b"}),
            json!({"display_name": "evil\u{202E}fdp"}),
            json!({"display_name": "\u{200B}"}),
            json!({}),
        ] {
            let response = patch(&env, &session, bad.clone()).await;
            assert_eq!(
                response.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{bad}: {}",
                response.text
            );
            assert_eq!(response.code(), "VALIDATION_ERROR");
            assert_eq!(
                response.body["error"]["details"]["fields"],
                json!(["display_name"]),
                "{bad}"
            );
        }
        for bad in [
            json!({"display_name": 7}),
            json!({"display_name": "Ok", "email": "other@example.test"}),
            json!({"display_name": "Ok", "instance_role": "operator"}),
        ] {
            let response = patch(&env, &session, bad.clone()).await;
            assert_eq!(
                response.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{bad}: {}",
                response.text
            );
        }
        assert_eq!(stored_name(&env, id).await, "prof-bad");
        assert_eq!(
            env.count("SELECT count(*) FROM audit_events WHERE action='auth.profile_updated'")
                .await,
            0
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn profile_update_needs_a_session_and_the_csrf_token() {
        let env = TestEnv::new().await;
        let id = env.user("prof-auth").await;
        let session = env.login_ok("prof-auth").await;
        let anonymous = call(
            &env.app,
            Req::new("PATCH", "/api/v1/auth/profile").json(json!({"display_name": "Nope"})),
        )
        .await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "{}",
            anonymous.text
        );
        let without_csrf = call(
            &env.app,
            Req::new("PATCH", "/api/v1/auth/profile")
                .cookie_only(&session)
                .json(json!({"display_name": "Nope"})),
        )
        .await;
        assert_eq!(
            without_csrf.status,
            StatusCode::FORBIDDEN,
            "{}",
            without_csrf.text
        );
        assert_eq!(without_csrf.code(), "CSRF_FAILED");
        let wrong_csrf = call(
            &env.app,
            Req::new("PATCH", "/api/v1/auth/profile")
                .cookie_only(&session)
                .header("x-csrf-token", "wrong")
                .json(json!({"display_name": "Nope"})),
        )
        .await;
        assert_eq!(
            wrong_csrf.status,
            StatusCode::FORBIDDEN,
            "{}",
            wrong_csrf.text
        );
        assert_eq!(stored_name(&env, id).await, "prof-auth");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn profile_update_only_touches_the_calling_account() {
        let env = TestEnv::new().await;
        let first = env.user("prof-a").await;
        let second = env.user("prof-b").await;
        let session = env.login_ok("prof-a").await;
        let response = patch(&env, &session, json!({"display_name": "Changed"})).await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.text);
        assert_eq!(stored_name(&env, first).await, "Changed");
        assert_eq!(stored_name(&env, second).await, "prof-b");
    }

    struct World {
        env: TestEnv,
        owner: Session,
        org: Uuid,
        workspace: Uuid,
        file: Uuid,
    }

    async fn create_org(env: &TestEnv, operator: &Session, owner: Uuid, slug: &str) -> Uuid {
        let created = call(
            &env.app,
            Req::new("POST", "/api/v1/organizations")
                .session(operator)
                .json(json!({"name": slug, "slug": slug, "owner_user_id": owner.to_string()})),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        uuid(&created.body, "id")
    }

    /// Creates a workspace, a project and two files; returns the workspace
    /// and both file ids.
    async fn content(env: &TestEnv, owner: &Session, org: Uuid, slug: &str) -> (Uuid, Uuid, Uuid) {
        let workspace = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/workspaces"))
                .session(owner)
                .json(json!({"name": slug, "slug": slug})),
        )
        .await;
        assert_eq!(workspace.status, StatusCode::CREATED, "{}", workspace.text);
        let workspace = uuid(&workspace.body, "id");
        let project = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/workspaces/{workspace}/projects"))
                .session(owner)
                .json(json!({"name": "Project"})),
        )
        .await;
        assert_eq!(project.status, StatusCode::CREATED, "{}", project.text);
        let project = uuid(&project.body, "id");
        let mut files = Vec::new();
        for name in ["One", "Two"] {
            let file = call(
                &env.app,
                Req::new("POST", &format!("/api/v1/projects/{project}/files"))
                    .session(owner)
                    .json(json!({"name": name, "app": "photocraft", "format": "native"})),
            )
            .await;
            assert_eq!(file.status, StatusCode::CREATED, "{}", file.text);
            files.push(uuid(&file.body, "id"));
        }
        (workspace, files[0], files[1])
    }

    async fn world() -> World {
        let env = TestEnv::new().await;
        let operator = env.login_ok("op").await;
        let owner_id = env.user("ov-owner").await;
        let owner = env.login_ok("ov-owner").await;
        let org = create_org(&env, &operator, owner_id, "ov-a").await;
        let (workspace, file, _) = content(&env, &owner, org, "ov-ws").await;
        World {
            env,
            owner,
            org,
            workspace,
            file,
        }
    }

    async fn overview(env: &TestEnv, session: Option<&Session>, org: &str) -> Resp {
        let req = Req::new("GET", &format!("/api/v1/organizations/{org}/overview"));
        call(
            &env.app,
            match session {
                Some(session) => req.session(session),
                None => req,
            },
        )
        .await
    }

    async fn add_member(w: &World, name: &str, role: &str) -> (Uuid, Session) {
        let id = w.env.user(name).await;
        let session = w.env.login_ok(name).await;
        let response = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/organizations/{}/members", w.org))
                .session(&w.owner)
                .json(json!({"user_id": id.to_string(), "role": role})),
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED, "{}", response.text);
        (id, session)
    }

    async fn make_guest(w: &World, user: Uuid) {
        sqlx::query("UPDATE organization_members SET role='guest' WHERE org_id=$1 AND user_id=$2")
            .bind(w.org)
            .bind(user)
            .execute(&w.env.pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn overview_follows_the_permission_matrix() {
        let w = world().await;
        let org = w.org.to_string();
        let (_, admin) = add_member(&w, "ov-admin", "admin").await;
        let (_, member) = add_member(&w, "ov-member", "member").await;
        let (ws_admin_id, ws_admin) = add_member(&w, "ov-ws-admin", "member").await;
        let (guest_id, guest) = add_member(&w, "ov-guest", "member").await;
        make_guest(&w, guest_id).await;
        let made = call(
            &w.env.app,
            Req::new(
                "POST",
                &format!("/api/v1/workspaces/{}/members", w.workspace),
            )
            .session(&w.owner)
            .json(json!({"user_id": ws_admin_id.to_string(), "role": "admin"})),
        )
        .await;
        assert_eq!(made.status, StatusCode::CREATED, "{}", made.text);

        for (who, session) in [("owner", &w.owner), ("admin", &admin)] {
            let ok = overview(&w.env, Some(session), &org).await;
            assert_eq!(ok.status, StatusCode::OK, "{who}: {}", ok.text);
        }
        for (who, session) in [
            ("member", &member),
            ("workspace admin", &ws_admin),
            ("guest", &guest),
        ] {
            let denied = overview(&w.env, Some(session), &org).await;
            assert_eq!(
                denied.status,
                StatusCode::FORBIDDEN,
                "{who}: {}",
                denied.text
            );
        }
        // The instance operator has no role in this organization: 404, like
        // every other organization route.
        let operator = w.env.login_ok("op").await;
        let hidden = overview(&w.env, Some(&operator), &org).await;
        assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{}", hidden.text);
        w.env.user("ov-outsider").await;
        let outsider = w.env.login_ok("ov-outsider").await;
        let hidden = overview(&w.env, Some(&outsider), &org).await;
        assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{}", hidden.text);
        let anonymous = overview(&w.env, None, &org).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "{}",
            anonymous.text
        );
        let malformed = overview(&w.env, Some(&w.owner), "not-a-uuid").await;
        assert_eq!(
            malformed.status,
            StatusCode::NOT_FOUND,
            "{}",
            malformed.text
        );
        let unknown = overview(&w.env, Some(&w.owner), &Uuid::new_v4().to_string()).await;
        assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{}", unknown.text);
        // Another tenant's owner does not see this organization either.
        let other_owner_id = w.env.user("ov-other-owner").await;
        let other_owner = w.env.login_ok("ov-other-owner").await;
        create_org(&w.env, &operator, other_owner_id, "ov-b").await;
        let hidden = overview(&w.env, Some(&other_owner), &org).await;
        assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{}", hidden.text);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn overview_counts_only_the_own_organization() {
        let w = world().await;
        let org = w.org.to_string();
        let operator = w.env.login_ok("op").await;

        // People: owner, admin, member, guest, invited and a disabled account.
        add_member(&w, "cnt-admin", "admin").await;
        let (member_id, _) = add_member(&w, "cnt-member", "member").await;
        let (guest_id, _) = add_member(&w, "cnt-guest", "member").await;
        let (invited_id, _) = add_member(&w, "cnt-invited", "member").await;
        let (disabled_id, _) = add_member(&w, "cnt-disabled", "member").await;
        make_guest(&w, guest_id).await;
        sqlx::query(
            "UPDATE organization_members SET status='invited' WHERE org_id=$1 AND user_id=$2",
        )
        .bind(w.org)
        .bind(invited_id)
        .execute(&w.env.pool)
        .await
        .unwrap();
        sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
            .bind(disabled_id)
            .execute(&w.env.pool)
            .await
            .unwrap();
        // Two-factor: the member is enrolled, the guest only has a pending setup.
        sqlx::query(
            "INSERT INTO user_two_factor (user_id, secret_ciphertext, enabled_at, created_at) VALUES ($1, $2, now(), now())",
        )
        .bind(member_id)
        .bind(vec![7_u8; 40])
        .execute(&w.env.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO user_two_factor (user_id, secret_ciphertext, pending_session_id, pending_session_kind, pending_expires_at, created_at)              VALUES ($1, $2, gen_random_uuid(), 'browser', now() + interval '1 hour', now())",
        )
        .bind(guest_id)
        .bind(vec![7_u8; 40])
        .execute(&w.env.pool)
        .await
        .unwrap();

        // Content: second file in the trash, a lock, a grant, an invitation.
        let second_file: Uuid =
            sqlx::query_scalar("SELECT id FROM files WHERE org_id=$1 AND id<>$2")
                .bind(w.org)
                .bind(w.file)
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
        sqlx::query(
            "UPDATE files SET deleted_at=now(), deleted_by_user_id=created_by_user_id WHERE id=$1",
        )
        .bind(second_file)
        .execute(&w.env.pool)
        .await
        .unwrap();
        let lock = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/lock", w.file))
                .session(&w.owner)
                .json(json!({"client_id": "overview-test"})),
        )
        .await;
        assert_eq!(lock.status, StatusCode::OK, "{}", lock.text);
        let grant = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/grants", w.file))
                .session(&w.owner)
                .json(json!({"user_id": member_id.to_string(), "role": "viewer"})),
        )
        .await;
        assert_eq!(grant.status, StatusCode::CREATED, "{}", grant.text);
        let invitation = call(
            &w.env.app,
            Req::new(
                "POST",
                &format!("/api/v1/organizations/{}/invitations", w.org),
            )
            .session(&w.owner)
            .json(json!({"email": "cnt-new@example.test", "org_role": "member"})),
        )
        .await;
        assert_eq!(
            invitation.status,
            StatusCode::CREATED,
            "{}",
            invitation.text
        );
        sqlx::query(
            "UPDATE organizations SET quota_bytes=1000, committed_bytes=300, reserved_bytes=50 WHERE id=$1",
        )
        .bind(w.org)
        .execute(&w.env.pool)
        .await
        .unwrap();

        // Activity: events 2 and 5 days ago (one aggregated), one outside the window.
        for (days, count) in [(2_i32, 3_i32), (5, 1), (40, 9)] {
            sqlx::query(
                "INSERT INTO audit_events (id, org_id, action, result, created_at, event_count, last_at) \
                 VALUES ($1, $2, 'member.add', 'success', now() - make_interval(days => $3), $4, now() - make_interval(days => $3))",
            )
            .bind(Uuid::new_v4())
            .bind(w.org)
            .bind(days)
            .bind(count)
            .execute(&w.env.pool)
            .await
            .unwrap();
        }

        // A second organization with its own data must not leak into the counts.
        let other_owner_id = w.env.user("cnt-other-owner").await;
        let other_owner = w.env.login_ok("cnt-other-owner").await;
        let other = create_org(&w.env, &operator, other_owner_id, "ov-other").await;
        content(&w.env, &other_owner, other, "ov-other-ws").await;
        sqlx::query(
            "INSERT INTO audit_events (id, org_id, action, result, created_at, event_count, last_at) \
             VALUES ($1, $2, 'member.add', 'success', now() - interval '2 days', 100, now())",
        )
        .bind(Uuid::new_v4())
        .bind(other)
        .execute(&w.env.pool)
        .await
        .unwrap();

        let response = overview(&w.env, Some(&w.owner), &org).await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.text);
        let body = &response.body;
        assert_eq!(
            body["members"]["by_status"],
            json!({"active": 4, "invited": 1, "disabled": 1})
        );
        assert_eq!(body["members"]["total"], 6);
        assert_eq!(
            body["members"]["by_role"],
            json!({"owner": 1, "admin": 1, "member": 1, "guest": 1})
        );
        assert_eq!(
            body["two_factor"],
            json!({"active_members": 4, "enrolled": 1})
        );
        assert_eq!(
            body["counts"],
            json!({
                "workspaces": 1, "projects": 1, "files": 1, "active_locks": 1,
                "active_grants": 1, "pending_invitations": 1
            })
        );
        assert_eq!(
            body["storage"],
            json!({"used_bytes": 350, "committed_bytes": 300, "reserved_bytes": 50, "quota_bytes": 1000})
        );
        let days = body["activity_daily"].as_array().unwrap();
        assert_eq!(days.len(), 30);
        let today: String = sqlx::query_scalar("SELECT ((now() AT TIME ZONE 'UTC')::date)::text")
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(days[29]["date"], today);
        let at = |back: usize| -> i64 { days[29 - back]["count"].as_i64().unwrap() };
        assert_eq!(at(2), 3);
        assert_eq!(at(5), 1);
        assert_eq!(at(3), 0);
        assert_eq!(at(29), 0);
        let total: i64 = days.iter().map(|d| d["count"].as_i64().unwrap()).sum();
        let own_rows: i64 = w
            .env
            .count(&format!(
                "SELECT COALESCE(sum(event_count),0)::bigint FROM audit_events WHERE org_id='{}' AND created_at > now() - interval '29 days'",
                w.org
            ))
            .await;
        assert_eq!(total, own_rows);
        assert!(
            days.windows(2)
                .all(|pair| pair[0]["date"].as_str() < pair[1]["date"].as_str())
        );

        // The other organization reports only its own figures.
        let theirs = overview(&w.env, Some(&other_owner), &other.to_string()).await;
        assert_eq!(theirs.status, StatusCode::OK, "{}", theirs.text);
        assert_eq!(theirs.body["members"]["total"], 1);
        assert_eq!(theirs.body["counts"]["files"], 2);
        assert_eq!(theirs.body["counts"]["active_locks"], 0);
        assert_eq!(theirs.body["counts"]["active_grants"], 0);
        assert_eq!(theirs.body["storage"]["quota_bytes"], Value::Null);
        // No names, e-mail addresses or ids of people leak into the response.
        for needle in ["example.test", "cnt-", "ov-owner", "@"] {
            assert!(
                !response.text.contains(needle),
                "{needle} in {}",
                response.text
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn overview_ignores_expired_locks() {
        let w = world().await;
        let lock = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/lock", w.file))
                .session(&w.owner)
                .json(json!({"client_id": "overview-test"})),
        )
        .await;
        assert_eq!(lock.status, StatusCode::OK, "{}", lock.text);
        let first = overview(&w.env, Some(&w.owner), &w.org.to_string()).await;
        assert_eq!(first.body["counts"]["active_locks"], 1);
        sqlx::query("UPDATE file_locks SET acquired_at=now()-interval '2 hours', refreshed_at=now()-interval '2 hours', expires_at=now()-interval '1 hour' WHERE org_id=$1")
            .bind(w.org)
            .execute(&w.env.pool)
            .await
            .unwrap();
        let second = overview(&w.env, Some(&w.owner), &w.org.to_string()).await;
        assert_eq!(second.body["counts"]["active_locks"], 0);
        assert_eq!(second.body["counts"]["pending_invitations"], 0);
    }

    async fn grant(w: &World, kind: &str, id: Uuid, user: Uuid, role: &str) {
        let response = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/{kind}/{id}/grants"))
                .session(&w.owner)
                .json(json!({"user_id": user.to_string(), "role": role})),
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED, "{}", response.text);
    }

    async fn shared(w: &World, session: &Session) -> Value {
        let response = call(
            &w.env.app,
            Req::new(
                "GET",
                &format!("/api/v1/organizations/{}/shared-with-me", w.org),
            )
            .session(session),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.text);
        response.body
    }

    async fn file_detail(w: &World, session: &Session) -> Value {
        let response = call(
            &w.env.app,
            Req::new("GET", &format!("/api/v1/files/{}", w.file)).session(session),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.text);
        response.body
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn guest_with_a_file_grant_gets_no_workspace_or_project_names() {
        let w = world().await;
        let project: Uuid = sqlx::query_scalar("SELECT project_id FROM files WHERE id=$1")
            .bind(w.file)
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        let (guest_id, guest) = add_member(&w, "red-guest", "member").await;
        make_guest(&w, guest_id).await;
        grant(&w, "files", w.file, guest_id, "viewer").await;

        let body = shared(&w, &guest).await;
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["target_kind"], "file");
        assert_eq!(items[0]["file"]["name"], "One");
        assert_eq!(items[0]["workspace"]["id"], w.workspace.to_string());
        assert_eq!(items[0]["workspace"]["name"], Value::Null);
        assert_eq!(items[0]["project"], Value::Null);
        let text = serde_json::to_string(&body).unwrap();
        for name in ["ov-ws", "Project"] {
            assert!(!text.contains(name), "{name} in {text}");
        }
        let detail = file_detail(&w, &guest).await;
        assert_eq!(detail["project"]["id"], project.to_string());
        assert_eq!(detail["project"]["name"], Value::Null);
        assert_eq!(detail["name"], "One");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn guest_with_a_project_grant_sees_the_project_but_not_the_workspace_name() {
        let w = world().await;
        let project: Uuid = sqlx::query_scalar("SELECT project_id FROM files WHERE id=$1")
            .bind(w.file)
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        let (guest_id, guest) = add_member(&w, "red-guest-p", "member").await;
        make_guest(&w, guest_id).await;
        grant(&w, "projects", project, guest_id, "editor").await;

        let body = shared(&w, &guest).await;
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["target_kind"], "project");
        assert_eq!(items[0]["project"]["name"], "Project");
        assert_eq!(items[0]["workspace"]["name"], Value::Null);
        assert!(!serde_json::to_string(&body).unwrap().contains("ov-ws"));
        let detail = file_detail(&w, &guest).await;
        assert_eq!(detail["project"]["name"], "Project");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn member_keeps_workspace_and_project_names() {
        let w = world().await;
        let (member_id, member) = add_member(&w, "red-member", "member").await;
        grant(&w, "files", w.file, member_id, "viewer").await;
        let body = shared(&w, &member).await;
        let item = &body["items"][0];
        assert_eq!(item["workspace"]["name"], "ov-ws");
        assert_eq!(item["project"]["name"], "Project");
        assert_eq!(item["file"]["name"], "One");
        let detail = file_detail(&w, &member).await;
        assert_eq!(detail["project"]["name"], "Project");
    }
}
