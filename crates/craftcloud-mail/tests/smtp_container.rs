use craftcloud_mail::{
    MailMessage, MailTransport, SmtpConfig, SmtpTls, SmtpTransport, TransportError,
};

#[tokio::test]
#[ignore = "requires the isolated SMTP container from smtp-container.py"]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn starttls_delivery_validates_certificate_and_retries_transient_failure() {
    let host = std::env::var("CRAFTCLOUD_TEST_SMTP_HOST").expect("SMTP test host");
    let port = std::env::var("CRAFTCLOUD_TEST_SMTP_PORT")
        .expect("SMTP test port")
        .parse()
        .expect("SMTP test port number");
    let ca = std::fs::read(std::env::var("CRAFTCLOUD_TEST_SMTP_CA").expect("SMTP test CA path"))
        .expect("SMTP test CA");
    let config = SmtpConfig::new(
        host.clone(),
        port,
        SmtpTls::StartTls,
        None,
        None,
        "CraftCloud <sender@example.invalid>".to_owned(),
    )
    .with_test_root_certificate_pem(ca);
    let transport = SmtpTransport::new(&config).expect("test SMTP configuration");
    let message = || MailMessage {
        recipient: "recipient@example.invalid".to_owned(),
        subject: "CraftCloud SMTP integration".to_owned(),
        text: "synthetic integration message".to_owned(),
        html: "<!doctype html><p>synthetic integration message</p>".to_owned(),
        message_id: "<ac09-test@craftcloud.invalid>".to_owned(),
    };
    let untrusted = SmtpTransport::new(&SmtpConfig::new(
        host,
        port,
        SmtpTls::StartTls,
        None,
        None,
        "CraftCloud <sender@example.invalid>".to_owned(),
    ))
    .expect("SMTP configuration");
    assert!(
        untrusted.send(message()).await.is_err(),
        "untrusted server certificate must be rejected"
    );
    assert_eq!(
        transport.send(message()).await,
        Err(TransportError::Transient)
    );
    let retry = transport.send(message()).await;
    assert!(retry.is_ok(), "second delivery attempt failed: {retry:?}");
}
