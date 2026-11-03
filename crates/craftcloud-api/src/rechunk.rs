//! Offline conversion of immutable whole-blob history to local chunk manifests.

use std::collections::HashSet;

use craftcloud_db::members::OrgTx;
use craftcloud_db::rechunk::{self, Candidate, PrepareOutcome};
use craftcloud_db::{PgPool, maintenance};
use craftcloud_storage::{
    ArchiveCrypto, ArchiveStore, LocalStore, ObjectKey, ObjectStore, WrappedDataKey, WriteSink,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::uploads::{local_chunk_matches, stage_chunk_manifest};

const PAGE: i64 = 32;

/// Optional source for whole blobs whose local object was already removed.
pub struct RechunkArchive<'a> {
    pub store: &'a ArchiveStore,
    pub crypto: &'a ArchiveCrypto,
}

/// Result of an operator's dry run or conversion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RechunkReport {
    pub dry_run: bool,
    pub whole_versions: u64,
    pub already_chunked_versions: u64,
    pub converted_blobs: u64,
    pub converted_versions: u64,
    pub logical_bytes: u64,
}

/// Rechunks committed whole blobs, or inventories them without writes.
pub async fn run(
    pool: &PgPool,
    local: &LocalStore,
    file_id: Option<Uuid>,
    dry_run: bool,
    identity_keys: &[[u8; 32]],
    archive: Option<RechunkArchive<'_>>,
) -> Result<RechunkReport, String> {
    let (whole, chunked, bytes) = rechunk::inventory(pool, file_id)
        .await
        .map_err(|_| "cannot read rechunk inventory".to_owned())?;
    if dry_run {
        return Ok(RechunkReport {
            dry_run: true,
            whole_versions: u64::try_from(whole).unwrap_or(0),
            already_chunked_versions: u64::try_from(chunked).unwrap_or(0),
            logical_bytes: u64::try_from(bytes).unwrap_or(0),
            ..RechunkReport::default()
        });
    }
    if whole > 0 && identity_keys.is_empty() {
        return Err(
            "storage rechunk requires CRAFTCLOUD_SECRET_KEY_FILE for chunk identity".to_owned(),
        );
    }
    let Some(_lock) = maintenance::try_lock(pool)
        .await
        .map_err(|_| "cannot acquire rechunk maintenance lock".to_owned())?
    else {
        return Err(
            "storage rechunk is busy with backup, maintenance, archive, or another rechunk"
                .to_owned(),
        );
    };

    let mut report = RechunkReport {
        whole_versions: u64::try_from(whole).unwrap_or(0),
        already_chunked_versions: u64::try_from(chunked).unwrap_or(0),
        logical_bytes: 0,
        ..RechunkReport::default()
    };
    let mut after = None;
    loop {
        let candidates = rechunk::candidates(pool, file_id, after, PAGE)
            .await
            .map_err(|_| "cannot scan whole blobs for rechunk".to_owned())?;
        let Some(last) = candidates.last().map(|candidate| candidate.blob_id) else {
            break;
        };
        after = Some(last);
        for candidate in candidates {
            if candidate.files_referencing_blob != 1 {
                return Err(format!(
                    "blob {} is shared across files; rechunk refused",
                    candidate.blob_id
                ));
            }
            let converted =
                convert_one(pool, local, &candidate, identity_keys, archive.as_ref()).await?;
            if let Some((versions, bytes)) = converted {
                report.converted_blobs += 1;
                report.converted_versions += versions;
                let logical_bytes = bytes
                    .checked_mul(versions)
                    .ok_or_else(|| "rechunk logical byte total overflow".to_owned())?;
                report.logical_bytes = report
                    .logical_bytes
                    .checked_add(logical_bytes)
                    .ok_or_else(|| "rechunk byte total overflow".to_owned())?;
            }
        }
    }
    Ok(report)
}

async fn convert_one(
    pool: &PgPool,
    local: &LocalStore,
    candidate: &Candidate,
    identity_keys: &[[u8; 32]],
    archive: Option<&RechunkArchive<'_>>,
) -> Result<Option<(u64, u64)>, String> {
    let size = u64::try_from(candidate.size_bytes)
        .map_err(|_| "whole blob has an invalid size".to_owned())?;
    let sha: [u8; 32] = candidate
        .sha256
        .as_slice()
        .try_into()
        .map_err(|_| "whole blob has an invalid SHA-256".to_owned())?;
    let original_key = ObjectKey::parse(&candidate.object_key)
        .map_err(|_| "whole blob has an invalid object key".to_owned())?;
    let mut temporary_source = None;
    let source_key = match candidate.tier_state.as_str() {
        "local" | "archiving" | "recalling" if local.verify(&original_key, &sha).await.is_ok() => {
            original_key.clone()
        }
        "archived" | "local" | "archiving" | "recalling" => {
            let archive = archive.ok_or_else(|| "whole blob has no verified local copy; configure archive storage and the server key before rechunking".to_owned())?;
            let key_text = candidate
                .archive_object_key
                .as_deref()
                .ok_or_else(|| "archived whole blob is missing archive metadata".to_owned())?;
            let archive_key = ObjectKey::parse(key_text)
                .map_err(|_| "archived whole blob has an invalid archive key".to_owned())?;
            let wrapped = candidate.archive_wrapped_key.as_deref().ok_or_else(|| {
                "archived whole blob is missing its encryption envelope".to_owned()
            })?;
            let envelope = WrappedDataKey::from_bytes(wrapped)
                .map_err(|_| "archived whole blob has an invalid encryption envelope".to_owned())?;
            if envelope.blob_id() != candidate.blob_id || envelope.plaintext_len() != size {
                return Err("archived whole blob envelope does not match the database".to_owned());
            }
            let temp_key = ObjectKey::generate();
            let mut writer = local
                .begin_write_as(temp_key.clone(), size)
                .await
                .map_err(|_| "cannot stage archived whole blob locally".to_owned())?;
            let mut reader = crate::archive_transport::open_archive_range(
                archive.store,
                archive.crypto,
                &archive_key,
                &envelope,
                candidate.blob_id,
                size,
                None,
            )
            .await
            .map_err(|_| "cannot authenticate archived whole blob".to_owned())?;
            let mut digest = Sha256::new();
            let mut written = 0_u64;
            while let Some(block) = reader
                .next_chunk()
                .await
                .map_err(|_| "cannot read archived whole blob".to_owned())?
            {
                written = written
                    .checked_add(block.len() as u64)
                    .ok_or_else(|| "archived whole blob size overflow".to_owned())?;
                digest.update(&block);
                writer
                    .write(&block)
                    .await
                    .map_err(|_| "cannot stage archived whole blob locally".to_owned())?;
            }
            if written != size || <[u8; 32]>::from(digest.finalize()) != sha {
                return Err("archived whole blob failed size or SHA-256 verification".to_owned());
            }
            local
                .finalize_as(writer, temp_key.clone(), size, &sha)
                .await
                .map_err(|_| "cannot publish verified archived whole blob locally".to_owned())?;
            temporary_source = Some(temp_key.clone());
            temp_key
        }
        _ => return Err("whole blob has an unknown storage tier".to_owned()),
    };

    let result = stage_and_publish(
        pool,
        local,
        candidate,
        &source_key,
        size,
        sha,
        identity_keys,
    )
    .await;
    if let Some(key) = temporary_source
        && let Err(error) = local.delete(&key).await
    {
        tracing::warn!(%error, "cannot remove temporary archived source after rechunk");
    }
    result
}

async fn stage_and_publish(
    pool: &PgPool,
    local: &LocalStore,
    candidate: &Candidate,
    source_key: &ObjectKey,
    size: u64,
    sha: [u8; 32],
    identity_keys: &[[u8; 32]],
) -> Result<Option<(u64, u64)>, String> {
    let (manifest, candidate_keys) = stage_chunk_manifest(
        pool,
        local,
        source_key,
        size,
        sha,
        candidate.file_id,
        identity_keys,
    )
    .await
    .map_err(|_| "cannot verify and stage whole blob chunks".to_owned())?;
    let mut tx = OrgTx::begin(pool, candidate.org_id)
        .await
        .map_err(|_| "cannot lock organization for rechunk".to_owned())?
        .ok_or_else(|| "rechunk organization no longer exists".to_owned())?;
    let (outcome, mut prepared) = match tx
        .rechunk_prepare(
            candidate.blob_id,
            candidate.file_id,
            &manifest,
            time::OffsetDateTime::now_utc(),
        )
        .await
    {
        Ok(result) => result,
        Err(_) => {
            drop(tx);
            delete_candidates(local, &candidate_keys).await;
            return Err("cannot prepare whole blob conversion".to_owned());
        }
    };
    let Some(mut prepared) = prepared.take() else {
        drop(tx);
        delete_candidates(local, &candidate_keys).await;
        return match outcome {
            PrepareOutcome::AlreadyChunked => Ok(None),
            PrepareOutcome::SharedAcrossFiles => Err(format!(
                "blob {} is shared across files; rechunk refused",
                candidate.blob_id
            )),
            PrepareOutcome::MissingOrChanged => Err(format!(
                "blob {} changed during rechunk; retry the command",
                candidate.blob_id
            )),
            PrepareOutcome::Prepared => Err("rechunk preparation returned no plan".to_owned()),
        };
    };
    if outcome != PrepareOutcome::Prepared {
        drop(tx);
        delete_candidates(local, &candidate_keys).await;
        return Err("rechunk preparation returned an inconsistent state".to_owned());
    }
    let reuse_candidates = prepared.reuse_candidates.clone();
    for reuse in &reuse_candidates {
        if !local_chunk_matches(local, reuse).await
            && !tx
                .rechunk_replace_chunk(&mut prepared, reuse)
                .await
                .map_err(|_| "cannot replace a damaged local chunk".to_owned())?
        {
            drop(tx);
            delete_candidates(local, &candidate_keys).await;
            return Err("a matching chunk changed during rechunk; retry the command".to_owned());
        }
    }
    let new_object_key = ObjectKey::generate();
    if !tx
        .rechunk_publish(
            &prepared,
            new_object_key.as_str(),
            time::OffsetDateTime::now_utc(),
        )
        .await
        .map_err(|_| "cannot publish chunk mappings".to_owned())?
    {
        drop(tx);
        delete_candidates(local, &candidate_keys).await;
        return Err("whole blob changed before chunk mappings could be published".to_owned());
    }
    let versions = prepared.versions.len() as u64;
    if tx.commit().await.is_err() {
        // Commit outcome can be ambiguous. Keep all candidates for referenced
        // chunks; ordinary delayed reconciliation handles any true strays.
        return Err(
            "rechunk commit outcome is uncertain; keep candidate objects and rerun inventory"
                .to_owned(),
        );
    }
    let selected = prepared.selected_object_keys;
    let unused: Vec<ObjectKey> = candidate_keys
        .into_iter()
        .filter(|key| !selected.contains(key.as_str()))
        .collect();
    delete_candidates(local, &unused).await;
    Ok(Some((versions, size)))
}

async fn delete_candidates(local: &LocalStore, keys: &[ObjectKey]) {
    let unique: HashSet<&str> = keys.iter().map(ObjectKey::as_str).collect();
    for key in unique {
        if let Ok(key) = ObjectKey::parse(key)
            && let Err(error) = local.delete(&key).await
        {
            tracing::warn!(%error, "cannot remove an unused rechunk candidate");
        }
    }
}
