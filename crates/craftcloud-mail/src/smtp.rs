use std::fmt;
use std::future::Future;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use lettre::message::{Mailbox, MultiPart, SinglePart, header::ContentType};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
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

    /// Creates SMTP settings with a secret already held in zeroizing memory.
    pub fn new_with_password(
        host: String,
        port: u16,
        tls: SmtpTls,
        username: Option<String>,
        password: Option<Zeroizing<String>>,
        from: String,
    ) -> Self {
        let mut config = Self::new(host, port, tls, username, None, from);
        config.password = password;
        config
    }

    /// Adds a custom trust root for isolated TLS integration tests.
    #[cfg(feature = "test-util")]
    pub fn with_test_root_certificate_pem(mut self, pem: Vec<u8>) -> Self {
        self.test_root_certificate_pem = Some(pem);
        self
    }

    pub fn with_xoauth2_token(
        &self,
        username: &str,
        token: &str,
    ) -> Result<AsyncSmtpTransport<Tokio1Executor>, TransportError> {
        let mut builder = match self.tls {
            SmtpTls::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.host),
            SmtpTls::ImplicitTls => AsyncSmtpTransport::<Tokio1Executor>::relay(&self.host),
        }
        .map_err(|_| TransportError::Permanent)?
        .port(self.port)
        .timeout(Some(self.timeout));
        #[cfg(feature = "test-util")]
        if let Some(pem) = &self.test_root_certificate_pem {
            let certificate = Certificate::from_pem(pem).map_err(|_| TransportError::Permanent)?;
            let tls = TlsParameters::builder(self.host.clone())
                .add_root_certificate(certificate)
                .build_rustls()
                .map_err(|_| TransportError::Permanent)?;
            builder = builder.tls(match self.tls {
                SmtpTls::StartTls => Tls::Required(tls),
                SmtpTls::ImplicitTls => Tls::Wrapper(tls),
            });
        }
        builder = builder
            .authentication(vec![Mechanism::Xoauth2])
            .credentials(Credentials::new(username.to_owned(), token.to_owned()));
        Ok(builder.build())
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
    Authentication,
    Provider {
        permanent: bool,
        code: &'static str,
        retry_after: Option<Duration>,
    },
}

impl TransportError {
    pub fn is_permanent(&self) -> bool {
        match self {
            Self::Permanent | Self::Authentication => true,
            Self::Provider { permanent, .. } => *permanent,
            Self::Transient => false,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Permanent => "permanent",
            Self::Authentication => "smtp_authentication_failed",
            Self::Provider { code, .. } => code,
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Provider { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Transient => "temporary mail delivery failure",
            Self::Permanent => "permanent mail delivery failure",
            Self::Authentication => "SMTP authentication failure",
            Self::Provider { .. } => "mail provider failure",
        })
    }
}

impl std::error::Error for TransportError {}

pub(crate) fn smtp_error(error: &lettre::transport::smtp::Error) -> TransportError {
    if error.status().is_some_and(|code| code.to_string() == "535") {
        TransportError::Authentication
    } else if error.is_permanent() {
        TransportError::Permanent
    } else {
        TransportError::Transient
    }
}

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

    /// Whether generic failures belong to an SMTP connection. The queue keeps
    /// the `smtp_permanent` and `smtp_transient` codes for those transports.
    fn uses_smtp(&self) -> bool {
        true
    }

    /// Whether the transport is currently configured and may consume queue entries.
    fn is_enabled(&self) -> bool {
        true
    }
}

/// Atomically replaceable mail transport. Requests already in flight retain
/// their original transport; later requests see the replacement.
#[derive(Default)]
pub struct SwitchableTransport {
    current: RwLock<Option<Arc<crate::ConfiguredTransport>>>,
}

impl SwitchableTransport {
    pub fn replace(&self, transport: Option<Arc<crate::ConfiguredTransport>>) {
        if let Ok(mut current) = self.current.write() {
            *current = transport;
        }
    }

    fn current(&self) -> Option<Arc<crate::ConfiguredTransport>> {
        self.current.read().ok()?.clone()
    }
}

impl MailTransport for SwitchableTransport {
    fn send(
        &self,
        message: MailMessage,
    ) -> impl Future<Output = Result<Delivery, TransportError>> + Send {
        let current = self.current();
        async move {
            match current {
                Some(transport) => transport.send(message).await,
                None => Err(TransportError::Transient),
            }
        }
    }

    fn uses_smtp(&self) -> bool {
        self.current()
            .is_some_and(|transport| transport.uses_smtp())
    }

    fn is_enabled(&self) -> bool {
        self.current().is_some()
    }
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
    use crate::{MailTransportConfig, TransportStatus};

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

    #[test]
    fn switchable_transport_tracks_live_configuration_rebuilds() {
        let transport = SwitchableTransport::default();
        assert!(!transport.is_enabled());
        let config = SmtpConfig::new(
            "127.0.0.1".to_owned(),
            2525,
            SmtpTls::StartTls,
            None,
            None,
            "operator@example.invalid".to_owned(),
        );
        let configured = MailTransportConfig::Smtp(config)
            .build(TransportStatus::default())
            .unwrap();
        transport.replace(Some(Arc::new(configured)));
        assert!(transport.is_enabled());
        assert!(transport.uses_smtp());
        transport.replace(None);
        assert!(!transport.is_enabled());
    }
}
