//! AC-03 HTTP security tests. Run with `cargo xtask db-test`.

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use crate::common::{Req, Session, TestEnv, call};
    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use uuid::Uuid;

    struct Fixture {
        env: TestEnv,
        owner: Session,
        org: Uuid,
        workspace: Uuid,
        project: Uuid,
        file: Uuid,
        sibling: Uuid,
    }

    async fn fixture(app: &str) -> Fixture {
        fixture_in(TestEnv::new().await, app).await
    }

    async fn fixture_in(env: TestEnv, app: &str) -> Fixture {
        let operator = env.login_ok("op").await;
        let owner_id = env.user("ac03-owner").await;
        let owner = env.login_ok("ac03-owner").await;
        let created=call(&env.app,Req::new("POST","/api/v1/organizations").session(&operator).json(json!({"name":"AC03","slug":format!("ac03-{}",Uuid::new_v4().simple()),"owner_user_id":owner_id.to_string()}))).await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        let org = Uuid::parse_str(created.body["id"].as_str().unwrap()).unwrap();
        let workspace = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/workspaces"))
                .session(&owner)
                .json(json!({"name":"AC03 workspace","slug":"ac03"})),
        )
        .await;
        assert_eq!(workspace.status, StatusCode::CREATED, "{}", workspace.text);
        let workspace_id = Uuid::parse_str(workspace.body["id"].as_str().unwrap()).unwrap();
        let project = call(
            &env.app,
            Req::new(
                "POST",
                &format!("/api/v1/workspaces/{workspace_id}/projects"),
            )
            .session(&owner)
            .json(json!({"name":"Project"})),
        )
        .await;
        assert_eq!(project.status, StatusCode::CREATED, "{}", project.text);
        let project_id = Uuid::parse_str(project.body["id"].as_str().unwrap()).unwrap();
        let file = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/projects/{project_id}/files"))
                .session(&owner)
                .json(json!({"name":"File","app":app,"format":"native"})),
        )
        .await;
        assert_eq!(file.status, StatusCode::CREATED, "{}", file.text);
        let file_id = Uuid::parse_str(file.body["id"].as_str().unwrap()).unwrap();
        let sibling = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/projects/{project_id}/files"))
                .session(&owner)
                .json(json!({"name":"Sibling","app":app,"format":"native"})),
        )
        .await;
        assert_eq!(sibling.status, StatusCode::CREATED, "{}", sibling.text);
        Fixture {
            env,
            owner,
            org,
            workspace: workspace_id,
            project: project_id,
            file: file_id,
            sibling: Uuid::parse_str(sibling.body["id"].as_str().unwrap()).unwrap(),
        }
    }

    async fn add_member(f: &Fixture, name: &str) -> (Uuid, Session) {
        let id = f.env.user(name).await;
        let session = f.env.login_ok(name).await;
        let response = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/organizations/{}/members", f.org))
                .session(&f.owner)
                .json(json!({"user_id":id.to_string(),"role":"member"})),
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED, "{}", response.text);
        (id, session)
    }

    async fn add_workspace_role(f: &Fixture, name: &str, role: &str) -> (Uuid, Session) {
        let (id, session) = add_member(f, name).await;
        let response = call(
            &f.env.app,
            Req::new(
                "POST",
                &format!("/api/v1/workspaces/{}/members", f.workspace),
            )
            .session(&f.owner)
            .json(json!({"user_id":id.to_string(),"role":role})),
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED, "{}", response.text);
        (id, session)
    }

    fn uuid(value: &Value, key: &str) -> Uuid {
        Uuid::parse_str(value[key].as_str().unwrap()).unwrap()
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn organization_grants_and_locks_are_scoped_filtered_and_paginated() {
        let f = fixture("photocraft").await;
        let (_, workspace_admin) = add_workspace_role(&f, "ac08-ws-admin", "admin").await;
        let (grantee_id, _) = add_member(&f, "ac08-grantee").await;
        let second_ws = call(
            &f.env.app,
            Req::new(
                "POST",
                &format!("/api/v1/organizations/{}/workspaces", f.org),
            )
            .session(&f.owner)
            .json(json!({"name":"Second workspace","slug":"ac08-second"})),
        )
        .await;
        assert_eq!(second_ws.status, StatusCode::CREATED, "{}", second_ws.text);
        let second_ws_id = uuid(&second_ws.body, "id");
        let project = call(
            &f.env.app,
            Req::new(
                "POST",
                &format!("/api/v1/workspaces/{second_ws_id}/projects"),
            )
            .session(&f.owner)
            .json(json!({"name":"Other project"})),
        )
        .await;
        assert_eq!(project.status, StatusCode::CREATED, "{}", project.text);
        let other_file = call(
            &f.env.app,
            Req::new(
                "POST",
                &format!("/api/v1/projects/{}/files", uuid(&project.body, "id")),
            )
            .session(&f.owner)
            .json(json!({"name":"Other file","app":"photocraft","format":"native"})),
        )
        .await;
        assert_eq!(
            other_file.status,
            StatusCode::CREATED,
            "{}",
            other_file.text
        );
        let other_file_id = uuid(&other_file.body, "id");
        for file in [f.file, other_file_id] {
            let grant = call(
                &f.env.app,
                Req::new("POST", &format!("/api/v1/files/{file}/grants"))
                    .session(&f.owner)
                    .json(json!({"user_id":grantee_id.to_string(),"role":"viewer"})),
            )
            .await;
            assert_eq!(grant.status, StatusCode::CREATED, "{}", grant.text);
            let locked = call(
                &f.env.app,
                Req::new("POST", &format!("/api/v1/files/{file}/lock"))
                    .session(&f.owner)
                    .json(json!({"client_id":"admin-console-test"})),
            )
            .await;
            assert_eq!(locked.status, StatusCode::OK, "{}", locked.text);
        }

        let grants = call(
            &f.env.app,
            Req::new(
                "GET",
                &format!(
                    "/api/v1/organizations/{}/grants?limit=1&target_kind=file",
                    f.org
                ),
            )
            .session(&f.owner),
        )
        .await;
        assert_eq!(grants.status, StatusCode::OK, "{}", grants.text);
        assert_eq!(grants.body["items"].as_array().unwrap().len(), 1);
        let mut grant_targets = vec![(
            grants.body["items"][0]["target_id"]
                .as_str()
                .unwrap()
                .to_owned(),
            grants.body["items"][0]["target_name"]
                .as_str()
                .unwrap()
                .to_owned(),
        )];
        assert!(grants.body["next_cursor"].is_string());
        let cursor = grants.body["next_cursor"].as_str().unwrap();
        let second_page = call(
            &f.env.app,
            Req::new(
                "GET",
                &format!(
                    "/api/v1/organizations/{}/grants?limit=1&target_kind=file&cursor={cursor}",
                    f.org
                ),
            )
            .session(&f.owner),
        )
        .await;
        assert_eq!(second_page.status, StatusCode::OK, "{}", second_page.text);
        assert_eq!(second_page.body["items"].as_array().unwrap().len(), 1);
        grant_targets.push((
            second_page.body["items"][0]["target_id"]
                .as_str()
                .unwrap()
                .to_owned(),
            second_page.body["items"][0]["target_name"]
                .as_str()
                .unwrap()
                .to_owned(),
        ));
        grant_targets.sort();
        let mut expected_targets = vec![
            (f.file.to_string(), "File".to_owned()),
            (other_file_id.to_string(), "Other file".to_owned()),
        ];
        expected_targets.sort();
        assert_eq!(grant_targets, expected_targets);
        let filtered = call(
            &f.env.app,
            Req::new(
                "GET",
                &format!(
                    "/api/v1/organizations/{}/grants?user_id={grantee_id}&workspace_id={}",
                    f.org, f.workspace
                ),
            )
            .session(&workspace_admin),
        )
        .await;
        assert_eq!(filtered.status, StatusCode::OK, "{}", filtered.text);
        assert_eq!(filtered.body["items"].as_array().unwrap().len(), 1);
        assert_eq!(
            filtered.body["items"][0]["workspace_id"],
            f.workspace.to_string()
        );
        let locks = call(
            &f.env.app,
            Req::new("GET", &format!("/api/v1/organizations/{}/locks", f.org))
                .session(&workspace_admin),
        )
        .await;
        assert_eq!(locks.status, StatusCode::OK, "{}", locks.text);
        assert_eq!(locks.body["items"].as_array().unwrap().len(), 1);
        assert_eq!(locks.body["items"][0]["file_id"], f.file.to_string());
        assert_eq!(locks.body["items"][0]["session_kind"], "browser");
        assert!(!locks.text.contains("session_id"));
        let raw_session: Uuid =
            sqlx::query_scalar("SELECT holder_session_id FROM file_locks WHERE file_id=$1")
                .bind(f.file)
                .fetch_one(&f.env.pool)
                .await
                .unwrap();
        assert!(!locks.text.contains(&raw_session.to_string()));
        let outsider_id = f.env.user("ac08-outsider").await;
        let outsider = f.env.login_ok("ac08-outsider").await;
        let hidden = call(
            &f.env.app,
            Req::new("GET", &format!("/api/v1/organizations/{}/grants", f.org)).session(&outsider),
        )
        .await;
        assert_eq!(
            hidden.status,
            StatusCode::NOT_FOUND,
            "outsider {outsider_id}: {}",
            hidden.text
        );
        let operator = f.env.login_ok("op").await;
        let foreign_owner_id = f.env.user("ac08-foreign-owner").await;
        let foreign_owner = f.env.login_ok("ac08-foreign-owner").await;
        let foreign_org = call(
            &f.env.app,
            Req::new("POST", "/api/v1/organizations")
                .session(&operator)
                .json(json!({"name":"Foreign org","slug":format!("ac08-foreign-{}", Uuid::new_v4().simple()),"owner_user_id":foreign_owner_id.to_string()})),
        ).await;
        assert_eq!(
            foreign_org.status,
            StatusCode::CREATED,
            "{}",
            foreign_org.text
        );
        let foreign_org_id = uuid(&foreign_org.body, "id");
        let foreign_workspace = call(
            &f.env.app,
            Req::new(
                "POST",
                &format!("/api/v1/organizations/{foreign_org_id}/workspaces"),
            )
            .session(&foreign_owner)
            .json(json!({"name":"Foreign workspace","slug":"foreign"})),
        )
        .await;
        assert_eq!(
            foreign_workspace.status,
            StatusCode::CREATED,
            "{}",
            foreign_workspace.text
        );
        for workspace_id in [uuid(&foreign_workspace.body, "id"), Uuid::new_v4()] {
            for collection in ["grants", "locks"] {
                let response = call(
                    &f.env.app,
                    Req::new(
                        "GET",
                        &format!(
                            "/api/v1/organizations/{}/{collection}?workspace_id={workspace_id}",
                            f.org
                        ),
                    )
                    .session(&f.owner),
                )
                .await;
                assert_eq!(
                    response.status,
                    StatusCode::NOT_FOUND,
                    "{collection} {workspace_id}: {}",
                    response.text
                );
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn grant_crud_authorization_matrix_covers_organization_and_workspace_roles() {
        let f = fixture("photocraft").await;
        let operator = f.env.login_ok("op").await;
        let (org_admin_id, org_admin) = add_member(&f, "ac03-grant-org-admin").await;
        let org_admin_role = call(
            &f.env.app,
            Req::new(
                "PATCH",
                &format!("/api/v1/organizations/{}/members/{org_admin_id}", f.org),
            )
            .session(&f.owner)
            .json(json!({"role":"admin"})),
        )
        .await;
        assert_eq!(
            org_admin_role.status,
            StatusCode::OK,
            "{}",
            org_admin_role.text
        );
        let (_, ws_admin) = add_workspace_role(&f, "ac03-grant-ws-admin", "admin").await;
        let (_, editor) = add_workspace_role(&f, "ac03-grant-editor", "editor").await;
        let (_, viewer) = add_workspace_role(&f, "ac03-grant-viewer", "viewer").await;
        let (_, member) = add_member(&f, "ac03-grant-member").await;
        let guest_id = f.env.user("ac03-grant-guest").await;
        let guest = f.env.login_ok("ac03-grant-guest").await;
        sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'guest','active')")
            .bind(f.org).bind(guest_id).execute(&f.env.pool).await.unwrap();

        for (name, actor) in [
            ("owner", &f.owner),
            ("org admin", &org_admin),
            ("workspace admin", &ws_admin),
            ("workspace editor", &editor),
        ] {
            let (target_id, _) =
                add_member(&f, &format!("ac03-grant-target-{}", name.replace(' ', "-"))).await;
            let created = call(
                &f.env.app,
                Req::new("POST", &format!("/api/v1/files/{}/grants", f.file))
                    .session(actor)
                    .json(json!({"user_id":target_id.to_string(),"role":"viewer"})),
            )
            .await;
            assert_eq!(
                created.status,
                StatusCode::CREATED,
                "{name}: {}",
                created.text
            );
            let grant_id = uuid(&created.body, "id");
            let listed = call(
                &f.env.app,
                Req::new("GET", &format!("/api/v1/files/{}/grants", f.file)).cookie_only(actor),
            )
            .await;
            assert_eq!(listed.status, StatusCode::OK, "{name}: {}", listed.text);
            assert!(
                listed.body["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|g| g["id"] == grant_id.to_string())
            );
            let changed = call(
                &f.env.app,
                Req::new("PATCH", &format!("/api/v1/grants/{grant_id}"))
                    .session(actor)
                    .if_match("\"grant-v1\"")
                    .json(json!({"role":"editor"})),
            )
            .await;
            assert_eq!(changed.status, StatusCode::OK, "{name}: {}", changed.text);
            assert_eq!(changed.body["role"], "editor");
            let revoked = call(
                &f.env.app,
                Req::new("DELETE", &format!("/api/v1/grants/{grant_id}")).session(actor),
            )
            .await;
            assert_eq!(
                revoked.status,
                StatusCode::NO_CONTENT,
                "{name}: {}",
                revoked.text
            );
        }

        let (target_id, _) = add_member(&f, "ac03-grant-denied-target").await;
        let owned = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/grants", f.file))
                .session(&f.owner)
                .json(json!({"user_id":target_id.to_string(),"role":"viewer"})),
        )
        .await;
        assert_eq!(owned.status, StatusCode::CREATED, "{}", owned.text);
        let grant_id = uuid(&owned.body, "id");
        for (name, actor) in [("workspace viewer", &viewer)] {
            assert_eq!(
                call(
                    &f.env.app,
                    Req::new("GET", &format!("/api/v1/files/{}/grants", f.file)).cookie_only(actor)
                )
                .await
                .status,
                StatusCode::FORBIDDEN,
                "{name} grant list",
            );
            assert_eq!(
                call(
                    &f.env.app,
                    Req::new("POST", &format!("/api/v1/files/{}/grants", f.file))
                        .session(actor)
                        .json(json!({"user_id":target_id.to_string(),"role":"viewer"}))
                )
                .await
                .status,
                StatusCode::FORBIDDEN,
                "{name} grant create",
            );
            assert_eq!(
                call(
                    &f.env.app,
                    Req::new("PATCH", &format!("/api/v1/grants/{grant_id}"))
                        .session(actor)
                        .if_match("\"grant-v1\"")
                        .json(json!({"role":"editor"}))
                )
                .await
                .status,
                StatusCode::FORBIDDEN,
                "{name} grant change",
            );
            assert_eq!(
                call(
                    &f.env.app,
                    Req::new("DELETE", &format!("/api/v1/grants/{grant_id}")).session(actor)
                )
                .await
                .status,
                StatusCode::FORBIDDEN,
                "{name} grant revoke",
            );
        }
        for (name, actor) in [
            ("organization member", &member),
            ("organization guest", &guest),
        ] {
            assert_eq!(
                call(
                    &f.env.app,
                    Req::new("GET", &format!("/api/v1/files/{}", f.file)).cookie_only(actor)
                )
                .await
                .status,
                StatusCode::NOT_FOUND,
                "{name} without grant",
            );
        }

        let foreign_id = f.env.user("ac03-grant-foreign").await;
        let foreign_owner = f.env.login_ok("ac03-grant-foreign").await;
        let foreign_org = call(
            &f.env.app,
            Req::new("POST", "/api/v1/organizations")
                .session(&operator)
                .json(json!({"name":"Foreign","slug":format!("ac03-foreign-{}",Uuid::new_v4().simple()),"owner_user_id":foreign_id.to_string()})),
        )
        .await;
        assert_eq!(
            foreign_org.status,
            StatusCode::CREATED,
            "{}",
            foreign_org.text
        );
        let foreign_org_id = uuid(&foreign_org.body, "id");
        let foreign_workspace = call(
            &f.env.app,
            Req::new(
                "POST",
                &format!("/api/v1/organizations/{foreign_org_id}/workspaces"),
            )
            .session(&foreign_owner)
            .json(json!({"name":"Foreign workspace","slug":"foreign"})),
        )
        .await;
        assert_eq!(
            foreign_workspace.status,
            StatusCode::CREATED,
            "{}",
            foreign_workspace.text
        );
        let foreign_workspace_id = uuid(&foreign_workspace.body, "id");
        let foreign_project = call(
            &f.env.app,
            Req::new(
                "POST",
                &format!("/api/v1/workspaces/{foreign_workspace_id}/projects"),
            )
            .session(&foreign_owner)
            .json(json!({"name":"Foreign project"})),
        )
        .await;
        assert_eq!(
            foreign_project.status,
            StatusCode::CREATED,
            "{}",
            foreign_project.text
        );
        let foreign_project_id = uuid(&foreign_project.body, "id");
        let foreign_file = call(
            &f.env.app,
            Req::new(
                "POST",
                &format!("/api/v1/projects/{foreign_project_id}/files"),
            )
            .session(&foreign_owner)
            .json(json!({"name":"Foreign file","app":"photocraft","format":"native"})),
        )
        .await;
        assert_eq!(
            foreign_file.status,
            StatusCode::CREATED,
            "{}",
            foreign_file.text
        );
        let foreign_file_id = uuid(&foreign_file.body, "id");
        let unknown_target = Uuid::new_v4();
        for (route, method, body) in [
            (
                format!("/api/v1/files/{foreign_file_id}/grants"),
                "GET",
                None,
            ),
            (
                format!("/api/v1/files/{foreign_file_id}/grants"),
                "POST",
                Some(json!({"user_id":target_id.to_string(),"role":"viewer"})),
            ),
            (
                format!("/api/v1/projects/{foreign_project_id}/grants"),
                "GET",
                None,
            ),
            (
                format!("/api/v1/projects/{foreign_project_id}/grants"),
                "POST",
                Some(json!({"user_id":target_id.to_string(),"role":"viewer"})),
            ),
            (
                format!("/api/v1/files/{foreign_file_id}/lock"),
                "POST",
                Some(json!({"client_id":"desktop"})),
            ),
        ] {
            let mut request = Req::new(method, &route).session(&f.owner);
            if let Some(body) = &body {
                request = request.json(body.clone());
            }
            let hidden_foreign = call(&f.env.app, request).await;
            assert_eq!(
                hidden_foreign.status,
                StatusCode::NOT_FOUND,
                "{method} {route}: {}",
                hidden_foreign.text
            );
            let unknown_route = route
                .replace(&foreign_file_id.to_string(), &unknown_target.to_string())
                .replace(&foreign_project_id.to_string(), &unknown_target.to_string());
            let mut request = Req::new(method, &unknown_route).session(&f.owner);
            if let Some(body) = &body {
                request = request.json(body.clone());
            }
            let hidden_unknown = call(&f.env.app, request).await;
            assert_eq!(
                hidden_unknown.status, hidden_foreign.status,
                "{method} {unknown_route}: {}",
                hidden_unknown.text
            );
        }
        let cross_org = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/grants", f.file))
                .session(&f.owner)
                .json(json!({"user_id":foreign_id.to_string(),"role":"viewer"})),
        )
        .await;
        assert_eq!(cross_org.status, StatusCode::CONFLICT, "{}", cross_org.text);
        assert_eq!(cross_org.body["error"]["code"], "GRANTEE_NOT_MEMBER");
        let unknown_id = f.env.user("ac03-grant-unknown").await;
        let unknown_recipient = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/grants", f.file))
                .session(&f.owner)
                .json(json!({"user_id":unknown_id.to_string(),"role":"viewer"})),
        )
        .await;
        assert_eq!(
            unknown_recipient.status,
            StatusCode::CONFLICT,
            "{}",
            unknown_recipient.text
        );
        assert_eq!(
            unknown_recipient.body["error"]["code"],
            "GRANTEE_NOT_MEMBER"
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn file_grant_does_not_reveal_project_or_sibling_and_shared_item_is_minimal() {
        let f = fixture("photocraft").await;
        let (user_id, recipient) = add_member(&f, "ac03-recipient").await;
        let grant = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/grants", f.file))
                .session(&f.owner)
                .json(json!({"user_id":user_id.to_string(),"role":"viewer"})),
        )
        .await;
        assert_eq!(grant.status, StatusCode::CREATED, "{}", grant.text);
        assert_eq!(grant.body["target_kind"], "file");
        assert_eq!(grant.body["grantee"]["user_id"], user_id.to_string());
        let grant_id = uuid(&grant.body, "id");
        assert_eq!(
            call(
                &f.env.app,
                Req::new("GET", &format!("/api/v1/files/{}/grants", f.file))
                    .cookie_only(&recipient)
            )
            .await
            .status,
            StatusCode::FORBIDDEN,
        );
        assert_eq!(
            call(
                &f.env.app,
                Req::new("POST", &format!("/api/v1/files/{}/grants", f.file))
                    .session(&recipient)
                    .json(json!({"user_id":Uuid::new_v4().to_string(),"role":"viewer"}))
            )
            .await
            .status,
            StatusCode::FORBIDDEN,
        );
        assert_eq!(
            call(
                &f.env.app,
                Req::new("PATCH", &format!("/api/v1/grants/{grant_id}"))
                    .session(&recipient)
                    .if_match("\"grant-v1\"")
                    .json(json!({"role":"editor"}))
            )
            .await
            .status,
            StatusCode::FORBIDDEN,
        );
        assert_eq!(
            call(
                &f.env.app,
                Req::new("DELETE", &format!("/api/v1/grants/{grant_id}")).session(&recipient)
            )
            .await
            .status,
            StatusCode::FORBIDDEN,
        );
        let file = call(
            &f.env.app,
            Req::new("GET", &format!("/api/v1/files/{}", f.file)).cookie_only(&recipient),
        )
        .await;
        assert_eq!(file.status, StatusCode::OK, "{}", file.text);
        assert_eq!(file.body["can_share"], false);
        assert_eq!(file.body["project"]["id"], f.project.to_string());
        assert_eq!(file.body["project"]["name"], "Project");
        assert_eq!(
            call(
                &f.env.app,
                Req::new("GET", &format!("/api/v1/files/{}", f.sibling)).cookie_only(&recipient)
            )
            .await
            .status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(
                &f.env.app,
                Req::new("GET", &format!("/api/v1/projects/{}", f.project)).cookie_only(&recipient)
            )
            .await
            .status,
            StatusCode::NOT_FOUND
        );
        let (outsider_id, outsider) = add_member(&f, "ac03-outsider").await;
        assert_eq!(
            call(
                &f.env.app,
                Req::new("GET", &format!("/api/v1/files/{}", f.file)).cookie_only(&outsider)
            )
            .await
            .status,
            StatusCode::NOT_FOUND
        );
        let guest_id = f.env.user("ac03-guest").await;
        let guest = f.env.login_ok("ac03-guest").await;
        sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'guest','active')").bind(f.org).bind(guest_id).execute(&f.env.pool).await.unwrap();
        assert_eq!(
            call(
                &f.env.app,
                Req::new("GET", &format!("/api/v1/files/{}", f.file)).cookie_only(&guest)
            )
            .await
            .status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(
                &f.env.app,
                Req::new("GET", &format!("/api/v1/organizations/{}/members", f.org))
                    .cookie_only(&guest)
            )
            .await
            .status,
            StatusCode::FORBIDDEN
        );
        let shared = call(
            &f.env.app,
            Req::new(
                "GET",
                &format!("/api/v1/organizations/{}/shared-with-me", f.org),
            )
            .cookie_only(&recipient),
        )
        .await;
        assert_eq!(shared.status, StatusCode::OK, "{}", shared.text);
        assert_eq!(shared.body["items"].as_array().unwrap().len(), 1);
        let item = &shared.body["items"][0];
        assert_eq!(item["target_id"], f.file.to_string());
        assert_eq!(item["project"]["id"], f.project.to_string());
        assert_eq!(item["file"]["id"], f.file.to_string());
        let forbidden_search = call(
            &f.env.app,
            Req::new(
                "GET",
                &format!("/api/v1/organizations/{}/members?query=ac03", f.org),
            )
            .cookie_only(&recipient),
        )
        .await;
        assert_eq!(forbidden_search.status, StatusCode::FORBIDDEN);
        let allowed_search = call(
            &f.env.app,
            Req::new(
                "GET",
                &format!("/api/v1/organizations/{}/members?query=ac03", f.org),
            )
            .cookie_only(&f.owner),
        )
        .await;
        assert_eq!(
            allowed_search.status,
            StatusCode::OK,
            "{}",
            allowed_search.text
        );
        let missing = call(
            &f.env.app,
            Req::new("PATCH", &format!("/api/v1/grants/{grant_id}"))
                .session(&f.owner)
                .json(json!({"role":"editor"})),
        )
        .await;
        assert_eq!(missing.status, StatusCode::PRECONDITION_REQUIRED);
        let changed = call(
            &f.env.app,
            Req::new("PATCH", &format!("/api/v1/grants/{grant_id}"))
                .session(&f.owner)
                .if_match("\"grant-v1\"")
                .json(json!({"role":"editor"})),
        )
        .await;
        assert_eq!(changed.status, StatusCode::OK, "{}", changed.text);
        assert_eq!(changed.body["version"], 2);
        let denied_reshare = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/grants", f.file))
                .session(&recipient)
                .json(json!({"user_id":outsider_id.to_string(),"role":"viewer"})),
        )
        .await;
        assert_eq!(
            denied_reshare.status,
            StatusCode::FORBIDDEN,
            "{}",
            denied_reshare.text
        );
        let denied_grant_list = call(
            &f.env.app,
            Req::new("GET", &format!("/api/v1/files/{}/grants", f.file)).cookie_only(&recipient),
        )
        .await;
        assert_eq!(denied_grant_list.status, StatusCode::FORBIDDEN);
        let denied_grant_change = call(
            &f.env.app,
            Req::new("PATCH", &format!("/api/v1/grants/{grant_id}"))
                .session(&recipient)
                .if_match("\"grant-v2\"")
                .json(json!({"role":"viewer"})),
        )
        .await;
        assert_eq!(denied_grant_change.status, StatusCode::FORBIDDEN);
        let denied_grant_revoke = call(
            &f.env.app,
            Req::new("DELETE", &format!("/api/v1/grants/{grant_id}")).session(&recipient),
        )
        .await;
        assert_eq!(denied_grant_revoke.status, StatusCode::FORBIDDEN);
        let workspace_editor_id = f.env.user("ac03-ws-editor").await;
        let workspace_editor = f.env.login_ok("ac03-ws-editor").await;
        assert_eq!(
            call(
                &f.env.app,
                Req::new("POST", &format!("/api/v1/organizations/{}/members", f.org))
                    .session(&f.owner)
                    .json(json!({"user_id":workspace_editor_id.to_string(),"role":"member"}))
            )
            .await
            .status,
            StatusCode::CREATED
        );
        assert_eq!(
            call(
                &f.env.app,
                Req::new(
                    "POST",
                    &format!("/api/v1/workspaces/{}/members", f.workspace)
                )
                .session(&f.owner)
                .json(json!({"user_id":workspace_editor_id.to_string(),"role":"editor"}))
            )
            .await
            .status,
            StatusCode::CREATED
        );
        let can_share = call(
            &f.env.app,
            Req::new("GET", &format!("/api/v1/files/{}", f.sibling)).cookie_only(&workspace_editor),
        )
        .await;
        assert_eq!(can_share.body["can_share"], true);
        let ws_grant = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/grants", f.sibling))
                .session(&workspace_editor)
                .json(json!({"user_id":outsider_id.to_string(),"role":"viewer"})),
        )
        .await;
        assert_eq!(ws_grant.status, StatusCode::CREATED, "{}", ws_grant.text);
        let stale = call(
            &f.env.app,
            Req::new("PATCH", &format!("/api/v1/grants/{grant_id}"))
                .session(&f.owner)
                .if_match("\"grant-v1\"")
                .json(json!({"role":"viewer"})),
        )
        .await;
        assert_eq!(stale.status, StatusCode::PRECONDITION_FAILED);
        let revoked = call(
            &f.env.app,
            Req::new("DELETE", &format!("/api/v1/grants/{grant_id}")).session(&f.owner),
        )
        .await;
        assert_eq!(revoked.status, StatusCode::NO_CONTENT);
        let repeated = call(
            &f.env.app,
            Req::new("DELETE", &format!("/api/v1/grants/{grant_id}")).session(&f.owner),
        )
        .await;
        assert_eq!(repeated.status, StatusCode::NO_CONTENT);
        assert_eq!(
            call(
                &f.env.app,
                Req::new("GET", &format!("/api/v1/files/{}", f.file)).cookie_only(&recipient)
            )
            .await
            .status,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn project_editor_grant_covers_files_but_not_project_management() {
        let f = fixture("photocraft").await;
        let (recipient_id, recipient) = add_member(&f, "ac03-project-editor").await;
        let grant = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/projects/{}/grants", f.project))
                .session(&f.owner)
                .json(json!({"user_id":recipient_id.to_string(),"role":"editor"})),
        )
        .await;
        assert_eq!(grant.status, StatusCode::CREATED, "{}", grant.text);
        assert_eq!(
            call(
                &f.env.app,
                Req::new("GET", &format!("/api/v1/projects/{}", f.project)).cookie_only(&recipient)
            )
            .await
            .status,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &f.env.app,
                Req::new("GET", &format!("/api/v1/files/{}", f.file)).cookie_only(&recipient)
            )
            .await
            .status,
            StatusCode::OK
        );
        let renamed = call(
            &f.env.app,
            Req::new("PATCH", &format!("/api/v1/files/{}", f.file))
                .session(&recipient)
                .if_match("\"file-v1\"")
                .json(json!({"name":"Granted edit"})),
        )
        .await;
        assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.text);
        let new_file = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/projects/{}/files", f.project))
                .session(&recipient)
                .json(json!({"name":"Granted file","app":"photocraft","format":"native"})),
        )
        .await;
        assert_eq!(new_file.status, StatusCode::CREATED, "{}", new_file.text);
        let project = call(
            &f.env.app,
            Req::new("GET", &format!("/api/v1/projects/{}", f.project)).cookie_only(&recipient),
        )
        .await;
        let project_etag = project.headers.get("etag").unwrap().to_str().unwrap();
        let project_rename = call(
            &f.env.app,
            Req::new("PATCH", &format!("/api/v1/projects/{}", f.project))
                .session(&recipient)
                .if_match(project_etag)
                .json(json!({"name":"Forbidden rename"})),
        )
        .await;
        assert_eq!(project_rename.status, StatusCode::FORBIDDEN);
        assert_eq!(
            call(
                &f.env.app,
                Req::new("GET", &format!("/api/v1/projects/{}/grants", f.project))
                    .cookie_only(&recipient)
            )
            .await
            .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(
                &f.env.app,
                Req::new(
                    "GET",
                    &format!("/api/v1/organizations/{}/members?query=ac03", f.org)
                )
                .cookie_only(&recipient)
            )
            .await
            .status,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn lock_is_bound_to_browser_session_and_takeover_is_explicit() {
        let f = fixture("photocraft").await;
        let first = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/lock", f.file))
                .session(&f.owner)
                .json(json!({"client_id":"desktop"})),
        )
        .await;
        assert_eq!(first.status, StatusCode::OK, "{}", first.text);
        assert_eq!(first.body["held_by_current_session"], true);
        let created_audit: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='file.locked'")
                .fetch_one(&f.env.pool)
                .await
                .unwrap();
        assert_eq!(created_audit, 1);
        let second = f.env.login_ok("ac03-owner").await;
        let blocked = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/lock", f.file))
                .session(&second)
                .json(json!({"client_id":"desktop"})),
        )
        .await;
        assert_eq!(blocked.status, StatusCode::LOCKED, "{}", blocked.text);
        assert_eq!(blocked.body["error"]["code"], "FILE_LOCKED");
        assert_eq!(blocked.body["error"]["details"]["mine"], true);
        assert_eq!(
            blocked.body["error"]["details"]["held_by_current_session"],
            false
        );
        let taken = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/lock", f.file))
                .session(&second)
                .json(json!({"client_id":"desktop","takeover":true})),
        )
        .await;
        assert_eq!(taken.status, StatusCode::OK, "{}", taken.text);
        assert_eq!(taken.body["held_by_current_session"], true);
        let takeover_audit: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events WHERE action='file.lock_taken_over'",
        )
        .fetch_one(&f.env.pool)
        .await
        .unwrap();
        assert_eq!(takeover_audit, 1);
        let old = call(
            &f.env.app,
            Req::new("GET", &format!("/api/v1/files/{}", f.file)).cookie_only(&f.owner),
        )
        .await;
        assert_eq!(old.body["lock"]["mine"], true);
        assert_eq!(old.body["lock"]["held_by_current_session"], false);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn concurrent_different_users_acquire_exactly_one_lease() {
        let f = fixture("photocraft").await;
        let (editor_id, editor) = add_member(&f, "ac03-concurrent-editor").await;
        assert_eq!(
            call(
                &f.env.app,
                Req::new(
                    "POST",
                    &format!("/api/v1/workspaces/{}/members", f.workspace)
                )
                .session(&f.owner)
                .json(json!({"user_id":editor_id.to_string(),"role":"editor"}))
            )
            .await
            .status,
            StatusCode::CREATED
        );
        let request = |session: &Session, client_id: &'static str| {
            Req::new("POST", &format!("/api/v1/files/{}/lock", f.file))
                .session(session)
                .json(json!({"client_id":client_id}))
        };
        let (owner_result, editor_result) = tokio::join!(
            call(&f.env.app, request(&f.owner, "owner")),
            call(&f.env.app, request(&editor, "editor"))
        );
        assert_eq!(
            (
                (owner_result.status == StatusCode::OK) as usize
                    + (editor_result.status == StatusCode::OK) as usize,
                (owner_result.status == StatusCode::LOCKED) as usize
                    + (editor_result.status == StatusCode::LOCKED) as usize
            ),
            (1, 1),
            "owner={}; editor={}",
            owner_result.text,
            editor_result.text
        );
        let conflict = if owner_result.status == StatusCode::LOCKED {
            owner_result
        } else {
            editor_result
        };
        assert_eq!(conflict.body["error"]["code"], "FILE_LOCKED");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn active_lease_blocks_file_and_project_writes_but_not_reads() {
        let f = fixture("photocraft").await;
        let initial = call(
            &f.env.app,
            Req::new("GET", &format!("/api/v1/files/{}", f.file)).cookie_only(&f.owner),
        )
        .await;
        let etag = initial
            .headers
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let lease = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/lock", f.file))
                .session(&f.owner)
                .json(json!({"client_id":"desktop"})),
        )
        .await;
        assert_eq!(lease.status, StatusCode::OK, "{}", lease.text);
        let other = f.env.login_ok("ac03-owner").await;

        let rename = call(
            &f.env.app,
            Req::new("PATCH", &format!("/api/v1/files/{}", f.file))
                .session(&other)
                .if_match(&etag)
                .json(json!({"name":"Renamed"})),
        )
        .await;
        assert_eq!(rename.status, StatusCode::LOCKED, "{}", rename.text);
        assert_eq!(rename.body["error"]["code"], "FILE_LOCKED");
        let trash = call(
            &f.env.app,
            Req::new("DELETE", &format!("/api/v1/files/{}", f.file)).session(&other),
        )
        .await;
        assert_eq!(trash.status, StatusCode::LOCKED, "{}", trash.text);
        let create_upload = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/uploads", f.file))
                .session(&other)
                .json(json!({"size_bytes":1,"sha256":"00".repeat(32)})),
        )
        .await;
        assert_eq!(
            create_upload.status,
            StatusCode::LOCKED,
            "{}",
            create_upload.text
        );
        let project_trash = call(
            &f.env.app,
            Req::new("DELETE", &format!("/api/v1/projects/{}", f.project)).session(&other),
        )
        .await;
        assert_eq!(
            project_trash.status,
            StatusCode::LOCKED,
            "{}",
            project_trash.text
        );
        let read = call(
            &f.env.app,
            Req::new("GET", &format!("/api/v1/files/{}", f.file)).cookie_only(&other),
        )
        .await;
        assert_eq!(read.status, StatusCode::OK, "{}", read.text);

        // Same holder may trash its file. The lease remains until expiry/release and guards restore.
        let own_trash = call(
            &f.env.app,
            Req::new("DELETE", &format!("/api/v1/files/{}", f.file)).session(&f.owner),
        )
        .await;
        assert_eq!(
            own_trash.status,
            StatusCode::NO_CONTENT,
            "{}",
            own_trash.text
        );
        let restore = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/restore", f.file)).session(&other),
        )
        .await;
        assert_eq!(restore.status, StatusCode::LOCKED, "{}", restore.text);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn leases_guard_writes_project_restore_and_audit_only_state_changes() {
        let f = fixture("photocraft").await;
        let initial = call(
            &f.env.app,
            Req::new("GET", &format!("/api/v1/files/{}", f.file)).cookie_only(&f.owner),
        )
        .await;
        let etag = initial
            .headers
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let audit_count = |action: &'static str| {
            let pool = f.env.pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audit_events WHERE action=$1")
                    .bind(action)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };

        let acquire = || {
            Req::new("POST", &format!("/api/v1/files/{}/lock", f.file))
                .session(&f.owner)
                .json(json!({"client_id":"first-session"}))
        };
        assert_eq!(call(&f.env.app, acquire()).await.status, StatusCode::OK);
        assert_eq!(audit_count("file.locked").await, 1);
        assert_eq!(
            call(&f.env.app, acquire()).await.status,
            StatusCode::OK,
            "same-session acquire is heartbeat"
        );
        assert_eq!(audit_count("file.locked").await, 1);

        let second = f.env.login_ok("ac03-owner").await;
        let rename = call(
            &f.env.app,
            Req::new("PATCH", &format!("/api/v1/files/{}", f.file))
                .session(&second)
                .if_match(&etag)
                .json(json!({"name":"Renamed"})),
        )
        .await;
        assert_eq!(rename.status, StatusCode::LOCKED, "{}", rename.text);
        let trash = call(
            &f.env.app,
            Req::new("DELETE", &format!("/api/v1/files/{}", f.file)).session(&second),
        )
        .await;
        assert_eq!(trash.status, StatusCode::LOCKED, "{}", trash.text);
        let create_upload = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/uploads", f.file))
                .session(&second)
                .json(json!({"size_bytes":1,"sha256":"00".repeat(32)})),
        )
        .await;
        assert_eq!(
            create_upload.status,
            StatusCode::LOCKED,
            "{}",
            create_upload.text
        );
        assert_eq!(
            call(
                &f.env.app,
                Req::new("GET", &format!("/api/v1/files/{}", f.file)).cookie_only(&second)
            )
            .await
            .status,
            StatusCode::OK
        );

        assert_eq!(
            call(
                &f.env.app,
                Req::new("DELETE", &format!("/api/v1/files/{}", f.file)).session(&f.owner)
            )
            .await
            .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(
                &f.env.app,
                Req::new("POST", &format!("/api/v1/files/{}/restore", f.file)).session(&second)
            )
            .await
            .status,
            StatusCode::LOCKED
        );
        assert_eq!(
            call(
                &f.env.app,
                Req::new(
                    "DELETE",
                    &format!("/api/v1/files/{}/lock?force=true", f.file)
                )
                .session(&second)
            )
            .await
            .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(audit_count("file.lock_forced").await, 1);
        assert_eq!(
            call(
                &f.env.app,
                Req::new("POST", &format!("/api/v1/files/{}/restore", f.file)).session(&second)
            )
            .await
            .status,
            StatusCode::OK
        );

        assert_eq!(call(&f.env.app, acquire()).await.status, StatusCode::OK);
        assert_eq!(
            call(
                &f.env.app,
                Req::new("DELETE", &format!("/api/v1/projects/{}", f.project)).session(&f.owner)
            )
            .await
            .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(
                &f.env.app,
                Req::new("POST", &format!("/api/v1/projects/{}/restore", f.project))
                    .session(&second)
            )
            .await
            .status,
            StatusCode::LOCKED
        );
        assert_eq!(
            call(
                &f.env.app,
                Req::new(
                    "DELETE",
                    &format!("/api/v1/files/{}/lock?force=true", f.file)
                )
                .session(&second)
            )
            .await
            .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(
                &f.env.app,
                Req::new("POST", &format!("/api/v1/projects/{}/restore", f.project))
                    .session(&second)
            )
            .await
            .status,
            StatusCode::OK
        );

        assert_eq!(call(&f.env.app, acquire()).await.status, StatusCode::OK);
        assert_eq!(
            call(
                &f.env.app,
                Req::new("DELETE", &format!("/api/v1/files/{}/lock", f.file)).session(&f.owner)
            )
            .await
            .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(audit_count("file.unlocked").await, 1);

        // Expiry uses database time, without sleeping for the lease duration.
        assert_eq!(call(&f.env.app, acquire()).await.status, StatusCode::OK);
        sqlx::query("UPDATE file_locks SET acquired_at=CURRENT_TIMESTAMP-interval '2 seconds',refreshed_at=CURRENT_TIMESTAMP-interval '2 seconds',expires_at=CURRENT_TIMESTAMP-interval '1 second' WHERE org_id=$1 AND file_id=$2")
            .bind(f.org).bind(f.file).execute(&f.env.pool).await.unwrap();
        let read = call(
            &f.env.app,
            Req::new("GET", &format!("/api/v1/files/{}", f.file)).cookie_only(&second),
        )
        .await;
        assert!(read.body["lock"].is_null());
        assert_eq!(
            call(
                &f.env.app,
                Req::new("POST", &format!("/api/v1/files/{}/lock", f.file))
                    .session(&second)
                    .json(json!({"client_id":"second-session"}))
            )
            .await
            .status,
            StatusCode::OK
        );
        assert_eq!(audit_count("file.lock_taken_over").await, 0);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn figcraft_lock_is_rejected_with_contract_error() {
        let f = fixture("figcraft").await;
        let response = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/lock", f.file))
                .session(&f.owner)
                .json(json!({"client_id":"desktop"})),
        )
        .await;
        assert_eq!(response.status, StatusCode::CONFLICT);
        assert_eq!(response.body["error"]["code"], "LOCK_NOT_SUPPORTED");
    }

    fn device_config() -> craftcloud_api::ApiConfig {
        craftcloud_api::ApiConfig {
            trusted_proxies: vec!["127.0.0.1/32".parse().unwrap()],
            ..craftcloud_api::ApiConfig::default()
        }
    }

    /// Sends the request as a trusted proxy would forward an HTTPS request.
    async fn call_https(env: &TestEnv, req: Req) -> crate::common::Resp {
        use tower::ServiceExt;
        let mut request = req.build();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.1:443".parse::<std::net::SocketAddr>().unwrap(),
        ));
        request
            .headers_mut()
            .insert("x-forwarded-proto", "https".parse().unwrap());
        let response = env.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        crate::common::Resp {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            text: String::from_utf8_lossy(&bytes).into_owned(),
            bytes: bytes.to_vec(),
        }
    }

    /// Logs the user in as a desktop device and returns the access token.
    async fn device_token(env: &TestEnv, email: &str) -> String {
        let response = call_https(
            env,
            Req::new("POST", "/api/v1/auth/device/login").json(json!({
                "email": email,
                "password": crate::common::PASSWORD,
                "client_kind": "app",
                "client_id": "photocraft",
                "device_name": "PhotoCraft test"
            })),
        )
        .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.text);
        response.body["access_token"].as_str().unwrap().to_owned()
    }

    fn bearer(method: &'static str, path: &str, token: &str) -> Req {
        Req::new(method, path).header("Authorization", &format!("Bearer {token}"))
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn device_lease_is_foreign_to_the_browser_session_and_ends_with_device_logout() {
        let f = fixture_in(
            TestEnv::with(crate::common::settings(), device_config()).await,
            "photocraft",
        )
        .await;
        let token = device_token(&f.env, "ac03-owner@example.test").await;
        let lock_path = format!("/api/v1/files/{}/lock", f.file);
        let acquired = call(
            &f.env.app,
            bearer("POST", &lock_path, &token).json(json!({"client_id":"photocraft"})),
        )
        .await;
        assert_eq!(acquired.status, StatusCode::OK, "{}", acquired.text);
        assert_eq!(acquired.body["held_by_current_session"], true);
        let kind: String =
            sqlx::query_scalar("SELECT holder_session_kind FROM file_locks WHERE file_id=$1")
                .bind(f.file)
                .fetch_one(&f.env.pool)
                .await
                .unwrap();
        assert_eq!(kind, "device");

        // The browser session of the same user sees its own lease, but not as this session.
        let seen = call(
            &f.env.app,
            Req::new("GET", &format!("/api/v1/files/{}", f.file)).cookie_only(&f.owner),
        )
        .await;
        assert_eq!(seen.body["lock"]["mine"], true);
        assert_eq!(seen.body["lock"]["held_by_current_session"], false);
        let blocked = call(
            &f.env.app,
            Req::new("POST", &lock_path)
                .session(&f.owner)
                .json(json!({"client_id":"browser"})),
        )
        .await;
        assert_eq!(blocked.status, StatusCode::LOCKED, "{}", blocked.text);
        assert_eq!(
            blocked.body["error"]["details"]["held_by_current_session"],
            false
        );
        let trash_blocked = call(
            &f.env.app,
            Req::new("DELETE", &format!("/api/v1/files/{}", f.file)).session(&f.owner),
        )
        .await;
        assert_eq!(trash_blocked.status, StatusCode::LOCKED);
        // The device itself recognizes the lease as its own.
        let own = call(
            &f.env.app,
            bearer("GET", &format!("/api/v1/files/{}", f.file), &token),
        )
        .await;
        assert_eq!(own.body["lock"]["held_by_current_session"], true);

        // Device logout removes the lease in the same transaction.
        let logout = call_https(&f.env, bearer("POST", "/api/v1/auth/device/logout", &token)).await;
        assert!(logout.status.is_success(), "{}", logout.text);
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM file_locks WHERE file_id=$1")
            .bind(f.file)
            .fetch_one(&f.env.pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);
        let after = call(
            &f.env.app,
            Req::new("POST", &lock_path)
                .session(&f.owner)
                .json(json!({"client_id":"browser"})),
        )
        .await;
        assert_eq!(after.status, StatusCode::OK, "{}", after.text);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn lease_taken_while_the_body_streams_blocks_upload_completion() {
        use sha2::{Digest, Sha256};
        let f = fixture("photocraft").await;
        let data = b"lease versus upload".to_vec();
        let sha: String = Sha256::digest(&data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let created = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/uploads", f.file))
                .session(&f.owner)
                .json(json!({"size_bytes":data.len(),"sha256":sha})),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        let upload = created.body["upload_id"].as_str().unwrap().to_owned();
        // Another session of the same user takes the lease after the upload session exists.
        let other = f.env.login_ok("ac03-owner").await;
        let lease = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/lock", f.file))
                .session(&other)
                .json(json!({"client_id":"other"})),
        )
        .await;
        assert_eq!(lease.status, StatusCode::OK, "{}", lease.text);
        let put = call(
            &f.env.app,
            Req::new("PUT", &format!("/api/v1/uploads/{upload}/content"))
                .session(&f.owner)
                .body(axum::body::Body::from(data.clone())),
        )
        .await;
        assert_eq!(put.status, StatusCode::LOCKED, "{}", put.text);
        assert_eq!(put.body["error"]["code"], "FILE_LOCKED");
        let state: String = sqlx::query_scalar("SELECT state FROM upload_sessions WHERE id=$1")
            .bind(Uuid::parse_str(&upload).unwrap())
            .fetch_one(&f.env.pool)
            .await
            .unwrap();
        assert_eq!(state, "failed");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn member_search_is_rate_limited_per_user() {
        let mut settings = crate::common::settings();
        settings.member_search_limit = 2;
        let f = fixture_in(
            TestEnv::with(settings, craftcloud_api::ApiConfig::default()).await,
            "photocraft",
        )
        .await;
        let path = format!("/api/v1/organizations/{}/members?query=ac", f.org);
        for _ in 0..2 {
            let ok = call(&f.env.app, Req::new("GET", &path).cookie_only(&f.owner)).await;
            assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
        }
        let limited = call(&f.env.app, Req::new("GET", &path).cookie_only(&f.owner)).await;
        assert_eq!(
            limited.status,
            StatusCode::TOO_MANY_REQUESTS,
            "{}",
            limited.text
        );
        assert_eq!(limited.body["error"]["code"], "RATE_LIMITED");
        // A plain listing is not a search and is not counted.
        let list = call(
            &f.env.app,
            Req::new("GET", &format!("/api/v1/organizations/{}/members", f.org))
                .cookie_only(&f.owner),
        )
        .await;
        assert_eq!(list.status, StatusCode::OK, "{}", list.text);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn lock_routes_answer_403_for_forbidden_and_404_for_hidden() {
        let f = fixture("photocraft").await;
        let (_, viewer) = add_workspace_role(&f, "ac03-lock-viewer", "viewer").await;
        f.env.user("ac03-lock-outsider").await;
        let outsider = f.env.login_ok("ac03-lock-outsider").await;
        let path = format!("/api/v1/files/{}/lock", f.file);
        let denied = call(
            &f.env.app,
            Req::new("POST", &path)
                .session(&viewer)
                .json(json!({"client_id":"v"})),
        )
        .await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.text);
        let denied_release = call(&f.env.app, Req::new("DELETE", &path).session(&viewer)).await;
        assert_eq!(
            denied_release.status,
            StatusCode::FORBIDDEN,
            "{}",
            denied_release.text
        );
        let denied_force = call(
            &f.env.app,
            Req::new("DELETE", &format!("{path}?force=true")).session(&viewer),
        )
        .await;
        assert_eq!(
            denied_force.status,
            StatusCode::FORBIDDEN,
            "{}",
            denied_force.text
        );
        let hidden = call(
            &f.env.app,
            Req::new("POST", &path)
                .session(&outsider)
                .json(json!({"client_id":"o"})),
        )
        .await;
        assert_eq!(hidden.status, StatusCode::NOT_FOUND, "{}", hidden.text);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn trashed_targets_reject_grants_and_leases() {
        let f = fixture("photocraft").await;
        let (target, _) = add_member(&f, "ac03-trash-target").await;
        let trash_file = call(
            &f.env.app,
            Req::new("DELETE", &format!("/api/v1/files/{}", f.sibling)).session(&f.owner),
        )
        .await;
        assert_eq!(
            trash_file.status,
            StatusCode::NO_CONTENT,
            "{}",
            trash_file.text
        );
        let grant = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/grants", f.sibling))
                .session(&f.owner)
                .json(json!({"user_id":target.to_string(),"role":"viewer"})),
        )
        .await;
        assert_eq!(grant.status, StatusCode::CONFLICT, "{}", grant.text);
        assert_eq!(grant.body["error"]["code"], "FILE_DELETED");
        let lease = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/lock", f.sibling))
                .session(&f.owner)
                .json(json!({"client_id":"x"})),
        )
        .await;
        assert_eq!(lease.status, StatusCode::CONFLICT, "{}", lease.text);
        assert_eq!(lease.body["error"]["code"], "FILE_DELETED");
        let trash_project = call(
            &f.env.app,
            Req::new("DELETE", &format!("/api/v1/projects/{}", f.project)).session(&f.owner),
        )
        .await;
        assert_eq!(
            trash_project.status,
            StatusCode::NO_CONTENT,
            "{}",
            trash_project.text
        );
        let project_grant = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/projects/{}/grants", f.project))
                .session(&f.owner)
                .json(json!({"user_id":target.to_string(),"role":"viewer"})),
        )
        .await;
        assert_eq!(
            project_grant.status,
            StatusCode::CONFLICT,
            "{}",
            project_grant.text
        );
        assert_eq!(project_grant.body["error"]["code"], "PROJECT_DELETED");
        let lease = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/lock", f.file))
                .session(&f.owner)
                .json(json!({"client_id":"x"})),
        )
        .await;
        assert_eq!(lease.status, StatusCode::CONFLICT, "{}", lease.text);
        assert_eq!(lease.body["error"]["code"], "PROJECT_DELETED");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn concurrent_grant_revocation_audits_exactly_once() {
        let f = fixture("photocraft").await;
        let (target, _) = add_member(&f, "ac03-revoke-target").await;
        let created = call(
            &f.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/grants", f.file))
                .session(&f.owner)
                .json(json!({"user_id":target.to_string(),"role":"viewer"})),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        let path = format!("/api/v1/grants/{}", created.body["id"].as_str().unwrap());
        let (a, b) = tokio::join!(
            call(&f.env.app, Req::new("DELETE", &path).session(&f.owner)),
            call(&f.env.app, Req::new("DELETE", &path).session(&f.owner)),
        );
        assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.text);
        assert_eq!(b.status, StatusCode::NO_CONTENT, "{}", b.text);
        let audits: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='grant.revoked'")
                .fetch_one(&f.env.pool)
                .await
                .unwrap();
        assert_eq!(audits, 1);
    }
}
