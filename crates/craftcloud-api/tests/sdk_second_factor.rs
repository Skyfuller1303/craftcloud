//! AC-02 and AC-04 together: the client SDK completes a device login that needs a
//! second factor, against the real router and database.
//!
//! These tests need PostgreSQL and run through `cargo xtask db-test`.

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use axum::{Router, body::Body, extract::ConnectInfo, http::StatusCode};
    use craftcloud_api::{ApiConfig, Clock, Services, router_with_shared};
    use craftcloud_auth::base32_decode;
    use craftcloud_client::{CloudClient, CloudError, CloudTransport, Proof, Request, Response};
    use craftcloud_core::Capabilities;
    use hmac::{Hmac, Mac};
    use serde_json::json;
    use sha1::Sha1;
    use tower::ServiceExt;

    use crate::common::{PASSWORD, Req, TestEnv, call, settings};

    const KEY: [u8; 32] = [73; 32];
    type HmacSha1 = Hmac<Sha1>;

    /// Runs SDK requests through the in-process router as if a trusted proxy had
    /// terminated TLS in front of it.
    struct RouterTransport {
        router: Router,
        runtime: tokio::runtime::Handle,
    }
    impl CloudTransport for RouterTransport {
        fn send(&self, request: Request) -> Result<Response, CloudError> {
            let mut builder = axum::http::Request::builder()
                .method(request.method.as_str())
                .uri(request.path.as_str())
                .header("x-forwarded-proto", "https");
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            let mut http = builder
                .body(Body::from(request.body))
                .map_err(|_| CloudError::InvalidUrl)?;
            http.extensions_mut()
                .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 443))));
            let router = self.router.clone();
            self.runtime.block_on(async move {
                let response = match router.oneshot(http).await {
                    Ok(response) => response,
                    Err(never) => match never {},
                };
                let status = response.status().as_u16();
                let headers = response
                    .headers()
                    .iter()
                    .filter_map(|(name, value)| {
                        Some((name.to_string(), value.to_str().ok()?.to_owned()))
                    })
                    .collect();
                let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                    .await
                    .map_err(|_| CloudError::InvalidResponse)?
                    .to_vec();
                Ok(Response {
                    status,
                    headers,
                    body,
                })
            })
        }
    }

    fn otp(secret: &str, unix_seconds: i64) -> String {
        let key = base32_decode(secret).unwrap();
        let step = (unix_seconds / 30) as u64;
        let mut mac = HmacSha1::new_from_slice(&key).unwrap();
        mac.update(&step.to_be_bytes());
        let digest = mac.finalize().into_bytes();
        let offset = (digest[19] & 0x0f) as usize;
        let value =
            u32::from_be_bytes(digest[offset..offset + 4].try_into().unwrap()) & 0x7fff_ffff;
        format!("{:06}", value % 1_000_000)
    }

    struct Enabled {
        secret: String,
        recovery_codes: Vec<String>,
    }

    async fn enable_two_factor(env: &TestEnv, router: &Router, name: &str) -> Enabled {
        env.user(name).await;
        let session = env.login_ok(name).await;
        let setup = call(
            router,
            Req::new("POST", "/api/v1/auth/2fa/setup")
                .session(&session)
                .json(json!({"password":PASSWORD})),
        )
        .await;
        assert_eq!(setup.status, StatusCode::OK, "{}", setup.text);
        let secret = setup.body["secret"].as_str().unwrap().to_owned();
        let confirmed = call(
            router,
            Req::new("POST", "/api/v1/auth/2fa/confirm")
                .session(&session)
                .json(json!({"code":otp(&secret, env.clock.now().unix_timestamp())})),
        )
        .await;
        assert_eq!(confirmed.status, StatusCode::OK, "{}", confirmed.text);
        let recovery_codes = confirmed.body["recovery_codes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|code| code.as_str().unwrap().to_owned())
            .collect();
        // The enrollment consumed this TOTP step; the logins below need a later one.
        env.clock.advance(std::time::Duration::from_secs(30));
        Enabled {
            secret,
            recovery_codes,
        }
    }

    async fn router(env: &TestEnv) -> Router {
        let services = Services::new(env.pool.clone(), None, settings())
            .with_clock(env.clock.clone())
            .with_two_factor_keys(Some(KEY), None);
        router_with_shared(
            Capabilities::development_defaults(),
            &ApiConfig {
                trusted_proxies: vec!["127.0.0.1/32".parse().unwrap()],
                ..ApiConfig::default()
            },
            Some(Arc::new(services)),
        )
    }

    fn sdk_client(router: Router) -> CloudClient<RouterTransport> {
        CloudClient::new(
            "https://cloud.test",
            RouterTransport {
                router,
                runtime: tokio::runtime::Handle::current(),
            },
        )
        .unwrap()
    }

    fn first_factor(client: &CloudClient<RouterTransport>, name: &str) -> String {
        match client.login(
            &format!("{name}@example.test"),
            PASSWORD,
            "app",
            "sdk-second-factor-test",
            "SDK second factor test",
        ) {
            Err(CloudError::SecondFactorChallenge(id)) => id,
            other => panic!("expected a second-factor challenge, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_sdk_completes_a_device_login_with_a_totp_code() {
        let env = TestEnv::new().await;
        let router = router(&env).await;
        let enabled = enable_two_factor(&env, &router, "sdk-totp").await;
        let client = sdk_client(router);
        let code = otp(&enabled.secret, env.clock.now().unix_timestamp());
        let signed_in = tokio::task::spawn_blocking(move || {
            let challenge = first_factor(&client, "sdk-totp");
            assert!(!client.is_authenticated());
            // A wrong code is refused and leaves the client signed out.
            assert_eq!(
                client.complete_second_factor(&challenge, Proof::totp("abcdef")),
                Err(CloudError::Unauthorized)
            );
            assert!(!client.is_authenticated());
            client
                .complete_second_factor(&challenge, Proof::totp(code))
                .unwrap();
            assert!(client.is_authenticated());
            // The new device session really works.
            let user = client.current_user().unwrap();
            assert_eq!(user.email, "sdk-totp@example.test");
            client
        })
        .await
        .unwrap();
        // The challenge was consumed: it cannot create a second device session.
        let replay = tokio::task::spawn_blocking(move || {
            signed_in.complete_second_factor("not-the-challenge", Proof::totp("000000"))
        })
        .await
        .unwrap();
        assert_eq!(replay, Err(CloudError::Unauthorized));
        let sessions: i64 =
            sqlx::query_scalar("SELECT count(*) FROM device_sessions WHERE revoked_at IS NULL")
                .fetch_one(&env.pool)
                .await
                .unwrap();
        assert_eq!(sessions, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_sdk_completes_a_device_login_with_a_recovery_code_exactly_once() {
        let env = TestEnv::new().await;
        let router = router(&env).await;
        let enabled = enable_two_factor(&env, &router, "sdk-recovery").await;
        let recovery = enabled.recovery_codes[0].clone();
        let client = sdk_client(router.clone());
        let used = recovery.clone();
        tokio::task::spawn_blocking(move || {
            let challenge = first_factor(&client, "sdk-recovery");
            client
                .complete_second_factor(&challenge, Proof::recovery(used))
                .unwrap();
            assert!(client.is_authenticated());
        })
        .await
        .unwrap();
        // The same recovery code is spent.
        let second = sdk_client(router);
        let result = tokio::task::spawn_blocking(move || {
            let challenge = first_factor(&second, "sdk-recovery");
            second.complete_second_factor(&challenge, Proof::recovery(recovery))
        })
        .await
        .unwrap();
        assert_eq!(result, Err(CloudError::Unauthorized));
    }
}
