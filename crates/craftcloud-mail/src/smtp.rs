use std::fmt;
use std::time::Duration;

use lettre::message::{Mailbox, MultiPart, SinglePart, header::ContentType};
use lettre::transport::smtp::authentication::Credentials;
#[cfg(feature = "test-util")]
use lettre::transport::smtp::client::{Certificate, Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use zeroize::Zeroizing;

/// SMTP transport TLS mode. Both choices require certificate validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmtpTls {
    StartTls,
    ImplicitTls,
}

/// SMTP settings; secret is held in memory and omitted from Debug output.
#[derive(Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub tls: SmtpTls,
    pub username: Option<String>,
    password: Option<Zeroizing<String>>,
    pub from: String,
    pub timeout: Duration,
    #[cfg(feature = "test-util")]
    test_root_certificate_pem: Option<Vec<u8>>,
}

impl SmtpConfig {
    pub fn new(
        host: String,
        port: u16,
        tls: SmtpTls,
        username: Option<String>,
        password: Option<String>,
        from: String,
    ) -> Self {
        Self {
            host,
            port,
            tls,
            username,
            password: password.map(Zeroizing::new),
            from,
            timeout: Duration::from_secs(15),
            #[cfg(feature = "test-util")]
            test_root_certificate_pem: None,
        }
    }

    /// Adds a custom trust root for isolated TLS integration tests.
    #[cfg(feature = "test-util")]
    pub fn with_test_root_certificate_pem(mut self, pem: Vec<u8>) -> Self {
        self.test_root_certificate_pem = Some(pem);
        self
    }
}

impl fmt::Debug for SmtpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmtpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("tls", &self.tls)
            .field("username", &self.username.as_ref().map(|_| "[redacted]"))
            .field("password", &"[redacted]")
            .field("from", &"[redacted]")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delivery;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    Transient,
    Permanent,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Transient => "temporary mail delivery failure",
            Self::Permanent => "permanent mail delivery failure",
        })
    }
}

impl std::error::Error for TransportError {}

pub struct MailMessage {
    pub recipient: String,
    pub subject: String,
    pub text: String,
    pub html: String,
    pub message_id: String,
}

pub trait MailTransport: Send + Sync {
    fn send(
        &self,
        message: MailMessage,
    ) -> impl std::future::Future<Output = Result<Delivery, TransportError>> + Send;
}

pub struct SmtpTransport {
    from: Mailbox,
    client: AsyncSmtpTransport<Tokio1Executor>,
}

impl SmtpTransport {
    pub fn new(config: &SmtpConfig) -> Result<Self, TransportError> {
        let from = config
            .from
            .parse::<Mailbox>()
            .map_err(|_| TransportError::Permanent)?;
        let mut builder = match config.tls {
            SmtpTls::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host),
            SmtpTls::ImplicitTls => AsyncSmtpTransport::<Tokio1Executor>::relay(&config.host),
        }
        .map_err(|_| TransportError::Permanent)?
        .port(config.port)
        .timeout(Some(config.timeout));
        #[cfg(feature = "test-util")]
        if let Some(pem) = &config.test_root_certificate_pem {
            let certificate = Certificate::from_pem(pem).map_err(|_| TransportError::Permanent)?;
            let tls = TlsParameters::builder(config.host.clone())
                .add_root_certificate(certificate)
                .build_rustls()
                .map_err(|_| TransportError::Permanent)?;
            builder = builder.tls(match config.tls {
                SmtpTls::StartTls => Tls::Required(tls),
                SmtpTls::ImplicitTls => Tls::Wrapper(tls),
            });
        }
        if let (Some(username), Some(password)) = (&config.username, &config.password) {
            builder = builder.credentials(Credentials::new(username.clone(), password.to_string()));
        }
        Ok(Self {
            from,
            client: builder.build(),
        })
    }
}

impl MailTransport for SmtpTransport {
    async fn send(&self, message: MailMessage) -> Result<Delivery, TransportError> {
        let recipient = message
            .recipient
            .parse()
            .map_err(|_| TransportError::Permanent)?;
        let content = Message::builder()
            .from(self.from.clone())
            .to(recipient)
            .subject(message.subject)
            .message_id(Some(message.message_id))
            .multipart(
                MultiPart::alternative()
                    .singlepart(
                        SinglePart::builder()
                            .header(ContentType::TEXT_PLAIN)
                            .body(message.text),
                    )
                    .singlepart(
                        SinglePart::builder()
                            .header(ContentType::TEXT_HTML)
                            .body(message.html),
                    ),
            )
            .map_err(|_| TransportError::Permanent)?;
        self.client
            .send(content)
            .await
            .map(|_| Delivery)
            .map_err(|error| {
                if error.is_permanent() {
                    TransportError::Permanent
                } else {
                    TransportError::Transient
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_redacts_relay_credentials_and_addresses() {
        let config = SmtpConfig::new(
            "smtp.internal".to_owned(),
            587,
            SmtpTls::StartTls,
            Some("private-user".to_owned()),
            Some("private-password".to_owned()),
            "Private Name <private@example.invalid>".to_owned(),
        );
        let debug = format!("{config:?}");
        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains("private-user"));
        assert!(!debug.contains("private-password"));
        assert!(!debug.contains("private@example.invalid"));
    }
}
