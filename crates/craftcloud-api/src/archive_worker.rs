//! Bounded AC-12 archive publication worker.
//!
//! The caller supplies configured storage and crypto. This slice deliberately
//! does not start a runtime task; startup guards and server wiring remain owned
//! by the composition root.

use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use craftcloud_db::{DbError, maintenance as db_maintenance, members::OrgTx};
use craftcloud_storage::{
    ArchiveCrypto, LocalStore, ObjectKey, ObjectStore, StorageError, WrappedDataKey,
};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

const CANDIDATE_LIMIT: i64 = 32;
const ORG_WINDOW: i64 = 256;
const VERIFY_HEAD_TIMEOUT: Duration = Duration::from_secs(15);
const VERIFY_IO_TIMEOUT: Duration = Duration::from_secs(30);
const VERIFY_COOLDOWN: time::Duration = time::Duration::hours(1);
const VERIFY_CLAIM_TTL: time::Duration = time::Duration::minutes(15);
const DEFAULT_VERIFY_BYTES_PER_RUN: u64 = 4 * 1024 * 1024;
const DEFAULT_VERIFY_TIME_PER_RUN: Duration = Duration::from_secs(5);
static ORG_CURSOR: OnceLock<Mutex<Option<Uuid>>> = OnceLock::new();

/// Per-maintenance-run budget for authenticated archive verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveVerifyBudget {
    /// Maximum plaintext bytes authenticated in one run.
    pub max_bytes: u64,
    /// Maximum wall-clock time spent verifying in one run.
    pub max_time: Duration,
}

impl Default for ArchiveVerifyBudget {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_VERIFY_BYTES_PER_RUN,
            max_time: DEFAULT_VERIFY_TIME_PER_RUN,
        }
    }
}

impl ArchiveVerifyBudget {
    /// Creates a valid budget. At least one complete AEAD block is needed to
    /// make progress; zero time cannot perform a provider operation.
    #[must_use]
    pub fn new(max_bytes: u64, max_time: Duration) -> Option<Self> {
        (max_bytes >= craftcloud_storage::CHUNK_BYTES as u64 && !max_time.is_zero()).then_some(
            Self {
                max_bytes,
                max_time,
            },
        )
    }
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
    archive_batch_with_verify_budget(
        pool,
        local,
        archive,
        crypto,
        now,
        archive_after,
        ArchiveVerifyBudget::default(),
    )
    .await
}

/// Same as [`archive_batch`] with an explicit per-run archive verification budget.
pub async fn archive_batch_with_verify_budget<S: ObjectStore>(
    pool: &PgPool,
    local: &LocalStore,
    archive: &S,
    crypto: &ArchiveCrypto,
    now: OffsetDateTime,
    archive_after: Duration,
    verify_budget: ArchiveVerifyBudget,
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
    drop(_run_lock);
    verify_archive_sample(pool, archive, crypto, now, verify_budget).await?;
    Ok(report)
}

/// Authenticates archive blocks within the configured per-run byte and time
/// budgets. The durable claim and block offset let later runs resume safely.
/// Provider errors stay bounded before entering audit state.
async fn verify_archive_sample<S: ObjectStore>(
    pool: &PgPool,
    archive: &S,
    crypto: &ArchiveCrypto,
    now: OffsetDateTime,
    budget: ArchiveVerifyBudget,
) -> Result<(), DbError> {
    let stale_before = now
        .checked_sub(VERIFY_CLAIM_TTL)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let due_before = now
        .checked_sub(VERIFY_COOLDOWN)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let candidate =
        db_maintenance::archive_verify_claim(pool, now, stale_before, due_before).await?;
    let Some(candidate) = candidate else {
        return Ok(());
    };
    let result = verify_archive_candidate(archive, crypto, &candidate, budget).await;
    match result {
        ArchiveVerification::Progress(offset) => {
            save_archive_verify_progress(pool, &candidate, offset, false, now).await?;
        }
        ArchiveVerification::Full(offset) => {
            save_archive_verify_progress(pool, &candidate, offset, true, now).await?;
        }
        ArchiveVerification::Failed(error) => {
            db_maintenance::archive_verify_failed(pool, &candidate, error, now).await?;
        }
    }
    Ok(())
}

async fn save_archive_verify_progress(
    pool: &PgPool,
    candidate: &db_maintenance::ArchiveVerifyCandidate,
    offset: u64,
    complete: bool,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    let offset = i64::try_from(offset).map_err(|_| DbError::Internal)?;
    let _ = db_maintenance::archive_verify_progress(pool, candidate, offset, complete, now).await?;
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
    Progress(u64),
    Full(u64),
    Failed(&'static str),
}

fn verification_timeout(
    offset: u64,
    remaining_budget: Duration,
    io_timeout: Duration,
) -> ArchiveVerification {
    if remaining_budget <= io_timeout {
        ArchiveVerification::Progress(offset)
    } else {
        ArchiveVerification::Failed("ARCHIVE_VERIFY_TIMEOUT")
    }
}

fn verification_completion(
    offset: u64,
    size: u64,
    elapsed: Duration,
    budget: Duration,
) -> ArchiveVerification {
    if offset == size && elapsed < budget {
        ArchiveVerification::Full(offset)
    } else {
        ArchiveVerification::Progress(offset)
    }
}

async fn verify_archive_candidate<S: ObjectStore>(
    archive: &S,
    crypto: &ArchiveCrypto,
    candidate: &db_maintenance::ArchiveVerifyCandidate,
    budget: ArchiveVerifyBudget,
) -> ArchiveVerification {
    let key = match ObjectKey::parse(&candidate.archive_object_key) {
        Ok(key) => key,
        Err(_) => return ArchiveVerification::Failed("ARCHIVE_METADATA_INVALID"),
    };
    let size = match u64::try_from(candidate.size_bytes) {
        Ok(size) => size,
        Err(_) => return ArchiveVerification::Failed("ARCHIVE_METADATA_INVALID"),
    };
    let mut offset = match u64::try_from(candidate.verified_offset) {
        Ok(offset)
            if offset <= size
                && (offset == size || offset % craftcloud_storage::CHUNK_BYTES as u64 == 0) =>
        {
            offset
        }
        _ => return ArchiveVerification::Failed("ARCHIVE_METADATA_INVALID"),
    };
    let start = Instant::now();
    let expected_len = match craftcloud_storage::encrypted_len(size) {
        Ok(len) => len,
        Err(_) => return ArchiveVerification::Failed("ARCHIVE_METADATA_INVALID"),
    };
    let remaining_head_time = budget.max_time.saturating_sub(start.elapsed());
    let head_timeout = remaining_head_time.min(VERIFY_HEAD_TIMEOUT);
    let head = match tokio::time::timeout(head_timeout, archive.head(&key)).await {
        Err(_) => return verification_timeout(offset, remaining_head_time, VERIFY_HEAD_TIMEOUT),
        Ok(Err(error)) => return ArchiveVerification::Failed(archive_verify_error(error.into())),
        Ok(Ok(head)) => head,
    };
    if head.len != expected_len {
        return ArchiveVerification::Failed("ARCHIVE_INTEGRITY_FAILED");
    }
    let envelope = match WrappedDataKey::from_bytes(&candidate.archive_wrapped_key) {
        Ok(envelope) => envelope,
        Err(_) => return ArchiveVerification::Failed("ARCHIVE_METADATA_INVALID"),
    };
    if candidate.sha256.len() != 32 {
        return ArchiveVerification::Failed("ARCHIVE_METADATA_INVALID");
    }
    if size == 0 {
        let remaining_budget = budget.max_time.saturating_sub(start.elapsed());
        let io_timeout = remaining_budget.min(VERIFY_IO_TIMEOUT);
        let checked = tokio::time::timeout(
            io_timeout,
            crate::archive_transport::open_archive_range(
                archive,
                crypto,
                &key,
                &envelope,
                candidate.id,
                size,
                None,
            ),
        )
        .await;
        return match checked {
            Err(_) => verification_timeout(0, remaining_budget, VERIFY_IO_TIMEOUT),
            Ok(Err(error)) => ArchiveVerification::Failed(archive_verify_error(error)),
            Ok(Ok(_reader)) => verification_completion(0, 0, start.elapsed(), budget.max_time),
        };
    }
    let mut bytes = 0_u64;
    while offset < size && start.elapsed() < budget.max_time {
        let remaining_budget = budget.max_time.saturating_sub(start.elapsed());
        if remaining_budget.is_zero() {
            break;
        }
        let io_timeout = remaining_budget.min(VERIFY_IO_TIMEOUT);
        let end = offset
            .saturating_add(craftcloud_storage::CHUNK_BYTES as u64)
            .min(size);
        if bytes.saturating_add(end - offset) > budget.max_bytes {
            break;
        }
        let range = craftcloud_storage::ByteRange {
            start: offset,
            end: Some(end),
        };
        let verified = tokio::time::timeout(io_timeout, async {
            let mut reader = crate::archive_transport::open_archive_range(
                archive,
                crypto,
                &key,
                &envelope,
                candidate.id,
                size,
                Some(range),
            )
            .await?;
            let mut total = 0_u64;
            while let Some(chunk) = reader.next_chunk().await? {
                total += chunk.len() as u64;
            }
            Ok::<u64, crate::archive_transport::ArchiveTransportError>(total)
        })
        .await;
        let block_bytes = match verified {
            Err(_) => return verification_timeout(offset, remaining_budget, VERIFY_IO_TIMEOUT),
            Ok(Err(error)) => return ArchiveVerification::Failed(archive_verify_error(error)),
            Ok(Ok(block_bytes)) if block_bytes == end - offset => block_bytes,
            Ok(Ok(_)) => return ArchiveVerification::Failed("ARCHIVE_INTEGRITY_FAILED"),
        };
        offset = end;
        bytes = bytes.saturating_add(block_bytes);
        if start.elapsed() >= budget.max_time || bytes >= budget.max_bytes {
            break;
        }
    }
    verification_completion(offset, size, start.elapsed(), budget.max_time)
}

#[cfg(test)]
mod archive_verify_policy_tests {
    use super::*;

    #[test]
    fn verification_defaults_are_bounded() {
        let budget = ArchiveVerifyBudget::default();
        assert_eq!(budget.max_bytes, 4 * 1024 * 1024);
        assert_eq!(budget.max_time, Duration::from_secs(5));
    }

    #[test]
    fn expired_run_budget_keeps_checkpoint_but_provider_timeout_is_error() {
        assert!(matches!(
            verification_timeout(64 * 1024, Duration::from_secs(5), Duration::from_secs(30)),
            ArchiveVerification::Progress(65_536),
        ));
        assert!(matches!(
            verification_completion(
                256 * 1024,
                256 * 1024,
                Duration::from_secs(5),
                Duration::from_secs(5)
            ),
            ArchiveVerification::Progress(262_144),
        ));
        assert!(matches!(
            verification_completion(
                256 * 1024,
                256 * 1024,
                Duration::from_secs(4),
                Duration::from_secs(5)
            ),
            ArchiveVerification::Full(262_144),
        ));
        assert!(matches!(
            verification_timeout(64 * 1024, Duration::from_secs(31), Duration::from_secs(30)),
            ArchiveVerification::Failed("ARCHIVE_VERIFY_TIMEOUT"),
        ));
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
    craftcloud_storage::crash_point("archive-db-committed", 0);

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
    craftcloud_storage::crash_point("archive-local-deleted", 0);
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
