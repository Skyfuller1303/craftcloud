//! Composition of configured transports; secrets are loaded only at startup.
use std::path::Path;

use craftcloud_mail::{
    ConfiguredTransport, GraphConfig, MailTransportConfig, OAuthConfig, OAuthCredential,
    OAuthProvider, SmtpConfig, SmtpOAuthConfig, TransportStatus,
};

use crate::config::MailSettings;

fn path(value: &Path) -> Result<String, &'static str> {
    value
        .to_str()
        .map(str::to_owned)
        .ok_or("mail secret path is invalid")
}

fn credential(
    cert: Option<&Path>,
    key: Option<&Path>,
    secret: Option<&Path>,
) -> Result<OAuthCredential, &'static str> {
    match (cert, key, secret) {
        (Some(cert), Some(key), None) => Ok(OAuthCredential::CertificateAndKeyFiles {
            certificate_file: path(cert)?,
            key_file: path(key)?,
        }),
        (None, None, Some(secret)) => Ok(OAuthCredential::ClientSecretFile(path(secret)?)),
        _ => Err("mail OAuth credentials are inconsistent"),
    }
}

pub fn build(
    settings: &MailSettings,
    status: TransportStatus,
) -> Result<ConfiguredTransport, &'static str> {
    let transport = match settings {
        MailSettings::Smtp {
            host,
            port,
            tls,
            username,
            password_file,
            from,
        } => {
            let password = password_file
                .as_deref()
                .map(crate::secret::load_secret_file)
                .transpose()
                .map_err(|_| "SMTP password file is unavailable or invalid")?;
            MailTransportConfig::Smtp(SmtpConfig::new(
                host.clone(),
                *port,
                *tls,
                username.clone(),
                password.as_ref().map(|value| value.expose().to_owned()),
                from.clone(),
            ))
        }
        MailSettings::Graph {
            tenant_id,
            client_id,
            certificate_file,
            key_file,
            client_secret_file,
            sender,
            save_to_sent,
        } => MailTransportConfig::Graph(GraphConfig {
            oauth: OAuthConfig {
                tenant_id: tenant_id.clone(),
                client_id: client_id.clone(),
                credential: credential(
                    certificate_file.as_deref(),
                    key_file.as_deref(),
                    client_secret_file.as_deref(),
                )?,
            },
            sender: sender.clone(),
            save_to_sent: *save_to_sent,
        }),
        MailSettings::SmtpOAuthMicrosoft {
            host,
            port,
            tls,
            username,
            from,
            tenant_id,
            client_id,
            certificate_file,
            key_file,
            client_secret_file,
        } => MailTransportConfig::SmtpOAuth(SmtpOAuthConfig {
            smtp: SmtpConfig::new(
                host.clone(),
                *port,
                *tls,
                Some(username.clone()),
                None,
                from.clone(),
            ),
            provider: OAuthProvider::Microsoft(OAuthConfig {
                tenant_id: tenant_id.clone(),
                client_id: client_id.clone(),
                credential: credential(
                    certificate_file.as_deref(),
                    key_file.as_deref(),
                    client_secret_file.as_deref(),
                )?,
            }),
        }),
        MailSettings::SmtpOAuthGoogle {
            host,
            port,
            tls,
            username,
            from,
            service_account_file,
        } => MailTransportConfig::SmtpOAuth(SmtpOAuthConfig {
            smtp: SmtpConfig::new(
                host.clone(),
                *port,
                *tls,
                Some(username.clone()),
                None,
                from.clone(),
            ),
            provider: OAuthProvider::Google {
                service_account_file: path(service_account_file)?,
                sender: username.clone(),
            },
        }),
    };
    transport
        .build(status)
        .map_err(|_| "mail transport credentials or settings are invalid")
}
