//! Bounded AC-12 archive publication worker.
//!
//! The caller supplies configured storage and crypto. This slice deliberately
//! does not start a runtime task; startup guards and server wiring remain owned
//! by the composition root.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use craftcloud_db::{DbError, maintenance as db_maintenance, members::OrgTx};
use craftcloud_storage::{
    ArchiveCrypto, LocalStore, ObjectKey, ObjectStore, StorageError, WrappedDataKey,
};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

const CANDIDATE_LIMIT: i64 = 32;
const ORG_WINDOW: i64 = 256;
const VERIFY_COOLDOWN: time::Duration = time::Duration::hours(1);
const VERIFY_HEAD_TIMEOUT: Duration = Duration::from_secs(15);
const VERIFY_READBACK_TIMEOUT: Duration = Duration::from_secs(30);
const FULL_VERIFY_MAX_BYTES: u64 = 64 * 1024;
static ORG_CURSOR: OnceLock<Mutex<Option<Uuid>>> = OnceLock::new();
static ARCHIVE_VERIFY_TIMES: OnceLock<Mutex<HashMap<Uuid, OffsetDateTime>>> = OnceLock::new();

fn full_verify_enabled(size: u64) -> bool {
    size <= FULL_VERIFY_MAX_BYTES
}

/// Work performed by one bounded archive batch.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveBatch {
    /// Candidates claimed and attempted.
    pub attempted: u64,
    /// Local copies deleted after verified archive commits.
    pub archived: u64,
    /// Candidate work that failed and was scheduled for retry.
    pub retried: u64,
}

/// Claims and archives at most 32 due blobs while holding the global
/// maintenance lock shared with backup and garbage collection.
///
/// `archive_after` is the delay after supersession. Errors are reduced to
/// bounded database codes; provider messages and object contents are never
/// logged or persisted.
///
/// # Errors
///
/// Database failures abort the batch. Per-blob storage or integrity failures
/// are recorded for retry and do not abort remaining candidates.
pub async fn archive_batch<S: ObjectStore>(
    pool: &PgPool,
    local: &LocalStore,
    archive: &S,
    crypto: &ArchiveCrypto,
    now: OffsetDateTime,
    archive_after: Duration,
) -> Result<ArchiveBatch, DbError> {
    let Some(_run_lock) = db_maintenance::try_lock(pool).await? else {
        return Ok(ArchiveBatch::default());
    };
    let seconds = i64::try_from(archive_after.as_secs()).unwrap_or(i64::MAX);
    let delay = time::Duration::seconds(seconds);
    let Some(cutoff) = now.checked_sub(delay) else {
        return Ok(ArchiveBatch::default());
    };
    let cursor = ORG_CURSOR
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .to_owned();
    let mut orgs = db_maintenance::org_window(pool, cursor, ORG_WINDOW).await?;
    if orgs.is_empty() && cursor.is_some() {
        orgs = db_maintenance::org_window(pool, None, ORG_WINDOW).await?;
    }
    if orgs.is_empty() {
        return Ok(ArchiveBatch::default());
    }
    *ORG_CURSOR
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = orgs.last().copied();
    let stale_before = now
        .checked_sub(time::Duration::minutes(15))
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    for (org_id, blob_id, claim_token) in craftcloud_db::archive_intent::stale_archive_not_due(
        pool,
        cutoff,
        stale_before,
        &orgs,
        CANDIDATE_LIMIT,
    )
    .await?
    {
        let Some(mut tx) = OrgTx::begin(pool, org_id).await? else {
            continue;
        };
        tx.maint_archive_release_stale_not_due(blob_id, claim_token, cutoff, stale_before)
            .await?;
        tx.commit().await?;
    }
    let candidates =
        db_maintenance::archive_candidates(pool, cutoff, now, &orgs, &[], CANDIDATE_LIMIT).await?;
    let mut report = ArchiveBatch::default();
    let pending = db_maintenance::pending_archive_local_deletes(pool, now, CANDIDATE_LIMIT).await?;
    for (org_id, blob_id, claim_token) in pending {
        if complete_local_delete(pool, local, org_id, blob_id, claim_token, now).await? {
            report.archived += 1;
        }
    }
    for (org_id, blob_id) in candidates {
        report.attempted += 1;
        match archive_one(pool, local, archive, crypto, org_id, blob_id, cutoff, now).await? {
            WorkResult::Archived => report.archived += 1,
            WorkResult::Retried => report.retried += 1,
            WorkResult::Skipped => {}
        }
    }
    verify_archive_sample(pool, archive, crypto, now).await?;
    Ok(report)
}

/// Checks at most one archived object per batch. Full readback is capped at
/// 64 KiB; larger objects get a bounded head/length check. The process-local
/// cooldown prevents a single archived object from being checked every worker
/// tick. Provider errors stay bounded before entering audit state.
async fn verify_archive_sample<S: ObjectStore>(
    pool: &PgPool,
    archive: &S,
    crypto: &ArchiveCrypto,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    let candidates = db_maintenance::archive_verify_candidates(pool, CANDIDATE_LIMIT).await?;
    let candidate = {
        let mut checked = ARCHIVE_VERIFY_TIMES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let cutoff = now.checked_sub(VERIFY_COOLDOWN);
        checked.retain(|_, at| cutoff.is_none_or(|cutoff| *at > cutoff));
        candidates
            .into_iter()
            .find(|candidate| !checked.contains_key(&candidate.id))
    };
    let Some(candidate) = candidate else {
        return Ok(());
    };
    ARCHIVE_VERIFY_TIMES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(candidate.id, now);
    match verify_archive_candidate(archive, crypto, &candidate).await {
        ArchiveVerification::Full => {
            db_maintenance::record_archive_verification(pool, candidate.id, None, now).await?;
        }
        ArchiveVerification::HeadOnly => {
            db_maintenance::record_archive_head_check(pool, candidate.id, now).await?;
        }
        ArchiveVerification::Failed(error) => {
            db_maintenance::record_archive_verification(pool, candidate.id, Some(error), now)
                .await?;
        }
    }
    Ok(())
}

fn archive_verify_error(error: crate::archive_transport::ArchiveTransportError) -> &'static str {
    use crate::archive_transport::ArchiveTransportError as E;
    use StorageError as S;
    match error {
        E::Storage(S::NotFound) => "ARCHIVE_MISSING",
        E::Storage(_) => "ARCHIVE_VERIFY_FAILED",
        E::Crypto(_) | E::Integrity => "ARCHIVE_INTEGRITY_FAILED",
    }
}

enum ArchiveVerification {
    Full,
    HeadOnly,
    Failed(&'static str),
}

async fn verify_archive_candidate<S: ObjectStore>(
    archive: &S,
    crypto: &ArchiveCrypto,
    candidate: &db_maintenance::ArchiveVerifyCandidate,
) -> ArchiveVerification {
    let key = match ObjectKey::parse(&candidate.archive_object_key) {
        Ok(key) => key,
        Err(_) => return ArchiveVerification::Failed("ARCHIVE_METADATA_INVALID"),
    };
    let size = match u64::try_from(candidate.size_bytes) {
        Ok(size) => size,
        Err(_) => return ArchiveVerification::Failed("ARCHIVE_METADATA_INVALID"),
    };
    let expected_len = match craftcloud_storage::encrypted_len(size) {
        Ok(len) => len,
        Err(_) => return ArchiveVerification::Failed("ARCHIVE_METADATA_INVALID"),
    };
    let head = match tokio::time::timeout(VERIFY_HEAD_TIMEOUT, archive.head(&key)).await {
        Err(_) => return ArchiveVerification::Failed("ARCHIVE_VERIFY_TIMEOUT"),
        Ok(Err(error)) => return ArchiveVerification::Failed(archive_verify_error(error.into())),
        Ok(Ok(head)) => head,
    };
    if head.len != expected_len {
        return ArchiveVerification::Failed("ARCHIVE_INTEGRITY_FAILED");
    }
    if !full_verify_enabled(size) {
        return ArchiveVerification::HeadOnly;
    }
    let envelope = match WrappedDataKey::from_bytes(&candidate.archive_wrapped_key) {
        Ok(envelope) => envelope,
        Err(_) => return ArchiveVerification::Failed("ARCHIVE_METADATA_INVALID"),
    };
    let expected_sha: [u8; 32] = match candidate.sha256.as_slice().try_into() {
        Ok(digest) => digest,
        Err(_) => return ArchiveVerification::Failed("ARCHIVE_METADATA_INVALID"),
    };
    match tokio::time::timeout(
        VERIFY_READBACK_TIMEOUT,
        crate::archive_transport::verify_archive(
            archive,
            crypto,
            &key,
            &envelope,
            candidate.id,
            size,
            &expected_sha,
        ),
    )
    .await
    {
        Err(_) => ArchiveVerification::Failed("ARCHIVE_VERIFY_TIMEOUT"),
        Ok(Err(error)) => ArchiveVerification::Failed(archive_verify_error(error)),
        Ok(Ok(())) => ArchiveVerification::Full,
    }
}

#[cfg(test)]
mod archive_verify_policy_tests {
    use super::*;

    #[test]
    fn full_readback_stops_at_sixty_four_kibibytes() {
        assert!(full_verify_enabled(FULL_VERIFY_MAX_BYTES));
        assert!(!full_verify_enabled(FULL_VERIFY_MAX_BYTES + 1));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkResult {
    Archived,
    Retried,
    Skipped,
}

#[allow(clippy::too_many_arguments)]
async fn archive_one<S: ObjectStore>(
    pool: &PgPool,
    local: &LocalStore,
    archive: &S,
    crypto: &ArchiveCrypto,
    org_id: Uuid,
    blob_id: Uuid,
    cutoff: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<WorkResult, DbError> {
    let claim_token = Uuid::new_v4();
    let stale_before = now
        .checked_sub(time::Duration::minutes(15))
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let Some(mut tx) = OrgTx::begin(pool, org_id).await? else {
        return Ok(WorkResult::Skipped);
    };
    let Some(claim) = tx
        .maint_archive_claim(blob_id, cutoff, now, stale_before, claim_token)
        .await?
    else {
        tx.commit().await?;
        return Ok(WorkResult::Skipped);
    };
    tx.commit().await?;

    let local_key = match ObjectKey::parse(&claim.object_key) {
        Ok(key) => key,
        Err(_) => {
            retry(
                pool,
                org_id,
                blob_id,
                claim_token,
                now,
                "INVALID_LOCAL_KEY",
                None,
                None,
            )
            .await?;
            return Ok(WorkResult::Retried);
        }
    };
    let expected_sha: [u8; 32] = match claim.sha256.as_slice().try_into() {
        Ok(digest) => digest,
        Err(_) => {
            retry(
                pool,
                org_id,
                blob_id,
                claim_token,
                now,
                "INVALID_DIGEST",
                None,
                None,
            )
            .await?;
            return Ok(WorkResult::Retried);
        }
    };
    let size = match u64::try_from(claim.size_bytes) {
        Ok(size) => size,
        Err(_) => {
            retry(
                pool,
                org_id,
                blob_id,
                claim_token,
                now,
                "INVALID_SIZE",
                None,
                None,
            )
            .await?;
            return Ok(WorkResult::Retried);
        }
    };
    // Persist the destination and envelope before any archive-store operation.
    // Retries keep this exact pair; immutable storage will never be overwritten.
    let intent = match (&claim.archive_object_key, &claim.archive_wrapped_key) {
        (None, None) => {
            let archive_key = ObjectKey::generate();
            let (envelope, _data_key) = match crypto.create_data_key(blob_id, size) {
                Ok(value) => value,
                Err(_) => {
                    retry(
                        pool,
                        org_id,
                        blob_id,
                        claim_token,
                        now,
                        "CRYPTO_FAILED",
                        None,
                        None,
                    )
                    .await?;
                    return Ok(WorkResult::Retried);
                }
            };
            let Some(mut tx) = OrgTx::begin(pool, org_id).await? else {
                return Ok(WorkResult::Skipped);
            };
            let saved = tx
                .maint_archive_intent(
                    blob_id,
                    claim_token,
                    archive_key.as_str(),
                    &envelope.to_bytes(),
                )
                .await?;
            tx.commit().await?;
            if !saved {
                return Ok(WorkResult::Skipped);
            }
            Ok((archive_key, envelope))
        }
        (Some(key), Some(wrapped)) => {
            match (ObjectKey::parse(key), WrappedDataKey::from_bytes(wrapped)) {
                (Ok(archive_key), Ok(envelope)) => Ok((archive_key, envelope)),
                _ => Err(()),
            }
        }
        _ => Err(()),
    };
    let (archive_key, envelope) = match intent {
        Ok(intent) => intent,
        Err(()) => {
            retry(
                pool,
                org_id,
                blob_id,
                claim_token,
                now,
                "INVALID_ARCHIVE_INTENT",
                None,
                None,
            )
            .await?;
            return Ok(WorkResult::Retried);
        }
    };
    let uploaded = crate::archive_transport::upload_intent_verified(
        local,
        archive,
        crypto,
        blob_id,
        &local_key,
        &archive_key,
        &envelope,
        size,
        &expected_sha,
    )
    .await;
    let copy = match uploaded {
        Ok(copy) => copy,
        Err(error) => {
            let code = match error {
                crate::archive_transport::ArchiveTransportError::Integrity => "INTEGRITY_FAILED",
                crate::archive_transport::ArchiveTransportError::Crypto(_) => "CRYPTO_FAILED",
                crate::archive_transport::ArchiveTransportError::Storage(_) => "STORAGE_FAILED",
            };
            retry(pool, org_id, blob_id, claim_token, now, code, None, None).await?;
            return Ok(WorkResult::Retried);
        }
    };
    let archive_key_text = copy.object_key.as_str().to_owned();
    let envelope = copy.envelope.to_bytes();

    // Commit the verified archive reference before touching local bytes.
    let Some(mut tx) = OrgTx::begin(pool, org_id).await? else {
        return Ok(WorkResult::Skipped);
    };
    let finish = tx
        .maint_archive_finish(
            blob_id,
            claim_token,
            &archive_key_text,
            &envelope,
            cutoff,
            now,
        )
        .await?;
    tx.commit().await?;
    if finish != db_maintenance::ArchiveFinish::Archived {
        return Ok(WorkResult::Skipped);
    }

    complete_local_delete(pool, local, org_id, blob_id, claim_token, now)
        .await
        .map(|deleted| {
            if deleted {
                WorkResult::Archived
            } else {
                WorkResult::Skipped
            }
        })
}

/// Retries post-commit local cleanup, including claims left by a crashed run.
/// The caller must hold the global maintenance lock.
async fn complete_local_delete(
    pool: &PgPool,
    local: &LocalStore,
    org_id: Uuid,
    blob_id: Uuid,
    claim_token: Uuid,
    now: OffsetDateTime,
) -> Result<bool, DbError> {
    // Keep org/blob locks across local deletion and completion. A concurrent
    // restore cannot make this blob head between the recheck and deletion.
    let Some(mut tx) = OrgTx::begin(pool, org_id).await? else {
        return Ok(false);
    };
    let target = tx
        .maint_archive_local_delete_target(blob_id, claim_token)
        .await?;
    let Some(target) = target else {
        tx.commit().await?;
        return Ok(false);
    };
    let local_key = match ObjectKey::parse(&target) {
        Ok(key) => key,
        Err(_) => {
            let retry_at = now.checked_add(time::Duration::minutes(5)).unwrap_or(now);
            tx.maint_archive_local_delete_failed(
                blob_id,
                claim_token,
                "INVALID_LOCAL_KEY",
                retry_at,
            )
            .await?;
            tx.commit().await?;
            return Ok(false);
        }
    };
    if !matches!(
        local.delete(&local_key).await,
        Ok(()) | Err(StorageError::NotFound)
    ) {
        let retry_at = now.checked_add(time::Duration::minutes(5)).unwrap_or(now);
        tx.maint_archive_local_delete_failed(blob_id, claim_token, "LOCAL_DELETE_FAILED", retry_at)
            .await?;
        tx.commit().await?;
        return Ok(false);
    }
    if !tx
        .maint_archive_local_delete_finish(blob_id, claim_token, now)
        .await?
    {
        // Deletion already happened under valid locks; failed bookkeeping is
        // surfaced as a DB error so recovery can observe pending state.
        return Err(DbError::Internal);
    }
    tx.commit().await?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
async fn retry(
    pool: &PgPool,
    org_id: Uuid,
    blob_id: Uuid,
    claim_token: Uuid,
    now: OffsetDateTime,
    error_code: &str,
    archive_key: Option<&str>,
    wrapped_key: Option<&[u8]>,
) -> Result<(), DbError> {
    let Some(mut tx) = OrgTx::begin(pool, org_id).await? else {
        return Ok(());
    };
    // Attempts are not exposed in ArchiveClaim; fixed bounded delay prevents
    // hot-looping while preserving automatic crash recovery via stale claims.
    let retry_at = now.checked_add(time::Duration::minutes(1)).unwrap_or(now);
    let _ = tx
        .maint_archive_retry(
            blob_id,
            claim_token,
            error_code,
            retry_at,
            archive_key,
            wrapped_key,
        )
        .await?;
    tx.commit().await
}
