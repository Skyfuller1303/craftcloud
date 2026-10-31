//! Atomic security notice drafts inserted beside account security changes.

use sqlx::{Postgres, Transaction};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::DbError;
use crate::email_outbox::{self, EmailDraft};

#[derive(Clone, Copy)]
pub enum SecurityEvent {
    PasswordChanged,
    PasswordReset,
    TwoFactorEnabled,
    TwoFactorDisabled,
    TwoFactorReset,
    NewLogin,
}

impl SecurityEvent {
    fn text(self) -> &'static str {
        match self {
            Self::PasswordChanged => "Your password was changed.",
            Self::PasswordReset => "Your password was reset.",
            Self::TwoFactorEnabled => "Two-factor authentication was enabled.",
            Self::TwoFactorDisabled => "Two-factor authentication was disabled.",
            Self::TwoFactorReset => "Two-factor authentication was reset by an administrator.",
            Self::NewLogin => "A new device signed in to your CraftCloud account.",
        }
    }
}

/// Adds a no-link security email to the caller's account transaction.
/// The caller already holds the user row lock. A notice never blocks the
/// security action: when the per-recipient limit is reached the action still
/// commits and the notice is stored as `suppressed` for the operator status.
/// Returns whether the notice was queued for delivery.
pub async fn enqueue_in(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    now: OffsetDateTime,
    event: SecurityEvent,
) -> Result<bool, DbError> {
    let account: Option<(String, String)> = sqlx::query_as(
        "SELECT normalized_email,display_name FROM users WHERE id=$1 AND disabled_at IS NULL",
    )
    .bind(user_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((recipient, recipient_name)) = account else {
        return Err(DbError::Internal);
    };
    let draft = EmailDraft {
        id: Uuid::new_v4(),
        recipient,
        template: "security_notice".to_owned(),
        language: "en".to_owned(),
        params: serde_json::json!({
            "recipient_name": recipient_name,
            "security_event": event.text(),
        }),
        encrypted_token: None,
        expires_at: now + Duration::hours(24),
        actor_id: Some(user_id),
        is_resend: false,
        now,
    };
    email_outbox::enqueue_notice_in(tx, &draft).await
}
