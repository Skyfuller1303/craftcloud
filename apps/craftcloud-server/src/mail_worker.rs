//! Bounded SMTP outbox worker. Message content and recipients are never logged.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use craftcloud_auth::decrypt_secret_for;
use craftcloud_db::PgPool;
use craftcloud_mail::{MailMessage, MailTransport, render};
use tokio::sync::watch;
use zeroize::Zeroize;

const BATCH_SIZE: i64 = 1;
const CLAIM_LEASE: Duration = Duration::from_secs(60);

pub async fn run(
    pool: PgPool,
    transport: Arc<impl MailTransport + 'static>,
    key: [u8; 32],
    old_key: Option<[u8; 32]>,
    public_url: String,
    message_id_domain: String,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        if *stop.borrow() {
            break;
        }
        let now = time::OffsetDateTime::now_utc();
        if craftcloud_db::email_outbox::expire_pending(&pool, now)
            .await
            .is_err()
        {
            tracing::error!("mail queue cleanup failed");
            sleep_or_stop(&mut stop, Duration::from_secs(5)).await;
            continue;
        }
        let lease_until =
            now + time::Duration::seconds(i64::try_from(CLAIM_LEASE.as_secs()).unwrap_or(60));
        let claims =
            match craftcloud_db::email_outbox::claim_batch(&pool, now, lease_until, BATCH_SIZE)
                .await
            {
                Ok(claims) => claims,
                Err(_) => {
                    tracing::error!("mail queue claim failed");
                    sleep_or_stop(&mut stop, Duration::from_secs(5)).await;
                    continue;
                }
            };
        if claims.is_empty() {
            sleep_or_stop(&mut stop, Duration::from_secs(2)).await;
            continue;
        }
        for claim in claims {
            if *stop.borrow() {
                break;
            }
            deliver_one(
                &pool,
                transport.as_ref(),
                &key,
                old_key.as_ref(),
                &public_url,
                &message_id_domain,
                claim,
            )
            .await;
        }
    }
}

async fn deliver_one(
    pool: &PgPool,
    transport: &impl MailTransport,
    key: &[u8; 32],
    old_key: Option<&[u8; 32]>,
    public_url: &str,
    message_id_domain: &str,
    claim: craftcloud_db::email_outbox::EmailClaim,
) {
    let params = claim.params.as_object().and_then(|object| {
        object
            .iter()
            .map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_owned())))
            .collect::<Option<BTreeMap<_, _>>>()
    });
    let mut params = match params {
        Some(params) => params,
        None => {
            fail(pool, &claim, true, "invalid_params").await;
            return;
        }
    };
    if !params.contains_key("expires_at") {
        let Ok(expires_at) = claim
            .expires_at
            .format(&time::format_description::well_known::Rfc3339)
        else {
            fail(pool, &claim, true, "invalid_expiry").await;
            return;
        };
        params.insert("expires_at".to_owned(), expires_at);
    }
    let token_template = matches!(
        claim.template.as_str(),
        "invitation" | "password_reset" | "email_verification" | "email_change"
    );
    if token_template != claim.encrypted_token.is_some() {
        fail(pool, &claim, true, "token_template_mismatch").await;
        return;
    }
    let mut capability_hash = None;
    if let Some(ciphertext) = claim.encrypted_token.as_deref() {
        let decrypted = decrypt_secret_for(key, ciphertext, claim.id, b"email").or_else(|_| {
            old_key.map_or(
                Err(craftcloud_auth::SecretBoxError::InvalidCiphertext),
                |old| decrypt_secret_for(old, ciphertext, claim.id, b"email"),
            )
        });
        let token = match decrypted {
            Ok(token) => token,
            Err(_) => {
                fail(pool, &claim, true, "token_decryption").await;
                return;
            }
        };
        let token = match String::from_utf8(token) {
            Ok(token) => token,
            Err(error) => {
                let mut bytes = error.into_bytes();
                bytes.zeroize();
                fail(pool, &claim, true, "token_encoding").await;
                return;
            }
        };
        let Some(parsed) = craftcloud_auth::OpaqueToken::parse(&token) else {
            let mut token = token;
            token.zeroize();
            fail(pool, &claim, true, "token_encoding").await;
            return;
        };
        capability_hash = Some(parsed.hash());
        let action_url = match action_url(public_url, &claim.template, &token) {
            Some(url) => url,
            None => {
                let mut token = token;
                token.zeroize();
                fail(pool, &claim, true, "token_template_mismatch").await;
                return;
            }
        };
        params.insert("action_url".to_owned(), action_url);
        let mut token = token;
        token.zeroize();
    }
    let rendered = match render(&claim.template, &claim.language, &params) {
        Some(rendered) => rendered,
        None => {
            fail(pool, &claim, true, "unknown_template").await;
            return;
        }
    };
    let message = MailMessage {
        recipient: claim.recipient.clone(),
        subject: rendered.subject,
        text: rendered.text,
        html: rendered.html,
        message_id: format!("<{}@{}>", claim.id, message_id_domain),
    };
    match craftcloud_db::email_outbox::delivery_is_current(
        pool,
        claim.id,
        claim.lease_token,
        time::OffsetDateTime::now_utc(),
        capability_hash.as_ref(),
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            fail(pool, &claim, true, "capability_stale").await;
            return;
        }
        Err(_) => {
            fail(pool, &claim, false, "preflight_database").await;
            return;
        }
    }
    let result = tokio::time::timeout(Duration::from_secs(20), transport.send(message)).await;
    match result {
        Ok(Ok(_)) => {
            match craftcloud_db::email_outbox::mark_sent(
                pool,
                claim.id,
                claim.lease_token,
                time::OffsetDateTime::now_utc(),
            )
            .await
            {
                Ok(true) => tracing::info!("mail delivered"),
                Ok(false) => tracing::warn!("mail delivery claim expired before completion"),
                Err(_) => tracing::error!("mail delivery status update failed"),
            }
        }
        Ok(Err(err)) => {
            fail_with_retry(
                pool,
                &claim,
                err.is_permanent(),
                queue_failure_code(err.code()),
                err.retry_after(),
            )
            .await
        }
        Err(_) => fail(pool, &claim, false, "smtp_timeout").await,
    }
}

fn queue_failure_code(code: &'static str) -> &'static str {
    match code {
        "ErrorAccessDenied" => "provider_access_denied",
        "ErrorInvalidUser" => "provider_invalid_user",
        "ErrorMailboxNotEnabledForRESTAPI" => "provider_mailbox_unavailable",
        "ErrorRecipientNotFound" => "provider_recipient_not_found",
        value
            if !value.is_empty()
                && value.len() <= 64
                && value.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
                }) =>
        {
            value
        }
        _ => "provider_failure",
    }
}

fn action_url(public_url: &str, template: &str, token: &str) -> Option<String> {
    let route = match template {
        "invitation" => "/invite",
        "password_reset" => "/reset-password",
        "email_verification" => "/admin/verify-email",
        "email_change" => "/admin/change-email",
        _ => return None,
    };
    Some(format!(
        "{}{route}#token={token}",
        public_url.trim_end_matches('/')
    ))
}

async fn fail(
    pool: &PgPool,
    claim: &craftcloud_db::email_outbox::EmailClaim,
    permanent: bool,
    code: &'static str,
) {
    fail_with_retry(pool, claim, permanent, code, None).await;
}

async fn fail_with_retry(
    pool: &PgPool,
    claim: &craftcloud_db::email_outbox::EmailClaim,
    permanent: bool,
    code: &'static str,
    provider_delay: Option<Duration>,
) {
    let now = time::OffsetDateTime::now_utc();
    let provider_delay = provider_delay
        .map(|value| {
            time::Duration::seconds(i64::try_from(value.as_secs().min(86_400)).unwrap_or(86_400))
        })
        .unwrap_or(time::Duration::ZERO);
    let retry = now + retry_delay(claim.attempts).max(provider_delay);
    match craftcloud_db::email_outbox::mark_failed(
        pool,
        claim.id,
        claim.lease_token,
        now,
        retry,
        permanent,
        code,
    )
    .await
    {
        Ok(true) => tracing::warn!(permanent, "mail delivery failed"),
        Ok(false) => tracing::warn!("mail delivery claim expired before completion"),
        Err(_) => tracing::error!("mail failure status update failed"),
    }
}

async fn sleep_or_stop(stop: &mut watch::Receiver<bool>, duration: Duration) {
    tokio::select! {
        _ = tokio::time::sleep(duration) => {},
        _ = stop.changed() => {},
    }
}

fn retry_delay(attempts: i32) -> time::Duration {
    let attempts = u32::try_from(attempts).unwrap_or(1).clamp(1, 8);
    let seconds = 30_i64 * (1_i64 << attempts.saturating_sub(1));
    time::Duration::seconds(seconds)
}

#[cfg(test)]
mod tests {
    use super::{action_url, retry_delay};

    #[test]
    fn provider_codes_fit_the_existing_queue_constraint() {
        for input in [
            "ErrorAccessDenied",
            "ErrorInvalidUser",
            "ErrorMailboxNotEnabledForRESTAPI",
            "ErrorRecipientNotFound",
            "invalid_client",
            "provider_rate_limited",
            "untrusted provider body",
        ] {
            let code = super::queue_failure_code(input);
            assert!(!code.is_empty() && code.len() <= 64);
            assert!(
                code.bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            );
        }
        assert_eq!(
            super::queue_failure_code("invalid_client"),
            "invalid_client"
        );
    }

    #[test]
    fn capability_urls_use_fixed_spa_routes_and_fragments() {
        assert_eq!(
            action_url(
                "https://cloud.example/",
                "email_verification",
                "abcDEF_123-"
            ),
            Some("https://cloud.example/admin/verify-email#token=abcDEF_123-".to_owned())
        );
        assert_eq!(
            action_url("https://cloud.example", "email_change", "token"),
            Some("https://cloud.example/admin/change-email#token=token".to_owned())
        );
        assert_eq!(
            action_url("https://cloud.example", "security_notice", "token"),
            None
        );
    }

    #[test]
    fn retry_backoff_is_exponential_and_bounded() {
        assert_eq!(retry_delay(1).whole_seconds(), 30);
        assert_eq!(retry_delay(2).whole_seconds(), 60);
        assert_eq!(retry_delay(3).whole_seconds(), 120);
        assert_eq!(retry_delay(8).whole_seconds(), 3840);
        assert_eq!(retry_delay(i32::MAX), retry_delay(8));
        assert_eq!(retry_delay(-1), retry_delay(1));
    }
}
