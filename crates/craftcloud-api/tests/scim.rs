//! HTTP boundary tests for SCIM provisioning (AC-10).

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use axum::http::{StatusCode, header};
    use serde_json::json;
    use uuid::Uuid;

    use crate::common::{Req, Session, TestEnv, call};

    const USER_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
    const GROUP_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";

    struct World {
        env: TestEnv,
        operator: Session,
        owner: Session,
        owner_id: Uuid,
        org: Uuid,
    }

    impl World {
        async fn new() -> Self {
            let env = TestEnv::new().await;
            let operator = env.login_ok("op").await;
            let owner_id = env.user("ac10-owner").await;
            let owner = env.login_ok("ac10-owner").await;
            let slug = format!("ac10-{}", Uuid::new_v4().simple());
            let response = call(
                &env.app,
                Req::new("POST", "/api/v1/organizations")
                    .session(&operator)
                    .json(json!({"name":"AC10 Test","slug":slug,"owner_user_id":owner_id})),
            )
            .await;
            assert_eq!(response.status, StatusCode::CREATED, "{}", response.text);
            let org = response.body["id"]
                .as_str()
                .and_then(|id| Uuid::parse_str(id).ok())
                .unwrap();
            crate::common::approve_example_domain(&env.pool, org).await;
            Self {
                env,
                operator,
                owner,
                owner_id,
                org,
            }
        }
        async fn token(&self) -> String {
            let r = call(
                &self.env.app,
                Req::new(
                    "POST",
                    &format!("/api/v1/organizations/{}/scim/token", self.org),
                )
                .session(&self.owner),
            )
            .await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.text);
            r.body["token"].as_str().unwrap().to_owned()
        }
        async fn scim(&self, method: &'static str, path: &str, token: &str) -> crate::common::Resp {
            call(
                &self.env.app,
                Req::new(method, path).header("authorization", &format!("Bearer {token}")),
            )
            .await
        }
        fn url(&self, resource: &str) -> String {
            format!("/scim/v2/organizations/{}/{}", self.org, resource)
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL; run cargo xtask db-test"]
    async fn token_rotation_is_once_only_and_scim_auth_is_separate() {
        let w = World::new().await;
        let old = w.token().await;
        let rotated = call(
            &w.env.app,
            Req::new(
                "POST",
                &format!("/api/v1/organizations/{}/scim/token", w.org),
            )
            .session(&w.owner),
        )
        .await;
        assert_eq!(rotated.status, StatusCode::OK);
        let fresh = rotated.body["token"].as_str().unwrap();
        assert_ne!(fresh, old);
        assert_eq!(
            w.scim("GET", &w.url("Users"), &old).await.status,
            StatusCode::UNAUTHORIZED
        );
        let users = w.scim("GET", &w.url("Users"), fresh).await;
        assert_eq!(users.status, StatusCode::OK, "{}", users.text);
        assert_eq!(users.headers[header::CONTENT_TYPE], "application/scim+json");
        let cookie_auth = call(
            &w.env.app,
            Req::new("GET", &w.url("Users"))
                .session(&w.owner)
                .header("authorization", &format!("Bearer {fresh}")),
        )
        .await;
        assert_eq!(cookie_auth.status, StatusCode::UNAUTHORIZED);
        let normal_api = call(
            &w.env.app,
            Req::new("GET", "/api/v1/auth/session")
                .header("authorization", &format!("Bearer {fresh}")),
        )
        .await;
        assert_eq!(normal_api.status, StatusCode::UNAUTHORIZED);
        let revoke = call(
            &w.env.app,
            Req::new(
                "DELETE",
                &format!("/api/v1/organizations/{}/scim/token", w.org),
            )
            .session(&w.owner),
        )
        .await;
        assert_eq!(revoke.status, StatusCode::NO_CONTENT);
        assert_eq!(
            w.scim("GET", &w.url("Users"), fresh).await.status,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL; run cargo xtask db-test"]
    async fn users_are_member_only_and_support_exact_scim_filter_and_page_cap() {
        let w = World::new().await;
        let token = w.token().await;
        let create = json!({"schemas":[USER_SCHEMA],"userName":"scim@example.test","displayName":"SCIM","active":true});
        let added = call(
            &w.env.app,
            Req::new("POST", &w.url("Users"))
                .header("authorization", &format!("Bearer {token}"))
                .json(create),
        )
        .await;
        assert_eq!(added.status, StatusCode::CREATED, "{}", added.text);
        let user_id = Uuid::parse_str(added.body["id"].as_str().unwrap()).unwrap();
        let role: String = sqlx::query_scalar("SELECT role FROM organization_members m JOIN scim_users s ON s.user_id=m.user_id WHERE m.org_id=$1 AND s.id=$2").bind(w.org).bind(user_id).fetch_one(&w.env.pool).await.unwrap();
        let verified: Option<time::OffsetDateTime> = sqlx::query_scalar("SELECT u.email_verified_at FROM users u JOIN scim_users s ON s.user_id=u.id WHERE s.org_id=$1 AND s.id=$2").bind(w.org).bind(user_id).fetch_one(&w.env.pool).await.unwrap();
        assert_eq!(role, "member");
        assert!(verified.is_none());
        assert_eq!(added.body["schemas"][0], USER_SCHEMA);
        let second = call(&w.env.app, Req::new("POST", &w.url("Users")).header("authorization", &format!("Bearer {token}" )).json(json!({"schemas":[USER_SCHEMA],"userName":"second@example.test","displayName":"Second"}))).await;
        assert_eq!(second.status, StatusCode::CREATED, "{}", second.text);
        let list = w
            .scim(
                "GET",
                &format!("{}?startIndex=1&count=500", w.url("Users")),
                &token,
            )
            .await;
        assert_eq!(list.status, StatusCode::OK);
        assert_eq!(list.body["itemsPerPage"], 2);
        assert_eq!(list.body["Resources"].as_array().unwrap().len(), 2);
        let filtered = w
            .scim(
                "GET",
                &format!(
                    "{}?filter=userName%20eq%20%22scim%40example.test%22",
                    w.url("Users")
                ),
                &token,
            )
            .await;
        assert_eq!(filtered.status, StatusCode::OK, "{}", filtered.text);
        assert_eq!(filtered.body["totalResults"], 1);
        let bad_filter = w
            .scim(
                "GET",
                &format!("{}?filter=userName%20co%20%22x%22", w.url("Users")),
                &token,
            )
            .await;
        assert_eq!(bad_filter.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL; run cargo xtask db-test"]
    async fn groups_reject_foreign_members_and_emit_scim_group_shape() {
        let w = World::new().await;
        let token = w.token().await;
        let group_body = json!({"schemas":[GROUP_SCHEMA],"displayName":"Engineering","members":[]});
        let made = call(
            &w.env.app,
            Req::new("POST", &w.url("Groups"))
                .header("authorization", &format!("Bearer {token}"))
                .json(group_body),
        )
        .await;
        assert_eq!(made.status, StatusCode::CREATED, "{}", made.text);
        assert_eq!(made.headers[header::CONTENT_TYPE], "application/scim+json");
        assert_eq!(made.body["schemas"][0], GROUP_SCHEMA);
        assert_eq!(made.body["members"], json!([]));
        let foreign_id = Uuid::new_v4();
        let foreign = call(&w.env.app, Req::new("POST", &w.url("Groups")).header("authorization", &format!("Bearer {token}" )).json(json!({"schemas":[GROUP_SCHEMA],"displayName":"Foreign","members":[{"value":foreign_id}]}))).await;
        assert_eq!(foreign.status, StatusCode::BAD_REQUEST, "{}", foreign.text);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL; run cargo xtask db-test"]
    async fn bearer_is_bound_to_its_organization() {
        let w = World::new().await;
        let first = w.token().await;
        let second_owner = w.env.user("ac10-owner-two").await;
        let second_session = w.env.login_ok("ac10-owner-two").await;
        let slug = format!("ac10-{}", Uuid::new_v4().simple());
        let created = call(
            &w.env.app,
            Req::new("POST", "/api/v1/organizations")
                .session(&w.operator)
                .json(json!({"name":"Second AC10","slug":slug,"owner_user_id":second_owner})),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        let second_org = Uuid::parse_str(created.body["id"].as_str().unwrap()).unwrap();
        let second_token = call(
            &w.env.app,
            Req::new(
                "POST",
                &format!("/api/v1/organizations/{second_org}/scim/token"),
            )
            .session(&second_session),
        )
        .await;
        assert_eq!(second_token.status, StatusCode::OK, "{}", second_token.text);
        let second_token = second_token.body["token"].as_str().unwrap();
        assert_eq!(
            w.scim(
                "GET",
                &format!("/scim/v2/organizations/{second_org}/Users"),
                &first
            )
            .await
            .status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            w.scim(
                "GET",
                &format!("/scim/v2/organizations/{}/Users", w.org),
                second_token
            )
            .await
            .status,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL; run cargo xtask db-test"]
    async fn authenticated_scim_failures_are_aggregated_without_request_data() {
        let w = World::new().await;
        let token = w.token().await;
        let sensitive = "private-payload@example.test";
        for _ in 0..2 {
            let response = call(
                &w.env.app,
                Req::new("POST", &w.url("Users"))
                    .header("authorization", &format!("Bearer {token}"))
                    .json(
                        json!({"schemas":[USER_SCHEMA],"userName":sensitive,"unexpected":"secret"}),
                    ),
            )
            .await;
            assert_eq!(response.status, StatusCode::BAD_REQUEST);
        }
        let (rows, total, audit_text): (i64, i64, String) = sqlx::query_as(
            "SELECT count(*), COALESCE(sum(event_count),0)::bigint, COALESCE(string_agg(row_to_json(a)::text,' '),'') FROM audit_events a WHERE org_id=$1 AND action='scim.users_create_failed'",
        )
        .bind(w.org)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!((rows, total), (1, 2));
        assert!(!audit_text.contains(sensitive));
        assert!(!audit_text.contains("unexpected"));
        assert!(!audit_text.contains(&token));

        let second_owner = w.env.user("ac10-audit-other").await;
        let second_session = w.env.login_ok("ac10-audit-other").await;
        let slug = format!("ac10-{}", Uuid::new_v4().simple());
        let created = call(
            &w.env.app,
            Req::new("POST", "/api/v1/organizations")
                .session(&w.operator)
                .json(json!({"name":"Other org","slug":slug,"owner_user_id":second_owner})),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        let other_org = Uuid::parse_str(created.body["id"].as_str().unwrap()).unwrap();
        let other_token = call(
            &w.env.app,
            Req::new(
                "POST",
                &format!("/api/v1/organizations/{other_org}/scim/token"),
            )
            .session(&second_session),
        )
        .await;
        assert_eq!(other_token.status, StatusCode::OK, "{}", other_token.text);
        let other_token = other_token.body["token"].as_str().unwrap().to_owned();
        for (path, credential) in [
            (w.url("Users"), "cc_scim_wrong".to_owned()),
            (
                format!("/scim/v2/organizations/{other_org}/Users"),
                token.clone(),
            ),
            (w.url("Users"), other_token.clone()),
        ] {
            assert_eq!(
                w.scim("GET", &path, &credential).await.status,
                StatusCode::UNAUTHORIZED
            );
        }
        call(
            &w.env.app,
            Req::new(
                "DELETE",
                &format!("/api/v1/organizations/{}/scim/token", w.org),
            )
            .session(&w.owner),
        )
        .await;
        assert_eq!(
            w.scim("GET", &w.url("Users"), &token).await.status,
            StatusCode::UNAUTHORIZED
        );
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events WHERE org_id=$1 AND action LIKE 'scim.%_failed'",
        )
        .bind(w.org)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL; run cargo xtask db-test"]
    async fn protected_accounts_collisions_and_patch_failures_preserve_state() {
        let w = World::new().await;
        let token = w.token().await;
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO scim_users(org_id,id,user_id,user_name,display_name,active) VALUES($1,$2,$3,'ac10-owner@example.test','Protected Owner',true)")
            .bind(w.org).bind(id).bind(w.owner_id).execute(&w.env.pool).await.unwrap();
        let path = w.url(&format!("Users/{id}"));
        let patch = json!({"schemas":["urn:ietf:params:scim:api:messages:2.0:PatchOp"],"Operations":[{"op":"replace","path":"displayName","value":"Changed"},{"op":"replace","path":"roles","value":["owner"]}]});
        let rejected = call(
            &w.env.app,
            Req::new("PATCH", &path)
                .header("authorization", &format!("Bearer {token}"))
                .json(patch),
        )
        .await;
        assert_eq!(rejected.status, StatusCode::BAD_REQUEST);
        let display: String =
            sqlx::query_scalar("SELECT display_name FROM scim_users WHERE org_id=$1 AND id=$2")
                .bind(w.org)
                .bind(id)
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
        assert_eq!(display, "Protected Owner");
        let denied = call(
            &w.env.app,
            Req::new("DELETE", &path).header("authorization", &format!("Bearer {token}")),
        )
        .await;
        assert_eq!(denied.status, StatusCode::BAD_REQUEST, "{}", denied.text);
        let admin = w.env.user("ac10-admin").await;
        sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'admin','active')").bind(w.org).bind(admin).execute(&w.env.pool).await.unwrap();
        for (protected_id, protected_email, label) in [
            (admin, "ac10-admin@example.test", "Admin"),
            (w.env.operator_id, "op@example.test", "Operator"),
        ] {
            let protected_scim_id = Uuid::new_v4();
            sqlx::query("INSERT INTO scim_users(org_id,id,user_id,user_name,display_name,active) VALUES($1,$2,$3,$4,$5,true)")
                .bind(w.org).bind(protected_scim_id).bind(protected_id).bind(protected_email).bind(label).execute(&w.env.pool).await.unwrap();
            let protected_path = w.url(&format!("Users/{protected_scim_id}"));
            let result = call(
                &w.env.app,
                Req::new("DELETE", &protected_path)
                    .header("authorization", &format!("Bearer {token}")),
            )
            .await;
            let expected = if protected_id == admin {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::BAD_REQUEST
            };
            assert_eq!(
                result.status, expected,
                "managed administrators remain subject to deprovisioning"
            );
        }
        let collision = call(&w.env.app, Req::new("POST", &w.url("Users")).header("authorization", &format!("Bearer {token}")).json(json!({"schemas":[USER_SCHEMA],"userName":"ac10-owner@example.test","displayName":"Attempted link"}))).await;
        assert_eq!(
            collision.status,
            StatusCode::CONFLICT,
            "email collision must not auto-link"
        );
        let linked: i64 =
            sqlx::query_scalar("SELECT count(*) FROM scim_users WHERE org_id=$1 AND user_id=$2")
                .bind(w.org)
                .bind(w.owner_id)
                .fetch_one(&w.env.pool)
                .await
                .unwrap();
        assert_eq!(linked, 1);
        let events = call(
            &w.env.app,
            Req::new(
                "GET",
                &format!("/api/v1/organizations/{}/scim/events", w.org),
            )
            .session(&w.owner),
        )
        .await;
        assert_eq!(events.status, StatusCode::OK);
        assert!(!events.text.contains(&token));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL; run cargo xtask db-test"]
    async fn deactivation_sets_tenant_cutoff_and_reactivation_keeps_cutoff() {
        let w = World::new().await;
        let token = w.token().await;
        let user = w.env.user("ac10-deactivate").await;
        sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'member','active')")
            .bind(w.org).bind(user).execute(&w.env.pool).await.unwrap();
        let user_session = w.env.login_ok("ac10-deactivate").await;
        let other_owner = w.env.user("ac10-other-owner").await;
        let slug = format!("ac10-{}", Uuid::new_v4().simple());
        let other_org_response = call(
            &w.env.app,
            Req::new("POST", "/api/v1/organizations")
                .session(&w.operator)
                .json(json!({"name":"Other AC10","slug":slug,"owner_user_id":other_owner})),
        )
        .await;
        assert_eq!(
            other_org_response.status,
            StatusCode::CREATED,
            "{}",
            other_org_response.text
        );
        let other_org = Uuid::parse_str(other_org_response.body["id"].as_str().unwrap()).unwrap();
        sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'member','active')")
            .bind(other_org).bind(user).execute(&w.env.pool).await.unwrap();
        let local_members = call(
            &w.env.app,
            Req::new("GET", &format!("/api/v1/organizations/{}/members", w.org))
                .cookie_only(&user_session),
        )
        .await;
        let other_members = call(
            &w.env.app,
            Req::new("GET", &format!("/api/v1/organizations/{other_org}/members"))
                .cookie_only(&user_session),
        )
        .await;
        assert_eq!(
            local_members.status,
            StatusCode::OK,
            "{}",
            local_members.text
        );
        assert_eq!(
            other_members.status,
            StatusCode::OK,
            "{}",
            other_members.text
        );
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO scim_users(org_id,id,user_id,user_name,display_name,active) VALUES($1,$2,$3,'ac10-deactivate@example.test','Deactivate',true)")
            .bind(w.org).bind(id).bind(user).execute(&w.env.pool).await.unwrap();
        let path = w.url(&format!("Users/{id}"));
        let deleted = call(
            &w.env.app,
            Req::new("DELETE", &path).header("authorization", &format!("Bearer {token}")),
        )
        .await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.text);
        let deactivated_org = call(
            &w.env.app,
            Req::new("GET", &format!("/api/v1/organizations/{}/members", w.org))
                .cookie_only(&user_session),
        )
        .await;
        let surviving_org = call(
            &w.env.app,
            Req::new("GET", &format!("/api/v1/organizations/{other_org}/members"))
                .cookie_only(&user_session),
        )
        .await;
        assert_ne!(
            deactivated_org.status,
            StatusCode::OK,
            "deactivation must deny the user's old session in that tenant"
        );
        assert_eq!(
            surviving_org.status,
            StatusCode::OK,
            "{}",
            surviving_org.text
        );
        let status: String = sqlx::query_scalar(
            "SELECT status FROM organization_members WHERE org_id=$1 AND user_id=$2",
        )
        .bind(w.org)
        .bind(user)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        let cutoff: Option<time::OffsetDateTime> = sqlx::query_scalar("SELECT revoked_before FROM organization_session_cutoffs WHERE org_id=$1 AND user_id=$2").bind(w.org).bind(user).fetch_optional(&w.env.pool).await.unwrap();
        assert_eq!(status, "disabled");
        assert!(cutoff.is_some());
        sqlx::query("INSERT INTO scim_users(org_id,id,user_id,user_name,display_name,active) VALUES($1,$2,$3,'ac10-deactivate@example.test','Deactivate',false)")
            .bind(w.org).bind(id).bind(user).execute(&w.env.pool).await.unwrap();
        let reactivated = call(&w.env.app, Req::new("PUT", &path).header("authorization", &format!("Bearer {token}")).json(json!({"schemas":[USER_SCHEMA],"id":id,"userName":"ac10-deactivate@example.test","displayName":"Reactivate","active":true}))).await;
        assert_eq!(reactivated.status, StatusCode::OK, "{}", reactivated.text);
        let stale_after_reactivation = call(
            &w.env.app,
            Req::new("GET", &format!("/api/v1/organizations/{}/members", w.org))
                .cookie_only(&user_session),
        )
        .await;
        assert_ne!(
            stale_after_reactivation.status,
            StatusCode::OK,
            "reactivation must not revive the pre-deactivation session"
        );
        let still_cut_off: Option<time::OffsetDateTime> = sqlx::query_scalar("SELECT revoked_before FROM organization_session_cutoffs WHERE org_id=$1 AND user_id=$2").bind(w.org).bind(user).fetch_optional(&w.env.pool).await.unwrap();
        assert_eq!(still_cut_off, cutoff);
        let status: String = sqlx::query_scalar(
            "SELECT status FROM organization_members WHERE org_id=$1 AND user_id=$2",
        )
        .bind(w.org)
        .bind(user)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        assert_eq!(status, "active");
    }
}
