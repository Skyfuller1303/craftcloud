//! Persisted TOTP state and short-lived login challenges.

use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::DbError;
use crate::audit::{AuditCtx, NewEvent, record_in};
use crate::device_sessions::NewDeviceSession;
use crate::sessions::NewSession;

/// Consecutive failed second-factor proofs after which an account is locked
/// until an operator resets its factor (NIST SP 800-63B, section 5.2.2).
pub const MAX_FAILED_ATTEMPTS: i32 = 100;

#[derive(Debug, Clone, Copy)]
pub enum VerifiedFactor {
    TotpStep(i64),
    RecoverySalt([u8; 16]),
}

/// Commits challenge consumption, replay prevention, and browser session
/// creation under one account lock. A concurrent reset waits and then revokes
/// the session, or commits first and makes this operation fail.
#[allow(clippy::too_many_arguments)] // Explicit proof bindings belong to one atomic operation.
pub async fn complete_browser_login(
    pool: &PgPool,
    id_hash: &[u8; 32],
    user_id: Uuid,
    client_address: &str,
    verified_hash: &str,
    secret_ciphertext: &[u8],
    factor: VerifiedFactor,
    new: &NewSession,
    audit: &AuditCtx,
) -> Result<Option<Uuid>, DbError> {
    match complete_browser_login_with_email(
        pool,
        id_hash,
        user_id,
        client_address,
        verified_hash,
        secret_ciphertext,
        factor,
        new,
        audit,
        false,
    )
    .await?
    {
        LoginOutcome::Created(id) => Ok(Some(id)),
        _ => Ok(None),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginOutcome {
    Created(Uuid),
    Rejected,
    EmailRateLimited,
}

#[allow(clippy::too_many_arguments)]
pub async fn complete_browser_login_with_email(
    pool: &PgPool,
    id_hash: &[u8; 32],
    user_id: Uuid,
    client_address: &str,
    verified_hash: &str,
    secret_ciphertext: &[u8],
    factor: VerifiedFactor,
    new: &NewSession,
    audit: &AuditCtx,
    mail_enabled: bool,
) -> Result<LoginOutcome, DbError> {
    let mut tx = pool.begin().await?;
    let account: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM users WHERE id=$1 AND password_hash=$2 AND disabled_at IS NULL FOR UPDATE",
    )
    .bind(user_id)
    .bind(verified_hash)
    .fetch_optional(&mut *tx)
    .await?;
    if account.is_none() {
        return Ok(LoginOutcome::Rejected);
    }
    let live: Option<Vec<u8>> = sqlx::query_scalar("SELECT id_hash FROM two_factor_login_challenges WHERE id_hash=$1 AND user_id=$2 AND login_kind='browser' AND client_address=$3 AND attempts<5 AND used_at IS NULL AND expires_at>$4 FOR UPDATE")
        .bind(id_hash.as_slice()).bind(user_id).bind(client_address).bind(new.now).fetch_optional(&mut *tx).await?;
    if live.is_none() {
        return Ok(LoginOutcome::Rejected);
    }
    let enabled: Option<Vec<u8>> = sqlx::query_scalar("SELECT secret_ciphertext FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL AND failed_attempts<$2 FOR UPDATE")
        .bind(user_id).bind(MAX_FAILED_ATTEMPTS).fetch_optional(&mut *tx).await?;
    if enabled.as_deref() != Some(secret_ciphertext) {
        return Ok(LoginOutcome::Rejected);
    }
    let recovery_used = match factor {
        VerifiedFactor::TotpStep(step) => sqlx::query("UPDATE user_two_factor SET last_accepted_step=$2,failed_attempts=0 WHERE user_id=$1 AND enabled_at IS NOT NULL AND (last_accepted_step IS NULL OR last_accepted_step<$2)")
            .bind(user_id).bind(step).execute(&mut *tx).await?.rows_affected() == 1,
        VerifiedFactor::RecoverySalt(salt) => sqlx::query("UPDATE user_two_factor_recovery_codes SET used_at=$3 WHERE user_id=$1 AND salt=$2 AND used_at IS NULL AND EXISTS(SELECT 1 FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL)")
            .bind(user_id).bind(salt.as_slice()).bind(new.now).execute(&mut *tx).await?.rows_affected() == 1,
    };
    if !recovery_used {
        return Ok(LoginOutcome::Rejected);
    }
    reset_failed_attempts(&mut tx, user_id).await?;
    let consumed = sqlx::query("UPDATE two_factor_login_challenges SET used_at=$3 WHERE id_hash=$1 AND user_id=$2 AND used_at IS NULL AND attempts<5 AND expires_at>$3")
        .bind(id_hash.as_slice()).bind(user_id).bind(new.now).execute(&mut *tx).await?.rows_affected() == 1;
    if !consumed {
        return Ok(LoginOutcome::Rejected);
    }
    let id = Uuid::new_v4();
    let inserted = sqlx::query("INSERT INTO sessions(id,user_id,token_hash,created_at,last_seen_at,absolute_expires_at) VALUES($1,$2,$3,$4,$4,$5)")
        .bind(id).bind(user_id).bind(new.token_hash.as_slice()).bind(new.now).bind(new.absolute_expires_at).execute(&mut *tx).await?.rows_affected() == 1;
    if !inserted {
        return Ok(LoginOutcome::Rejected);
    }
    let by = AuditCtx {
        actor_id: Some(user_id),
        request_id: audit.request_id.clone(),
    };
    record_in(
        &mut tx,
        &by,
        &NewEvent::new("auth.login").target("user", user_id),
    )
    .await?;
    if matches!(factor, VerifiedFactor::RecoverySalt(_)) {
        record_in(
            &mut tx,
            &by,
            &NewEvent::new("auth.recovery_code_used").target("user", user_id),
        )
        .await?;
    }
    if mail_enabled
        && crate::mail_security::enqueue_in(
            &mut tx,
            user_id,
            new.now,
            crate::mail_security::SecurityEvent::NewLogin,
        )
        .await?
            != crate::email_outbox::EnqueueOutcome::Queued
    {
        tx.rollback().await?;
        return Ok(LoginOutcome::EmailRateLimited);
    }
    tx.commit().await?;
    Ok(LoginOutcome::Created(id))
}

/// Desktop counterpart of [`complete_browser_login`].
#[allow(clippy::too_many_arguments)] // Explicit proof bindings belong to one atomic operation.
pub async fn complete_device_login(
    pool: &PgPool,
    id_hash: &[u8; 32],
    user_id: Uuid,
    client_address: &str,
    verified_hash: &str,
    secret_ciphertext: &[u8],
    factor: VerifiedFactor,
    new: &NewDeviceSession,
    audit: &AuditCtx,
) -> Result<Option<Uuid>, DbError> {
    match complete_device_login_with_email(
        pool,
        id_hash,
        user_id,
        client_address,
        verified_hash,
        secret_ciphertext,
        factor,
        new,
        audit,
        false,
    )
    .await?
    {
        LoginOutcome::Created(id) => Ok(Some(id)),
        _ => Ok(None),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn complete_device_login_with_email(
    pool: &PgPool,
    id_hash: &[u8; 32],
    user_id: Uuid,
    client_address: &str,
    verified_hash: &str,
    secret_ciphertext: &[u8],
    factor: VerifiedFactor,
    new: &NewDeviceSession,
    audit: &AuditCtx,
    mail_enabled: bool,
) -> Result<LoginOutcome, DbError> {
    let mut tx = pool.begin().await?;
    let account: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM users WHERE id=$1 AND password_hash=$2 AND disabled_at IS NULL FOR UPDATE",
    )
    .bind(user_id)
    .bind(verified_hash)
    .fetch_optional(&mut *tx)
    .await?;
    if account.is_none() {
        return Ok(LoginOutcome::Rejected);
    }
    let live: Option<Vec<u8>> = sqlx::query_scalar("SELECT id_hash FROM two_factor_login_challenges WHERE id_hash=$1 AND user_id=$2 AND login_kind='device' AND client_address=$3 AND attempts<5 AND used_at IS NULL AND expires_at>$4 FOR UPDATE")
        .bind(id_hash.as_slice()).bind(user_id).bind(client_address).bind(new.now).fetch_optional(&mut *tx).await?;
    if live.is_none() {
        return Ok(LoginOutcome::Rejected);
    }
    let enabled: Option<Vec<u8>> = sqlx::query_scalar("SELECT secret_ciphertext FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL AND failed_attempts<$2 FOR UPDATE")
        .bind(user_id).bind(MAX_FAILED_ATTEMPTS).fetch_optional(&mut *tx).await?;
    if enabled.as_deref() != Some(secret_ciphertext) {
        return Ok(LoginOutcome::Rejected);
    }
    let accepted = match factor {
        VerifiedFactor::TotpStep(step) => sqlx::query("UPDATE user_two_factor SET last_accepted_step=$2,failed_attempts=0 WHERE user_id=$1 AND enabled_at IS NOT NULL AND (last_accepted_step IS NULL OR last_accepted_step<$2)")
            .bind(user_id).bind(step).execute(&mut *tx).await?.rows_affected() == 1,
        VerifiedFactor::RecoverySalt(salt) => sqlx::query("UPDATE user_two_factor_recovery_codes SET used_at=$3 WHERE user_id=$1 AND salt=$2 AND used_at IS NULL AND EXISTS(SELECT 1 FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL)")
            .bind(user_id).bind(salt.as_slice()).bind(new.now).execute(&mut *tx).await?.rows_affected() == 1,
    };
    if !accepted {
        return Ok(LoginOutcome::Rejected);
    }
    reset_failed_attempts(&mut tx, user_id).await?;
    let consumed = sqlx::query("UPDATE two_factor_login_challenges SET used_at=$3 WHERE id_hash=$1 AND user_id=$2 AND used_at IS NULL AND attempts<5 AND expires_at>$3")
        .bind(id_hash.as_slice()).bind(user_id).bind(new.now).execute(&mut *tx).await?.rows_affected() == 1;
    if !consumed {
        return Ok(LoginOutcome::Rejected);
    }
    let id = Uuid::new_v4();
    let family = Uuid::new_v4();
    sqlx::query("INSERT INTO device_sessions(id,user_id,name,client_kind,client_id,refresh_hash,refresh_family,access_hash,access_expires_at,created_at,last_used_at,idle_expires_at,absolute_expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$10,$11,$12)")
        .bind(id).bind(user_id).bind(&new.name).bind(&new.client_kind).bind(&new.client_id).bind(new.refresh_hash.as_slice()).bind(family).bind(new.access_hash.as_slice()).bind(new.access_expires_at).bind(new.now).bind(new.idle_expires_at).bind(new.absolute_expires_at).execute(&mut *tx).await?;
    let by = AuditCtx {
        actor_id: Some(user_id),
        request_id: audit.request_id.clone(),
    };
    record_in(
        &mut tx,
        &by,
        &NewEvent::new("auth.device_login").target("device_session", id),
    )
    .await?;
    if matches!(factor, VerifiedFactor::RecoverySalt(_)) {
        record_in(
            &mut tx,
            &by,
            &NewEvent::new("auth.recovery_code_used").target("user", user_id),
        )
        .await?;
    }
    if mail_enabled
        && crate::mail_security::enqueue_in(
            &mut tx,
            user_id,
            new.now,
            crate::mail_security::SecurityEvent::NewLogin,
        )
        .await?
            != crate::email_outbox::EnqueueOutcome::Queued
    {
        tx.rollback().await?;
        return Ok(LoginOutcome::EmailRateLimited);
    }
    tx.commit().await?;
    Ok(LoginOutcome::Created(id))
}

#[derive(Debug, Clone, FromRow)]
pub struct SecretRecord {
    pub secret_ciphertext: Vec<u8>,
    pub last_accepted_step: Option<i64>,
    pub failed_attempts: i32,
    pub enabled_at: Option<OffsetDateTime>,
    pub pending_session_id: Option<Uuid>,
    pub pending_session_kind: Option<String>,
    pub pending_expires_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, FromRow)]
pub struct LoginChallenge {
    pub user_id: Uuid,
    pub password_hash_digest: Vec<u8>,
    pub login_kind: String,
    pub client_address: String,
    pub device_client_kind: Option<String>,
    pub device_client_id: Option<String>,
    pub device_name: Option<String>,
    pub attempts: i16,
    pub expires_at: OffsetDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct RecoveryHash {
    pub salt: Vec<u8>,
    pub code_hash: Vec<u8>,
}

/// Returns whether a factor is enabled and how many recovery codes remain.
pub async fn status(pool: &PgPool, user_id: Uuid) -> Result<(bool, i64), DbError> {
    let record: Option<(bool, i64)> = sqlx::query_as(
        "SELECT f.enabled_at IS NOT NULL, (SELECT count(*) FROM user_two_factor_recovery_codes r WHERE r.user_id=f.user_id AND r.used_at IS NULL) FROM user_two_factor f WHERE f.user_id=$1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(record.unwrap_or((false, 0)))
}

/// Loads encrypted secret state for internal verification.
pub async fn secret(pool: &PgPool, user_id: Uuid) -> Result<Option<SecretRecord>, DbError> {
    Ok(sqlx::query_as::<_, SecretRecord>(
        "SELECT secret_ciphertext,last_accepted_step,failed_attempts,enabled_at,pending_session_id,pending_session_kind,pending_expires_at FROM user_two_factor WHERE user_id=$1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?)
}

/// Result of [`begin_setup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupOutcome {
    /// A fresh pending secret was stored.
    Started,
    /// The account already has an enabled factor.
    AlreadyEnabled,
    /// Another session holds a live pending setup.
    Pending,
    /// The session is gone or the password changed since it was verified.
    SessionGone,
}

/// Starts or replaces a pending setup without disturbing an active factor.
pub async fn begin_setup(
    pool: &PgPool,
    user_id: Uuid,
    session_id: Uuid,
    session_kind: &str,
    verified_hash: &str,
    ciphertext: &[u8],
    now: OffsetDateTime,
) -> Result<SetupOutcome, DbError> {
    let mut tx = pool.begin().await?;
    let user: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM users WHERE id=$1 AND password_hash=$2 AND disabled_at IS NULL FOR UPDATE",
    )
    .bind(user_id)
    .bind(verified_hash)
    .fetch_optional(&mut *tx)
    .await?;
    if user.is_none() {
        return Ok(SetupOutcome::SessionGone);
    }
    let live_session: Option<Uuid> = if session_kind == "device" {
        sqlx::query_scalar("SELECT id FROM device_sessions WHERE id=$1 AND user_id=$2 AND revoked_at IS NULL AND absolute_expires_at>$3 FOR SHARE").bind(session_id).bind(user_id).bind(now).fetch_optional(&mut *tx).await?
    } else {
        sqlx::query_scalar("SELECT id FROM sessions WHERE id=$1 AND user_id=$2 AND revoked_at IS NULL AND absolute_expires_at>$3 FOR SHARE").bind(session_id).bind(user_id).bind(now).fetch_optional(&mut *tx).await?
    };
    if live_session.is_none() {
        return Ok(SetupOutcome::SessionGone);
    }
    let active: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL)",
    )
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    if active {
        tx.commit().await?;
        return Ok(SetupOutcome::AlreadyEnabled);
    }
    let changed = sqlx::query(
        "INSERT INTO user_two_factor(user_id,secret_ciphertext,enabled_at,last_accepted_step,pending_session_id,pending_session_kind,pending_expires_at,created_at) VALUES($1,$2,NULL,NULL,$3,$4,$5 + interval '10 minutes',$5) ON CONFLICT(user_id) DO UPDATE SET secret_ciphertext=EXCLUDED.secret_ciphertext,last_accepted_step=NULL,failed_attempts=0,pending_session_id=EXCLUDED.pending_session_id,pending_session_kind=EXCLUDED.pending_session_kind,pending_expires_at=EXCLUDED.pending_expires_at,created_at=EXCLUDED.created_at WHERE user_two_factor.enabled_at IS NULL AND (user_two_factor.pending_expires_at<=$5 OR (user_two_factor.pending_session_id=$3 AND user_two_factor.pending_session_kind=$4))",
    )
    .bind(user_id)
    .bind(ciphertext)
    .bind(session_id)
    .bind(session_kind)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed != 1 {
        tx.commit().await?;
        return Ok(SetupOutcome::Pending);
    }
    sqlx::query("DELETE FROM two_factor_login_challenges WHERE user_id=$1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(SetupOutcome::Started)
}

/// Enables a pending factor, stores the fresh recovery hashes, and retires
/// outstanding login challenges in one transaction.
#[allow(clippy::too_many_arguments)] // Session, enrollment and audit must be checked together.
pub async fn confirm_setup(
    pool: &PgPool,
    user_id: Uuid,
    session_id: Uuid,
    session_kind: &str,
    expected_ciphertext: &[u8],
    accepted_step: i64,
    hashes: &[RecoveryHash],
    now: OffsetDateTime,
    audit: &AuditCtx,
) -> Result<bool, DbError> {
    match confirm_setup_with_email(
        pool,
        user_id,
        session_id,
        session_kind,
        expected_ciphertext,
        accepted_step,
        hashes,
        now,
        audit,
        false,
    )
    .await?
    {
        FactorChangeOutcome::Changed => Ok(true),
        FactorChangeOutcome::Unchanged | FactorChangeOutcome::EmailRateLimited => Ok(false),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactorChangeOutcome {
    Changed,
    Unchanged,
    EmailRateLimited,
}

#[allow(clippy::too_many_arguments)]
pub async fn confirm_setup_with_email(
    pool: &PgPool,
    user_id: Uuid,
    session_id: Uuid,
    session_kind: &str,
    expected_ciphertext: &[u8],
    accepted_step: i64,
    hashes: &[RecoveryHash],
    now: OffsetDateTime,
    audit: &AuditCtx,
    mail_enabled: bool,
) -> Result<FactorChangeOutcome, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
    let live_session: Option<Uuid> = if session_kind == "device" {
        sqlx::query_scalar("SELECT id FROM device_sessions WHERE id=$1 AND user_id=$2 AND revoked_at IS NULL AND absolute_expires_at>$3 FOR SHARE").bind(session_id).bind(user_id).bind(now).fetch_optional(&mut *tx).await?
    } else {
        sqlx::query_scalar("SELECT id FROM sessions WHERE id=$1 AND user_id=$2 AND revoked_at IS NULL AND absolute_expires_at>$3 FOR SHARE").bind(session_id).bind(user_id).bind(now).fetch_optional(&mut *tx).await?
    };
    if live_session.is_none() {
        return Ok(FactorChangeOutcome::Unchanged);
    }
    let changed = sqlx::query(
        "UPDATE user_two_factor SET enabled_at=$5,last_accepted_step=$2,pending_session_id=NULL,pending_session_kind=NULL,pending_expires_at=NULL WHERE user_id=$1 AND enabled_at IS NULL AND pending_session_id=$3 AND pending_session_kind=$4 AND pending_expires_at>$5 AND secret_ciphertext=$6 RETURNING user_id",
    )
    .bind(user_id)
    .bind(accepted_step)
    .bind(session_id)
    .bind(session_kind)
    .bind(now)
    .bind(expected_ciphertext)
    .fetch_optional(&mut *tx)
    .await?
    .is_some();
    if changed {
        sqlx::query("UPDATE users SET security_version=security_version+1 WHERE id=$1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        for hash in hashes {
            sqlx::query("INSERT INTO user_two_factor_recovery_codes(user_id,salt,code_hash,created_at) VALUES($1,$2,$3,$4)")
                .bind(user_id)
                .bind(&hash.salt)
                .bind(&hash.code_hash)
                .bind(now)
                .execute(&mut *tx)
                .await?;
        }
        let device_audit = AuditCtx {
            actor_id: Some(user_id),
            request_id: audit.request_id.clone(),
        };
        if session_kind == "device" {
            sqlx::query(
                "UPDATE sessions SET revoked_at=$2 WHERE user_id=$1 AND revoked_at IS NULL",
            )
            .bind(user_id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            crate::locks::delete_for_user_tx(
                &mut tx,
                user_id,
                crate::locks::LockSessionKind::Browser,
                None,
            )
            .await?;
            crate::device_sessions::revoke_other_for_user_tx(
                &mut tx,
                user_id,
                session_id,
                now,
                &device_audit,
            )
            .await?;
        } else {
            sqlx::query("UPDATE sessions SET revoked_at=$3 WHERE user_id=$1 AND id<>$2 AND revoked_at IS NULL")
                .bind(user_id).bind(session_id).bind(now).execute(&mut *tx).await?;
            crate::locks::delete_for_user_tx(
                &mut tx,
                user_id,
                crate::locks::LockSessionKind::Browser,
                Some(session_id),
            )
            .await?;
            crate::device_sessions::revoke_all_for_user_tx(&mut tx, user_id, now, &device_audit)
                .await?;
        }
        let by = AuditCtx {
            actor_id: Some(user_id),
            request_id: audit.request_id.clone(),
        };
        record_in(
            &mut tx,
            &by,
            &NewEvent::new("auth.two_factor_enabled").target("user", user_id),
        )
        .await?;
    }
    sqlx::query("DELETE FROM two_factor_login_challenges WHERE user_id=$1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    if changed
        && mail_enabled
        && crate::mail_security::enqueue_in(
            &mut tx,
            user_id,
            now,
            crate::mail_security::SecurityEvent::TwoFactorEnabled,
        )
        .await?
            != crate::email_outbox::EnqueueOutcome::Queued
    {
        tx.rollback().await?;
        return Ok(FactorChangeOutcome::EmailRateLimited);
    }
    tx.commit().await?;
    Ok(if changed {
        FactorChangeOutcome::Changed
    } else {
        FactorChangeOutcome::Unchanged
    })
}

/// Replays a TOTP step and rotates recovery hashes as one transaction.
#[allow(clippy::too_many_arguments)] // Keep proof consumption and replacement in one transaction.
pub async fn replace_recovery_codes_verified(
    pool: &PgPool,
    user_id: Uuid,
    session_id: Uuid,
    session_kind: &str,
    expected_ciphertext: &[u8],
    proof: VerifiedFactor,
    hashes: &[RecoveryHash],
    now: OffsetDateTime,
    audit: &AuditCtx,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
    let live_session: Option<Uuid> = if session_kind == "device" {
        sqlx::query_scalar("SELECT id FROM device_sessions WHERE id=$1 AND user_id=$2 AND revoked_at IS NULL AND absolute_expires_at>$3 FOR SHARE").bind(session_id).bind(user_id).bind(now).fetch_optional(&mut *tx).await?
    } else {
        sqlx::query_scalar("SELECT id FROM sessions WHERE id=$1 AND user_id=$2 AND revoked_at IS NULL AND absolute_expires_at>$3 FOR SHARE").bind(session_id).bind(user_id).bind(now).fetch_optional(&mut *tx).await?
    };
    if live_session.is_none() {
        return Ok(false);
    }
    let current_ciphertext: Option<Vec<u8>> = sqlx::query_scalar("SELECT secret_ciphertext FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL AND failed_attempts<$2 FOR UPDATE")
        .bind(user_id).bind(MAX_FAILED_ATTEMPTS).fetch_optional(&mut *tx).await?;
    if current_ciphertext.as_deref() != Some(expected_ciphertext) {
        return Ok(false);
    }
    let accepted = match proof {
        VerifiedFactor::TotpStep(step) => sqlx::query("UPDATE user_two_factor SET last_accepted_step=$2 WHERE user_id=$1 AND enabled_at IS NOT NULL AND (last_accepted_step IS NULL OR last_accepted_step<$2)")
            .bind(user_id).bind(step).execute(&mut *tx).await?.rows_affected() == 1,
        VerifiedFactor::RecoverySalt(salt) => sqlx::query("UPDATE user_two_factor_recovery_codes SET used_at=$3 WHERE user_id=$1 AND salt=$2 AND used_at IS NULL AND EXISTS(SELECT 1 FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL)")
            .bind(user_id).bind(salt.as_slice()).bind(now).execute(&mut *tx).await?.rows_affected() == 1,
    };
    if !accepted {
        return Ok(false);
    }
    sqlx::query("DELETE FROM user_two_factor_recovery_codes WHERE user_id=$1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    for hash in hashes {
        sqlx::query("INSERT INTO user_two_factor_recovery_codes(user_id,salt,code_hash,created_at) VALUES($1,$2,$3,$4)")
            .bind(user_id).bind(&hash.salt).bind(&hash.code_hash).bind(now).execute(&mut *tx).await?;
    }
    let by = AuditCtx {
        actor_id: Some(user_id),
        request_id: audit.request_id.clone(),
    };
    record_in(
        &mut tx,
        &by,
        &NewEvent::new("auth.recovery_codes_regenerated").target("user", user_id),
    )
    .await?;
    if matches!(proof, VerifiedFactor::RecoverySalt(_)) {
        record_in(
            &mut tx,
            &by,
            &NewEvent::new("auth.recovery_code_used").target("user", user_id),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Lists unused salted code hashes for constant-time verification in auth.
pub async fn recovery_hashes(pool: &PgPool, user_id: Uuid) -> Result<Vec<RecoveryHash>, DbError> {
    Ok(sqlx::query_as::<_, RecoveryHash>(
        "SELECT salt,code_hash FROM user_two_factor_recovery_codes WHERE user_id=$1 AND used_at IS NULL ORDER BY salt LIMIT 10",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

/// Disables a user's factor, retires challenges, revokes other sessions and
/// records the security event in one account-locked transaction.
#[allow(clippy::too_many_arguments)] // Explicit online proof and offline operator reset context.
pub async fn reset_for_user(
    pool: &PgPool,
    user_id: Uuid,
    keep_session: Option<Uuid>,
    keep_session_kind: Option<&str>,
    verified_hash: Option<&str>,
    expected_ciphertext: Option<&[u8]>,
    proof: Option<VerifiedFactor>,
    now: OffsetDateTime,
    audit: &AuditCtx,
    action: &str,
) -> Result<bool, DbError> {
    match reset_for_user_with_email(
        pool,
        user_id,
        keep_session,
        keep_session_kind,
        verified_hash,
        expected_ciphertext,
        proof,
        now,
        audit,
        action,
        false,
    )
    .await?
    {
        FactorChangeOutcome::Changed => Ok(true),
        FactorChangeOutcome::Unchanged | FactorChangeOutcome::EmailRateLimited => Ok(false),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn reset_for_user_with_email(
    pool: &PgPool,
    user_id: Uuid,
    keep_session: Option<Uuid>,
    keep_session_kind: Option<&str>,
    verified_hash: Option<&str>,
    expected_ciphertext: Option<&[u8]>,
    proof: Option<VerifiedFactor>,
    now: OffsetDateTime,
    audit: &AuditCtx,
    action: &str,
    mail_enabled: bool,
) -> Result<FactorChangeOutcome, DbError> {
    let mut tx = pool.begin().await?;
    let user: Option<Uuid> = if let Some(hash) = verified_hash {
        sqlx::query_scalar("SELECT id FROM users WHERE id=$1 AND password_hash=$2 FOR UPDATE")
            .bind(user_id)
            .bind(hash)
            .fetch_optional(&mut *tx)
            .await?
    } else {
        sqlx::query_scalar("SELECT id FROM users WHERE id=$1 FOR UPDATE")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?
    };
    if user.is_none() {
        return Ok(FactorChangeOutcome::Unchanged);
    }
    if let Some(session_id) = keep_session {
        let live: Option<Uuid> = if keep_session_kind == Some("device") {
            sqlx::query_scalar("SELECT id FROM device_sessions WHERE id=$1 AND user_id=$2 AND revoked_at IS NULL AND absolute_expires_at>$3 FOR SHARE").bind(session_id).bind(user_id).bind(now).fetch_optional(&mut *tx).await?
        } else {
            sqlx::query_scalar("SELECT id FROM sessions WHERE id=$1 AND user_id=$2 AND revoked_at IS NULL AND absolute_expires_at>$3 FOR SHARE").bind(session_id).bind(user_id).bind(now).fetch_optional(&mut *tx).await?
        };
        if live.is_none() {
            return Ok(FactorChangeOutcome::Unchanged);
        }
    }
    if let Some(expected_ciphertext) = expected_ciphertext {
        // A locked account is only released by the operator path (no expected ciphertext).
        let current_ciphertext: Option<Vec<u8>> = sqlx::query_scalar("SELECT secret_ciphertext FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL AND failed_attempts<$2 FOR UPDATE")
            .bind(user_id).bind(MAX_FAILED_ATTEMPTS).fetch_optional(&mut *tx).await?;
        if current_ciphertext.as_deref() != Some(expected_ciphertext) {
            return Ok(FactorChangeOutcome::Unchanged);
        }
    }
    if let Some(proof) = proof {
        let accepted = match proof {
            VerifiedFactor::TotpStep(step) => sqlx::query("UPDATE user_two_factor SET last_accepted_step=$2 WHERE user_id=$1 AND enabled_at IS NOT NULL AND (last_accepted_step IS NULL OR last_accepted_step<$2)")
                .bind(user_id).bind(step).execute(&mut *tx).await?.rows_affected() == 1,
            VerifiedFactor::RecoverySalt(salt) => sqlx::query("UPDATE user_two_factor_recovery_codes SET used_at=$3 WHERE user_id=$1 AND salt=$2 AND used_at IS NULL AND EXISTS(SELECT 1 FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL)")
                .bind(user_id).bind(salt.as_slice()).bind(now).execute(&mut *tx).await?.rows_affected() == 1,
        };
        if !accepted {
            return Ok(FactorChangeOutcome::Unchanged);
        }
    }
    let changed = sqlx::query("DELETE FROM user_two_factor WHERE user_id=$1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        > 0;
    sqlx::query("DELETE FROM two_factor_login_challenges WHERE user_id=$1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    if changed {
        sqlx::query("UPDATE users SET security_version=security_version+1 WHERE id=$1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        crate::figcraft::enqueue_all_user_revocations(&mut tx, user_id, "security_changed", now)
            .await?;
        if let Some(session_id) = keep_session {
            if keep_session_kind == Some("device") {
                sqlx::query(
                    "UPDATE sessions SET revoked_at=$2 WHERE user_id=$1 AND revoked_at IS NULL",
                )
                .bind(user_id)
                .bind(now)
                .execute(&mut *tx)
                .await?;
                crate::locks::delete_for_user_tx(
                    &mut tx,
                    user_id,
                    crate::locks::LockSessionKind::Browser,
                    None,
                )
                .await?;
                crate::device_sessions::revoke_other_for_user_tx(
                    &mut tx,
                    user_id,
                    session_id,
                    now,
                    &AuditCtx {
                        actor_id: audit.actor_id,
                        request_id: audit.request_id.clone(),
                    },
                )
                .await?;
            } else {
                sqlx::query("UPDATE sessions SET revoked_at=$3 WHERE user_id=$1 AND id<>$2 AND revoked_at IS NULL").bind(user_id).bind(session_id).bind(now).execute(&mut *tx).await?;
                crate::locks::delete_for_user_tx(
                    &mut tx,
                    user_id,
                    crate::locks::LockSessionKind::Browser,
                    Some(session_id),
                )
                .await?;
                let device_audit = AuditCtx {
                    actor_id: audit.actor_id,
                    request_id: audit.request_id.clone(),
                };
                crate::device_sessions::revoke_all_for_user_tx(
                    &mut tx,
                    user_id,
                    now,
                    &device_audit,
                )
                .await?;
            }
        } else {
            sqlx::query(
                "UPDATE sessions SET revoked_at=$2 WHERE user_id=$1 AND revoked_at IS NULL",
            )
            .bind(user_id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            crate::locks::delete_for_user_tx(
                &mut tx,
                user_id,
                crate::locks::LockSessionKind::Browser,
                None,
            )
            .await?;
            let device_audit = AuditCtx {
                actor_id: audit.actor_id,
                request_id: audit.request_id.clone(),
            };
            crate::device_sessions::revoke_all_for_user_tx(&mut tx, user_id, now, &device_audit)
                .await?;
        }
        record_in(
            &mut tx,
            audit,
            &NewEvent::new(action).target("user", user_id),
        )
        .await?;
        if matches!(proof, Some(VerifiedFactor::RecoverySalt(_))) {
            record_in(
                &mut tx,
                audit,
                &NewEvent::new("auth.recovery_code_used").target("user", user_id),
            )
            .await?;
        }
    }
    if changed && mail_enabled {
        let event = if action == "auth.two_factor_disabled" {
            crate::mail_security::SecurityEvent::TwoFactorDisabled
        } else {
            crate::mail_security::SecurityEvent::TwoFactorReset
        };
        if crate::mail_security::enqueue_in(&mut tx, user_id, now, event).await?
            != crate::email_outbox::EnqueueOutcome::Queued
        {
            tx.rollback().await?;
            return Ok(FactorChangeOutcome::EmailRateLimited);
        }
    }
    tx.commit().await?;
    Ok(if changed {
        FactorChangeOutcome::Changed
    } else {
        FactorChangeOutcome::Unchanged
    })
}

/// Inserts a challenge only if the account remains enabled and owns an active factor.
#[allow(clippy::too_many_arguments)]
pub async fn create_challenge(
    pool: &PgPool,
    id_hash: &[u8; 32],
    user_id: Uuid,
    password_hash_digest: &[u8; 32],
    login_kind: &str,
    client_address: &str,
    device_client_kind: Option<&str>,
    device_client_id: Option<&str>,
    device_name: Option<&str>,
    now: OffsetDateTime,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let enabled: Option<bool> =
        sqlx::query_scalar("SELECT disabled_at IS NULL FROM users WHERE id=$1 FOR UPDATE")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    if enabled != Some(true) {
        tx.commit().await?;
        return Ok(false);
    }
    let factor_active: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL)",
    )
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    if !factor_active {
        tx.commit().await?;
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO two_factor_login_challenges(id_hash,user_id,password_hash_digest,login_kind,client_address,device_client_kind,device_client_id,device_name,created_at,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$9 + interval '5 minutes')",
    )
    .bind(id_hash.as_slice())
    .bind(user_id)
    .bind(password_hash_digest.as_slice())
    .bind(login_kind)
    .bind(client_address)
    .bind(device_client_kind)
    .bind(device_client_id)
    .bind(device_name)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Fetches a live challenge after checking login kind, address and attempt cap.
pub async fn challenge(
    pool: &PgPool,
    id_hash: &[u8; 32],
    login_kind: &str,
    client_address: &str,
    now: OffsetDateTime,
) -> Result<Option<LoginChallenge>, DbError> {
    Ok(sqlx::query_as::<_, LoginChallenge>(
        "SELECT user_id,password_hash_digest,login_kind,client_address,device_client_kind,device_client_id,device_name,attempts,expires_at FROM two_factor_login_challenges WHERE id_hash=$1 AND login_kind=$2 AND client_address=$3 AND attempts<5 AND used_at IS NULL AND expires_at>$4",
    )
    .bind(id_hash.as_slice())
    .bind(login_kind)
    .bind(client_address)
    .bind(now)
    .fetch_optional(pool)
    .await?)
}

/// Counts an invalid attempt only if the challenge is still live.
pub async fn failed_attempt(
    pool: &PgPool,
    id_hash: &[u8; 32],
    now: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query("UPDATE two_factor_login_challenges SET attempts=attempts+1 WHERE id_hash=$1 AND attempts<5 AND used_at IS NULL AND expires_at>$2")
        .bind(id_hash.as_slice())
        .bind(now)
        .execute(pool)
        .await?;
    Ok(())
}

/// Deletes challenges past their five-minute lifetime.
pub async fn delete_expired_challenges(pool: &PgPool, now: OffsetDateTime) -> Result<u64, DbError> {
    Ok(sqlx::query(
        "DELETE FROM two_factor_login_challenges WHERE expires_at<$1 OR used_at IS NOT NULL",
    )
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected())
}

async fn reset_failed_attempts(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: Uuid,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE user_two_factor SET failed_attempts=0 WHERE user_id=$1 AND failed_attempts<>0",
    )
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Counts one failed second-factor proof against the account under its row
/// lock. Returns true when this failure reached [`MAX_FAILED_ATTEMPTS`] and the
/// account is now locked until an operator resets the factor.
pub async fn record_failed_proof(
    pool: &PgPool,
    user_id: Uuid,
    audit: &AuditCtx,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
    let count: Option<i32> = sqlx::query_scalar("UPDATE user_two_factor SET failed_attempts=failed_attempts+1 WHERE user_id=$1 AND enabled_at IS NOT NULL AND failed_attempts<$2 RETURNING failed_attempts")
        .bind(user_id)
        .bind(MAX_FAILED_ATTEMPTS)
        .fetch_optional(&mut *tx)
        .await?;
    let locked = count == Some(MAX_FAILED_ATTEMPTS);
    if locked {
        let by = AuditCtx {
            actor_id: Some(user_id),
            request_id: audit.request_id.clone(),
        };
        record_in(
            &mut tx,
            &by,
            &NewEvent::new("auth.two_factor_locked").target("user", user_id),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(locked)
}
