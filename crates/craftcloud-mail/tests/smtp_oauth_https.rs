#![cfg(feature = "test-util")]

use craftcloud_mail::{
    MailMessage, MailTransport, OAuthConfig, OAuthCredential, OAuthProvider, SmtpConfig,
    SmtpOAuthConfig, SmtpOAuthTransport, SmtpTls, TransportError, TransportStatus,
};

#[tokio::test]
#[ignore = "requires the isolated SMTP OAuth servers from smtp-oauth-container.py"]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn smtp_oauth_tls_protocol_cases() {
    let case = std::env::var("CRAFTCLOUD_SMTP_OAUTH_CASE").expect("SMTP OAuth case");
    let ca_path = std::env::var("CRAFTCLOUD_SMTP_OAUTH_CA").expect("SMTP OAuth CA path");
    let root_ca = std::fs::read(ca_path).expect("SMTP OAuth CA");
    let smtp_ca =
        std::fs::read(std::env::var("CRAFTCLOUD_SMTP_OAUTH_SMTP_CA").expect("SMTP server CA path"))
            .expect("SMTP server CA");
    let token_url = std::env::var("CRAFTCLOUD_SMTP_OAUTH_TOKEN_URL").expect("token URL");
    let host = "127.0.0.1".to_owned();
    let port = std::env::var("CRAFTCLOUD_SMTP_OAUTH_SMTP_PORT")
        .expect("SMTP port")
        .parse()
        .expect("SMTP port number");
    let sender = "sender@example.invalid";
    let configured_sender = if case == "wrong_sender" {
        "other@example.invalid"
    } else {
        sender
    };
    let smtp = SmtpConfig::new(
        host,
        port,
        SmtpTls::StartTls,
        Some(sender.to_owned()),
        None,
        format!("CraftCloud <{configured_sender}>"),
    )
    .with_test_root_certificate_pem(smtp_ca);
    let provider = if case == "google" {
        OAuthProvider::Google {
            service_account_file: std::env::var("CRAFTCLOUD_SMTP_OAUTH_SERVICE_ACCOUNT")
                .expect("service account path"),
            sender: sender.to_owned(),
        }
    } else {
        OAuthProvider::Microsoft(OAuthConfig {
            tenant_id: "12345678-1234-1234-1234-123456789012".to_owned(),
            client_id: "synthetic-client".to_owned(),
            credential: OAuthCredential::ClientSecretFile(
                std::env::var("CRAFTCLOUD_SMTP_OAUTH_SECRET").expect("secret path"),
            ),
        })
    };
    let status = TransportStatus::default();
    let transport = SmtpOAuthTransport::with_test_endpoints(
        SmtpOAuthConfig { smtp, provider },
        status,
        &token_url,
        &root_ca,
    );
    let message = || MailMessage {
        recipient: "recipient@example.invalid".to_owned(),
        subject: "Synthetic SMTP OAuth test".to_owned(),
        text: "synthetic".to_owned(),
        html: "<p>synthetic</p>".to_owned(),
        message_id: "<smtp-oauth-test@craftcloud.invalid>".to_owned(),
    };

    if case == "wrong_sender" {
        assert!(matches!(transport, Err(TransportError::Permanent)));
        return;
    }
    let transport = transport.expect("SMTP OAuth transport");
    match case.as_str() {
        "success" | "google" => assert!(transport.send(message()).await.is_ok()),
        "expired" => {
            assert!(transport.send(message()).await.is_ok());
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            assert!(transport.send(message()).await.is_ok());
        }
        "535_refresh" | "334_refresh" => assert!(transport.send(message()).await.is_ok()),
        "permanent_535" | "334_permanent" => assert_eq!(
            transport.send(message()).await,
            Err(TransportError::Authentication)
        ),
        "untrusted_ca" => assert!(transport.send(message()).await.is_err()),
        _ => assert_eq!(case, "known SMTP OAuth case"),
    }
}
