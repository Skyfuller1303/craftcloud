//! SMTP delivery and static, escaped transactional mail templates.

mod smtp;
mod templates;

pub use smtp::{
    Delivery, MailMessage, MailTransport, SmtpConfig, SmtpTls, SmtpTransport, TransportError,
};
pub use templates::{RenderedMail, render};
