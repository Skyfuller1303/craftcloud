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

fn resolve_oauth_credential(
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
    let environment = crate::instance_setting_environment(config);
    // Environment mail never reads the database: stored values (and stored
    // ciphertext that may not decrypt) cannot influence or break it.
    let environment_mail = environment.1.contains("mail.transport");
    let db_values = if environment_mail {
        BTreeMap::new()
    } else {
        instance_settings::settings(pool)
            .await
            .map_err(|_| "mail settings are invalid")?
            .into_iter()
            .map(|row| (row.key, row.value))
            .collect::<BTreeMap<_, _>>()
    };
    let instance_id: Uuid = if environment_mail {
        Uuid::nil()
    } else {
        sqlx::query_scalar("SELECT operator_user_id FROM instance_setup WHERE id=true")
            .fetch_optional(pool)
            .await
            .map_err(|_| "mail settings are invalid")?
            .unwrap_or_else(Uuid::nil)
    };
    build(
        config,
        &environment,
        &db_values,
        instance_id,
        current,
        old,
        status,
    )
}

/// Composes the transport from environment values and stored rows. Pure so
/// that precedence rules are testable without a database.
fn build(
    config: &Config,
    environment: &(BTreeMap<String, Value>, std::collections::BTreeSet<String>),
    db_values: &BTreeMap<String, Value>,
    instance_id: Uuid,
    current: Option<&[u8; 32]>,
    old: Option<&[u8; 32]>,
    status: TransportStatus,
) -> Result<Option<ConfiguredTransport>, &'static str> {
    let env_values_locked = environment;
    // Environment mail never mixes in stored rows, whatever the caller passes.
    let no_rows = BTreeMap::new();
    let db_values = if environment.1.contains("mail.transport") {
        &no_rows
    } else {
        db_values
    };
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
                db_values,
                &defaults,
            ),
        );
    }
    let transport = text(&values, "mail.transport").unwrap_or_else(|| "none".to_owned());
    if transport == "none" {
        return Ok(None);
    }
    let mail = match transport.as_str() {
        "smtp" => MailTransportConfig::Smtp(smtp_settings(
            config,
            &values,
            db_values,
            &env_values_locked.1,
            instance_id,
            current,
            old,
        )?),
        "graph" => {
            let credential = resolve_oauth_credential(
                config,
                db_values,
                &env_values_locked.1,
                instance_id,
                current,
                old,
            )?;
            MailTransportConfig::Graph(graph_config(&values, credential)?)
        }
        "smtp_oauth_microsoft" => {
            let credential = resolve_oauth_credential(
                config,
                db_values,
                &env_values_locked.1,
                instance_id,
                current,
                old,
            )?;
            MailTransportConfig::SmtpOAuth(SmtpOAuthConfig {
                smtp: smtp_settings(
                    config,
                    &values,
                    db_values,
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
                decrypt_db_secret("mail.service_account", db_values, instance_id, current, old)?
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
                    db_values,
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

    fn secret_path(name: &str) -> String {
        std::env::temp_dir()
            .join(format!("craftcloud-synthetic-{name}"))
            .to_string_lossy()
            .into_owned()
    }

    fn config_from(pairs: &[(&str, &str)]) -> Config {
        let file = std::env::temp_dir().join("craftcloud-synthetic-credential");
        let file = file.to_string_lossy().into_owned();
        let mut env = BTreeMap::from([
            ("CRAFTCLOUD_DATABASE_URL_FILE".to_owned(), file.clone()),
            ("CRAFTCLOUD_SECRET_KEY_FILE".to_owned(), file),
            (
                "CRAFTCLOUD_PUBLIC_URL".to_owned(),
                "https://cloud.example.test".to_owned(),
            ),
        ]);
        env.extend(
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
        );
        Config::from_env_map(&env).unwrap()
    }

    type Environment = (BTreeMap<String, Value>, std::collections::BTreeSet<String>);

    fn environment_of(config: &Config, names: &[&str]) -> Environment {
        crate::instance_setting_environment_with(config, &|name| names.contains(&name))
    }

    fn encrypted_row(key: &str, plaintext: &[u8]) -> (String, Value) {
        let ciphertext = encrypt_secret_for(
            &[0x5a; 32],
            Uuid::from_u128(0x1234),
            format!("instance-setting:{key}").as_bytes(),
            plaintext,
        )
        .unwrap();
        (
            key.to_owned(),
            serde_json::json!({
                "ciphertext": base64::engine::general_purpose::STANDARD.encode(ciphertext)
            }),
        )
    }

    fn row(key: &str, value: Value) -> (String, Value) {
        (key.to_owned(), serde_json::json!({ "value": value }))
    }

    fn graph_env(secret: &str) -> Vec<(&'static str, String)> {
        vec![
            ("CRAFTCLOUD_MAIL_TRANSPORT", "graph".to_owned()),
            (
                "CRAFTCLOUD_GRAPH_TENANT_ID",
                "00000000-0000-0000-0000-000000000001".to_owned(),
            ),
            (
                "CRAFTCLOUD_GRAPH_CLIENT_ID",
                "00000000-0000-0000-0000-000000000002".to_owned(),
            ),
            ("CRAFTCLOUD_GRAPH_SENDER", "mailer@example.org".to_owned()),
            ("CRAFTCLOUD_GRAPH_CLIENT_SECRET_FILE", secret.to_owned()),
        ]
    }

    fn config_of(pairs: &[(&str, String)]) -> Config {
        let borrowed = pairs
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect::<Vec<_>>();
        config_from(&borrowed)
    }
    const GRAPH_NAMES: [&str; 5] = [
        "MAIL_TRANSPORT",
        "GRAPH_TENANT_ID",
        "GRAPH_CLIENT_ID",
        "GRAPH_SENDER",
        "GRAPH_CLIENT_SECRET_FILE",
    ];

    #[test]
    fn environment_mail_locks_every_mail_setting_and_credential() {
        let config = config_of(&graph_env(&secret_path("graph")));
        let (_, locked) = environment_of(&config, &GRAPH_NAMES);
        for key in crate::mail_setting_keys() {
            assert!(locked.contains(key), "{key} must be locked");
        }
        assert!(!locked.contains("retention.trash_days"));
        // Mail not configured through the environment stays editable.
        let (_, open) = environment_of(&config_from(&[]), &[]);
        assert!(open.is_empty());
    }

    #[test]
    fn graph_environment_secret_ignores_database_certificate() {
        let secret = secret_path("graph");
        let config = config_of(&graph_env(&secret));
        let environment = environment_of(&config, &GRAPH_NAMES);
        // The stored certificate pair would otherwise outrank the secret file.
        let rows = BTreeMap::from([
            encrypted_row("mail.certificate", b"-----BEGIN CERTIFICATE-----"),
            encrypted_row("mail.private_key", b"-----BEGIN PRIVATE KEY-----"),
        ]);
        let credential = resolve_oauth_credential(
            &config,
            &rows,
            &environment.1,
            Uuid::from_u128(0x1234),
            Some(&[0x5a; 32]),
            None,
        )
        .unwrap();
        assert!(matches!(credential, OAuthCredential::ClientSecretFile(path) if path == secret));
    }

    #[test]
    fn environment_certificate_ignores_database_client_secret() {
        let mut pairs = graph_env("unused");
        pairs.retain(|(key, _)| !key.ends_with("CLIENT_SECRET_FILE"));
        pairs.push(("CRAFTCLOUD_GRAPH_CERT_FILE", secret_path("cert")));
        pairs.push(("CRAFTCLOUD_GRAPH_KEY_FILE", secret_path("key")));
        let config = config_of(&pairs);
        let mut names = GRAPH_NAMES.to_vec();
        names.retain(|name| *name != "GRAPH_CLIENT_SECRET_FILE");
        names.extend(["GRAPH_CERT_FILE", "GRAPH_KEY_FILE"]);
        let environment = environment_of(&config, &names);
        let rows = BTreeMap::from([encrypted_row("mail.client_secret", b"db client secret")]);
        let credential = resolve_oauth_credential(
            &config,
            &rows,
            &environment.1,
            Uuid::from_u128(0x1234),
            Some(&[0x5a; 32]),
            None,
        )
        .unwrap();
        assert!(matches!(
            credential,
            OAuthCredential::CertificateAndKeyFiles { .. }
        ));
    }

    #[test]
    fn undecryptable_database_secret_cannot_disable_environment_mail() {
        let smtp = [
            ("CRAFTCLOUD_MAIL_TRANSPORT", "smtp"),
            ("CRAFTCLOUD_SMTP_HOST", "smtp.example.org"),
            ("CRAFTCLOUD_SMTP_PORT", "587"),
            ("CRAFTCLOUD_SMTP_TLS", "starttls"),
            ("CRAFTCLOUD_SMTP_FROM", "mailer@example.org"),
        ];
        let config = config_from(&smtp);
        let environment = environment_of(
            &config,
            &[
                "MAIL_TRANSPORT",
                "SMTP_HOST",
                "SMTP_PORT",
                "SMTP_TLS",
                "SMTP_FROM",
            ],
        );
        // Ciphertext that does not decrypt (for example after a key change)
        // and a conflicting stored host must not matter for environment mail.
        let rows = BTreeMap::from([
            encrypted_row("mail.password", b"stale"),
            row("mail.host", serde_json::json!("evil.example")),
        ]);
        let built = build(
            &config,
            &environment,
            &rows,
            Uuid::from_u128(0x9999),
            Some(&[0x11; 32]),
            None,
            TransportStatus::default(),
        );
        assert!(built.unwrap().is_some());
        // The same kind of row does break a database-configured transport.
        let open_config = config_from(&[]);
        let open = environment_of(&open_config, &[]);
        let rows = BTreeMap::from([
            row("mail.transport", serde_json::json!("smtp")),
            row("mail.host", serde_json::json!("db.example")),
            row("mail.port", serde_json::json!(587)),
            row("mail.from", serde_json::json!("a@example.org")),
            encrypted_row("mail.password", b"stale"),
        ]);
        assert!(
            build(
                &open_config,
                &open,
                &rows,
                Uuid::from_u128(0x9999),
                Some(&[0x11; 32]),
                None,
                TransportStatus::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn database_oauth_transport_without_stored_credential_is_reported_missing() {
        let open_config = config_from(&[]);
        let open = environment_of(&open_config, &[]);
        let rows = BTreeMap::from([
            row("mail.transport", serde_json::json!("graph")),
            row("mail.tenant_id", serde_json::json!("tenant")),
            row("mail.client_id", serde_json::json!("client")),
            row("mail.sender", serde_json::json!("a@example.org")),
        ]);
        let result = build(
            &open_config,
            &open,
            &rows,
            Uuid::from_u128(0x1234),
            Some(&[0x5a; 32]),
            None,
            TransportStatus::default(),
        );
        assert_eq!(result.err(), Some("mail OAuth credentials are invalid"));
    }
}
