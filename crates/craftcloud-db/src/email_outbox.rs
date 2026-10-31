//! Durable outbound e-mail queue and database-backed abuse limits.
//!
//! The API inserts a draft in the same transaction as the triggering action.
//! The worker claims rows with a lease, then completes them using the lease
//! token. Delivery is at-least-once across a process crash after SMTP accepts
//! a message and before the database records success.

use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::DbError;
use crate::audit::{AuditCtx, Detail, NewEvent, record_in};
use crate::members::OrgTx;

/// A message staged by an API transaction.
#[derive(Clone)]
pub struct EmailDraft {
    /// Stable outbox identifier and encryption owner id.
    pub id: Uuid,
    /// Normalized recipient address.
    pub recipient: String,
    /// Stable template identifier; no rendered content is stored.
    pub template: String,
    /// Supported template language (`en` or `de`).
    pub language: String,
    /// Non-secret template parameters. Never place link tokens here.
    pub params: Value,
    /// Token ciphertext using the AC-02 server key and AAD `email`.
    pub encrypted_token: Option<Vec<u8>>,
    /// Message expiry.
    pub expires_at: OffsetDateTime,
    /// Initiating account, if any.
    pub actor_id: Option<Uuid>,
    /// Whether this draft was caused by an explicit resend action.
    pub is_resend: bool,
    /// Transaction timestamp.
    pub now: OffsetDateTime,
}

/// Result of durable limits checked while enqueueing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// The message was inserted.
    Queued,
    /// Recipient exceeded five messages per template in one hour.
    RecipientLimit,
    /// Actor exceeded thirty messages in one hour (verification links are
    /// counted separately from all other templates).
    ActorLimit,
    /// This recipient/template was resent less than one minute ago.
    ResendTooSoon,
}

/// A leased queue row returned to the worker.
#[derive(Clone)]
pub struct EmailClaim {
    /// Stable message identifier. Use it in `Message-ID`.
    pub id: Uuid,
    /// Lease generation token; required for all completion operations.
    pub lease_token: Uuid,
    /// Recipient address.
    pub recipient: String,
    /// Stable template identifier.
    pub template: String,
    /// Template language.
    pub language: String,
    /// Non-secret template parameters.
    pub params: Value,
    /// Encrypted capability token, if this message carries one.
    pub encrypted_token: Option<Vec<u8>>,
    /// Message expiry.
    pub expires_at: OffsetDateTime,
    /// Number of this and prior delivery attempts; incremented at claim time.
    pub attempts: i32,
}

/// Aggregate delivery information safe for operator display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmailStatus {
    /// Last successful delivery, if any.
    pub last_success_at: Option<OffsetDateTime>,
    /// Number of terminal failures retained in the queue.
    pub failed_count: i64,
    /// Number of security notices dropped by the per-recipient limit and still
    /// retained (rows are purged after seven days). The security action itself succeeded.
    pub suppressed_count: i64,
}

/// Checks durable limits and inserts the draft inside the caller's transaction.
/// The caller must hold the business locks and commit/rollback the transaction.
pub async fn enqueue_in(
    tx: &mut Transaction<'_, Postgres>,
    draft: &EmailDraft,
) -> Result<EnqueueOutcome, DbError> {
    enqueue_with_status(tx, draft, "queued").await
}

/// Applies the same durable limits as [`enqueue_in`] and records the attempt
/// in the rate windows, but stores a terminal `expired` row without the token
/// so the message is never delivered. Used where the caller must stay
/// indistinguishable from a real send (for example an address that cannot be
/// claimed).
pub async fn enqueue_discarded_in(
    tx: &mut Transaction<'_, Postgres>,
    draft: &EmailDraft,
) -> Result<EnqueueOutcome, DbError> {
    enqueue_with_status(tx, draft, "expired").await
}

async fn enqueue_with_status(
    tx: &mut Transaction<'_, Postgres>,
    draft: &EmailDraft,
    status: &'static str,
) -> Result<EnqueueOutcome, DbError> {
    // Serialize durable-rate checks across concurrent transactions. Sending
    // is low volume and this fixed key avoids cross-bucket lock-order cycles.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind("email-outbox-admission")
        .execute(&mut **tx)
        .await?;
    if draft.is_resend {
        let recent: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM outbound_emails WHERE recipient=$1 AND template=$2 \
             AND status<>'suppressed' AND created_at > $3 - interval '1 minute')",
        )
        .bind(&draft.recipient)
        .bind(&draft.template)
        .bind(draft.now)
        .fetch_one(&mut **tx)
        .await?;
        if recent {
            return Ok(EnqueueOutcome::ResendTooSoon);
        }
    }
    // Security notices are counted per event, so a flood of new-login
    // notices cannot suppress a later "password changed" notice.
    let recipient_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbound_emails WHERE recipient=$1 AND template=$2 \
         AND status<>'suppressed' AND created_at > $3 - interval '1 hour' \
         AND coalesce(params->>'security_event','') = coalesce($4::jsonb->>'security_event','')",
    )
    .bind(&draft.recipient)
    .bind(&draft.template)
    .bind(draft.now)
    .bind(&draft.params)
    .fetch_one(&mut **tx)
    .await?;
    if recipient_count >= 5 {
        return Ok(EnqueueOutcome::RecipientLimit);
    }
    if let Some(actor_id) = draft.actor_id {
        // Verification links (including operator bulk issuance) have their own
        // budget so they cannot exhaust the actor's notices and test mails.
        let actor_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM outbound_emails WHERE actor_id=$1 \
             AND status<>'suppressed' AND created_at > $2 - interval '1 hour' \
             AND (template='email_verification') = ($3='email_verification')",
        )
        .bind(actor_id)
        .bind(draft.now)
        .bind(&draft.template)
        .fetch_one(&mut **tx)
        .await?;
        if actor_count >= 30 {
            return Ok(EnqueueOutcome::ActorLimit);
        }
    }
    sqlx::query(
        "INSERT INTO outbound_emails(id,recipient,template,language,params,encrypted_token,expires_at,actor_id,is_resend,created_at,available_at,status) \
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$10,$11)",
    )
    .bind(draft.id)
    .bind(&draft.recipient)
    .bind(&draft.template)
    .bind(&draft.language)
    .bind(&draft.params)
    .bind(if status == "queued" {
        draft.encrypted_token.as_deref()
    } else {
        None
    })
    .bind(draft.expires_at)
    .bind(draft.actor_id)
    .bind(draft.is_resend)
    .bind(draft.now)
    .bind(status)
    .execute(&mut **tx)
    .await?;
    Ok(EnqueueOutcome::Queued)
}

/// Enqueues a no-link security notice without ever failing the caller's
/// security action. When a durable limit refuses the notice, a terminal
/// `suppressed` row is stored instead so the operator status can count it.
/// The row holds no token and no rendered content and is never delivered.
/// Returns whether the notice was queued for delivery.
pub async fn enqueue_notice_in(
    tx: &mut Transaction<'_, Postgres>,
    draft: &EmailDraft,
) -> Result<bool, DbError> {
    if enqueue_in(tx, draft).await? == EnqueueOutcome::Queued {
        return Ok(true);
    }
    sqlx::query(
        "INSERT INTO outbound_emails(id,recipient,template,language,params,expires_at,actor_id,is_resend,status,created_at,available_at) \
         VALUES($1,$2,$3,$4,'{}'::jsonb,$5,$6,false,'suppressed',$7,$7)",
    )
    .bind(draft.id)
    .bind(&draft.recipient)
    .bind(&draft.template)
    .bind(&draft.language)
    .bind(draft.expires_at)
    .bind(draft.actor_id)
    .bind(draft.now)
    .execute(&mut **tx)
    .await?;
    Ok(false)
}

impl OrgTx {
    /// Enqueues mail in this organization's existing transaction.
    pub async fn enqueue_email(&mut self, draft: &EmailDraft) -> Result<EnqueueOutcome, DbError> {
        enqueue_in(&mut self.tx, draft).await
    }
}

/// Claims up to `limit` messages and reclaims expired leases.
pub async fn claim_batch(
    pool: &PgPool,
    now: OffsetDateTime,
    lease_until: OffsetDateTime,
    limit: i64,
) -> Result<Vec<EmailClaim>, DbError> {
    if limit <= 0 || lease_until <= now {
        return Ok(Vec::new());
    }
    let mut tx = pool.begin().await?;
    sqlx::query(
        "WITH due AS (SELECT id FROM outbound_emails WHERE expires_at<=$1 AND status IN ('queued','sending') \
         ORDER BY expires_at,id LIMIT 100 FOR UPDATE SKIP LOCKED) \
         UPDATE outbound_emails e SET status='expired',encrypted_token=NULL,claimed_until=NULL,lease_token=NULL \
         FROM due WHERE e.id=due.id",
    )
    .bind(now)
    .execute(&mut *tx)
    .await?;
    let exhausted: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT id,actor_id FROM outbound_emails WHERE expires_at>$1 AND status IN ('queued','sending') \
         AND ((status='queued' AND (attempts>=8 OR created_at<=$1-interval '24 hours')) \
           OR (status='sending' AND claimed_until<=$1 AND (attempts>=8 OR created_at<=$1-interval '24 hours'))) \
         ORDER BY id LIMIT 100 FOR UPDATE SKIP LOCKED",
    )
    .bind(now)
    .fetch_all(&mut *tx)
    .await?;
    for (id, actor_id) in exhausted {
        sqlx::query("UPDATE outbound_emails SET status='failed',encrypted_token=NULL,claimed_until=NULL,lease_token=NULL,failure_code='attempt_limit' WHERE id=$1")
            .bind(id).execute(&mut *tx).await?;
        record_in(
            &mut tx,
            &AuditCtx {
                actor_id,
                request_id: None,
            },
            &NewEvent::new("email.delivery_failed")
                .target("email", id)
                .detail("code", Detail::Code("attempt_limit")),
        )
        .await?;
    }
    let candidates: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM outbound_emails WHERE expires_at>$1 AND created_at>$1-interval '24 hours' \
         AND attempts<8 AND available_at<=$1 AND (status='queued' OR (status='sending' AND claimed_until<=$1)) \
         ORDER BY available_at,created_at,id LIMIT $2 FOR UPDATE SKIP LOCKED",
    )
    .bind(now)
    .bind(limit.min(100))
    .fetch_all(&mut *tx)
    .await?;
    let mut claims = Vec::with_capacity(candidates.len());
    for id in candidates {
        let lease_token = Uuid::new_v4();
        let claim = sqlx::query_as::<_, (String, String, String, Value, Option<Vec<u8>>, OffsetDateTime, i32)>(
            "UPDATE outbound_emails SET status='sending',attempts=attempts+1,claimed_until=$2,lease_token=$3 \
             WHERE id=$1 RETURNING recipient,template,language,params,encrypted_token,expires_at,attempts",
        )
        .bind(id)
        .bind(lease_until)
        .bind(lease_token)
        .fetch_one(&mut *tx)
        .await?;
        claims.push(EmailClaim {
            id,
            lease_token,
            recipient: claim.0,
            template: claim.1,
            language: claim.2,
            params: claim.3,
            encrypted_token: claim.4,
            expires_at: claim.5,
            attempts: claim.6,
        });
    }
    tx.commit().await?;
    Ok(claims)
}

/// Marks a message sent, but only for the currently held lease.
pub async fn mark_sent(
    pool: &PgPool,
    id: Uuid,
    lease_token: Uuid,
    now: OffsetDateTime,
) -> Result<bool, DbError> {
    let result = sqlx::query(
        "UPDATE outbound_emails SET status='sent', sent_at=$3, encrypted_token=NULL, \
         claimed_until=NULL, lease_token=NULL, failure_code=NULL \
         WHERE id=$1 AND status='sending' AND lease_token=$2",
    )
    .bind(id)
    .bind(lease_token)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Revalidates the lease and link capability immediately before SMTP send.
/// Token rows must still be the current unexpired capability for the same
/// recipient. Messages without a capability pass `None`.
pub async fn delivery_is_current(
    pool: &PgPool,
    id: Uuid,
    lease_token: Uuid,
    now: OffsetDateTime,
    token_hash: Option<&[u8; 32]>,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM outbound_emails e \
         WHERE e.id=$1 AND e.status='sending' AND e.lease_token=$2 \
           AND e.claimed_until>$3 AND e.expires_at>$3 AND e.attempts<=8 \
           AND CASE e.template \
             WHEN 'invitation' THEN $4::bytea IS NOT NULL AND EXISTS( \
               SELECT 1 FROM invitations i WHERE i.token_hash=$4 AND i.normalized_email=e.recipient \
                 AND i.accepted_at IS NULL AND i.revoked_at IS NULL AND i.invalidated_at IS NULL AND i.expires_at>$3) \
             WHEN 'password_reset' THEN $4::bytea IS NOT NULL AND EXISTS( \
               SELECT 1 FROM password_reset_tokens t JOIN users u ON u.id=t.user_id \
               WHERE t.token_hash=$4 AND u.normalized_email=e.recipient AND u.disabled_at IS NULL \
                 AND t.used_at IS NULL AND t.expires_at>$3) \
             WHEN 'email_verification' THEN $4::bytea IS NOT NULL AND EXISTS( \
               SELECT 1 FROM email_verification_tokens t JOIN users u ON u.id=t.user_id \
               WHERE t.token_hash=$4 AND u.normalized_email=e.recipient AND u.disabled_at IS NULL \
                 AND u.email_verified_at IS NULL AND t.expires_at>$3) \
             WHEN 'email_change' THEN $4::bytea IS NOT NULL AND EXISTS( \
               SELECT 1 FROM pending_email_changes c JOIN users u ON u.id=c.user_id \
               WHERE c.token_hash=$4 AND c.normalized_email=e.recipient AND u.disabled_at IS NULL \
                 AND c.expires_at>$3) \
             ELSE $4::bytea IS NULL \
           END)",
    )
    .bind(id)
    .bind(lease_token)
    .bind(now)
    .bind(token_hash.map(|hash| hash.as_slice()))
    .fetch_one(pool)
    .await?)
}

/// Records a delivery failure and schedules retry, or terminates the message.
pub async fn mark_failed(
    pool: &PgPool,
    id: Uuid,
    lease_token: Uuid,
    now: OffsetDateTime,
    retry_at: OffsetDateTime,
    permanent: bool,
    failure_code: &'static str,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let terminal: Option<(String, Option<Uuid>)> = sqlx::query_as(
        "UPDATE outbound_emails SET \
         status=CASE WHEN expires_at <= $3 THEN 'expired' WHEN $4 OR attempts >= 8 OR created_at <= $3 - interval '24 hours' THEN 'failed' ELSE 'queued' END, \
         available_at=CASE WHEN expires_at <= $3 OR $4 OR attempts >= 8 OR created_at <= $3 - interval '24 hours' THEN available_at ELSE $5 END, \
         encrypted_token=CASE WHEN expires_at <= $3 OR $4 OR attempts >= 8 OR created_at <= $3 - interval '24 hours' THEN NULL ELSE encrypted_token END, \
         failure_code=$6, claimed_until=NULL, lease_token=NULL \
         WHERE id=$1 AND status='sending' AND lease_token=$2 RETURNING status,actor_id",
    )
    .bind(id)
    .bind(lease_token)
    .bind(now)
    .bind(permanent)
    .bind(retry_at)
    .bind(failure_code)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((status, actor_id)) = terminal else {
        return Ok(false);
    };
    if status == "failed" {
        record_in(
            &mut tx,
            &AuditCtx {
                actor_id,
                request_id: None,
            },
            &NewEvent::new("email.delivery_failed")
                .target("email", id)
                .detail("code", Detail::Code(failure_code)),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Expires due queued or leased messages and erases their capabilities.
pub async fn expire_pending(pool: &PgPool, now: OffsetDateTime) -> Result<u64, DbError> {
    let result = sqlx::query(
        "WITH due AS (SELECT id FROM outbound_emails WHERE status IN ('queued','sending') AND expires_at<=$1 \
         ORDER BY expires_at,id LIMIT 100 FOR UPDATE SKIP LOCKED) \
         UPDATE outbound_emails e SET status='expired',encrypted_token=NULL,claimed_until=NULL,lease_token=NULL \
         FROM due WHERE e.id=due.id",
    )
    .bind(now)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Returns counts and timestamp for the operator status panel.
pub async fn status(pool: &PgPool) -> Result<EmailStatus, DbError> {
    let (last_success_at, failed_count, suppressed_count): (Option<OffsetDateTime>, i64, i64) =
        sqlx::query_as(
            "SELECT max(sent_at) FILTER (WHERE status='sent'), count(*) FILTER (WHERE status='failed'), \
             count(*) FILTER (WHERE status='suppressed') FROM outbound_emails",
        )
        .fetch_one(pool)
        .await?;
    Ok(EmailStatus {
        last_success_at,
        failed_count,
        suppressed_count,
    })
}

/// Deletes terminal rows (`sent`, `failed`, `expired`, `suppressed`) created
/// more than seven days ago by the database clock. This keeps the one-hour
/// rate windows and a week of failed and suppressed counts. The newest `sent`
/// row is kept so the operator status can still show the last success.
pub async fn purge_terminal(pool: &PgPool) -> Result<u64, DbError> {
    let result = sqlx::query(
        "DELETE FROM outbound_emails WHERE status IN ('sent','failed','expired','suppressed') \
         AND created_at < now() - interval '7 days' \
         AND id IS DISTINCT FROM (SELECT id FROM outbound_emails WHERE status='sent' \
                                  ORDER BY sent_at DESC, id LIMIT 1)",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
