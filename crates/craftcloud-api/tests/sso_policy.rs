//! Current SSO/MFA policy and tenant-specific session revocation boundaries.
#[cfg(test)]
mod common;
use axum::http::StatusCode;
use common::{Req, TestEnv, call};
use serde_json::json;
use uuid::Uuid;
#[tokio::test]
#[ignore = "requires PostgreSQL; run cargo xtask db-test"]
async fn local_recovery_mfa_changes_and_cutoffs_preserve_tenant_boundaries() {
    let env = TestEnv::new().await;
    let owner = env.user("sso-owner").await;
    let member = env.user("sso-member").await;
    let org = Uuid::new_v4();
    let other = Uuid::new_v4();
    for (id, slug) in [(org, "sso-policy"), (other, "other-policy")] {
        sqlx::query("INSERT INTO organizations(id,slug,name,owner_user_id) VALUES($1,$2,'Synthetic organization',$3)").bind(id).bind(slug).bind(owner).execute(&env.pool).await.unwrap();
        sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'owner','active'),($1,$3,'member','active')").bind(id).bind(owner).bind(member).execute(&env.pool).await.unwrap();
    }
    let secret =
        craftcloud_auth::encrypt_secret_for(&[0x42; 32], org, b"oidc-client", b"synthetic-secret")
            .unwrap();
    sqlx::query("INSERT INTO organization_sso(org_id,discovery_url,client_id,client_secret_ciphertext,allowed_domains,enabled,sso_required,trust_mfa) VALUES($1,'https://id.example.test/.well-known/openid-configuration','synthetic-client',$2,ARRAY['example.test'],true,true,true)").bind(org).bind(secret).execute(&env.pool).await.unwrap();
    // Global issuer/subject binding remains the same identity in another
    // explicitly configured organization; email claims are not the key.
    let other_secret = craftcloud_auth::encrypt_secret_for(
        &[0x42; 32],
        other,
        b"oidc-client",
        b"synthetic-secret",
    )
    .unwrap();
    sqlx::query("INSERT INTO organization_sso(org_id,discovery_url,client_id,client_secret_ciphertext,allowed_domains,enabled) VALUES($1,'https://id.example.test/.well-known/openid-configuration','other-client',$2,ARRAY['example.test'],true)").bind(other).bind(other_secret).execute(&env.pool).await.unwrap();
    sqlx::query("INSERT INTO external_identities(org_id,issuer,subject,user_id) VALUES($1,'https://id.example.test','global-subject',$2)").bind(org).bind(member).execute(&env.pool).await.unwrap();
    let mut tx = env.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
        .bind(other)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let identity = craftcloud_db::oidc::Identity {
        issuer: "https://id.example.test".into(),
        subject: "global-subject".into(),
        email: Some("reassigned@elsewhere.test".into()),
        email_verified: false,
        display_name: None,
        mfa: false,
    };
    let resolved = craftcloud_db::oidc::resolve_identity(
        &mut tx,
        other,
        1,
        &identity,
        None,
        time::OffsetDateTime::now_utc(),
        &craftcloud_db::audit::AuditCtx {
            actor_id: None,
            request_id: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(resolved, craftcloud_db::oidc::Resolution::Resolved(member));
    tx.commit().await.unwrap();
    let local = env.login_ok("sso-member").await;
    let recovery = env.login_ok("sso-owner").await;
    let operator = env.login_ok("op").await;
    let path = format!("/api/v1/organizations/{org}");
    let denied = call(&env.app, Req::new("GET", &path).session(&local)).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(denied.code(), "SSO_REQUIRED");
    assert_eq!(
        call(&env.app, Req::new("GET", &path).session(&recovery))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        call(&env.app, Req::new("GET", &path).session(&operator))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'member','active')").bind(org).bind(env.operator_id).execute(&env.pool).await.unwrap();
    assert_eq!(
        call(&env.app, Req::new("GET", &path).session(&operator))
            .await
            .status,
        StatusCode::OK
    );
    let other_path = format!("/api/v1/organizations/{other}");
    assert_eq!(
        call(&env.app, Req::new("GET", &other_path).session(&local))
            .await
            .status,
        StatusCode::OK
    );
    let token = craftcloud_auth::OpaqueToken::parse(&local.token).unwrap();
    let session: Uuid = sqlx::query_scalar("SELECT id FROM sessions WHERE token_hash=$1")
        .bind(token.hash().as_slice())
        .fetch_one(&env.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sso_session_bindings(session_kind,session_id,org_id,user_id,mfa,provider_mfa) VALUES('browser',$1,$2,$3,true,true)").bind(session).bind(org).bind(member).execute(&env.pool).await.unwrap();
    sqlx::query("UPDATE organizations SET require_two_factor=true WHERE id=$1")
        .bind(org)
        .execute(&env.pool)
        .await
        .unwrap();
    assert_eq!(
        call(&env.app, Req::new("GET", &path).session(&local))
            .await
            .status,
        StatusCode::OK
    );
    sqlx::query("UPDATE organization_sso SET trust_mfa=false WHERE org_id=$1")
        .bind(org)
        .execute(&env.pool)
        .await
        .unwrap();
    let denied = call(&env.app, Req::new("GET", &path).session(&local)).await;
    assert_eq!(denied.code(), "SSO_MFA_REQUIRED");
    let secret =
        craftcloud_auth::encrypt_secret_for(&[0x42; 32], member, b"totp", &[0x31; 20]).unwrap();
    sqlx::query("INSERT INTO user_two_factor(user_id,secret_ciphertext,enabled_at,created_at) VALUES($1,$2,now(),now())").bind(member).bind(secret).execute(&env.pool).await.unwrap();
    sqlx::query("UPDATE sso_session_bindings SET provider_mfa=false WHERE session_id=$1")
        .bind(session)
        .execute(&env.pool)
        .await
        .unwrap();
    assert_eq!(
        call(&env.app, Req::new("GET", &path).session(&local))
            .await
            .status,
        StatusCode::OK
    );
    sqlx::query("DELETE FROM user_two_factor WHERE user_id=$1")
        .bind(member)
        .execute(&env.pool)
        .await
        .unwrap();
    assert_eq!(
        call(&env.app, Req::new("GET", &path).session(&local))
            .await
            .code(),
        "SSO_MFA_REQUIRED"
    );
    sqlx::query("INSERT INTO organization_session_cutoffs(org_id,user_id,revoked_before) VALUES($1,$2,now()+interval '1 second')").bind(org).bind(member).execute(&env.pool).await.unwrap();
    let scoped = call(&env.app, Req::new("GET", &path).session(&local)).await;
    assert_eq!(scoped.status, StatusCode::FORBIDDEN);
    assert_eq!(scoped.code(), "ORGANIZATION_REAUTHENTICATION_REQUIRED");
    assert_eq!(
        call(&env.app, Req::new("GET", &other_path).session(&local))
            .await
            .status,
        StatusCode::OK
    );
    let unknown = format!("/api/v1/organizations/{}", Uuid::new_v4());
    assert_eq!(
        call(&env.app, Req::new("GET", &unknown).session(&local))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let profile = call(
        &env.app,
        Req::new("GET", "/api/v1/auth/session").session(&local),
    )
    .await;
    assert_eq!(profile.status, StatusCode::OK);
    assert_ne!(profile.body, json!(null));
}
