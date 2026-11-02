//! AC-13 operator API tests. Requires PostgreSQL via CC_TEST_DATABASE_URL.

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine;
use craftcloud_auth::decrypt_secret_for;
use serde_json::{Value, json};

use crate::{
    Services,
    app_access_tests::support::{Req, TestEnv, call, settings},
};

fn services(env: &TestEnv, key: Option<[u8; 32]>) -> Services {
    let services = Services::new(env.pool.clone(), None, settings()).with_clock(env.clock.clone());
    match key {
        Some(key) => services.with_two_factor_keys(Some(key), None),
        None => services,
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn setup_progress_authorization_environment_priority_and_secret_storage() {
    let env = TestEnv::new().await;
    let owner = env.login_ok("op").await;
    let app = env.router_with_services(services(&env, Some([0x5a; 32])));

    let initial = call(&app, Req::new("GET", "/api/v1/admin/setup").session(&owner)).await;
    assert_eq!(
        initial.status,
        axum::http::StatusCode::OK,
        "{}",
        initial.text
    );
    assert_eq!(initial.body["steps"][0]["status"], "done");

    let other_user = env.user("second-operator").await;
    let _ = other_user;
    let second = env.login_ok("second-operator").await;
    for request in [
        Req::new("GET", "/api/v1/admin/setup").session(&second),
        Req::new("GET", "/api/v1/admin/setup/system-check").session(&second),
        Req::new("GET", "/api/v1/admin/instance-settings").session(&second),
        Req::new("PATCH", "/api/v1/admin/setup/steps/system_check")
            .json(json!({"status":"done"}))
            .session(&second),
        Req::new("PATCH", "/api/v1/admin/instance-settings")
            .json(json!({"settings":{"mail.transport":{"value":"smtp"}}}))
            .session(&second),
        Req::new("POST", "/api/v1/admin/instance-settings/email-test").session(&second),
    ] {
        let forbidden = call(&app, request).await;
        assert_eq!(
            forbidden.status,
            axum::http::StatusCode::FORBIDDEN,
            "{}",
            forbidden.text
        );
    }

    let csrf = call(
        &app,
        Req::new("PATCH", "/api/v1/admin/setup/steps/system_check")
            .json(json!({"status":"done"}))
            .cookie_only(&owner),
    )
    .await;
    assert_eq!(csrf.status, axum::http::StatusCode::FORBIDDEN);

    let finish_early = call(
        &app,
        Req::new("PATCH", "/api/v1/admin/setup/steps/summary")
            .json(json!({"status":"done"}))
            .session(&owner),
    )
    .await;
    assert_eq!(finish_early.status, axum::http::StatusCode::CONFLICT);
    assert_eq!(finish_early.code(), "SETUP_STEPS_INCOMPLETE");

    for (step, status) in [
        ("system_check", "done"),
        ("two_factor", "done"),
        ("email_delivery", "skipped"),
        ("organization", "done"),
        ("workspace_project", "done"),
        ("invitations", "skipped"),
        ("optional_services", "skipped"),
    ] {
        let response = call(
            &app,
            Req::new("PATCH", &format!("/api/v1/admin/setup/steps/{step}"))
                .json(json!({"status":status}))
                .session(&owner),
        )
        .await;
        assert_eq!(
            response.status,
            axum::http::StatusCode::OK,
            "{step}: {}",
            response.text
        );
    }
    let finished = call(
        &app,
        Req::new("PATCH", "/api/v1/admin/setup/steps/summary")
            .json(json!({"status":"done"}))
            .session(&owner),
    )
    .await;
    assert_eq!(
        finished.status,
        axum::http::StatusCode::OK,
        "{}",
        finished.text
    );
    assert_eq!(finished.body["complete"], true);
    let locked = call(
        &app,
        Req::new("PATCH", "/api/v1/admin/setup/steps/organization")
            .json(json!({"status":"skipped"}))
            .session(&owner),
    )
    .await;
    assert_eq!(locked.status, axum::http::StatusCode::CONFLICT);

    let override_services = services(&env, Some([0x5a; 32])).with_instance_setting_environment(
        BTreeMap::from([("mail.transport".to_owned(), json!("graph"))]),
        BTreeSet::from(["mail.transport".to_owned()]),
    );
    let override_app = env.router_with_services(override_services);
    let settings_response = call(
        &override_app,
        Req::new("GET", "/api/v1/admin/instance-settings").session(&owner),
    )
    .await;
    assert_eq!(settings_response.status, axum::http::StatusCode::OK);
    assert_eq!(
        settings_response.body["settings"]["mail.transport"]["value"],
        "graph"
    );
    assert_eq!(
        settings_response.body["settings"]["mail.transport"]["source"],
        "environment"
    );
    assert_eq!(
        settings_response.body["settings"]["mail.transport"]["locked"],
        true
    );
    let locked_write = call(
        &override_app,
        Req::new("PATCH", "/api/v1/admin/instance-settings")
            .json(json!({"settings":{"mail.transport":{"value":"smtp"}}}))
            .session(&owner),
    )
    .await;
    assert_eq!(locked_write.status, axum::http::StatusCode::CONFLICT);

    let secret = "ac13 synthetic secret only";
    let saved = call(
        &app,
        Req::new("PATCH", "/api/v1/admin/instance-settings")
            .json(json!({"settings":{"mail.password":{"set":secret}}}))
            .session(&owner),
    )
    .await;
    assert_eq!(saved.status, axum::http::StatusCode::OK, "{}", saved.text);
    assert!(!saved.text.contains(secret));
    let stored: Value =
        sqlx::query_scalar("SELECT value FROM instance_settings WHERE key='mail.password'")
            .fetch_one(&env.pool)
            .await
            .unwrap();
    let ciphertext = stored["ciphertext"].as_str().unwrap();
    assert!(!ciphertext.contains(secret));
    let ciphertext = base64::engine::general_purpose::STANDARD
        .decode(ciphertext)
        .unwrap();
    let instance_id: uuid::Uuid =
        sqlx::query_scalar("SELECT operator_user_id FROM instance_setup WHERE id=true")
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(
        decrypt_secret_for(
            &[0x5a; 32],
            &ciphertext,
            instance_id,
            b"instance-setting:mail.password"
        )
        .unwrap(),
        secret.as_bytes()
    );

    let metadata = call(
        &app,
        Req::new("GET", "/api/v1/admin/instance-settings").session(&owner),
    )
    .await;
    assert_eq!(metadata.status, axum::http::StatusCode::OK);
    assert!(!metadata.text.contains(secret));
    assert!(!metadata.text.contains("ciphertext"));
    assert!(metadata.body["settings"]["mail.password"]["configured_at"].is_string());

    let before: time::OffsetDateTime =
        sqlx::query_scalar("SELECT updated_at FROM instance_settings WHERE key='mail.password'")
            .fetch_one(&env.pool)
            .await
            .unwrap();
    let empty = call(
        &app,
        Req::new("PATCH", "/api/v1/admin/instance-settings")
            .json(json!({"settings":{"mail.password":{}}}))
            .session(&owner),
    )
    .await;
    assert_eq!(empty.status, axum::http::StatusCode::OK);
    let after: time::OffsetDateTime =
        sqlx::query_scalar("SELECT updated_at FROM instance_settings WHERE key='mail.password'")
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(before, after);

    let audit_details: Vec<Value> = sqlx::query_scalar("SELECT details FROM audit_events WHERE action='instance_setting.updated' ORDER BY created_at DESC LIMIT 1")
        .fetch_all(&env.pool).await.unwrap();
    for details in audit_details {
        assert!(!details.to_string().contains(secret));
        assert!(!details.to_string().contains(&ciphertext_as_base64(&stored)));
    }

    env.clock.advance(std::time::Duration::from_secs(6 * 60));
    let stale = call(
        &app,
        Req::new("PATCH", "/api/v1/admin/instance-settings")
            .json(json!({"settings":{"mail.host":{"value":"attacker.example"}}}))
            .session(&owner),
    )
    .await;
    assert_eq!(stale.status, axum::http::StatusCode::FORBIDDEN);
    assert_eq!(stale.code(), "FRESH_AUTH_REQUIRED");
}

fn ciphertext_as_base64(stored: &Value) -> String {
    stored["ciphertext"].as_str().unwrap_or_default().to_owned()
}
