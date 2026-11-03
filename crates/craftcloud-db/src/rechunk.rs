//! Offline conversion of whole-blob versions to file-scoped chunk manifests.

use std::collections::HashSet;

use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::DbError;
use crate::members::OrgTx;
use crate::uploads::{ChunkReuseCandidate, NewChunk};

/// One committed whole blob selected by the offline rechunk command.
#[derive(Debug, Clone, FromRow)]
pub struct Candidate {
    pub blob_id: Uuid,
    pub org_id: Uuid,
    pub workspace_id: Uuid,
    pub quota_group_id: Uuid,
    pub file_id: Uuid,
    pub object_key: String,
    pub size_bytes: i64,
    pub sha256: Vec<u8>,
    pub tier_state: String,
    pub archive_object_key: Option<String>,
    pub archive_wrapped_key: Option<Vec<u8>>,
    pub files_referencing_blob: i64,
}

/// Rechecks one keyset page of committed whole blobs.
pub async fn candidates(
    pool: &PgPool,
    file_id: Option<Uuid>,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<Candidate>, DbError> {
    Ok(sqlx::query_as(
        "SELECT b.id AS blob_id,b.org_id,b.workspace_id,b.quota_group_id,(min(v.file_id::text) FILTER (WHERE f.deleted_at IS NULL))::uuid AS file_id, \
                b.object_key,b.size_bytes,b.sha256,b.tier_state,b.archive_object_key,b.archive_wrapped_key, \
                count(DISTINCT v.file_id)::bigint AS files_referencing_blob \
         FROM blobs b JOIN file_versions v ON v.blob_id=b.id AND v.org_id=b.org_id \
         JOIN files f ON f.id=v.file_id AND f.org_id=v.org_id \
         WHERE b.state='committed' AND b.layout='whole' \
           AND EXISTS (SELECT 1 FROM file_versions selected JOIN files selected_file ON selected_file.id=selected.file_id AND selected_file.org_id=selected.org_id \
                       WHERE selected.org_id=b.org_id AND selected.blob_id=b.id AND selected_file.deleted_at IS NULL \
                         AND ($1::uuid IS NULL OR selected.file_id=$1)) \
           AND ($2::uuid IS NULL OR b.id>$2) \
         GROUP BY b.id ORDER BY b.id LIMIT $3",
    )
    .bind(file_id)
    .bind(after)
    .bind(limit.clamp(1, 100))
    .fetch_all(pool)
    .await?)
}

/// Current whole and already-converted version counts and selected logical bytes.
pub async fn inventory(pool: &PgPool, file_id: Option<Uuid>) -> Result<(i64, i64, i64), DbError> {
    Ok(sqlx::query_as(
        "SELECT count(*) FILTER (WHERE b.layout='whole')::bigint, \
                count(*) FILTER (WHERE b.layout='chunked')::bigint, \
                COALESCE(sum(b.size_bytes) FILTER (WHERE b.layout='whole'),0)::bigint \
         FROM file_versions v JOIN blobs b ON b.id=v.blob_id AND b.org_id=v.org_id \
         JOIN files f ON f.id=v.file_id AND f.org_id=v.org_id \
         WHERE b.state='committed' AND f.deleted_at IS NULL AND ($1::uuid IS NULL OR v.file_id=$1)",
    )
    .bind(file_id)
    .fetch_one(pool)
    .await?)
}

/// A locked, prepared atomic replacement. New candidate objects are selected
/// or remain unreferenced; no version mappings change until `publish`.
#[derive(Debug)]
pub struct Prepared {
    pub blob_id: Uuid,
    pub new_blob_id: Uuid,
    pub file_id: Uuid,
    pub versions: Vec<Uuid>,
    pub chunk_ids: Vec<Uuid>,
    pub reuse_candidates: Vec<ChunkReuseCandidate>,
    pub selected_object_keys: HashSet<String>,
    pub total_size: i64,
    pub workspace_id: Uuid,
    pub quota_group_id: Uuid,
    pub sha256: Vec<u8>,
}

/// Result of rechecking a candidate under the organization and row locks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareOutcome {
    Prepared,
    AlreadyChunked,
    MissingOrChanged,
    SharedAcrossFiles,
}

impl OrgTx {
    /// Locks and stages chunk-row choices for one whole blob. Caller must
    /// verify every returned local reuse candidate before publishing mappings.
    pub async fn rechunk_prepare(
        &mut self,
        blob_id: Uuid,
        file_id: Uuid,
        chunks: &[NewChunk],
        now: OffsetDateTime,
    ) -> Result<(PrepareOutcome, Option<Prepared>), DbError> {
        let file = sqlx::query_scalar::<_, Option<Uuid>>(
            "SELECT head_version_id FROM files WHERE id=$1 AND org_id=$2 AND deleted_at IS NULL FOR UPDATE",
        )
        .bind(file_id)
        .bind(self.org_id)
        .fetch_optional(&mut *self.tx)
        .await?;
        if file.is_none() {
            return Ok((PrepareOutcome::MissingOrChanged, None));
        }
        let blob: Option<(String, String, i64, Vec<u8>, Uuid, Uuid)> = sqlx::query_as(
            "SELECT state,layout,size_bytes,sha256,workspace_id,quota_group_id FROM blobs WHERE id=$1 AND org_id=$2 FOR UPDATE",
        )
        .bind(blob_id)
        .bind(self.org_id)
        .fetch_optional(&mut *self.tx)
        .await?;
        let Some((state, layout, size, sha, workspace_id, quota_group_id)) = blob else {
            return Ok((PrepareOutcome::MissingOrChanged, None));
        };
        if layout == "chunked" {
            return Ok((PrepareOutcome::AlreadyChunked, None));
        }
        if state != "committed" {
            return Ok((PrepareOutcome::MissingOrChanged, None));
        }
        let version_rows: Vec<(Uuid, Uuid, i64, Vec<u8>)> = sqlx::query_as(
            "SELECT id,file_id,size_bytes,sha256 FROM file_versions WHERE org_id=$1 AND blob_id=$2 ORDER BY id FOR UPDATE",
        )
        .bind(self.org_id)
        .bind(blob_id)
        .fetch_all(&mut *self.tx)
        .await?;
        if version_rows.is_empty()
            || version_rows
                .iter()
                .any(|(_, version_file, _, _)| *version_file != file_id)
        {
            return Ok((PrepareOutcome::SharedAcrossFiles, None));
        }
        if version_rows
            .iter()
            .any(|(_, _, version_size, version_sha)| *version_size != size || version_sha != &sha)
        {
            return Ok((PrepareOutcome::MissingOrChanged, None));
        }
        let versions: Vec<Uuid> = version_rows.iter().map(|(id, _, _, _)| *id).collect();
        let has_mappings: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM version_chunks WHERE version_id=ANY($1::uuid[]))",
        )
        .bind(&versions)
        .fetch_one(&mut *self.tx)
        .await?;
        if has_mappings {
            return Ok((PrepareOutcome::MissingOrChanged, None));
        }
        let size_u64 = u64::try_from(size).map_err(|_| DbError::Internal)?;
        let mut offset = 0_i64;
        for chunk in chunks {
            if chunk.byte_offset != offset || chunk.size_bytes <= 0 {
                return Err(DbError::Internal);
            }
            offset = offset
                .checked_add(chunk.size_bytes)
                .ok_or(DbError::Internal)?;
        }
        if offset != size
            || (size_u64 == 0 && !chunks.is_empty())
            || (size_u64 > 0 && chunks.is_empty())
        {
            return Err(DbError::Internal);
        }

        let mut chunk_ids = Vec::with_capacity(chunks.len());
        let mut reuse_candidates = Vec::new();
        let mut selected_object_keys = HashSet::new();
        for chunk in chunks {
            let mut existing = None;
            for digest in &chunk.digest_candidates {
                existing = sqlx::query_as::<_, (Uuid, i64, Vec<u8>, String, String, String, String, i64)>(
                    "SELECT id,size_bytes,sha256,state,storage_tier,object_key,encoding,stored_bytes FROM chunks WHERE file_id=$1 AND digest=$2 FOR UPDATE",
                )
                .bind(file_id)
                .bind(digest.as_slice())
                .fetch_optional(&mut *self.tx)
                .await?;
                if existing.is_some() {
                    break;
                }
            }
            let chunk_id = if let Some((
                id,
                existing_size,
                existing_sha,
                existing_state,
                tier,
                key,
                encoding,
                stored_bytes,
            )) = existing
            {
                if existing_size != chunk.size_bytes
                    || existing_sha.as_slice() != chunk.sha256
                    || existing_state == "deleting"
                    || !matches!(tier.as_str(), "local" | "archiving" | "archive")
                {
                    return Err(DbError::Internal);
                }
                if tier == "local" {
                    if !reuse_candidates
                        .iter()
                        .any(|candidate: &ChunkReuseCandidate| candidate.chunk_id == id)
                    {
                        reuse_candidates.push(ChunkReuseCandidate {
                            chunk_id: id,
                            existing_object_key: key.clone(),
                            candidate_object_key: chunk.object_key.clone(),
                            sha256: chunk.sha256,
                            size_bytes: chunk.size_bytes,
                            stored_bytes,
                            encoding,
                            candidate_encoding: chunk.encoding.clone(),
                            candidate_stored_bytes: chunk.stored_bytes,
                        });
                    }
                    selected_object_keys.insert(key);
                } else {
                    sqlx::query("UPDATE chunks SET object_key=$2,encoding=$4,stored_bytes=$5,storage_tier='local',archive_object_key=NULL,archive_wrapped_key=NULL,archive_verified_at=NULL,archive_local_delete_pending=false,archive_claim_token=NULL,archive_claimed_at=NULL,archive_next_attempt_at=NULL,archive_error_code=NULL WHERE id=$1 AND file_id=$3")
                        .bind(id).bind(&chunk.object_key).bind(file_id).bind(&chunk.encoding).bind(chunk.stored_bytes).execute(&mut *self.tx).await?;
                    selected_object_keys.insert(chunk.object_key.clone());
                }
                id
            } else {
                let id = Uuid::new_v4();
                sqlx::query("INSERT INTO chunks (id,file_id,org_id,workspace_id,digest,sha256,object_key,size_bytes,encoding,stored_bytes,state,created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'orphaned',$11)")
                    .bind(id).bind(file_id).bind(self.org_id).bind(workspace_id).bind(chunk.digest.as_slice()).bind(chunk.sha256.as_slice()).bind(&chunk.object_key).bind(chunk.size_bytes).bind(&chunk.encoding).bind(chunk.stored_bytes).bind(now).execute(&mut *self.tx).await?;
                selected_object_keys.insert(chunk.object_key.clone());
                id
            };
            chunk_ids.push(chunk_id);
        }
        Ok((
            PrepareOutcome::Prepared,
            Some(Prepared {
                blob_id,
                new_blob_id: Uuid::new_v4(),
                file_id,
                versions,
                chunk_ids,
                reuse_candidates,
                selected_object_keys,
                total_size: size,
                workspace_id,
                quota_group_id,
                sha256: sha,
            }),
        ))
    }

    /// Replaces a damaged local chunk with the verified staged candidate.
    /// The former key is left for ordinary delayed storage reconciliation.
    pub async fn rechunk_replace_chunk(
        &mut self,
        prepared: &mut Prepared,
        reuse: &ChunkReuseCandidate,
    ) -> Result<bool, DbError> {
        let changed = sqlx::query("UPDATE chunks SET object_key=$4,encoding=$8,stored_bytes=$9,storage_tier='local',archive_object_key=NULL,archive_wrapped_key=NULL,archive_verified_at=NULL,archive_local_delete_pending=false,archive_claim_token=NULL,archive_claimed_at=NULL,archive_next_attempt_at=NULL,archive_error_code=NULL WHERE id=$1 AND org_id=$2 AND file_id=$3 AND object_key=$5 AND size_bytes=$6 AND sha256=$7 AND storage_tier='local' AND state <> 'deleting'")
            .bind(reuse.chunk_id).bind(self.org_id).bind(prepared.file_id).bind(&reuse.candidate_object_key).bind(&reuse.existing_object_key).bind(reuse.size_bytes).bind(reuse.sha256.as_slice()).bind(&reuse.candidate_encoding).bind(reuse.candidate_stored_bytes).execute(&mut *self.tx).await?.rows_affected() == 1;
        if changed {
            prepared
                .selected_object_keys
                .remove(&reuse.existing_object_key);
            prepared
                .selected_object_keys
                .insert(reuse.candidate_object_key.clone());
        }
        Ok(changed)
    }

    /// Publishes every version mapping and changes the immutable blob layout
    /// in the same transaction, without changing quota groups or file heads.
    pub async fn rechunk_publish(
        &mut self,
        prepared: &Prepared,
        object_key: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DbError> {
        sqlx::query("INSERT INTO blobs (id,org_id,workspace_id,quota_group_id,object_key,size_bytes,sha256,state,created_at,layout,tier_state) VALUES ($1,$2,$3,$4,$5,$6,$7,'committed',$8,'chunked','local')")
            .bind(prepared.new_blob_id).bind(self.org_id).bind(prepared.workspace_id).bind(prepared.quota_group_id).bind(object_key).bind(prepared.total_size).bind(&prepared.sha256).bind(now).execute(&mut *self.tx).await?;
        let moved = sqlx::query("UPDATE file_versions SET blob_id=$1 WHERE org_id=$2 AND file_id=$3 AND blob_id=$4 AND id=ANY($5::uuid[])")
            .bind(prepared.new_blob_id).bind(self.org_id).bind(prepared.file_id).bind(prepared.blob_id).bind(&prepared.versions).execute(&mut *self.tx).await?.rows_affected();
        if moved != prepared.versions.len() as u64 {
            return Err(DbError::Internal);
        }
        let mut offset = 0_i64;
        for (ordinal, chunk_id) in prepared.chunk_ids.iter().enumerate() {
            let size: i64 = sqlx::query_scalar("SELECT size_bytes FROM chunks WHERE id=$1 AND file_id=$2 AND storage_tier='local' AND state <> 'deleting' FOR UPDATE")
                .bind(chunk_id).bind(prepared.file_id).fetch_optional(&mut *self.tx).await?.ok_or(DbError::Internal)?;
            for version_id in &prepared.versions {
                sqlx::query("INSERT INTO version_chunks (version_id,file_id,ordinal,chunk_id,byte_offset) VALUES ($1,$2,$3,$4,$5)")
                    .bind(version_id).bind(prepared.file_id).bind(i32::try_from(ordinal).map_err(|_| DbError::Internal)?).bind(chunk_id).bind(offset).execute(&mut *self.tx).await?;
            }
            offset = offset.checked_add(size).ok_or(DbError::Internal)?;
        }
        if offset != prepared.total_size {
            return Err(DbError::Internal);
        }
        Ok(true)
    }
}

/// Counts and bytes from a selected version's logical content.
pub fn candidate_logical_bytes(candidate: &Candidate) -> u64 {
    u64::try_from(candidate.size_bytes).unwrap_or(0)
}
