//! AC-02 HTTP coverage for organization setup gating and hidden-resource parity.

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use crate::common::{Req, TestEnv, call};
    use axum::http::StatusCode;
    use craftcloud_api::Clock;
    use serde_json::json;
    use uuid::Uuid;

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn required_org_gate_preserves_workspace_and_grant_visibility() {
        let env = TestEnv::new().await;
        let operator = env.login_ok("op").await;
        let owner_id = env.user("mfa-owner").await;
        let owner = env.login_ok("mfa-owner").await;
        let member_id = env.user("mfa-member").await;
        let member = env.login_ok("mfa-member").await;
        let workspace_member_id = env.user("mfa-workspace-member").await;
        let workspace_member = env.login_ok("mfa-workspace-member").await;
        let guest_id = env.user("mfa-guest").await;
        let guest = env.login_ok("mfa-guest").await;
        let created = call(&env.app, Req::new("POST", "/api/v1/organizations").session(&operator)
            .json(json!({"name":"MFA Org","slug":format!("mfa-{}",Uuid::new_v4().simple()),"owner_user_id":owner_id.to_string()}))).await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        let org = Uuid::parse_str(created.body["id"].as_str().unwrap()).unwrap();
        let workspace = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/workspaces"))
                .session(&owner)
                .json(json!({"name":"Shared","slug":"shared"})),
        )
        .await;
        assert_eq!(workspace.status, StatusCode::CREATED, "{}", workspace.text);
        let workspace_id = Uuid::parse_str(workspace.body["id"].as_str().unwrap()).unwrap();
        for (user_id, role) in [(member_id, "member"), (workspace_member_id, "member")] {
            let added = call(
                &env.app,
                Req::new("POST", &format!("/api/v1/organizations/{org}/members"))
                    .session(&owner)
                    .json(json!({"user_id":user_id.to_string(),"role":role})),
            )
            .await;
            assert_eq!(added.status, StatusCode::CREATED, "{}", added.text);
        }
        // Guests enter through invitations; this fixture targets grant visibility.
        sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'guest','active')")
            .bind(org).bind(guest_id).execute(&env.pool).await.unwrap();
        let added = call(
            &env.app,
            Req::new(
                "POST",
                &format!("/api/v1/workspaces/{workspace_id}/members"),
            )
            .session(&owner)
            .json(json!({"user_id":workspace_member_id.to_string(),"role":"viewer"})),
        )
        .await;
        assert_eq!(added.status, StatusCode::CREATED, "{}", added.text);
        let project = call(
            &env.app,
            Req::new(
                "POST",
                &format!("/api/v1/workspaces/{workspace_id}/projects"),
            )
            .session(&owner)
            .json(json!({"name":"Files"})),
        )
        .await;
        assert_eq!(project.status, StatusCode::CREATED, "{}", project.text);
        let project_id = Uuid::parse_str(project.body["id"].as_str().unwrap()).unwrap();
        let mut file_ids = Vec::new();
        for name in ["Granted", "Hidden"] {
            let file = call(
                &env.app,
                Req::new("POST", &format!("/api/v1/projects/{project_id}/files"))
                    .session(&owner)
                    .json(json!({"name":name,"app":"photocraft","format":"native"})),
            )
            .await;
            assert_eq!(file.status, StatusCode::CREATED, "{}", file.text);
            file_ids.push(Uuid::parse_str(file.body["id"].as_str().unwrap()).unwrap());
        }
        let granted = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/files/{}/grants", file_ids[0]))
                .session(&owner)
                .json(json!({"user_id":guest_id.to_string(),"role":"viewer"})),
        )
        .await;
        assert_eq!(granted.status, StatusCode::CREATED, "{}", granted.text);

        let now = env.clock.now();
        sqlx::query("INSERT INTO user_two_factor(user_id,secret_ciphertext,enabled_at,last_accepted_step,created_at) VALUES($1,$2,$3,NULL,$3)")
            .bind(owner_id).bind(vec![42_u8; 32]).bind(now).execute(&env.pool).await.unwrap();
        sqlx::query("UPDATE organizations SET require_two_factor=true WHERE id=$1")
            .bind(org)
            .execute(&env.pool)
            .await
            .unwrap();

        let hidden_file = format!("/api/v1/files/{}", file_ids[1]);
        let unknown_file = format!("/api/v1/files/{}", Uuid::new_v4());
        let member_hidden = call(&env.app, Req::new("GET", &hidden_file).session(&member)).await;
        let member_unknown = call(&env.app, Req::new("GET", &unknown_file).session(&member)).await;
        assert_eq!(
            member_hidden.status,
            StatusCode::NOT_FOUND,
            "{}",
            member_hidden.text
        );
        assert_eq!(
            member_unknown.status,
            StatusCode::NOT_FOUND,
            "{}",
            member_unknown.text
        );

        let member_visible = call(
            &env.app,
            Req::new("GET", &format!("/api/v1/files/{}", file_ids[1])).session(&workspace_member),
        )
        .await;
        assert_eq!(
            member_visible.status,
            StatusCode::FORBIDDEN,
            "{}",
            member_visible.text
        );
        assert_eq!(member_visible.code(), "TWO_FACTOR_SETUP_REQUIRED");

        let guest_no_grant = call(&env.app, Req::new("GET", &hidden_file).session(&guest)).await;
        assert_eq!(
            guest_no_grant.status,
            StatusCode::NOT_FOUND,
            "{}",
            guest_no_grant.text
        );
        let guest_granted = call(
            &env.app,
            Req::new("GET", &format!("/api/v1/files/{}", file_ids[0])).session(&guest),
        )
        .await;
        assert_eq!(
            guest_granted.status,
            StatusCode::FORBIDDEN,
            "{}",
            guest_granted.text
        );
        assert_eq!(guest_granted.code(), "TWO_FACTOR_SETUP_REQUIRED");

        let other_org = call(
            &env.app,
            Req::new("POST", "/api/v1/organizations").session(&operator)
                .json(json!({"name":"Optional MFA Org","slug":format!("optional-{}",Uuid::new_v4().simple()),"owner_user_id":workspace_member_id.to_string()})),
        ).await;
        assert_eq!(other_org.status, StatusCode::CREATED);
        let other_id = other_org.body["id"].as_str().unwrap();
        let allowed = call(
            &env.app,
            Req::new("GET", &format!("/api/v1/organizations/{other_id}"))
                .session(&workspace_member),
        )
        .await;
        assert_eq!(allowed.status, StatusCode::OK);
        assert_eq!(allowed.body["require_two_factor"], false);
        let personal_setup = call(
            &env.app,
            Req::new("GET", "/api/v1/auth/2fa").session(&workspace_member),
        )
        .await;
        assert_eq!(personal_setup.status, StatusCode::OK);
    }
}
