//! Router-level OIDC interoperability tests using the synthetic Docker provider.

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use axum::{
        Router,
        http::{StatusCode, header},
    };
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use craftcloud_core::Capabilities;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    use super::common::{Req, Session, TestEnv, call, settings};

    const IDP_IMAGE: &str = "python:3.14.0-alpine3.22@sha256:8373231e1e906ddfb457748bfc032c4c06ada8c759b7b62d9c73ec2a3c56e710";
    const TEST_ORIGIN: &str = "http://127.0.0.1:31000";

    struct IdpContainer {
        name: String,
        base_url: String,
    }

    impl IdpContainer {
        fn start() -> Self {
            let name = format!(
                "craftcloud-ac10-idp-{}-{}",
                std::process::id(),
                Uuid::new_v4().simple()
            );
            let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("oidc_idp.py")
                .canonicalize()
                .expect("OIDC fixture exists");
            let source = fixture.to_string_lossy();
            let source = source.strip_prefix(r"\\?\").unwrap_or(&source);
            let mount = format!("type=bind,source={source},target=/app/provider.py,readonly");
            let output = Command::new("docker")
                .args([
                    "run",
                    "--rm",
                    "--detach",
                    "--name",
                    &name,
                    "--publish",
                    "127.0.0.1::8081",
                    "--memory",
                    "128m",
                    "--cpus",
                    "1",
                    "--mount",
                    &mount,
                    IDP_IMAGE,
                    "python",
                    "-u",
                    "/app/provider.py",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .expect("Docker must be installed");
            assert!(
                output.status.success(),
                "docker run failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );

            let mut container = Self {
                name,
                base_url: String::new(),
            };
            let port = Command::new("docker")
                .args(["port", &container.name, "8081/tcp"])
                .output()
                .expect("docker port failed");
            assert!(
                port.status.success(),
                "docker port failed: {}",
                String::from_utf8_lossy(&port.stderr)
            );
            let host_port = String::from_utf8_lossy(&port.stdout)
                .trim()
                .rsplit_once(':')
                .map(|(_, port)| port)
                .and_then(|port| port.parse::<u16>().ok())
                .expect("Docker returned a mapped host port");
            container.base_url = format!("http://127.0.0.1:{host_port}");
            container.wait_ready();
            container
        }

        fn wait_ready(&self) {
            for _ in 0..200 {
                if Command::new("docker")
                    .args(["exec", &self.name, "python", "-c", "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8081/stats', timeout=1)"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success())
                {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            panic!("OIDC provider container did not become ready");
        }
    }

    impl Drop for IdpContainer {
        fn drop(&mut self) {
            let _ = Command::new("docker")
                .args(["rm", "--force", &self.name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    struct FlowResult {
        callback: super::common::Resp,
        callback_uri: String,
        cookie: String,
    }

    async fn control(client: &reqwest::Client, base: &str, value: Value) {
        let response = client
            .post(format!("{base}/control"))
            .json(&value)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }

    async fn run_flow_inner(
        app: &Router,
        client: &reqwest::Client,
        org: Uuid,
        desktop: Option<Value>,
        rotate_after_start: bool,
    ) -> FlowResult {
        let mut body = json!({"organization_id":org});
        if let Some(desktop) = desktop {
            body["desktop"] = desktop;
        }
        let started = call(
            app,
            Req::new("POST", "/api/v1/auth/oidc/start")
                .origin(Some(TEST_ORIGIN))
                .json(body),
        )
        .await;
        assert_eq!(started.status, StatusCode::OK, "{}", started.text);
        let flow = started.body["authorization_url"]
            .as_str()
            .unwrap()
            .split("flow=")
            .nth(1)
            .unwrap()
            .to_owned();

        let launch = call(
            app,
            Req::new("GET", &format!("/api/v1/auth/oidc/authorize?flow={flow}")),
        )
        .await;
        assert_eq!(launch.status, StatusCode::SEE_OTHER, "{}", launch.text);
        let cookie = launch.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        if rotate_after_start {
            let provider =
                url::Url::parse(launch.headers[header::LOCATION].to_str().unwrap()).unwrap();
            control(
                client,
                &provider.origin().ascii_serialization(),
                json!({"rotate":true}),
            )
            .await;
        }
        let response = client
            .get(launch.headers[header::LOCATION].to_str().unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        let callback_url =
            url::Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
        let callback_uri = format!("{}?{}", callback_url.path(), callback_url.query().unwrap());
        let callback = call(
            app,
            Req::new("GET", &callback_uri).header("cookie", &cookie),
        )
        .await;
        FlowResult {
            callback,
            callback_uri,
            cookie,
        }
    }

    async fn run_flow(
        app: &Router,
        client: &reqwest::Client,
        org: Uuid,
        desktop: Option<Value>,
    ) -> FlowResult {
        run_flow_inner(app, client, org, desktop, false).await
    }

    async fn finish(
        app: &Router,
        flow: &FlowResult,
        password: Option<&str>,
    ) -> super::common::Resp {
        let pending_id = flow.callback.headers[header::LOCATION]
            .to_str()
            .unwrap()
            .strip_prefix("/admin/#oidc=")
            .unwrap();
        call(
            app,
            Req::new("POST", "/api/v1/auth/oidc/finish")
                .origin(Some(TEST_ORIGIN))
                .header("cookie", &flow.cookie)
                .json(json!({"pending_id":pending_id,"password":password})),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn save_sso(
        app: &Router,
        org: Uuid,
        operator: &Session,
        discovery_url: &str,
        version: i64,
        trust_mfa: bool,
        sso_required: bool,
    ) -> super::common::Resp {
        call(
            app,
            Req::new("PUT", &format!("/api/v1/organizations/{org}/sso"))
                .origin(Some(TEST_ORIGIN))
                .header("cookie", &format!("craftcloud_session={}", operator.token))
                .header("x-csrf-token", &operator.csrf)
                .json(json!({
                    "discovery_url": discovery_url,
                    "client_id":"craftcloud-test",
                    "allowed_domains":["example.test"],
                    "enabled":true,
                    "jit_enabled":true,
                    "sso_required":sso_required,
                    "trust_mfa":trust_mfa,
                    "version":version
                })),
        )
        .await
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL and Docker; run cargo xtask db-test"]
    async fn browser_oidc_api_flow_validates_tokens_rotation_and_replay() {
        assert_eq!(std::env::var("CRAFTCLOUD_TEST_OIDC_LOOPBACK").unwrap(), "1");
        let provider = IdpContainer::start();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        let mut auth = settings();
        auth.public_url = Some(TEST_ORIGIN.to_owned());
        auth.public_origin = Some(TEST_ORIGIN.to_owned());
        auth.cookie_secure = false;
        let env = TestEnv::with(auth.clone(), crate::ApiConfig::default()).await;
        let services = std::sync::Arc::new(
            crate::Services::new(env.pool.clone(), None, auth)
                .with_clock(env.clock.clone())
                .with_metrics(std::sync::Arc::clone(&env.metrics))
                .with_two_factor_keys(Some([0x42; 32]), None),
        );
        let app = crate::router_with_shared(
            Capabilities::development_defaults(),
            &crate::ApiConfig::default(),
            Some(services),
        );

        let login = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .origin(Some(TEST_ORIGIN))
                .json(json!({"email":"op@example.test","password":super::common::PASSWORD})),
        )
        .await;
        assert_eq!(login.status, StatusCode::OK, "{}", login.text);
        let operator_cookie = login.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .strip_prefix("craftcloud_session=")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let operator = Session {
            token: operator_cookie,
            csrf: login.body["csrf_token"].as_str().unwrap().to_owned(),
        };
        let org: Uuid =
            sqlx::query_scalar("SELECT id FROM organizations WHERE slug='operator-org'")
                .fetch_one(&env.pool)
                .await
                .unwrap();
        let configured = call(&app, Req::new("PUT", &format!("/api/v1/organizations/{org}/sso"))
            .origin(Some(TEST_ORIGIN))
            .header("cookie", &format!("craftcloud_session={}", operator.token))
            .header("x-csrf-token", &operator.csrf)
            .json(json!({
                "discovery_url": format!("{}/realm/.well-known/openid-configuration", provider.base_url),
                "client_id":"craftcloud-test", "client_secret":"synthetic-idp-secret",
                "allowed_domains":["example.test"], "enabled":true, "jit_enabled":true,
                "sso_required":false, "trust_mfa":true, "version":null
            }))).await;
        assert_eq!(configured.status, StatusCode::OK, "{}", configured.text);

        for mode in [
            "wrong_issuer",
            "wrong_audience",
            "expired",
            "wrong_nonce",
            "future_iat",
            "wrong_signature",
        ] {
            control(&client, &provider.base_url, json!({"mode":mode})).await;
            let flow = run_flow(&app, &client, org, None).await;
            assert_eq!(
                flow.callback.status,
                StatusCode::BAD_REQUEST,
                "mode={mode}: {}",
                flow.callback.text
            );
        }

        for mode in ["unverified", "foreign_domain"] {
            control(&client, &provider.base_url, json!({"mode":mode})).await;
            let flow = run_flow(&app, &client, org, None).await;
            assert_eq!(
                flow.callback.status,
                StatusCode::SEE_OTHER,
                "mode={mode}: {}",
                flow.callback.text
            );
            let response = finish(&app, &flow, None).await;
            assert_eq!(
                response.status,
                StatusCode::FORBIDDEN,
                "mode={mode}: {}",
                response.text
            );
            assert_eq!(response.code(), "SSO_IDENTITY_REJECTED");
        }

        control(&client, &provider.base_url, json!({"mode":"success"})).await;
        let flow = run_flow_inner(&app, &client, org, None, true).await;
        assert_eq!(
            flow.callback.status,
            StatusCode::SEE_OTHER,
            "{}",
            flow.callback.text
        );
        let completed = finish(&app, &flow, None).await;
        assert_eq!(completed.status, StatusCode::OK, "{}", completed.text);
        assert!(completed.headers.get(header::SET_COOKIE).is_some());

        let original_user: Uuid = sqlx::query_scalar(
            "SELECT user_id FROM external_identities WHERE org_id=$1 AND subject='employee-stable-subject'",
        )
        .bind(org)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        control(
            &client,
            &provider.base_url,
            json!({"mode":"success","email":"renamed@example.test"}),
        )
        .await;
        let changed_email = run_flow(&app, &client, org, None).await;
        assert_eq!(changed_email.callback.status, StatusCode::SEE_OTHER);
        let changed_login = finish(&app, &changed_email, None).await;
        assert_eq!(
            changed_login.status,
            StatusCode::OK,
            "{}",
            changed_login.text
        );
        let same_user: Uuid = sqlx::query_scalar(
            "SELECT user_id FROM external_identities WHERE org_id=$1 AND subject='employee-stable-subject'",
        )
        .bind(org)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(same_user, original_user);
        control(
            &client,
            &provider.base_url,
            json!({"subject":"reassigned-subject","email":"employee@example.test"}),
        )
        .await;
        let reassigned = run_flow(&app, &client, org, None).await;
        assert_eq!(
            finish(&app, &reassigned, None).await.status,
            StatusCode::FORBIDDEN
        );
        let stolen: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM external_identities WHERE subject='reassigned-subject')",
        )
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert!(!stolen, "email reassignment must not transfer an identity");

        let local_user = env.user("local").await;
        sqlx::query("UPDATE users SET email_verified_at=now() WHERE id=$1")
            .bind(local_user)
            .execute(&env.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'member','active')")
            .bind(org)
            .bind(local_user)
            .execute(&env.pool)
            .await
            .unwrap();
        control(
            &client,
            &provider.base_url,
            json!({"mode":"success","subject":"new-subject-for-local","email":"local@example.test"}),
        )
        .await;
        let local_link = run_flow(&app, &client, org, None).await;
        assert_eq!(local_link.callback.status, StatusCode::SEE_OTHER);
        let unproved = finish(&app, &local_link, None).await;
        assert_eq!(
            unproved.status,
            StatusCode::UNAUTHORIZED,
            "{}",
            unproved.text
        );
        assert_eq!(unproved.code(), "SSO_LINK_REQUIRED");
        let linked = finish(&app, &local_link, Some(super::common::PASSWORD)).await;
        assert_eq!(linked.status, StatusCode::OK, "{}", linked.text);
        let linked_user: Uuid = sqlx::query_scalar(
            "SELECT user_id FROM external_identities WHERE org_id=$1 AND subject='new-subject-for-local'",
        )
        .bind(org)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(linked_user, local_user);
        use crate::Clock;
        use hmac::{Hmac, Mac};
        let secret = [0x31; 20];
        let ciphertext =
            craftcloud_auth::encrypt_secret_for(&[0x42; 32], local_user, b"totp", &secret).unwrap();
        sqlx::query("INSERT INTO user_two_factor(user_id,secret_ciphertext,enabled_at,created_at) VALUES($1,$2,$3,$3)").bind(local_user).bind(ciphertext).bind(env.clock.now()).execute(&env.pool).await.unwrap();
        control(&client, &provider.base_url, json!({"mfa":false})).await;
        let totp_flow = run_flow(&app, &client, org, None).await;
        assert_eq!(
            finish(&app, &totp_flow, None).await.code(),
            "SSO_SECOND_FACTOR_REQUIRED"
        );
        let step = env.clock.now().unix_timestamp() / 30;
        let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&secret).unwrap();
        mac.update(&(step as u64).to_be_bytes());
        let digest = mac.finalize().into_bytes();
        let offset = (digest[19] & 15) as usize;
        let number =
            u32::from_be_bytes(digest[offset..offset + 4].try_into().unwrap()) & 0x7fff_ffff;
        let code = format!("{:06}", number % 1_000_000);
        let pending = totp_flow.callback.headers[header::LOCATION]
            .to_str()
            .unwrap()
            .strip_prefix("/admin/#oidc=")
            .unwrap();
        let proven = call(
            &app,
            Req::new("POST", "/api/v1/auth/oidc/finish")
                .origin(Some(TEST_ORIGIN))
                .header("cookie", &totp_flow.cookie)
                .json(json!({"pending_id":pending,"code":code})),
        )
        .await;
        assert_eq!(
            proven.status,
            StatusCode::OK,
            "local TOTP must complete SSO"
        );
        let replay_flow = run_flow(&app, &client, org, None).await;
        let pending = replay_flow.callback.headers[header::LOCATION]
            .to_str()
            .unwrap()
            .strip_prefix("/admin/#oidc=")
            .unwrap();
        let replay = call(
            &app,
            Req::new("POST", "/api/v1/auth/oidc/finish")
                .origin(Some(TEST_ORIGIN))
                .header("cookie", &replay_flow.cookie)
                .json(json!({"pending_id":pending,"code":code})),
        )
        .await;
        assert_eq!(replay.code(), "SSO_SECOND_FACTOR_REQUIRED");
        sqlx::query("UPDATE organizations SET require_two_factor=true WHERE id=$1")
            .bind(org)
            .execute(&env.pool)
            .await
            .unwrap();
        control(&client,&provider.base_url,json!({"subject":"provider-mfa-required","email":"provider-mfa@example.test","mfa":true})).await;
        let provider_mfa = run_flow(&app, &client, org, None).await;
        assert_eq!(
            finish(&app, &provider_mfa, None).await.status,
            StatusCode::OK,
            "signed provider MFA must satisfy the organization"
        );
        sqlx::query("UPDATE organizations SET require_two_factor=false WHERE id=$1")
            .bind(org)
            .execute(&env.pool)
            .await
            .unwrap();

        let verifier = "native-code-verifier-with-enough-entropy-1234567890";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        control(
            &client,
            &provider.base_url,
            json!({
                "mode":"success",
                "subject":"native-subject",
                "email":"native@example.test"
            }),
        )
        .await;
        let native = run_flow(
            &app,
            &client,
            org,
            Some(json!({
                "client_kind":"cli",
                "client_id":"craftcloud-test",
                "device_name":"OIDC test CLI",
                "redirect_uri":"http://127.0.0.1:43123/oidc/callback",
                "code_challenge":challenge,
                "state":"native-state-value-with-at-least-32-characters"
            })),
        )
        .await;
        assert_eq!(
            native.callback.status,
            StatusCode::SEE_OTHER,
            "{}",
            native.callback.text
        );
        let pending_id = native.callback.headers[header::LOCATION]
            .to_str()
            .unwrap()
            .strip_prefix("/admin/#oidc=")
            .unwrap();
        let native_finish = call(
            &app,
            Req::new("POST", "/api/v1/auth/oidc/finish")
                .origin(Some(TEST_ORIGIN))
                .header("cookie", &native.cookie)
                .json(json!({"pending_id":pending_id})),
        )
        .await;
        assert_eq!(
            native_finish.status,
            StatusCode::OK,
            "{}",
            native_finish.text
        );
        let redirect =
            url::Url::parse(native_finish.body["redirect_url"].as_str().unwrap()).unwrap();
        let state = redirect
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned();
        assert_eq!(state, "native-state-value-with-at-least-32-characters");
        let code = redirect
            .query_pairs()
            .find(|(key, _)| key == "code")
            .unwrap()
            .1
            .into_owned();
        let rejected = call(
            &app,
            Req::new("POST", "/api/v1/auth/oidc/device/exchange")
                .origin(Some(TEST_ORIGIN))
                .json(json!({
                    "code":code.clone(),
                    "code_verifier":"wrong-native-code-verifier-with-enough-length-123456"
                })),
        )
        .await;
        assert_eq!(
            rejected.status,
            StatusCode::BAD_REQUEST,
            "{}",
            rejected.text
        );
        let exchanged = call(
            &app,
            Req::new("POST", "/api/v1/auth/oidc/device/exchange")
                .origin(Some(TEST_ORIGIN))
                .json(json!({"code":code,"code_verifier":verifier})),
        )
        .await;
        assert_eq!(exchanged.status, StatusCode::OK, "{}", exchanged.text);
        assert!(exchanged.body["access_token"].as_str().is_some());
        let typed: craftcloud_client::DeviceLoginResponse =
            serde_json::from_value(exchanged.body.clone())
                .expect("real exchange must satisfy SDK contract");
        assert_eq!(typed.user.email, "native@example.test");
        let replay = call(
            &app,
            Req::new("POST", "/api/v1/auth/oidc/device/exchange")
                .origin(Some(TEST_ORIGIN))
                .json(json!({"code":code,"code_verifier":verifier})),
        )
        .await;
        assert_eq!(replay.status, StatusCode::BAD_REQUEST);

        let discovery_url = format!(
            "{}/realm/.well-known/openid-configuration",
            provider.base_url
        );
        let mut version = configured.body["version"].as_i64().unwrap();
        let saved = save_sso(&app, org, &operator, &discovery_url, version, false, false).await;
        assert_eq!(saved.status, StatusCode::OK, "{}", saved.text);
        version = saved.body["version"].as_i64().unwrap();
        control(&client, &provider.base_url, json!({
            "mode":"success", "subject":"mfa-untrusted", "email":"mfa-untrusted@example.test", "mfa":true
        })).await;
        let untrusted_mfa = run_flow(&app, &client, org, None).await;
        assert_eq!(untrusted_mfa.callback.status, StatusCode::SEE_OTHER);
        let untrusted_user = finish(&app, &untrusted_mfa, None).await;
        assert_eq!(
            untrusted_user.status,
            StatusCode::OK,
            "{}",
            untrusted_user.text
        );
        let untrusted_id: Uuid = sqlx::query_scalar(
            "SELECT user_id FROM external_identities WHERE org_id=$1 AND subject='mfa-untrusted'",
        )
        .bind(org)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        let untrusted_factors: (bool, bool) = sqlx::query_as(
            "SELECT mfa,provider_mfa FROM sso_session_bindings WHERE org_id=$1 AND user_id=$2 AND session_kind='browser'",
        ).bind(org).bind(untrusted_id).fetch_one(&env.pool).await.unwrap();
        assert_eq!(untrusted_factors, (false, false));

        let saved = save_sso(&app, org, &operator, &discovery_url, version, true, false).await;
        assert_eq!(saved.status, StatusCode::OK, "{}", saved.text);
        version = saved.body["version"].as_i64().unwrap();
        control(&client, &provider.base_url, json!({
            "mode":"success", "subject":"mfa-not-asserted", "email":"mfa-not-asserted@example.test", "mfa":false
        })).await;
        let no_mfa = run_flow(&app, &client, org, None).await;
        assert_eq!(no_mfa.callback.status, StatusCode::SEE_OTHER);
        let no_mfa_login = finish(&app, &no_mfa, None).await;
        assert_eq!(no_mfa_login.status, StatusCode::OK, "{}", no_mfa_login.text);
        let no_mfa_user: Uuid = sqlx::query_scalar(
            "SELECT user_id FROM external_identities WHERE org_id=$1 AND subject='mfa-not-asserted'",
        ).bind(org).fetch_one(&env.pool).await.unwrap();
        let no_mfa_factors: (bool, bool) = sqlx::query_as(
            "SELECT mfa,provider_mfa FROM sso_session_bindings WHERE org_id=$1 AND user_id=$2 AND session_kind='browser'",
        ).bind(org).bind(no_mfa_user).fetch_one(&env.pool).await.unwrap();
        assert_eq!(no_mfa_factors, (false, false));

        let saved = save_sso(&app, org, &operator, &discovery_url, version, false, false).await;
        assert_eq!(saved.status, StatusCode::OK, "{}", saved.text);
        version = saved.body["version"].as_i64().unwrap();
        sqlx::query("UPDATE organizations SET require_two_factor=true WHERE id=$1")
            .bind(org)
            .execute(&env.pool)
            .await
            .unwrap();
        control(&client, &provider.base_url, json!({
            "mode":"success", "subject":"mfa-setup-required", "email":"mfa-setup-required@example.test", "mfa":false
        })).await;
        let sessions_before: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
            .fetch_one(&env.pool)
            .await
            .unwrap();
        let setup_required = run_flow(&app, &client, org, None).await;
        assert_eq!(setup_required.callback.status, StatusCode::SEE_OTHER);
        let blocked_setup = finish(&app, &setup_required, None).await;
        assert_eq!(
            blocked_setup.status,
            StatusCode::FORBIDDEN,
            "{}",
            blocked_setup.text
        );
        assert_eq!(blocked_setup.code(), "SSO_TWO_FACTOR_SETUP_REQUIRED");
        let sessions_after: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
            .fetch_one(&env.pool)
            .await
            .unwrap();
        assert_eq!(sessions_after, sessions_before);
        sqlx::query("UPDATE organizations SET require_two_factor=false WHERE id=$1")
            .bind(org)
            .execute(&env.pool)
            .await
            .unwrap();

        let required_user = env.user("sso-required").await;
        sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'member','active')")
            .bind(org).bind(required_user).execute(&env.pool).await.unwrap();
        let required_login = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .origin(Some(TEST_ORIGIN))
                .json(
                    json!({"email":"sso-required@example.test","password":super::common::PASSWORD}),
                ),
        )
        .await;
        assert_eq!(
            required_login.status,
            StatusCode::OK,
            "{}",
            required_login.text
        );
        let required_cookie = required_login.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .strip_prefix("craftcloud_session=")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let saved = save_sso(&app, org, &operator, &discovery_url, version, true, true).await;
        assert_eq!(saved.status, StatusCode::OK, "{}", saved.text);
        version = saved.body["version"].as_i64().unwrap();
        let organization_uri = format!("/api/v1/organizations/{org}");
        let denied = call(
            &app,
            Req::new("GET", &organization_uri)
                .header("cookie", &format!("craftcloud_session={required_cookie}")),
        )
        .await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.text);
        assert_eq!(denied.code(), "SSO_REQUIRED");
        let saved = save_sso(&app, org, &operator, &discovery_url, version, true, false).await;
        assert_eq!(saved.status, StatusCode::OK, "{}", saved.text);
        let recovered = call(
            &app,
            Req::new("GET", &organization_uri)
                .header("cookie", &format!("craftcloud_session={required_cookie}")),
        )
        .await;
        assert_eq!(recovered.status, StatusCode::OK, "{}", recovered.text);

        // Recovery boundary regression: A preclaims an unused mailbox, operator
        // issues reset through the public API, and the victim consumes it.
        control(
            &client,
            &provider.base_url,
            json!({
                "mode":"success", "subject":"recovery-preclaim", "email":"victim@example.test"
            }),
        )
        .await;
        let b_owner = env.user("org-b-owner").await;
        let created_b = call(
            &app,
            Req::new("POST", "/api/v1/organizations")
                .origin(Some(TEST_ORIGIN))
                .header("cookie", &format!("craftcloud_session={}", operator.token))
                .header("x-csrf-token", &operator.csrf)
                .json(
                    json!({"name":"Independent B","slug":"independent-b","owner_user_id":b_owner}),
                ),
        )
        .await;
        assert_eq!(created_b.status, StatusCode::CREATED, "{}", created_b.text);
        let org_b = Uuid::parse_str(created_b.body["id"].as_str().unwrap()).unwrap();
        let b_owner_login = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .origin(Some(TEST_ORIGIN))
                .json(
                    json!({"email":"org-b-owner@example.test","password":super::common::PASSWORD}),
                ),
        )
        .await;
        assert_eq!(
            b_owner_login.status,
            StatusCode::OK,
            "{}",
            b_owner_login.text
        );
        let b_owner_session = Session {
            token: b_owner_login.headers[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .strip_prefix("craftcloud_session=")
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned(),
            csrf: b_owner_login.body["csrf_token"]
                .as_str()
                .unwrap()
                .to_owned(),
        };
        let invitation = call(
            &app,
            Req::new(
                "POST",
                &format!("/api/v1/organizations/{org_b}/invitations"),
            )
            .origin(Some(TEST_ORIGIN))
            .header(
                "cookie",
                &format!("craftcloud_session={}", b_owner_session.token),
            )
            .header("x-csrf-token", &b_owner_session.csrf)
            .json(json!({"email":"victim@example.test","org_role":"member"})),
        )
        .await;
        assert_eq!(
            invitation.status,
            StatusCode::CREATED,
            "{}",
            invitation.text
        );
        let invitation_url =
            url::Url::parse(invitation.body["invite_url"].as_str().unwrap()).unwrap();
        let invitation_token = invitation_url
            .fragment()
            .unwrap()
            .strip_prefix("token=")
            .unwrap()
            .to_owned();
        let preclaim = run_flow(&app, &client, org, None).await;
        assert_eq!(preclaim.callback.status, StatusCode::SEE_OTHER);
        let attacker_session = finish(&app, &preclaim, None).await;
        assert_eq!(
            attacker_session.status,
            StatusCode::OK,
            "{}",
            attacker_session.text
        );
        let victim: Uuid = sqlx::query_scalar(
            "SELECT user_id FROM external_identities WHERE issuer=$1 AND subject='recovery-preclaim'",
        )
        .bind(format!("{}/realm", provider.base_url))
        .fetch_one(&env.pool)
        .await
        .unwrap();
        let old_cookie = attacker_session.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let stale = run_flow(&app, &client, org, None).await;
        assert_eq!(stale.callback.status, StatusCode::SEE_OTHER);

        let issued = call(
            &app,
            Req::new(
                "POST",
                &format!("/api/v1/admin/users/{victim}/password-reset"),
            )
            .origin(Some(TEST_ORIGIN))
            .header("cookie", &format!("craftcloud_session={}", operator.token))
            .header("x-csrf-token", &operator.csrf),
        )
        .await;
        assert_eq!(issued.status, StatusCode::CREATED, "{}", issued.text);
        let reset_url = url::Url::parse(issued.body["reset_url"].as_str().unwrap()).unwrap();
        let reset_token = reset_url
            .fragment()
            .unwrap()
            .strip_prefix("token=")
            .unwrap();
        let reset = call(
            &app,
            Req::new("POST", "/api/v1/auth/password-reset")
                .origin(Some(TEST_ORIGIN))
                .json(json!({"token":reset_token,"new_password":"Victim-New-Passphrase-4729"})),
        )
        .await;
        assert_eq!(reset.status, StatusCode::NO_CONTENT, "{}", reset.text);
        assert_eq!(
            call(
                &app,
                Req::new("GET", "/api/v1/auth/session").header("cookie", &old_cookie)
            )
            .await
            .status,
            StatusCode::UNAUTHORIZED,
            "recovery must revoke the preclaim browser session"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM external_identities WHERE user_id=$1"
            )
            .bind(victim)
            .fetch_one(&env.pool)
            .await
            .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM sso_session_bindings WHERE user_id=$1"
            )
            .bind(victim)
            .fetch_one(&env.pool)
            .await
            .unwrap(),
            0
        );
        let victim_login = call(
            &app,
            Req::new("POST", "/api/v1/auth/login")
                .origin(Some(TEST_ORIGIN))
                .json(
                    json!({"email":"victim@example.test","password":"Victim-New-Passphrase-4729"}),
                ),
        )
        .await;
        assert_eq!(victim_login.status, StatusCode::OK, "{}", victim_login.text);
        let victim_cookie = victim_login.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let admitted = call(
            &app,
            Req::new("POST", "/api/v1/invitations/accept")
                .origin(Some(TEST_ORIGIN))
                .header("cookie", &victim_cookie)
                .header(
                    "x-csrf-token",
                    victim_login.body["csrf_token"].as_str().unwrap(),
                )
                .json(json!({"token":invitation_token})),
        )
        .await;
        assert_eq!(admitted.status, StatusCode::OK, "{}", admitted.text);
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT status FROM organization_members WHERE org_id=$1 AND user_id=$2"
            )
            .bind(org_b)
            .bind(victim)
            .fetch_one(&env.pool)
            .await
            .unwrap(),
            "active"
        );
        assert_eq!(finish(&app, &stale, None).await.code(), "SSO_LINK_REQUIRED");
        control(
            &client,
            &provider.base_url,
            json!({
                "mode":"success", "subject":"recovery-preclaim", "email":"victim@example.test"
            }),
        )
        .await;
        let attacker_retry = run_flow(&app, &client, org, None).await;
        assert_eq!(attacker_retry.callback.status, StatusCode::SEE_OTHER);
        let attacker_retry_result = finish(&app, &attacker_retry, None).await;
        assert_eq!(attacker_retry_result.code(), "SSO_LINK_REQUIRED");
        let devices_before: i64 =
            sqlx::query_scalar("SELECT count(*) FROM device_sessions WHERE user_id=$1")
                .bind(victim)
                .fetch_one(&env.pool)
                .await
                .unwrap();
        let verifier = "recovery-device-code-verifier-with-enough-entropy-123";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let attacker_device = run_flow(
            &app,
            &client,
            org,
            Some(json!({
                "client_kind":"cli", "client_id":"craftcloud-test", "device_name":"Recovery attacker",
                "redirect_uri":"http://127.0.0.1:43124/oidc/callback", "code_challenge":challenge,
                "state":"recovery-device-state-value-with-at-least-32-characters"
            })),
        ).await;
        assert_eq!(
            finish(&app, &attacker_device, None).await.code(),
            "SSO_LINK_REQUIRED"
        );
        let devices_after: i64 =
            sqlx::query_scalar("SELECT count(*) FROM device_sessions WHERE user_id=$1")
                .bind(victim)
                .fetch_one(&env.pool)
                .await
                .unwrap();
        assert_eq!(
            devices_after, devices_before,
            "recovery must not issue an unauthorized device session"
        );

        let stats_before_replay: Value = client
            .get(format!("{}/stats", provider.base_url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let replay = call(
            &app,
            Req::new("GET", &flow.callback_uri).header("cookie", &flow.cookie),
        )
        .await;
        assert_eq!(replay.status, StatusCode::BAD_REQUEST, "{}", replay.text);
        assert_eq!(replay.code(), "SSO_INVALID_FLOW");
        let stats_after_replay: Value = client
            .get(format!("{}/stats", provider.base_url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            stats_after_replay["token_calls"], stats_before_replay["token_calls"],
            "replaying callback must not contact token endpoint"
        );

        let stats: Value = client
            .get(format!("{}/stats", provider.base_url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(stats["token_calls"].as_u64().unwrap() >= 19);
        assert!(stats["jwks_calls"].as_u64().unwrap() >= 15);
    }
}

#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod common;
