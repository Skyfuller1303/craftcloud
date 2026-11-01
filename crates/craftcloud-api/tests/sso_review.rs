//! HTTP boundary tests for the AC-10 review fixes: operator domain approval and
//! the per-address limits of the OIDC and SCIM routes.
#[cfg(test)]
mod common;
#[cfg(test)]
mod tests {
    use crate::common::{Req, TestEnv, call, settings};
    use axum::http::{StatusCode, header};
    use serde_json::json;
    use uuid::Uuid;

    async fn organization_with_pending_claim(env: &TestEnv, slug: &str, domain: &str) -> Uuid {
        let operator = env.login_ok("op").await;
        let owner = env.user(&format!("{slug}-owner")).await;
        let created = call(
            &env.app,
            Req::new("POST", "/api/v1/organizations")
                .session(&operator)
                .json(json!({"name":slug,"slug":slug,"owner_user_id":owner})),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        let org = Uuid::parse_str(created.body["id"].as_str().unwrap()).unwrap();
        sqlx::query("INSERT INTO organization_sso(org_id,discovery_url,client_id,client_secret_ciphertext,allowed_domains,enabled) VALUES($1,'https://issuer.example.test','client',decode(repeat('ab',30),'hex'),ARRAY[$2::text],true)")
            .bind(org)
            .bind(domain)
            .execute(&env.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO sso_domain_claims(org_id,domain) VALUES($1,$2)")
            .bind(org)
            .bind(domain)
            .execute(&env.pool)
            .await
            .unwrap();
        org
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL; run cargo xtask db-test"]
    async fn domain_claims_are_decided_only_by_the_operator_and_stay_unique() {
        let env = TestEnv::new().await;
        let org_a = organization_with_pending_claim(&env, "claim-a", "corp.test").await;
        let org_b = organization_with_pending_claim(&env, "claim-b", "corp.test").await;
        let operator = env.login_ok("op").await;
        let owner = env.login_ok("claim-a-owner").await;
        let list = "/api/v1/admin/sso/domain-claims";
        let decide = |org: Uuid, domain: &str| format!("{list}/{org}/{domain}");

        // Owners see their own claim as pending but can neither list nor decide.
        let own = call(
            &env.app,
            Req::new("GET", &format!("/api/v1/organizations/{org_a}/sso")).session(&owner),
        )
        .await;
        assert_eq!(own.status, StatusCode::OK, "{}", own.text);
        assert_eq!(
            own.body["domain_claims"],
            json!([{"domain":"corp.test","status":"pending"}])
        );
        assert_eq!(
            call(&env.app, Req::new("GET", list).session(&owner))
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(
                &env.app,
                Req::new("PUT", &decide(org_a, "corp.test"))
                    .session(&owner)
                    .json(json!({"decision":"approved"}))
            )
            .await
            .status,
            StatusCode::FORBIDDEN
        );

        let pending = call(&env.app, Req::new("GET", list).session(&operator)).await;
        assert_eq!(pending.status, StatusCode::OK, "{}", pending.text);
        assert_eq!(pending.body["items"].as_array().unwrap().len(), 2);
        for bad in [json!({"decision":"maybe"}), json!({})] {
            assert!(
                call(
                    &env.app,
                    Req::new("PUT", &decide(org_a, "corp.test"))
                        .session(&operator)
                        .json(bad)
                )
                .await
                .status
                .is_client_error()
            );
        }
        assert!(
            call(
                &env.app,
                Req::new("PUT", &decide(org_a, "Not_A_Domain"))
                    .session(&operator)
                    .json(json!({"decision":"approved"}))
            )
            .await
            .status
            .is_client_error()
        );
        let missing = call(
            &env.app,
            Req::new("PUT", &decide(org_a, "unknown.test"))
                .session(&operator)
                .json(json!({"decision":"approved"})),
        )
        .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.text);

        let approved = call(
            &env.app,
            Req::new("PUT", &decide(org_a, "corp.test"))
                .session(&operator)
                .json(json!({"decision":"approved"})),
        )
        .await;
        assert_eq!(approved.status, StatusCode::OK, "{}", approved.text);
        assert_eq!(approved.body["status"], "approved");
        assert_eq!(
            env.count("SELECT count(*) FROM audit_events WHERE action='sso.domain_approved'")
                .await,
            1
        );
        // The same domain cannot be approved for the other organization.
        let taken = call(
            &env.app,
            Req::new("PUT", &decide(org_b, "corp.test"))
                .session(&operator)
                .json(json!({"decision":"approved"})),
        )
        .await;
        assert_eq!(taken.status, StatusCode::CONFLICT, "{}", taken.text);
        assert_eq!(taken.code(), "SSO_DOMAIN_ALREADY_APPROVED");
        let still_pending = call(&env.app, Req::new("GET", list).session(&operator)).await;
        assert_eq!(still_pending.body["items"].as_array().unwrap().len(), 1);
        assert_eq!(
            still_pending.body["items"][0]["organization_id"],
            json!(org_b)
        );
        let approved_list = call(
            &env.app,
            Req::new("GET", &format!("{list}?status=approved")).session(&operator),
        )
        .await;
        assert_eq!(
            approved_list.body["items"][0]["organization_id"],
            json!(org_a)
        );
        // The discovery hint follows the approval, not the claim.
        let hint = call(
            &env.app,
            Req::new("POST", "/api/v1/auth/oidc/discover").json(json!({"email":"a@corp.test"})),
        )
        .await;
        assert_eq!(hint.body["organization_id"], json!(org_a));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL; run cargo xtask db-test"]
    async fn scim_requests_are_limited_per_address_and_unauthenticated_ones_leave_no_audit_trail() {
        let mut limits = settings();
        limits.scim_ip_limit = 3;
        let env = TestEnv::with(limits, craftcloud_api::ApiConfig::default()).await;
        let org = Uuid::new_v4();
        let path = format!("/scim/v2/organizations/{org}/Users");
        for _ in 0..3 {
            let rejected = call(
                &env.app,
                Req::new("GET", &path).header("authorization", "Bearer cc_scim_not-a-token"),
            )
            .await;
            assert_eq!(rejected.status, StatusCode::UNAUTHORIZED);
        }
        let limited = call(
            &env.app,
            Req::new("GET", &path).header("authorization", "Bearer cc_scim_not-a-token"),
        )
        .await;
        assert_eq!(
            limited.status,
            StatusCode::TOO_MANY_REQUESTS,
            "{}",
            limited.text
        );
        assert!(limited.headers.contains_key(header::RETRY_AFTER));
        assert_eq!(
            limited.headers[header::CONTENT_TYPE],
            "application/scim+json"
        );
        assert_eq!(
            env.count("SELECT count(*) FROM audit_events WHERE action LIKE 'scim.%'")
                .await,
            0,
            "unauthenticated and throttled SCIM requests must not write audit rows"
        );
        // The limit belongs to /scim/v2 only: interactive login routes keep their own budget.
        let login = call(&env.app, Req::new("GET", "/api/v1/auth/session")).await;
        assert_ne!(login.status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL; run cargo xtask db-test"]
    async fn oidc_authorize_and_callback_have_a_dedicated_address_limit() {
        let mut limits = settings();
        limits.oidc_ip_limit = 2;
        let env = TestEnv::with(limits, craftcloud_api::ApiConfig::default()).await;
        let flow = "A".repeat(43);
        for _ in 0..2 {
            let unknown = call(
                &env.app,
                Req::new("GET", &format!("/api/v1/auth/oidc/authorize?flow={flow}")),
            )
            .await;
            assert_eq!(unknown.status, StatusCode::BAD_REQUEST, "{}", unknown.text);
        }
        let limited = call(
            &env.app,
            Req::new("GET", &format!("/api/v1/auth/oidc/authorize?flow={flow}")),
        )
        .await;
        assert_eq!(
            limited.status,
            StatusCode::TOO_MANY_REQUESTS,
            "{}",
            limited.text
        );
        assert!(limited.headers.contains_key(header::RETRY_AFTER));
        // callback shares the bucket; discovery keeps its own (login) budget.
        let callback = call(
            &env.app,
            Req::new("GET", "/api/v1/auth/oidc/callback?state=x"),
        )
        .await;
        assert_eq!(callback.status, StatusCode::TOO_MANY_REQUESTS);
        let discover = call(
            &env.app,
            Req::new("POST", "/api/v1/auth/oidc/discover").json(json!({"email":"a@nowhere.test"})),
        )
        .await;
        assert_eq!(discover.status, StatusCode::OK, "{}", discover.text);
    }
}
