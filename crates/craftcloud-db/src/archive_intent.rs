//! Fenced persistence for an archive object's key envelope before publication.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::{DbError, maintenance::ARCHIVE_DUE, members::OrgTx};

type ArchiveIntentRow = (String, Option<Uuid>, Option<String>, Option<Vec<u8>>);

/// Finds stale archive claims whose source is no longer due. The worker must
/// hold the global maintenance lock and revalidate each token in `OrgTx`.
pub async fn stale_archive_not_due(
    pool: &sqlx::PgPool,
    cutoff: OffsetDateTime,
    stale_before: OffsetDateTime,
    orgs: &[Uuid],
    limit: i64,
) -> Result<Vec<(Uuid, Uuid, Uuid)>, DbError> {
    Ok(sqlx::query_as(&format!(
        "SELECT b.org_id, b.id, b.archive_claim_token FROM blobs b \
         WHERE b.state = 'committed' AND b.tier_state = 'archiving' \
           AND b.archive_claim_token IS NOT NULL AND b.archive_claimed_at <= $2 \
           AND b.org_id = ANY($3) AND NOT ({ARCHIVE_DUE}) \
         ORDER BY b.archive_claimed_at, b.id LIMIT $4"
    ))
    .bind(cutoff)
    .bind(stale_before)
    .bind(orgs)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

impl OrgTx {
    /// Releases an expired claim only if the blob remains not due. The
    /// durable intent stays attached so a later attempt reuses its exact key.
    pub async fn maint_archive_release_stale_not_due(
        &mut self,
        blob_id: Uuid,
        claim_token: Uuid,
        cutoff: OffsetDateTime,
        stale_before: OffsetDateTime,
    ) -> Result<bool, DbError> {
        let row: Option<(String, Option<Uuid>, Option<OffsetDateTime>)> = sqlx::query_as(
            "SELECT tier_state, archive_claim_token, archive_claimed_at FROM blobs \
             WHERE id=$1 AND org_id=$2 FOR UPDATE",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .fetch_optional(&mut *self.tx)
        .await?;
        let Some((tier_state, token, claimed_at)) = row else {
            return Ok(false);
        };
        if tier_state != "archiving"
            || token != Some(claim_token)
            || claimed_at.is_none_or(|claimed_at| claimed_at > stale_before)
        {
            return Ok(false);
        }
        let due: bool = sqlx::query_scalar(&format!(
            "SELECT {ARCHIVE_DUE} FROM blobs b WHERE b.id=$2 AND b.org_id=$3"
        ))
        .bind(cutoff)
        .bind(blob_id)
        .bind(self.org_id)
        .fetch_optional(&mut *self.tx)
        .await?
        .unwrap_or(true);
        if due {
            return Ok(false);
        }
        let result = sqlx::query(
            "UPDATE blobs SET tier_state='local', archive_claim_token=NULL, archive_claimed_at=NULL \
             WHERE id=$1 AND org_id=$2 AND tier_state='archiving' AND archive_claim_token=$3 \
               AND archive_claimed_at <= $4",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .bind(claim_token)
        .bind(stale_before)
        .execute(&mut *self.tx)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Persists the immutable destination key and wrapped data key while this
    /// worker owns the `archiving` claim. Repeated calls may confirm the same
    /// intent; an existing different or partial intent is never replaced.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn maint_archive_intent(
        &mut self,
        blob_id: Uuid,
        claim_token: Uuid,
        archive_object_key: &str,
        archive_wrapped_key: &[u8],
    ) -> Result<bool, DbError> {
        let row: Option<ArchiveIntentRow> = sqlx::query_as(
            "SELECT tier_state, archive_claim_token, archive_object_key, archive_wrapped_key \
             FROM blobs WHERE id = $1 AND org_id = $2 FOR UPDATE",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .fetch_optional(&mut *self.tx)
        .await?;
        let Some((tier_state, token, existing_key, existing_wrapped)) = row else {
            return Ok(false);
        };
        if tier_state != "archiving" || token != Some(claim_token) {
            return Ok(false);
        }
        match (existing_key, existing_wrapped) {
            (None, None) => {
                let done = sqlx::query(
                    "UPDATE blobs SET archive_object_key = $4, archive_wrapped_key = $5, \
                         archive_verified_at = NULL \
                     WHERE id = $1 AND org_id = $2 AND archive_claim_token = $3 \
                       AND tier_state = 'archiving' AND archive_object_key IS NULL \
                       AND archive_wrapped_key IS NULL",
                )
                .bind(blob_id)
                .bind(self.org_id)
                .bind(claim_token)
                .bind(archive_object_key)
                .bind(archive_wrapped_key)
                .execute(&mut *self.tx)
                .await?;
                Ok(done.rows_affected() == 1)
            }
            (Some(key), Some(wrapped)) => {
                Ok(key == archive_object_key && wrapped == archive_wrapped_key)
            }
            _ => Ok(false),
        }
    }
}
