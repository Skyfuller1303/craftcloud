-- 0009: immutable file versions (CC-F08).
-- Purpose: a version binds a file to a verified blob. Versions are never
-- changed; the head of a file moves with a compare-and-set. A restore creates
-- a new version that references the blob of an older one (no new bytes).
-- Tenant scope: composite foreign keys tie a version to (org, workspace,
-- project, file) and to a blob of the same organization.
-- Restore note: forward-only; back up before applying. All foreign keys are
-- ON DELETE RESTRICT.

CREATE TABLE file_versions (
    id uuid PRIMARY KEY,
    org_id uuid NOT NULL,
    workspace_id uuid NOT NULL,
    project_id uuid NOT NULL,
    file_id uuid NOT NULL,
    -- 1, 2, 3, ... per file, assigned under the file row lock.
    number bigint NOT NULL,
    blob_id uuid NOT NULL,
    size_bytes bigint NOT NULL,
    sha256 bytea NOT NULL,
    -- The head this version was committed on top of (NULL for the first).
    base_version_id uuid NULL,
    restored_from_version_id uuid NULL,
    -- The upload that produced the content; unique, so a session commits once.
    upload_session_id uuid NULL,
    created_by_user_id uuid NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    created_at timestamptz NOT NULL,
    CONSTRAINT file_versions_number_positive CHECK (number >= 1),
    CONSTRAINT file_versions_size_non_negative CHECK (size_bytes >= 0),
    CONSTRAINT file_versions_sha256_length CHECK (octet_length(sha256) = 32),
    CONSTRAINT file_versions_number_unique UNIQUE (file_id, number),
    CONSTRAINT file_versions_file_id_unique UNIQUE (file_id, id),
    CONSTRAINT file_versions_upload_unique UNIQUE (upload_session_id),
    CONSTRAINT file_versions_file_fk FOREIGN KEY (org_id, workspace_id, project_id, file_id)
        REFERENCES files (org_id, workspace_id, project_id, id) ON DELETE RESTRICT,
    CONSTRAINT file_versions_blob_fk FOREIGN KEY (org_id, blob_id)
        REFERENCES blobs (org_id, id) ON DELETE RESTRICT,
    CONSTRAINT file_versions_base_fk FOREIGN KEY (file_id, base_version_id)
        REFERENCES file_versions (file_id, id) ON DELETE RESTRICT,
    CONSTRAINT file_versions_restored_fk FOREIGN KEY (file_id, restored_from_version_id)
        REFERENCES file_versions (file_id, id) ON DELETE RESTRICT,
    CONSTRAINT file_versions_upload_fk FOREIGN KEY (upload_session_id)
        REFERENCES upload_sessions (id) ON DELETE RESTRICT
);

CREATE INDEX file_versions_blob_idx ON file_versions (blob_id);

-- The head of a file is one of its own versions.
ALTER TABLE files
    ADD CONSTRAINT files_head_version_fk FOREIGN KEY (id, head_version_id)
        REFERENCES file_versions (file_id, id) ON DELETE RESTRICT;

-- A blob whose bytes failed verification on download is marked, not deleted.
ALTER TABLE blobs DROP CONSTRAINT blobs_state_valid;
ALTER TABLE blobs ADD CONSTRAINT blobs_state_valid
    CHECK (state IN ('staged', 'committed', 'orphaned', 'corrupt'));
