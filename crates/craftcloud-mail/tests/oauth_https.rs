#![cfg(feature = "test-util")]

use craftcloud_mail::{
    GraphConfig, GraphTransport, MailMessage, MailTransport, OAuthConfig, OAuthCredential,
    TransportError, TransportStatus,
};

#[tokio::test]
#[ignore = "requires the isolated OAuth HTTPS server from oauth-container.py"]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn graph_oauth_https_protocol_cases() {
    let case = std::env::var("CRAFTCLOUD_OAUTH_TEST_CASE").expect("OAuth case");
    let secret_path = std::env::var("CRAFTCLOUD_OAUTH_TEST_SECRET").expect("secret path");
    let credential = if case.starts_with("certificate_") {
        OAuthCredential::CertificateAndKeyFiles {
            certificate_file: std::env::var("CRAFTCLOUD_OAUTH_TEST_CERT")
                .expect("certificate path"),
            key_file: std::env::var("CRAFTCLOUD_OAUTH_TEST_KEY").expect("private key path"),
        }
    } else {
        OAuthCredential::ClientSecretFile(secret_path)
    };
    let config = GraphConfig {
        oauth: OAuthConfig {
            tenant_id: "12345678-1234-1234-1234-123456789012".into(),
            client_id: "synthetic-client".into(),
            credential,
        },
        sender: "sender@example.invalid".into(),
        save_to_sent: false,
    };
    let status = TransportStatus::default();
    let ca_path = std::env::var("CRAFTCLOUD_OAUTH_TEST_CA").expect("CA path");
    let root_ca = std::fs::read(ca_path).expect("test CA");
    let token_url = std::env::var("CRAFTCLOUD_OAUTH_TEST_TOKEN_URL").expect("token URL");
    let graph_url = std::env::var("CRAFTCLOUD_OAUTH_TEST_GRAPH_URL").expect("Graph URL");
    let transport = GraphTransport::with_test_endpoints(
        config,
        status.clone(),
        &token_url,
        &graph_url,
        &root_ca,
    );
    if case == "certificate_mismatch" || case == "certificate_expired" {
        assert!(transport.is_err());
        return;
    }
    let transport = transport.expect("transport");
    let message = || MailMessage {
        recipient: "recipient@example.invalid".into(),
        subject: "Synthetic OAuth test".into(),
        text: "synthetic".into(),
        html: "<p>synthetic</p>".into(),
        message_id: "<oauth-test@craftcloud.invalid>".into(),
    };
    let result = transport.send(message()).await;
    match case.as_str() {
        "success" | "certificate_success" => {
            assert!(result.is_ok());
            if case == "success" {
                assert!(transport.send(message()).await.is_ok());
            }
        }
        "401" => assert!(result.is_ok()),
        "429" => {
            assert!(matches!(
                &result,
                Err(TransportError::Provider {
                    code: "provider_rate_limited",
                    retry_after: Some(_),
                    ..
                })
            ));
            assert_eq!(
                result.err().and_then(|error| error.retry_after()),
                Some(std::time::Duration::from_secs(12))
            );
        }
        "403" => assert!(matches!(
            result,
            Err(TransportError::Provider {
                code: "ErrorAccessDenied",
                ..
            })
        )),
        "404" => assert!(matches!(
            result,
            Err(TransportError::Provider {
                permanent: true,
                ..
            })
        )),
        "unknown" => {
            assert!(result.is_err());
            let snapshot = status.snapshot();
            assert!(!format!("{snapshot:?}").contains("synthetic-secret-canary"));
        }
        "redirect" => assert!(result.is_err()),
        "oversized" => assert!(matches!(
            result,
            Err(TransportError::Provider {
                code: "provider_response_too_large",
                ..
            })
        )),
        "interrupted" => assert_eq!(result, Err(TransportError::Transient)),
        "token_429" | "token_503" => {
            let error = result.expect_err("temporary token endpoint failure");
            assert!(!error.is_permanent());
            assert_eq!(
                error.retry_after(),
                Some(std::time::Duration::from_secs(17))
            );
        }
        "untrusted_ca" => assert!(result.is_err()),
        "expiry" => {
            assert!(result.is_ok());
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            assert!(transport.send(message()).await.is_ok());
        }
        _ => assert_eq!(case, "known OAuth case"),
    }
}
