//! SCIM group mappings and deactivation HTTP tests (AC-10).

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use axum::http::{StatusCode, header};
    use serde_json::json;
    use uuid::Uuid;

    use crate::common::{PASSWORD, Req, Session, TestEnv, call};

    const USER_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
    const GROUP_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";

    struct World {
        env: TestEnv,
        operator: Session,
        owner: Session,
        org: Uuid,
        owner_id: Uuid,
    }

    impl World {
        async fn new() -> Self {
            let env = TestEnv::new().await;
            let operator = env.login_ok("op").await;
            let owner_id = env.user("ac10-groups-owner").await;
            let owner = env.login_ok("ac10-groups-owner").await;
            let created = call(
                &env.app,
                Req::new("POST", "/api/v1/organizations")
                    .session(&operator)
                    .json(json!({"name":"AC10 groups","slug":format!("ac10-{}", Uuid::new_v4().simple()),"owner_user_id":owner_id})),
            )
            .await;
            assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
            let org = Uuid::parse_str(created.body["id"].as_str().unwrap()).unwrap();
            crate::common::approve_example_domain(&env.pool, org).await;
            Self {
                env,
                operator,
                owner,
                org,
                owner_id,
            }
        }

        async fn scim_token(&self) -> String {
            let response = call(
                &self.env.app,
                Req::new(
                    "POST",
                    &format!("/api/v1/organizations/{}/scim/token", self.org),
                )
                .session(&self.owner),
            )
            .await;
            assert_eq!(response.status, StatusCode::OK, "{}", response.text);
            response.body["token"].as_str().unwrap().to_owned()
        }

        fn scim_url(&self, resource: &str) -> String {
            format!("/scim/v2/organizations/{}/{}", self.org, resource)
        }

        async fn scim(
            &self,
            method: &'static str,
            path: &str,
            token: &str,
            body: serde_json::Value,
        ) -> crate::common::Resp {
            call(
                &self.env.app,
                Req::new(method, path)
                    .header("authorization", &format!("Bearer {token}"))
                    .json(body),
            )
            .await
        }

        async fn create_workspace(&self, slug: &str) -> Uuid {
            let response = call(
                &self.env.app,
                Req::new(
                    "POST",
                    &format!("/api/v1/organizations/{}/workspaces", self.org),
                )
                .session(&self.owner)
                .json(json!({"name":"AC10 workspace","slug":slug})),
            )
            .await;
            assert_eq!(response.status, StatusCode::CREATED, "{}", response.text);
            Uuid::parse_str(response.body["id"].as_str().unwrap()).unwrap()
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL; run cargo xtask db-test"]
    async fn group_mapping_reconciles_only_scim_owned_roles_and_rejects_foreign_targets() {
        let w = World::new().await;
        let token = w.scim_token().await;
        let workspace = w.create_workspace("ac10-group-map").await;
        let foreign_org = call(
            &w.env.app,
            Req::new("POST", "/api/v1/organizations").session(&w.operator)
                .json(json!({"name":"AC10 foreign","slug":format!("ac10-{}",Uuid::new_v4().simple()),"owner_user_id":w.owner_id})),
        ).await;
        assert_eq!(
            foreign_org.status,
            StatusCode::CREATED,
            "{}",
            foreign_org.text
        );
        let foreign_org_id = Uuid::parse_str(foreign_org.body["id"].as_str().unwrap()).unwrap();
        let foreign_workspace_response = call(
            &w.env.app,
            Req::new(
                "POST",
                &format!("/api/v1/organizations/{foreign_org_id}/workspaces"),
            )
            .session(&w.owner)
            .json(json!({"name":"Foreign workspace","slug":"ac10-group-foreign"})),
        )
        .await;
        assert_eq!(
            foreign_workspace_response.status,
            StatusCode::CREATED,
            "{}",
            foreign_workspace_response.text
        );
        let foreign_workspace =
            Uuid::parse_str(foreign_workspace_response.body["id"].as_str().unwrap()).unwrap();

        let created_user = w
            .scim(
                "POST",
                &w.scim_url("Users"),
                &token,
                json!({"schemas":[USER_SCHEMA],"userName":"ac10-map@example.test","displayName":"Mapped"}),
            )
            .await;
        assert_eq!(
            created_user.status,
            StatusCode::CREATED,
            "{}",
            created_user.text
        );
        let scim_user = Uuid::parse_str(created_user.body["id"].as_str().unwrap()).unwrap();
        let user_id: Uuid =
            sqlx::query_scalar("SELECT user_id FROM scim_users WHERE org_id=$1 AND id=$2")
                .bind(w.org)
                .bind(scim_user)
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
        let viewer_response = w.scim("POST", &w.scim_url("Users"), &token,
            json!({"schemas":[USER_SCHEMA],"userName":"ac10-viewer@example.test","displayName":"Viewer"})).await;
        assert_eq!(
            viewer_response.status,
            StatusCode::CREATED,
            "{}",
            viewer_response.text
        );
        let viewer_scim_id = Uuid::parse_str(viewer_response.body["id"].as_str().unwrap()).unwrap();
        let viewer_user_id: Uuid =
            sqlx::query_scalar("SELECT user_id FROM scim_users WHERE org_id=$1 AND id=$2")
                .bind(w.org)
                .bind(viewer_scim_id)
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
        let admin_response = w.scim("POST", &w.scim_url("Users"), &token,
            json!({"schemas":[USER_SCHEMA],"userName":"ac10-manual-admin@example.test","displayName":"Manual Admin"})).await;
        assert_eq!(
            admin_response.status,
            StatusCode::CREATED,
            "{}",
            admin_response.text
        );
        let admin_scim_id = Uuid::parse_str(admin_response.body["id"].as_str().unwrap()).unwrap();
        let admin_user_id: Uuid =
            sqlx::query_scalar("SELECT user_id FROM scim_users WHERE org_id=$1 AND id=$2")
                .bind(w.org)
                .bind(admin_scim_id)
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
        let manual_viewer_response = w.scim("POST", &w.scim_url("Users"), &token,
            json!({"schemas":[USER_SCHEMA],"userName":"ac10-manual-viewer@example.test","displayName":"Manual Viewer"})).await;
        assert_eq!(
            manual_viewer_response.status,
            StatusCode::CREATED,
            "{}",
            manual_viewer_response.text
        );
        let manual_viewer_scim_id =
            Uuid::parse_str(manual_viewer_response.body["id"].as_str().unwrap()).unwrap();
        let manual_viewer_user_id: Uuid =
            sqlx::query_scalar("SELECT user_id FROM scim_users WHERE org_id=$1 AND id=$2")
                .bind(w.org)
                .bind(manual_viewer_scim_id)
                .fetch_one(&w.env.pool)
                .await
                .unwrap();

        let created_group = w
            .scim(
                "POST",
                &w.scim_url("Groups"),
                &token,
                json!({"schemas":[GROUP_SCHEMA],"displayName":"Editors","members":[{"value":scim_user}]}),
            )
            .await;
        assert_eq!(
            created_group.status,
            StatusCode::CREATED,
            "{}",
            created_group.text
        );
        assert_eq!(
            created_group.headers[header::CONTENT_TYPE],
            "application/scim+json"
        );
        assert_eq!(
            created_group.body["members"][0]["value"],
            scim_user.to_string()
        );
        let group_id = Uuid::parse_str(created_group.body["id"].as_str().unwrap()).unwrap();
        let viewer_group_response = w.scim("POST", &w.scim_url("Groups"), &token,
            json!({"schemas":[GROUP_SCHEMA],"displayName":"Viewers","members":[{"value":viewer_scim_id}]})).await;
        assert_eq!(
            viewer_group_response.status,
            StatusCode::CREATED,
            "{}",
            viewer_group_response.text
        );
        let viewer_group_id =
            Uuid::parse_str(viewer_group_response.body["id"].as_str().unwrap()).unwrap();
        let admin_group_response = w.scim("POST", &w.scim_url("Groups"), &token,
            json!({"schemas":[GROUP_SCHEMA],"displayName":"Manual admins","members":[{"value":admin_scim_id}]})).await;
        assert_eq!(
            admin_group_response.status,
            StatusCode::CREATED,
            "{}",
            admin_group_response.text
        );
        let admin_group_id =
            Uuid::parse_str(admin_group_response.body["id"].as_str().unwrap()).unwrap();
        let manual_viewer_group_response = w.scim("POST", &w.scim_url("Groups"), &token,
            json!({"schemas":[GROUP_SCHEMA],"displayName":"Manual viewers","members":[{"value":manual_viewer_scim_id}]})).await;
        assert_eq!(
            manual_viewer_group_response.status,
            StatusCode::CREATED,
            "{}",
            manual_viewer_group_response.text
        );
        let manual_viewer_group_id =
            Uuid::parse_str(manual_viewer_group_response.body["id"].as_str().unwrap()).unwrap();
        let foreign_member = w.scim("POST", &w.scim_url("Groups"), &token,
            json!({"schemas":[GROUP_SCHEMA],"displayName":"Invalid foreign member","members":[{"value":Uuid::new_v4()}]})).await;
        assert_eq!(foreign_member.status, StatusCode::BAD_REQUEST);

        let group_path = w.scim_url(&format!("Groups/{group_id}"));
        let read_group = w
            .scim("GET", &group_path, &token, serde_json::Value::Null)
            .await;
        assert_eq!(read_group.status, StatusCode::OK, "{}", read_group.text);
        assert_eq!(
            read_group.body["members"][0]["value"],
            scim_user.to_string()
        );
        let unknown_patch = w.scim("PATCH", &group_path, &token,
            json!({"schemas":["urn:ietf:params:scim:api:messages:2.0:PatchOp"],"Operations":[{"op":"replace","path":"unknown","value":"bad"}]})).await;
        assert_eq!(unknown_patch.status, StatusCode::BAD_REQUEST);
        let unchanged_group = w
            .scim("GET", &group_path, &token, serde_json::Value::Null)
            .await;
        assert_eq!(unchanged_group.body["displayName"], "Editors");
        assert_eq!(
            unchanged_group.body["members"][0]["value"],
            scim_user.to_string()
        );
        let replaced_group = w.scim("PUT", &w.scim_url(&format!("Groups/{viewer_group_id}")), &token,
            json!({"schemas":[GROUP_SCHEMA],"id":viewer_group_id,"displayName":"Viewers updated","members":[{"value":viewer_scim_id}]})).await;
        assert_eq!(
            replaced_group.status,
            StatusCode::OK,
            "{}",
            replaced_group.text
        );
        assert_eq!(replaced_group.body["displayName"], "Viewers updated");
        // An existing manual admin row is neither overwritten nor claimed by SCIM.
        sqlx::query("INSERT INTO workspace_members(org_id,workspace_id,user_id,role) VALUES($1,$2,$3,'admin')")
            .bind(w.org).bind(workspace).bind(admin_user_id).execute(&w.env.pool).await.unwrap();
        sqlx::query("INSERT INTO workspace_members(org_id,workspace_id,user_id,role) VALUES($1,$2,$3,'viewer')")
            .bind(w.org).bind(workspace).bind(manual_viewer_user_id).execute(&w.env.pool).await.unwrap();
        let mapping_url = format!("/api/v1/organizations/{}/scim/mappings", w.org);
        let mapped = call(
            &w.env.app,
            Req::new("PUT", &mapping_url).session(&w.owner).json(
                json!({"items":[{"group_id":group_id,"workspace_id":workspace,"role":"editor"},{"group_id":viewer_group_id,"workspace_id":workspace,"role":"viewer"},{"group_id":admin_group_id,"workspace_id":workspace,"role":"editor"},{"group_id":manual_viewer_group_id,"workspace_id":workspace,"role":"editor"}]}),
            ),
        )
        .await;
        assert_eq!(mapped.status, StatusCode::OK, "{}", mapped.text);
        let role: String = sqlx::query_scalar(
            "SELECT role FROM workspace_members WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3",
        )
        .bind(w.org)
        .bind(workspace)
        .bind(user_id)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(role, "editor");
        let owned_role: String = sqlx::query_scalar("SELECT applied_role FROM scim_workspace_ownership WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3")
            .bind(w.org).bind(workspace).bind(user_id).fetch_one(&w.env.pool).await.unwrap();
        assert_eq!(owned_role, "editor");
        let viewer_role: String = sqlx::query_scalar(
            "SELECT role FROM workspace_members WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3",
        )
        .bind(w.org)
        .bind(workspace)
        .bind(viewer_user_id)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(viewer_role, "viewer");
        let admin_role: String = sqlx::query_scalar(
            "SELECT role FROM workspace_members WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3",
        )
        .bind(w.org)
        .bind(workspace)
        .bind(admin_user_id)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(admin_role, "admin");
        let admin_owned: i64 = sqlx::query_scalar("SELECT count(*) FROM scim_workspace_ownership WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3")
            .bind(w.org).bind(workspace).bind(admin_user_id).fetch_one(&w.env.pool).await.unwrap();
        assert_eq!(admin_owned, 0);
        let preserved_viewer: String = sqlx::query_scalar(
            "SELECT role FROM workspace_members WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3",
        )
        .bind(w.org)
        .bind(workspace)
        .bind(manual_viewer_user_id)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(preserved_viewer, "viewer");
        let deleted_admin_group = w
            .scim(
                "DELETE",
                &w.scim_url(&format!("Groups/{admin_group_id}")),
                &token,
                serde_json::Value::Null,
            )
            .await;
        assert_eq!(
            deleted_admin_group.status,
            StatusCode::NO_CONTENT,
            "{}",
            deleted_admin_group.text
        );
        let preserved_admin: String = sqlx::query_scalar(
            "SELECT role FROM workspace_members WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3",
        )
        .bind(w.org)
        .bind(workspace)
        .bind(admin_user_id)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(preserved_admin, "admin");
        let deleted_manual_viewer_group = w
            .scim(
                "DELETE",
                &w.scim_url(&format!("Groups/{manual_viewer_group_id}")),
                &token,
                serde_json::Value::Null,
            )
            .await;
        assert_eq!(deleted_manual_viewer_group.status, StatusCode::NO_CONTENT);
        let preserved_viewer_after_removal: String = sqlx::query_scalar(
            "SELECT role FROM workspace_members WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3",
        )
        .bind(w.org)
        .bind(workspace)
        .bind(manual_viewer_user_id)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(preserved_viewer_after_removal, "viewer");
        let max_role = call(
            &w.env.app,
            Req::new("PUT", &mapping_url).session(&w.owner).json(
                json!({"items":[{"group_id":group_id,"workspace_id":workspace,"role":"admin"}]}),
            ),
        )
        .await;
        assert_eq!(max_role.status, StatusCode::UNPROCESSABLE_ENTITY);

        // The workspace API claims ownership even when the role value stays the same.
        let manual = call(
            &w.env.app,
            Req::new(
                "PATCH",
                &format!("/api/v1/workspaces/{workspace}/members/{user_id}"),
            )
            .session(&w.owner)
            .json(json!({"role":"editor"})),
        )
        .await;
        assert_eq!(manual.status, StatusCode::OK, "{}", manual.text);
        assert_eq!(w.env.count(&format!("SELECT count(*) FROM scim_workspace_ownership WHERE org_id='{}' AND workspace_id='{}' AND user_id='{}'", w.org, workspace, user_id)).await, 0);

        let removed = w.scim("PATCH", &group_path, &token, json!({"schemas":["urn:ietf:params:scim:api:messages:2.0:PatchOp"],"Operations":[{"op":"replace","path":"members","value":[]}]})).await;
        assert_eq!(removed.status, StatusCode::OK, "{}", removed.text);
        let retained: String = sqlx::query_scalar(
            "SELECT role FROM workspace_members WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3",
        )
        .bind(w.org)
        .bind(workspace)
        .bind(user_id)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(retained, "editor");
        let deleted_viewer = w
            .scim(
                "DELETE",
                &w.scim_url(&format!("Groups/{viewer_group_id}")),
                &token,
                serde_json::Value::Null,
            )
            .await;
        assert_eq!(
            deleted_viewer.status,
            StatusCode::NO_CONTENT,
            "{}",
            deleted_viewer.text
        );
        let removed_owned: i64 = sqlx::query_scalar("SELECT count(*) FROM workspace_members WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3")
            .bind(w.org).bind(workspace).bind(viewer_user_id).fetch_one(&w.env.pool).await.unwrap();
        let removed_ownership: i64 = sqlx::query_scalar("SELECT count(*) FROM scim_workspace_ownership WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3")
            .bind(w.org).bind(workspace).bind(viewer_user_id).fetch_one(&w.env.pool).await.unwrap();
        assert_eq!((removed_owned, removed_ownership), (0, 0));

        // Invalid foreign workspace mapping leaves the prior mapping intact.
        let rejected = call(
            &w.env.app,
            Req::new("PUT", &mapping_url).session(&w.owner).json(json!({"items":[{"group_id":group_id,"workspace_id":foreign_workspace,"role":"viewer"}]})),
        ).await;
        assert_eq!(
            rejected.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            rejected.text
        );
        let mappings = call(&w.env.app, Req::new("GET", &mapping_url).session(&w.owner)).await;
        assert_eq!(mappings.status, StatusCode::OK, "{}", mappings.text);
        assert!(
            mappings.body["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| { item["workspace_id"] == workspace.to_string() })
        );
        crate::common::approve_domain(&w.env.pool, foreign_org_id, "foreign.test").await;
        let foreign_token_response = call(
            &w.env.app,
            Req::new(
                "POST",
                &format!("/api/v1/organizations/{foreign_org_id}/scim/token"),
            )
            .session(&w.owner),
        )
        .await;
        assert_eq!(
            foreign_token_response.status,
            StatusCode::OK,
            "{}",
            foreign_token_response.text
        );
        let foreign_token = foreign_token_response.body["token"].as_str().unwrap();
        let foreign_group = call(
            &w.env.app,
            Req::new(
                "POST",
                &format!("/scim/v2/organizations/{foreign_org_id}/Groups"),
            )
            .header("authorization", &format!("Bearer {foreign_token}"))
            .json(json!({"schemas":[GROUP_SCHEMA],"displayName":"Foreign group","members":[]})),
        )
        .await;
        assert_eq!(
            foreign_group.status,
            StatusCode::CREATED,
            "{}",
            foreign_group.text
        );
        let foreign_group_id = Uuid::parse_str(foreign_group.body["id"].as_str().unwrap()).unwrap();
        let foreign_user = call(
            &w.env.app,
            Req::new("POST", &format!("/scim/v2/organizations/{foreign_org_id}/Users"))
                .header("authorization", &format!("Bearer {foreign_token}"))
                .json(json!({"schemas":[USER_SCHEMA],"userName":"ac10-foreign-member@foreign.test","displayName":"Foreign member"})),
        ).await;
        assert_eq!(
            foreign_user.status,
            StatusCode::CREATED,
            "{}",
            foreign_user.text
        );
        let foreign_scim_user_id =
            Uuid::parse_str(foreign_user.body["id"].as_str().unwrap()).unwrap();
        let foreign_member = w.scim("POST", &w.scim_url("Groups"), &token,
            json!({"schemas":[GROUP_SCHEMA],"displayName":"Cross tenant member","members":[{"value":foreign_scim_user_id}]})).await;
        assert_eq!(foreign_member.status, StatusCode::BAD_REQUEST);
        let foreign_group_mapping = call(&w.env.app, Req::new("PUT", &mapping_url).session(&w.owner)
            .json(json!({"items":[{"group_id":foreign_group_id,"workspace_id":workspace,"role":"editor"}]}))).await;
        assert_eq!(
            foreign_group_mapping.status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let mappings_after_foreign =
            call(&w.env.app, Req::new("GET", &mapping_url).session(&w.owner)).await;
        assert_eq!(mappings_after_foreign.status, StatusCode::OK);
        assert_eq!(
            mappings_after_foreign.body["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let events = call(
            &w.env.app,
            Req::new(
                "GET",
                &format!("/api/v1/organizations/{}/scim/events", w.org),
            )
            .session(&w.owner),
        )
        .await;
        assert_eq!(events.status, StatusCode::OK, "{}", events.text);
        assert!(!events.text.contains(&token));
        assert!(!events.text.contains("ac10-map@example.test"));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL; run cargo xtask db-test"]
    async fn scim_deactivation_revokes_tenant_sessions_and_leases_without_global_disable() {
        let env = TestEnv::with(
            crate::common::settings(),
            craftcloud_api::ApiConfig {
                trusted_proxies: vec!["127.0.0.1/32".parse().unwrap()],
                ..craftcloud_api::ApiConfig::default()
            },
        )
        .await;
        let operator = env.login_ok("op").await;
        let owner_id = env.user("ac10-groups-owner").await;
        let owner = env.login_ok("ac10-groups-owner").await;
        let created_org = call(&env.app, Req::new("POST", "/api/v1/organizations").session(&operator)
            .json(json!({"name":"AC10 groups","slug":format!("ac10-{}",Uuid::new_v4().simple()),"owner_user_id":owner_id}))).await;
        assert_eq!(
            created_org.status,
            StatusCode::CREATED,
            "{}",
            created_org.text
        );
        let w = World {
            env,
            operator,
            owner,
            org: Uuid::parse_str(created_org.body["id"].as_str().unwrap()).unwrap(),
            owner_id,
        };
        let token = w.scim_token().await;
        // Use a password-bearing local account, then link it to SCIM. SCIM-created users need no local password.
        let user_id = w.env.user("ac10-disable").await;
        sqlx::query("UPDATE users SET normalized_email=$2,password_hash=$3 WHERE id=$1")
            .bind(user_id)
            .bind("ac10-disable@example.test")
            .bind(craftcloud_auth::hash_password(PASSWORD).unwrap())
            .execute(&w.env.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'member','active')")
            .bind(w.org).bind(user_id).execute(&w.env.pool).await.unwrap();
        let scim_user = Uuid::new_v4();
        sqlx::query("INSERT INTO scim_users(org_id,id,user_id,user_name,display_name,active) VALUES($1,$2,$3,'ac10-disable@example.test','Disable',true)")
            .bind(w.org).bind(scim_user).bind(user_id).execute(&w.env.pool).await.unwrap();
        let user_session = w.env.login_ok("ac10-disable").await;

        let other_owner = w.env.user("ac10-disable-other-owner").await;
        let second_org_response = call(&w.env.app, Req::new("POST", "/api/v1/organizations").session(&w.operator)
            .json(json!({"name":"AC10 other","slug":format!("ac10-{}",Uuid::new_v4().simple()),"owner_user_id":other_owner}))).await;
        assert_eq!(
            second_org_response.status,
            StatusCode::CREATED,
            "{}",
            second_org_response.text
        );
        let second_org = Uuid::parse_str(second_org_response.body["id"].as_str().unwrap()).unwrap();
        sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'member','active')")
            .bind(second_org).bind(user_id).execute(&w.env.pool).await.unwrap();

        let workspace = w.create_workspace("ac10-deactivate").await;
        sqlx::query("INSERT INTO workspace_members(org_id,workspace_id,user_id,role) VALUES($1,$2,$3,'editor')")
            .bind(w.org).bind(workspace).bind(user_id).execute(&w.env.pool).await.unwrap();
        let project = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/workspaces/{workspace}/projects"))
                .session(&w.owner)
                .json(json!({"name":"AC10 project"})),
        )
        .await;
        assert_eq!(project.status, StatusCode::CREATED, "{}", project.text);
        let project_id = Uuid::parse_str(project.body["id"].as_str().unwrap()).unwrap();
        let file = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/projects/{project_id}/files"))
                .session(&w.owner)
                .json(json!({"name":"AC10 file","app":"photocraft","format":"native"})),
        )
        .await;
        assert_eq!(file.status, StatusCode::CREATED, "{}", file.text);
        let file_id = Uuid::parse_str(file.body["id"].as_str().unwrap()).unwrap();

        let device_login = call_https(
            &w.env,
            Req::new("POST", "/api/v1/auth/device/login").json(json!({
                "email":"ac10-disable@example.test", "password":PASSWORD,
                "client_kind":"app", "client_id":"photocraft", "device_name":"AC10 device"
            })),
        )
        .await;
        assert_eq!(device_login.status, StatusCode::OK, "{}", device_login.text);
        let device_token = device_login.body["access_token"].as_str().unwrap();
        let lock = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/files/{file_id}/lock"))
                .header("authorization", &format!("Bearer {device_token}"))
                .json(json!({"client_id":"photocraft"})),
        )
        .await;
        assert_eq!(lock.status, StatusCode::OK, "{}", lock.text);
        assert_eq!(
            w.env
                .count(&format!(
                    "SELECT count(*) FROM file_locks WHERE file_id='{}'",
                    file_id
                ))
                .await,
            1
        );
        let device_hash = craftcloud_auth::DeviceToken::parse(
            device_token,
            craftcloud_auth::DeviceTokenKind::Access,
        )
        .unwrap()
        .hash();
        let device_hash_matches: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM device_sessions WHERE user_id=$1 AND access_hash=$2 AND revoked_at IS NULL)")
            .bind(user_id).bind(device_hash.as_slice()).fetch_one(&w.env.pool).await.unwrap();
        assert!(
            device_hash_matches,
            "device session must store the server token hash"
        );

        let deactivated = call(
            &w.env.app,
            Req::new("PUT", &w.scim_url(&format!("Users/{scim_user}")))
                .header("authorization", &format!("Bearer {token}"))
                .json(json!({"schemas":[USER_SCHEMA],"id":scim_user,"userName":"ac10-disable@example.test","displayName":"Disable","active":false})),
        )
        .await;
        assert_eq!(deactivated.status, StatusCode::OK, "{}", deactivated.text);
        let lease_count = w
            .env
            .count(&format!(
                "SELECT count(*) FROM file_locks WHERE file_id='{}'",
                file_id
            ))
            .await;
        assert_eq!(lease_count, 0);
        let status: String = sqlx::query_scalar(
            "SELECT status FROM organization_members WHERE org_id=$1 AND user_id=$2",
        )
        .bind(w.org)
        .bind(user_id)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(status, "disabled");
        let cutoff: Option<time::OffsetDateTime> = sqlx::query_scalar("SELECT revoked_before FROM organization_session_cutoffs WHERE org_id=$1 AND user_id=$2")
            .bind(w.org).bind(user_id).fetch_optional(&w.env.pool).await.unwrap();
        assert!(cutoff.is_some());
        let disabled_at: Option<time::OffsetDateTime> =
            sqlx::query_scalar("SELECT disabled_at FROM users WHERE id=$1")
                .bind(user_id)
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
        assert!(
            disabled_at.is_none(),
            "SCIM deactivation must remain tenant-local"
        );
        let stale_browser = call(
            &w.env.app,
            Req::new("GET", &format!("/api/v1/workspaces/{workspace}")).cookie_only(&user_session),
        )
        .await;
        assert_ne!(stale_browser.status, StatusCode::OK);
        let stale_device = call(
            &w.env.app,
            Req::new("GET", &format!("/api/v1/files/{file_id}"))
                .header("authorization", &format!("Bearer {device_token}")),
        )
        .await;
        assert_ne!(stale_device.status, StatusCode::OK);
        let other_org = call(
            &w.env.app,
            Req::new(
                "GET",
                &format!("/api/v1/organizations/{second_org}/members"),
            )
            .cookie_only(&user_session),
        )
        .await;
        assert_eq!(other_org.status, StatusCode::OK, "{}", other_org.text);

        let active_row = call(&w.env.app, Req::new("PUT", &w.scim_url(&format!("Users/{scim_user}")))
            .header("authorization", &format!("Bearer {token}"))
            .json(json!({"schemas":[USER_SCHEMA],"id":scim_user,"userName":"ac10-disable@example.test","displayName":"Reactivated","active":true}))).await;
        assert_eq!(active_row.status, StatusCode::OK, "{}", active_row.text);
        let stale_after_reactivation = call(
            &w.env.app,
            Req::new("GET", &format!("/api/v1/workspaces/{workspace}")).cookie_only(&user_session),
        )
        .await;
        assert_ne!(stale_after_reactivation.status, StatusCode::OK);
        let stale_device_after_reactivation = call(
            &w.env.app,
            Req::new("GET", &format!("/api/v1/files/{file_id}"))
                .header("authorization", &format!("Bearer {device_token}")),
        )
        .await;
        assert_ne!(stale_device_after_reactivation.status, StatusCode::OK);
        let unchanged_cutoff: Option<time::OffsetDateTime> = sqlx::query_scalar("SELECT revoked_before FROM organization_session_cutoffs WHERE org_id=$1 AND user_id=$2")
            .bind(w.org).bind(user_id).fetch_optional(&w.env.pool).await.unwrap();
        assert_eq!(unchanged_cutoff, cutoff);
        // The test clock is intentionally frozen; a new login must be later
        // than the durable cutoff, exactly as a real subsequent request is.
        w.env.clock.advance(std::time::Duration::from_secs(1));
        let fresh_browser = w.env.login_ok("ac10-disable").await;
        let fresh = call(
            &w.env.app,
            Req::new("GET", &format!("/api/v1/workspaces/{workspace}")).cookie_only(&fresh_browser),
        )
        .await;
        assert_eq!(fresh.status, StatusCode::OK, "{}", fresh.text);
        let device_again = call_https(
            &w.env,
            Req::new("POST", "/api/v1/auth/device/login").json(json!({
                "email":"ac10-disable@example.test", "password":PASSWORD,
                "client_kind":"app", "client_id":"photocraft", "device_name":"AC10 fresh device"
            })),
        )
        .await;
        assert_eq!(device_again.status, StatusCode::OK, "{}", device_again.text);
        let fresh_device = device_again.body["access_token"].as_str().unwrap();
        let fresh_access = call(
            &w.env.app,
            Req::new("GET", &format!("/api/v1/files/{file_id}"))
                .header("authorization", &format!("Bearer {fresh_device}")),
        )
        .await;
        assert_eq!(fresh_access.status, StatusCode::OK, "{}", fresh_access.text);
    }

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
            body: serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
            text: String::from_utf8_lossy(&bytes).into_owned(),
            bytes: bytes.to_vec(),
        }
    }
}
