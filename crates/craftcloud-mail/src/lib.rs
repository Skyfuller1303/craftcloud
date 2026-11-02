//! SMTP delivery and static, escaped transactional mail templates.

mod oauth;
mod smtp;
mod templates;

pub use oauth::{
    ConfiguredTransport, GraphConfig, GraphTransport, MailTransportConfig, OAuthConfig,
    OAuthCredential, OAuthProvider, SmtpOAuthConfig, SmtpOAuthTransport, TransportStatus,
    TransportStatusSnapshot,
};
pub use smtp::{
    Delivery, MailMessage, MailTransport, SmtpConfig, SmtpTls, SmtpTransport, SwitchableTransport,
    TransportError,
};
pub use templates::{RenderedMail, render};
