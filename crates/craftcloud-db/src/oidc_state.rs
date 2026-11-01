//! Bounded, encrypted, single-use OIDC login state. Plain secrets never enter persistence.
use crate::DbError;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;
#[derive(sqlx::FromRow)]
pub struct StateRecord {
    pub org_id: Uuid,
    pub config_version: i64,
    pub binding_hash: Vec<u8>,
    pub secrets_ciphertext: Vec<u8>,
}
#[derive(sqlx::FromRow)]
pub struct PendingRecord {
    pub org_id: Uuid,
    pub config_version: i64,
    pub binding_hash: Vec<u8>,
    pub payload_ciphertext: Vec<u8>,
    pub attempts: i32,
    pub expires_at: OffsetDateTime,
}
/// Inserts a ten-minute state after cleaning expired rows, with durable admission limits.
#[allow(clippy::too_many_arguments)]
pub async fn insert_state(
    pool: &PgPool,
    hash: &[u8; 32],
    launch: &[u8; 32],
    org: Uuid,
    version: i64,
    binding: &[u8; 32],
    ciphertext: &[u8],
    now: OffsetDateTime,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(734981021)")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM oidc_transactions WHERE state_hash IN(SELECT state_hash FROM oidc_transactions WHERE expires_at<=$1 LIMIT 1000)").bind(now).execute(&mut *tx).await?;
    let admitted:bool=sqlx::query_scalar("SELECT (SELECT count(*) FROM oidc_transactions)<10000 AND (SELECT count(*) FROM oidc_transactions WHERE org_id=$1)<1000").bind(org).fetch_one(&mut *tx).await?;
    if !admitted {
        return Ok(false);
    }
    sqlx::query("INSERT INTO oidc_transactions(state_hash,org_id,config_version,binding_hash,secrets_ciphertext,created_at,expires_at,launch_hash) VALUES($1,$2,$3,$4,$5,$6,$6+interval '10 minutes',$7)").bind(hash.as_slice()).bind(org).bind(version).bind(binding.as_slice()).bind(ciphertext).bind(now).bind(launch.as_slice()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}
/// Consumes valid state before contacting the provider; replay never reaches token exchange.
pub async fn consume_state(
    pool: &PgPool,
    hash: &[u8; 32],
    binding: &[u8; 32],
    now: OffsetDateTime,
) -> Result<Option<StateRecord>, DbError> {
    Ok(sqlx::query_as("DELETE FROM oidc_transactions WHERE state_hash=$1 AND binding_hash=$2 AND expires_at>$3 RETURNING org_id,config_version,binding_hash,secrets_ciphertext").bind(hash.as_slice()).bind(binding.as_slice()).bind(now).fetch_optional(pool).await?)
}
/// Creates bounded follow-up state for password/TOTP proof or native PKCE redemption.
#[allow(clippy::too_many_arguments)]
pub async fn insert_pending(
    conn: &mut PgConnection,
    hash: &[u8; 32],
    org: Uuid,
    version: i64,
    binding: &[u8; 32],
    ciphertext: &[u8],
    now: OffsetDateTime,
    seconds: i64,
) -> Result<bool, DbError> {
    sqlx::query("SELECT pg_advisory_xact_lock(734981022)")
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM oidc_pending_logins WHERE token_hash IN(SELECT token_hash FROM oidc_pending_logins WHERE expires_at<=$1 LIMIT 1000)").bind(now).execute(&mut *conn).await?;
    let admitted:bool=sqlx::query_scalar("SELECT (SELECT count(*) FROM oidc_pending_logins)<10000 AND (SELECT count(*) FROM oidc_pending_logins WHERE org_id=$1)<1000").bind(org).fetch_one(&mut *conn).await?;
    if !admitted {
        return Ok(false);
    }
    sqlx::query("INSERT INTO oidc_pending_logins(token_hash,org_id,config_version,binding_hash,payload_ciphertext,created_at,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(hash.as_slice()).bind(org).bind(version).bind(binding.as_slice()).bind(ciphertext).bind(now).bind(now+time::Duration::seconds(seconds.clamp(1,600))).execute(conn).await?;
    Ok(true)
}
/// Reads pending state; the final transaction must recheck and consume it.
pub async fn pending(
    pool: &PgPool,
    hash: &[u8; 32],
    now: OffsetDateTime,
) -> Result<Option<PendingRecord>, DbError> {
    Ok(sqlx::query_as("SELECT org_id,config_version,binding_hash,payload_ciphertext,attempts,expires_at FROM oidc_pending_logins WHERE token_hash=$1 AND expires_at>$2 AND attempts<5").bind(hash.as_slice()).bind(now).fetch_optional(pool).await?)
}
/// Locks pending state inside the organization's transaction.
pub async fn pending_in(
    conn: &mut PgConnection,
    hash: &[u8; 32],
    now: OffsetDateTime,
) -> Result<Option<PendingRecord>, DbError> {
    Ok(sqlx::query_as("SELECT org_id,config_version,binding_hash,payload_ciphertext,attempts,expires_at FROM oidc_pending_logins WHERE token_hash=$1 AND expires_at>$2 AND attempts<5 FOR UPDATE").bind(hash.as_slice()).bind(now).fetch_optional(conn).await?)
}
/// Counts every proof attempt durably, including failures across server processes.
pub async fn attempted(conn: &mut PgConnection, hash: &[u8; 32]) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE oidc_pending_logins SET attempts=attempts+1 WHERE token_hash=$1 AND attempts<5",
    )
    .bind(hash.as_slice())
    .execute(conn)
    .await?;
    Ok(())
}
/// Removes a completed capability in the same transaction as session creation.
pub async fn consume_pending(conn: &mut PgConnection, hash: &[u8; 32]) -> Result<(), DbError> {
    sqlx::query("DELETE FROM oidc_pending_logins WHERE token_hash=$1")
        .bind(hash.as_slice())
        .execute(conn)
        .await?;
    Ok(())
}

/// Claims the browser launch exactly once and only then binds the browser cookie.
pub async fn claim_launch(
    pool: &PgPool,
    launch: &[u8; 32],
    binding: &[u8; 32],
    now: OffsetDateTime,
) -> Result<Option<StateRecord>, DbError> {
    Ok(sqlx::query_as("UPDATE oidc_transactions SET browser_claimed=true,binding_hash=$2 WHERE launch_hash=$1 AND NOT browser_claimed AND expires_at>$3 RETURNING org_id,config_version,binding_hash,secrets_ciphertext").bind(launch.as_slice()).bind(binding.as_slice()).bind(now).fetch_optional(pool).await?)
}
