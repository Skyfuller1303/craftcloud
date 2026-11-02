//! Runtime mail transport composition from environment or encrypted instance settings.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine;
use craftcloud_auth::decrypt_secret_for;
use craftcloud_db::{PgPool, instance_settings};
use craftcloud_mail::{
    ConfiguredTransport, GraphConfig, MailTransportConfig, OAuthConfig, OAuthCredential,
    OAuthProvider, SmtpConfig, SmtpOAuthConfig, SmtpTls, TransportStatus,
};
use serde_json::Value;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::config::{Config, MailSettings};

fn defaults(config: &Config) -> BTreeMap<String, Value> {
    let mut result = BTreeMap::new();
    let transport = config
        .mail
        .as_ref()
        .map(|mail| match mail {
            MailSettings::Smtp { .. } => "smtp",
            MailSettings::Graph { .. } => "graph",
            MailSettings::SmtpOAuthMicrosoft { .. } => "smtp_oauth_microsoft",
            MailSettings::SmtpOAuthGoogle { .. } => "smtp_oauth_google",
        })
        .unwrap_or("none");
    result.insert("mail.transport".into(), serde_json::json!(transport));
    if let Some(mail) = &config.mail {
        let (host, port, tls, username, from) = match mail {
            MailSettings::Smtp {
                host,
                port,
                tls,
                username,
                from,
                ..
            } => (
                Some(host),
                Some(port),
                Some(tls),
                username.as_ref(),
                Some(from),
            ),
            MailSettings::SmtpOAuthGoogle {
                host,
                port,
                tls,
                username,
                from,
                ..
            } => (
                Some(host),
                Some(port),
                Some(tls),
                Some(username),
                Some(from),
            ),
            MailSettings::SmtpOAuthMicrosoft {
                host,
                port,
                tls,
                username,
                from,
                ..
            } => (
                Some(host),
                Some(port),
                Some(tls),
                Some(username),
                Some(from),
            ),
            MailSettings::Graph { .. } => (None, None, None, None, None),
        };
        result.insert("mail.host".into(), serde_json::json!(host));
        result.insert("mail.port".into(), serde_json::json!(port));
        result.insert(
            "mail.tls".into(),
            serde_json::json!(tls.map(|v| match v {
                SmtpTls::StartTls => "starttls",
                SmtpTls::ImplicitTls => "implicit",
            })),
        );
        result.insert("mail.username".into(), serde_json::json!(username));
        result.insert("mail.from".into(), serde_json::json!(from));
        match mail {
            MailSettings::Graph {
                tenant_id,
                client_id,
                sender,
                save_to_sent,
                ..
            } => {
                result.insert("mail.tenant_id".into(), serde_json::json!(tenant_id));
                result.insert("mail.client_id".into(), serde_json::json!(client_id));
                result.insert("mail.sender".into(), serde_json::json!(sender));
                result.insert("mail.save_to_sent".into(), serde_json::json!(save_to_sent));
            }
            MailSettings::SmtpOAuthMicrosoft {
                tenant_id,
                client_id,
                ..
            } => {
                result.insert("mail.tenant_id".into(), serde_json::json!(tenant_id));
                result.insert("mail.client_id".into(), serde_json::json!(client_id));
            }
            _ => {}
        }
    }
    result
}

fn effective_value(
    key: &str,
    env_values: &BTreeMap<String, Value>,
    env_locked: &std::collections::BTreeSet<String>,
    db_values: &BTreeMap<String, Value>,
    default_values: &BTreeMap<String, Value>,
) -> Value {
    if env_locked.contains(key) {
        env_values
            .get(key)
            .cloned()
            .or_else(|| default_values.get(key).cloned())
            .unwrap_or(Value::Null)
    } else {
        db_values
            .get(key)
            .and_then(|value| value.get("value"))
            .cloned()
            .or_else(|| default_values.get(key).cloned())
            .unwrap_or(Value::Null)
    }
}

fn text(values: &BTreeMap<String, Value>, key: &str) -> Option<String> {
    values.get(key)?.as_str().map(str::to_owned)
}

fn tls(values: &BTreeMap<String, Value>) -> Result<SmtpTls, &'static str> {
    match text(values, "mail.tls").as_deref().unwrap_or("starttls") {
        "starttls" => Ok(SmtpTls::StartTls),
        "implicit" => Ok(SmtpTls::ImplicitTls),
        _ => Err("mail settings are invalid"),
    }
}

fn port(values: &BTreeMap<String, Value>) -> Result<u16, &'static str> {
    values
        .get("mail.port")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or("mail settings are invalid")
}

fn optional_path(path: Option<&Path>) -> Result<Option<String>, &'static str> {
    path.map(|path| {
        path.to_str()
            .map(str::to_owned)
            .ok_or("mail settings are invalid")
    })
    .transpose()
}

fn configured_secret_file<'a>(config: &'a Config, name: &str) -> Option<&'a Path> {
    match (&config.mail, name) {
        (Some(MailSettings::Smtp { password_file, .. }), "mail.password") => {
            password_file.as_deref()
        }
        (
            Some(MailSettings::Graph {
                client_secret_file, ..
            }),
            "mail.client_secret",
        )
        | (
            Some(MailSettings::SmtpOAuthMicrosoft {
                client_secret_file, ..
            }),
            "mail.client_secret",
        ) => client_secret_file.as_deref(),
        (
            Some(MailSettings::Graph {
                certificate_file, ..
            }),
            "mail.certificate",
        )
        | (
            Some(MailSettings::SmtpOAuthMicrosoft {
                certificate_file, ..
            }),
            "mail.certificate",
        ) => certificate_file.as_deref(),
        (Some(MailSettings::Graph { key_file, .. }), "mail.private_key")
        | (Some(MailSettings::SmtpOAuthMicrosoft { key_file, .. }), "mail.private_key") => {
            key_file.as_deref()
        }
        (
            Some(MailSettings::SmtpOAuthGoogle {
                service_account_file,
                ..
            }),
            "mail.service_account",
        ) => Some(service_account_file),
        _ => None,
    }
}

fn decrypt_db_secret(
    key: &str,
    rows: &BTreeMap<String, Value>,
    instance_id: Uuid,
    current: Option<&[u8; 32]>,
    old: Option<&[u8; 32]>,
) -> Result<Option<Zeroizing<Vec<u8>>>, &'static str> {
    let Some(encoded) = rows
        .get(key)
        .and_then(|value| value.get("ciphertext"))
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let ciphertext = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| "mail settings are invalid")?;
    let purpose = format!("instance-setting:{key}");
    let decrypt = |secret_key: &[u8; 32]| {
        decrypt_secret_for(secret_key, &ciphertext, instance_id, purpose.as_bytes())
    };
    let plaintext = current
        .and_then(|key| decrypt(key).ok())
        .or_else(|| old.and_then(|key| decrypt(key).ok()))
        .ok_or("mail settings are invalid")?;
    Ok(Some(Zeroizing::new(plaintext)))
}

fn secret_string(bytes: Zeroizing<Vec<u8>>) -> Result<Zeroizing<String>, &'static str> {
    String::from_utf8(bytes.to_vec())
        .map(Zeroizing::new)
        .map_err(|_| "mail settings are invalid")
}

async fn resolve_oauth_credential(
    config: &Config,
    rows: &BTreeMap<String, Value>,
    env_locked: &std::collections::BTreeSet<String>,
    instance_id: Uuid,
    current: Option<&[u8; 32]>,
    old: Option<&[u8; 32]>,
) -> Result<OAuthCredential, &'static str> {
    if !env_locked.contains("mail.client_secret")
        && let Some(secret) =
            decrypt_db_secret("mail.client_secret", rows, instance_id, current, old)?
    {
        return Ok(OAuthCredential::ClientSecret(secret_string(secret)?));
    }
    if !env_locked.contains("mail.certificate") && !env_locked.contains("mail.private_key") {
        let cert = decrypt_db_secret("mail.certificate", rows, instance_id, current, old)?;
        let private_key = decrypt_db_secret("mail.private_key", rows, instance_id, current, old)?;
        if let (Some(certificate_pem), Some(key_pem)) = (cert, private_key) {
            return Ok(OAuthCredential::CertificateAndKey {
                certificate_pem,
                key_pem,
            });
        }
    }
    if let (Some(certificate_file), Some(key_file)) = (
        configured_secret_file(config, "mail.certificate"),
        configured_secret_file(config, "mail.private_key"),
    ) {
        return Ok(OAuthCredential::CertificateAndKeyFiles {
            certificate_file: optional_path(Some(certificate_file))?
                .ok_or("mail settings are invalid")?,
            key_file: optional_path(Some(key_file))?.ok_or("mail settings are invalid")?,
        });
    }
    if let Some(path) = configured_secret_file(config, "mail.client_secret") {
        return Ok(OAuthCredential::ClientSecretFile(
            optional_path(Some(path))?.ok_or("mail settings are invalid")?,
        ));
    }
    Err("mail OAuth credentials are invalid")
}

fn smtp_settings(
    config: &Config,
    values: &BTreeMap<String, Value>,
    rows: &BTreeMap<String, Value>,
    env_locked: &std::collections::BTreeSet<String>,
    instance_id: Uuid,
    current: Option<&[u8; 32]>,
    old: Option<&[u8; 32]>,
) -> Result<SmtpConfig, &'static str> {
    let host = text(values, "mail.host")
        .filter(|value| !value.is_empty())
        .ok_or("mail settings are invalid")?;
    let port = port(values)?;
    let tls = tls(values)?;
    let username = text(values, "mail.username").filter(|value| !value.is_empty());
    let database_password = if !env_locked.contains("mail.password") {
        decrypt_db_secret("mail.password", rows, instance_id, current, old)?
    } else {
        None
    };
    let file_password = if database_password.is_none() {
        configured_secret_file(config, "mail.password")
            .map(crate::secret::load_secret_file)
            .transpose()
            .map_err(|_| "mail settings are invalid")?
    } else {
        None
    };
    let password = match (database_password, file_password) {
        (Some(bytes), _) => Some(secret_string(bytes)?),
        (None, Some(secret)) => Some(Zeroizing::new(secret.expose().to_owned())),
        _ => None,
    };
    let from = text(values, "mail.from")
        .filter(|value| !value.is_empty())
        .ok_or("mail settings are invalid")?;
    Ok(SmtpConfig::new_with_password(
        host, port, tls, username, password, from,
    ))
}

fn graph_config(
    values: &BTreeMap<String, Value>,
    credential: OAuthCredential,
) -> Result<GraphConfig, &'static str> {
    Ok(GraphConfig {
        oauth: OAuthConfig {
            tenant_id: text(values, "mail.tenant_id")
                .filter(|v| !v.is_empty())
                .ok_or("mail settings are invalid")?,
            client_id: text(values, "mail.client_id")
                .filter(|v| !v.is_empty())
                .ok_or("mail settings are invalid")?,
            credential,
        },
        sender: text(values, "mail.sender")
            .filter(|v| !v.is_empty())
            .ok_or("mail settings are invalid")?,
        save_to_sent: values
            .get("mail.save_to_sent")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

pub async fn load(
    pool: &PgPool,
    config: &Config,
    current: Option<&[u8; 32]>,
    old: Option<&[u8; 32]>,
    status: TransportStatus,
) -> Result<Option<ConfiguredTransport>, &'static str> {
    let env_values_locked = crate::instance_setting_environment(config);
    let rows = instance_settings::settings(pool)
        .await
        .map_err(|_| "mail settings are invalid")?;
    let db_values = rows
        .into_iter()
        .map(|row| (row.key, row.value))
        .collect::<BTreeMap<_, _>>();
    let defaults = defaults(config);
    let mut values = BTreeMap::new();
    for key in [
        "mail.transport",
        "mail.host",
        "mail.port",
        "mail.tls",
        "mail.username",
        "mail.from",
        "mail.tenant_id",
        "mail.client_id",
        "mail.sender",
        "mail.save_to_sent",
    ] {
        values.insert(
            key.to_owned(),
            effective_value(
                key,
                &env_values_locked.0,
                &env_values_locked.1,
                &db_values,
                &defaults,
            ),
        );
    }
    let transport = text(&values, "mail.transport").unwrap_or_else(|| "none".to_owned());
    if transport == "none" {
        return Ok(None);
    }
    let instance_id: Uuid =
        sqlx::query_scalar("SELECT operator_user_id FROM instance_setup WHERE id=true")
            .fetch_one(pool)
            .await
            .map_err(|_| "mail settings are invalid")?;
    let mail = match transport.as_str() {
        "smtp" => MailTransportConfig::Smtp(smtp_settings(
            config,
            &values,
            &db_values,
            &env_values_locked.1,
            instance_id,
            current,
            old,
        )?),
        "graph" => {
            let credential = resolve_oauth_credential(
                config,
                &db_values,
                &env_values_locked.1,
                instance_id,
                current,
                old,
            )
            .await?;
            MailTransportConfig::Graph(graph_config(&values, credential)?)
        }
        "smtp_oauth_microsoft" => {
            let credential = resolve_oauth_credential(
                config,
                &db_values,
                &env_values_locked.1,
                instance_id,
                current,
                old,
            )
            .await?;
            MailTransportConfig::SmtpOAuth(SmtpOAuthConfig {
                smtp: smtp_settings(
                    config,
                    &values,
                    &db_values,
                    &env_values_locked.1,
                    instance_id,
                    current,
                    old,
                )?,
                provider: OAuthProvider::Microsoft(OAuthConfig {
                    tenant_id: text(&values, "mail.tenant_id")
                        .filter(|v| !v.is_empty())
                        .ok_or("mail settings are invalid")?,
                    client_id: text(&values, "mail.client_id")
                        .filter(|v| !v.is_empty())
                        .ok_or("mail settings are invalid")?,
                    credential,
                }),
            })
        }
        "smtp_oauth_google" => {
            let username = text(&values, "mail.username").filter(|value| !value.is_empty());
            let account = if !env_values_locked.1.contains("mail.service_account") {
                decrypt_db_secret(
                    "mail.service_account",
                    &db_values,
                    instance_id,
                    current,
                    old,
                )?
            } else {
                None
            };
            let provider = if let Some(service_account_json) = account {
                OAuthProvider::GoogleServiceAccount {
                    service_account_json,
                    sender: username.clone().ok_or("mail settings are invalid")?,
                }
            } else {
                let path = configured_secret_file(config, "mail.service_account")
                    .and_then(|path| path.to_str())
                    .ok_or("mail settings are invalid")?;
                OAuthProvider::Google {
                    service_account_file: path.to_owned(),
                    sender: username.clone().ok_or("mail settings are invalid")?,
                }
            };
            MailTransportConfig::SmtpOAuth(SmtpOAuthConfig {
                smtp: smtp_settings(
                    config,
                    &values,
                    &db_values,
                    &env_values_locked.1,
                    instance_id,
                    current,
                    old,
                )?,
                provider,
            })
        }
        _ => return Err("mail settings are invalid"),
    };
    mail.build(status)
        .map(Some)
        .map_err(|_| "mail settings are invalid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use craftcloud_auth::encrypt_secret_for;

    #[test]
    fn graph_transport_does_not_require_smtp_host_or_port() {
        let values = BTreeMap::from([
            ("mail.tenant_id".to_owned(), serde_json::json!("tenant")),
            ("mail.client_id".to_owned(), serde_json::json!("client")),
            (
                "mail.sender".to_owned(),
                serde_json::json!("sender@example.invalid"),
            ),
        ]);
        let config = graph_config(
            &values,
            OAuthCredential::ClientSecret(Zeroizing::new("secret".to_owned())),
        )
        .unwrap();
        assert_eq!(config.sender, "sender@example.invalid");
    }

    #[test]
    fn database_secret_decryption_supports_key_rotation_and_key_scoped_aad() {
        let instance = Uuid::from_u128(0x1234);
        let current = [0x5a; 32];
        let old = [0x6b; 32];
        let secret = b"synthetic db credential";
        let old_ciphertext =
            encrypt_secret_for(&old, instance, b"instance-setting:mail.password", secret).unwrap();
        let rows = BTreeMap::from([(
            "mail.password".to_owned(),
            serde_json::json!({
                "ciphertext": base64::engine::general_purpose::STANDARD.encode(old_ciphertext)
            }),
        )]);
        let plaintext =
            decrypt_db_secret("mail.password", &rows, instance, Some(&current), Some(&old))
                .unwrap()
                .unwrap();
        assert_eq!(plaintext.as_slice(), secret);

        let current_ciphertext = encrypt_secret_for(
            &current,
            instance,
            b"instance-setting:mail.password",
            secret,
        )
        .unwrap();
        let current_rows = BTreeMap::from([(
            "mail.password".to_owned(),
            serde_json::json!({
                "ciphertext": base64::engine::general_purpose::STANDARD.encode(&current_ciphertext)
            }),
        )]);
        let current_plaintext = decrypt_db_secret(
            "mail.password",
            &current_rows,
            instance,
            Some(&current),
            Some(&old),
        )
        .unwrap()
        .unwrap();
        assert_eq!(current_plaintext.as_slice(), secret);
        let wrong_aad_rows = BTreeMap::from([(
            "mail.client_secret".to_owned(),
            serde_json::json!({
                "ciphertext": base64::engine::general_purpose::STANDARD.encode(current_ciphertext)
            }),
        )]);
        assert!(
            decrypt_db_secret(
                "mail.client_secret",
                &wrong_aad_rows,
                instance,
                Some(&current),
                Some(&old),
            )
            .is_err()
        );
    }
}
