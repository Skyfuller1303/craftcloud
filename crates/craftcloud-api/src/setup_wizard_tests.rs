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

async fn patch_settings(
    app: &axum::Router,
    session: &crate::app_access_tests::support::Session,
    body: Value,
) -> crate::app_access_tests::support::Resp {
    call(
        app,
        Req::new("PATCH", "/api/v1/admin/instance-settings")
            .json(body)
            .session(session),
    )
    .await
}

async fn stored(env: &TestEnv, key: &str) -> Option<Value> {
    sqlx::query_scalar("SELECT value FROM instance_settings WHERE key=$1")
        .bind(key)
        .fetch_optional(&env.pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn changing_a_credential_target_deletes_stored_secrets_unless_replaced() {
    let env = TestEnv::new().await;
    let owner = env.login_ok("op").await;
    let app = env.router_with_services(services(&env, Some([0x5a; 32])));

    let first = patch_settings(
        &app,
        &owner,
        json!({"settings":{
            "mail.transport":{"value":"smtp"},
            "mail.host":{"value":"old-mail.example"},
            "mail.port":{"value":587},
            "mail.password":{"set":"old password"},
            "mail.client_secret":{"set":"old client secret"},
        }}),
    )
    .await;
    assert_eq!(first.status, axum::http::StatusCode::OK, "{}", first.text);
    assert_eq!(first.body["credentials_cleared"], json!([]));
    assert!(stored(&env, "mail.password").await.is_some());

    // Re-sending unchanged target values keeps the credentials.
    let same = patch_settings(
        &app,
        &owner,
        json!({"settings":{"mail.host":{"value":"old-mail.example"},"mail.port":{"value":587}}}),
    )
    .await;
    assert_eq!(same.status, axum::http::StatusCode::OK, "{}", same.text);
    assert_eq!(same.body["credentials_cleared"], json!([]));
    assert!(stored(&env, "mail.password").await.is_some());

    // New host without a new password: every stored secret is deleted.
    let moved = patch_settings(
        &app,
        &owner,
        json!({"settings":{"mail.host":{"value":"new-mail.example"}}}),
    )
    .await;
    assert_eq!(moved.status, axum::http::StatusCode::OK, "{}", moved.text);
    assert_eq!(
        moved.body["credentials_cleared"],
        json!(["mail.client_secret", "mail.password"])
    );
    assert!(stored(&env, "mail.password").await.is_none());
    assert!(stored(&env, "mail.client_secret").await.is_none());
    assert!(
        moved.body["settings"]["mail.password"]["configured_at"].is_null(),
        "{}",
        moved.text
    );
    let audit: Vec<Value> = sqlx::query_scalar(
        "SELECT details FROM audit_events WHERE action='instance_setting.secret_cleared' ORDER BY details->>'key'",
    )
    .fetch_all(&env.pool)
    .await
    .unwrap();
    assert_eq!(
        audit,
        vec![
            json!({"key":"mail.client_secret","reason":"credential_target_changed"}),
            json!({"key":"mail.password","reason":"credential_target_changed"}),
        ]
    );

    // New host with a new password: the new credential is stored, others go.
    let replace = patch_settings(
        &app,
        &owner,
        json!({"settings":{
            "mail.password":{"set":"first new password"},
            "mail.client_secret":{"set":"second client secret"},
        }}),
    )
    .await;
    assert_eq!(replace.status, axum::http::StatusCode::OK);
    let moved_again = patch_settings(
        &app,
        &owner,
        json!({"settings":{
            "mail.host":{"value":"third-mail.example"},
            "mail.password":{"set":"new password for third host"},
        }}),
    )
    .await;
    assert_eq!(
        moved_again.status,
        axum::http::StatusCode::OK,
        "{}",
        moved_again.text
    );
    assert_eq!(
        moved_again.body["credentials_cleared"],
        json!(["mail.client_secret"])
    );
    let password = stored(&env, "mail.password").await.unwrap();
    let ciphertext = base64::engine::general_purpose::STANDARD
        .decode(password["ciphertext"].as_str().unwrap())
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
        b"new password for third host"
    );
    assert!(stored(&env, "mail.client_secret").await.is_none());

    // A transport change alone also clears credentials.
    let transport = patch_settings(
        &app,
        &owner,
        json!({"settings":{"mail.transport":{"value":"graph"}}}),
    )
    .await;
    assert_eq!(
        transport.body["credentials_cleared"],
        json!(["mail.password"])
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn environment_mail_locks_secret_writes_too() {
    let env = TestEnv::new().await;
    let owner = env.login_ok("op").await;
    let locked: BTreeSet<String> = ["mail.transport", "mail.password", "mail.client_secret"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let app = env.router_with_services(
        services(&env, Some([0x5a; 32])).with_instance_setting_environment(BTreeMap::new(), locked),
    );
    let response = patch_settings(
        &app,
        &owner,
        json!({"settings":{"mail.password":{"set":"database secret"}}}),
    )
    .await;
    assert_eq!(response.status, axum::http::StatusCode::CONFLICT);
    assert_eq!(response.code(), "SETTING_LOCKED");
    assert!(stored(&env, "mail.password").await.is_none());
    let read = call(
        &app,
        Req::new("GET", "/api/v1/admin/instance-settings").session(&owner),
    )
    .await;
    assert_eq!(read.body["settings"]["mail.password"]["locked"], true);
    assert_eq!(
        read.body["settings"]["mail.password"]["source"],
        "environment"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn secret_keys_reject_a_plain_value() {
    let env = TestEnv::new().await;
    let owner = env.login_ok("op").await;
    let app = env.router_with_services(services(&env, Some([0x5a; 32])));
    let response = patch_settings(
        &app,
        &owner,
        json!({"settings":{"mail.password":{"value":"typed into the wrong field"}}}),
    )
    .await;
    assert_eq!(
        response.status,
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        response.text
    );
    assert_eq!(response.code(), "VALIDATION_ERROR");
    assert!(!response.text.contains("typed into the wrong field"));
    assert!(stored(&env, "mail.password").await.is_none());
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn shortening_a_retention_period_needs_fresh_authentication_and_keeps_a_grace_value() {
    let env = TestEnv::new().await;
    let owner = env.login_ok("op").await;
    let app = env.router_with_services(services(&env, Some([0x5a; 32])));
    env.clock.advance(std::time::Duration::from_secs(6 * 60));

    // Shortening from the 30-day default is refused with a stale login.
    let stale = patch_settings(
        &app,
        &owner,
        json!({"settings":{"retention.trash_days":{"value":7}}}),
    )
    .await;
    assert_eq!(stale.status, axum::http::StatusCode::FORBIDDEN);
    assert_eq!(stale.code(), "FRESH_AUTH_REQUIRED");
    assert!(stored(&env, "retention.trash_days").await.is_none());
    // Lengthening and equal values do not need it.
    for days in [30, 90] {
        let longer = patch_settings(
            &app,
            &owner,
            json!({"settings":{"retention.trash_days":{"value":days}}}),
        )
        .await;
        assert_eq!(longer.status, axum::http::StatusCode::OK, "{}", longer.text);
    }
    assert_eq!(
        stored(&env, "retention.trash_days").await,
        Some(json!({"value":90}))
    );
    let shorter = patch_settings(
        &app,
        &owner,
        json!({"settings":{"retention.trash_days":{"value":60}}}),
    )
    .await;
    assert_eq!(shorter.status, axum::http::StatusCode::FORBIDDEN);
    assert_eq!(shorter.code(), "FRESH_AUTH_REQUIRED");

    // With a fresh login the shortening is stored together with the old period.
    let fresh = env.login_ok("op").await;
    let accepted = patch_settings(
        &app,
        &fresh,
        json!({"settings":{"retention.trash_days":{"value":14}}}),
    )
    .await;
    assert_eq!(
        accepted.status,
        axum::http::StatusCode::OK,
        "{}",
        accepted.text
    );
    assert_eq!(
        stored(&env, "retention.trash_days").await,
        Some(json!({"value":14,"previous":90}))
    );
    // A second shortening inside the grace period keeps the longest period.
    let again = patch_settings(
        &app,
        &fresh,
        json!({"settings":{"retention.trash_days":{"value":7}}}),
    )
    .await;
    assert_eq!(again.status, axum::http::StatusCode::OK);
    assert_eq!(
        stored(&env, "retention.trash_days").await,
        Some(json!({"value":7,"previous":90}))
    );
    let read = call(
        &app,
        Req::new("GET", "/api/v1/admin/instance-settings").session(&fresh),
    )
    .await;
    assert_eq!(read.body["settings"]["retention.trash_days"]["value"], 7);
}
