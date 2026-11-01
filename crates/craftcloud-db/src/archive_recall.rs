//! Bounded inventory queries for operator initiated archive recalls.
//!
//! The returned rows are hints only. A caller must acquire the global
//! maintenance lock, check local capacity, and then claim each row through
//! `OrgTx::maint_archive_recall_claim` before touching either store.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::{DbError, PgPool};

/// Archived blob selected for a recall attempt. Contains no tenant metadata or
/// object keys; those are fetched only after a fenced tenant-scoped claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
pub struct RecallCandidate {
    /// Owning organization, needed to acquire its transaction.
    pub org_id: Uuid,
    /// Blob to be recalled.
    pub blob_id: Uuid,
    /// Plaintext bytes needed locally.
    pub size_bytes: i64,
}

/// Lists a bounded set of committed archived blobs eligible for recall.
/// Existing active claims are excluded; stale claims are eligible for fenced
/// crash recovery. This query does not mutate state.
///
/// # Errors
///
/// Database failures only.
pub async fn candidates(
    pool: &PgPool,
    org_id: Option<Uuid>,
    stale_before: OffsetDateTime,
    limit: i64,
) -> Result<Vec<RecallCandidate>, DbError> {
    if limit <= 0 {
        return Ok(Vec::new());
    }
    candidates_after(pool, org_id, stale_before, None, limit).await
}

/// Lists one page after the last `(org_id, blob_id)` key. The caller keeps the
/// maintenance lock for the full inventory and recall, preventing archive GC
/// and archive publication from changing the candidate set between pages.
pub async fn candidates_after(
    pool: &PgPool,
    org_id: Option<Uuid>,
    stale_before: OffsetDateTime,
    after: Option<(Uuid, Uuid)>,
    limit: i64,
) -> Result<Vec<RecallCandidate>, DbError> {
    if limit <= 0 {
        return Ok(Vec::new());
    }
    let (after_org, after_blob) = after.map_or((None, None), |(org, blob)| (Some(org), Some(blob)));
    Ok(sqlx::query_as(
        "SELECT org_id, id AS blob_id, size_bytes FROM blobs \
         WHERE state = 'committed' AND tier_state IN ('archived', 'recalling') \
           AND archive_object_key IS NOT NULL AND archive_wrapped_key IS NOT NULL \
           AND (archive_claim_token IS NULL OR archive_claimed_at <= $1) \
           AND ($2::uuid IS NULL OR org_id = $2) \
           AND ($3::uuid IS NULL OR (org_id, id) > ($3, $4)) \
         ORDER BY org_id, id LIMIT $5",
    )
    .bind(stale_before)
    .bind(org_id)
    .bind(after_org)
    .bind(after_blob)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Sums the entire eligible inventory before any recall claims or writes.
/// Overflow or a database error fails closed.
pub async fn total_bytes(
    pool: &PgPool,
    org_id: Option<Uuid>,
    stale_before: OffsetDateTime,
) -> Result<i64, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(sum(size_bytes), 0)::bigint FROM blobs \
         WHERE state = 'committed' AND tier_state IN ('archived', 'recalling') \
           AND archive_object_key IS NOT NULL AND archive_wrapped_key IS NOT NULL \
           AND (archive_claim_token IS NULL OR archive_claimed_at <= $1) \
           AND ($2::uuid IS NULL OR org_id = $2)",
    )
    .bind(stale_before)
    .bind(org_id)
    .fetch_one(pool)
    .await?)
}
