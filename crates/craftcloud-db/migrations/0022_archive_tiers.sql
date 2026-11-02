-- AC-12: durable, crash-safe archive tier state for immutable blobs.
-- Archive object names and wrapped data keys are supplied by the storage worker.
-- Claims are fenced by a caller-generated token so stale workers cannot finalize.

-- Quota groups model one logical upload payload. Restore copies may share a
-- group without deduplicating unrelated uploads with identical bytes.
CREATE TABLE blob_quota_groups (
    id uuid PRIMARY KEY,
    org_id uuid NOT NULL,
    workspace_id uuid NOT NULL,
    size_bytes bigint NOT NULL CHECK (size_bytes >= 0),
    CONSTRAINT blob_quota_groups_workspace_fk FOREIGN KEY (org_id, workspace_id)
        REFERENCES workspaces (org_id, id) ON DELETE RESTRICT,
    CONSTRAINT blob_quota_groups_scope_unique UNIQUE (org_id, workspace_id, id),
    CONSTRAINT blob_quota_groups_size_unique UNIQUE (org_id, workspace_id, id, size_bytes)
);

ALTER TABLE blobs ADD COLUMN quota_group_id uuid NULL;
INSERT INTO blob_quota_groups (id, org_id, workspace_id, size_bytes)
    SELECT id, org_id, workspace_id, size_bytes FROM blobs;
UPDATE blobs SET quota_group_id = id;
ALTER TABLE blobs ALTER COLUMN quota_group_id SET NOT NULL;
ALTER TABLE blobs ADD CONSTRAINT blobs_quota_group_fk
    FOREIGN KEY (org_id, workspace_id, quota_group_id, size_bytes)
    REFERENCES blob_quota_groups (org_id, workspace_id, id, size_bytes) ON DELETE RESTRICT;
CREATE INDEX blobs_quota_group_idx ON blobs (org_id, workspace_id, quota_group_id, state);

CREATE FUNCTION assign_blob_quota_group() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.quota_group_id IS NULL THEN
        NEW.quota_group_id := NEW.id;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM blob_quota_groups q
        WHERE q.id = NEW.quota_group_id AND q.org_id = NEW.org_id
          AND q.workspace_id = NEW.workspace_id AND q.size_bytes = NEW.size_bytes
    ) THEN
        INSERT INTO blob_quota_groups (id, org_id, workspace_id, size_bytes)
        VALUES (NEW.quota_group_id, NEW.org_id, NEW.workspace_id, NEW.size_bytes)
        ON CONFLICT (id) DO NOTHING;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER blobs_quota_group_before_insert BEFORE INSERT ON blobs
    FOR EACH ROW EXECUTE FUNCTION assign_blob_quota_group();

ALTER TABLE blobs
    ADD COLUMN tier_state text NOT NULL DEFAULT 'local',
    ADD COLUMN archive_object_key text NULL,
    ADD COLUMN archive_wrapped_key bytea NULL,
    ADD COLUMN archive_claim_token uuid NULL,
    ADD COLUMN archive_claimed_at timestamptz NULL,
    ADD COLUMN archive_next_attempt_at timestamptz NULL,
    ADD COLUMN archive_error_code text NULL,
    ADD COLUMN archive_error_at timestamptz NULL,
    ADD COLUMN archive_attempts integer NOT NULL DEFAULT 0,
    ADD COLUMN archive_local_delete_error_code text NULL,
    ADD COLUMN archive_local_delete_error_at timestamptz NULL,
    ADD COLUMN archive_local_delete_attempts integer NOT NULL DEFAULT 0,
    ADD COLUMN archive_verified_at timestamptz NULL,
    ADD COLUMN archive_verify_claim_token uuid NULL,
    ADD COLUMN archive_verify_claimed_at timestamptz NULL,
    ADD COLUMN archive_verify_offset bigint NOT NULL DEFAULT 0,
    ADD COLUMN archive_verify_attempted_at timestamptz NULL,
    ADD COLUMN archive_verify_no_progress_attempts integer NOT NULL DEFAULT 0,
    ADD COLUMN archive_local_delete_pending boolean NOT NULL DEFAULT false,
    ADD COLUMN archive_delete_error_code text NULL,
    ADD COLUMN archive_delete_error_at timestamptz NULL,
    ADD COLUMN archive_delete_attempts integer NOT NULL DEFAULT 0,
    ADD COLUMN archive_delete_next_attempt_at timestamptz NULL,
    ADD COLUMN archive_delete_pending boolean NOT NULL DEFAULT false,
    ADD CONSTRAINT blobs_tier_state_valid
        CHECK (tier_state IN ('local', 'archiving', 'archived', 'recalling')),
    ADD CONSTRAINT blobs_archive_claim_consistent
        CHECK ((tier_state IN ('archiving', 'recalling')) =
               (archive_claim_token IS NOT NULL AND archive_claimed_at IS NOT NULL)
               OR (tier_state = 'archived' AND archive_claim_token IS NOT NULL
                   AND archive_claimed_at IS NOT NULL)),
    ADD CONSTRAINT blobs_archive_claim_pair
        CHECK ((archive_claim_token IS NULL) = (archive_claimed_at IS NULL)),
    ADD CONSTRAINT blobs_archive_verify_claim_pair
        CHECK ((archive_verify_claim_token IS NULL) = (archive_verify_claimed_at IS NULL)),
    ADD CONSTRAINT blobs_archive_verify_offset_nonnegative CHECK (archive_verify_offset >= 0),
    ADD CONSTRAINT blobs_archive_verify_attempts_nonnegative CHECK (archive_verify_no_progress_attempts >= 0),
    ADD CONSTRAINT blobs_archive_metadata_consistent
        CHECK (tier_state <> 'archived' OR
               (archive_object_key IS NOT NULL AND archive_wrapped_key IS NOT NULL)),
    ADD CONSTRAINT blobs_archive_delete_consistent
        CHECK ((NOT archive_delete_pending OR archive_object_key IS NOT NULL) AND
               (NOT archive_local_delete_pending OR tier_state = 'archived')),
    ADD CONSTRAINT blobs_archive_key_nonempty
        CHECK (archive_object_key IS NULL OR char_length(archive_object_key) BETWEEN 1 AND 1024),
    ADD CONSTRAINT blobs_archive_wrapped_key_nonempty
        CHECK (archive_wrapped_key IS NULL OR octet_length(archive_wrapped_key) > 0),
    ADD CONSTRAINT blobs_archive_error_code_valid
        CHECK (archive_error_code IS NULL OR archive_error_code ~ '^[A-Z0-9_]{1,64}$'),
    ADD CONSTRAINT blobs_archive_delete_error_code_valid
        CHECK (archive_delete_error_code IS NULL OR archive_delete_error_code ~ '^[A-Z0-9_]{1,64}$'),
    ADD CONSTRAINT blobs_archive_local_delete_error_code_valid
        CHECK (archive_local_delete_error_code IS NULL OR archive_local_delete_error_code ~ '^[A-Z0-9_]{1,64}$'),
    ADD CONSTRAINT blobs_archive_attempts_non_negative
        CHECK (archive_attempts >= 0 AND archive_delete_attempts >= 0
               AND archive_local_delete_attempts >= 0);

CREATE INDEX blobs_archive_candidates_idx
    ON blobs (archive_next_attempt_at NULLS FIRST, created_at, id)
    WHERE state = 'committed' AND tier_state IN ('local', 'archiving');
CREATE INDEX blobs_archive_recovery_idx
    ON blobs (archive_claimed_at, created_at, id)
    WHERE tier_state IN ('archiving', 'recalling');
CREATE INDEX blobs_archive_verify_candidates_idx
    ON blobs (archive_verify_attempted_at NULLS FIRST, archive_verified_at NULLS FIRST, created_at, id)
    WHERE state = 'committed' AND tier_state = 'archived';
CREATE INDEX blobs_archive_delete_retry_idx
    ON blobs (archive_delete_next_attempt_at NULLS FIRST, created_at, id)
    WHERE state = 'deleting' AND archive_object_key IS NOT NULL;

-- Archive backups retain immutable provider keys independently of blob rows.
-- The registry is deliberately durable until an operator releases a backup.
CREATE TABLE archive_backups (
    backup_id uuid PRIMARY KEY,
    manifest_sha256 text NULL CHECK (manifest_sha256 IS NULL OR manifest_sha256 ~ '^[0-9a-f]{64}$'),
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE archive_backup_refs (
    backup_id uuid NOT NULL REFERENCES archive_backups (backup_id) ON DELETE CASCADE,
    blob_id uuid NOT NULL,
    archive_object_key text NOT NULL CHECK (char_length(archive_object_key) BETWEEN 1 AND 1024),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (backup_id, blob_id),
    UNIQUE (backup_id, archive_object_key)
);
CREATE INDEX archive_backup_refs_object_idx ON archive_backup_refs (archive_object_key);

-- Upgrades and new installs require an explicit archive inventory before GC.
-- Restore resets this gate because backups newer than its snapshot may exist.
CREATE TABLE archive_gc_state (
    id boolean PRIMARY KEY DEFAULT true CHECK (id),
    inventory_complete boolean NOT NULL DEFAULT false,
    updated_at timestamptz NOT NULL DEFAULT now()
);
INSERT INTO archive_gc_state (id, inventory_complete) VALUES (true, false);
