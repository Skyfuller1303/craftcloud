-- 0025: file-scoped, immutable chunk manifests (AC-15 Phase A).
-- Existing blobs stay whole; no object is rewritten by this migration.

ALTER TABLE blobs ADD COLUMN layout text NOT NULL DEFAULT 'whole';
ALTER TABLE blobs ADD CONSTRAINT blobs_layout_valid CHECK (layout IN ('whole', 'chunked'));

CREATE TABLE chunks (
    id uuid PRIMARY KEY,
    -- Retained after file purge until orphan chunk objects are physically GC'd.
    file_id uuid NOT NULL,
    org_id uuid NOT NULL,
    workspace_id uuid NOT NULL,
    digest bytea NOT NULL,
    sha256 bytea NOT NULL,
    object_key text NOT NULL UNIQUE,
    size_bytes bigint NOT NULL,
    refcount bigint NOT NULL DEFAULT 0,
    state text NOT NULL DEFAULT 'staged',
    storage_tier text NOT NULL DEFAULT 'local',
    archive_object_key text,
    archive_wrapped_key bytea,
    archive_verified_at timestamptz,
    archive_claim_token uuid,
    archive_claimed_at timestamptz,
    archive_next_attempt_at timestamptz,
    archive_error_code text,
    archive_local_delete_pending boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL,
    CONSTRAINT chunks_digest_len CHECK (octet_length(digest) = 32),
    CONSTRAINT chunks_sha256_len CHECK (octet_length(sha256) = 32),
    CONSTRAINT chunks_key_valid CHECK (object_key ~ '^[0-9a-f]{32}$'),
    CONSTRAINT chunks_size_valid CHECK (size_bytes BETWEEN 1 AND 4194304),
    CONSTRAINT chunks_refcount_valid CHECK (refcount >= 0),
    CONSTRAINT chunks_state_valid CHECK (state IN ('staged', 'committed', 'orphaned', 'deleting')),
    CONSTRAINT chunks_tier_valid CHECK (storage_tier IN ('local', 'archiving', 'archive')),
    CONSTRAINT chunks_archive_pair CHECK ((archive_object_key IS NULL) = (archive_wrapped_key IS NULL)),
    CONSTRAINT chunks_file_digest_unique UNIQUE (file_id, digest),
    CONSTRAINT chunks_id_file_unique UNIQUE (id, file_id)
);
CREATE INDEX chunks_gc_idx ON chunks (state, created_at);

CREATE TABLE upload_chunks (
    upload_id uuid NOT NULL REFERENCES upload_sessions (id) ON DELETE RESTRICT,
    file_id uuid NOT NULL REFERENCES files (id) ON DELETE RESTRICT,
    ordinal integer NOT NULL,
    chunk_id uuid NOT NULL,
    byte_offset bigint NOT NULL,
    PRIMARY KEY (upload_id, ordinal),
    FOREIGN KEY (chunk_id, file_id) REFERENCES chunks (id, file_id) ON DELETE RESTRICT,
    CONSTRAINT upload_chunks_ordinal_valid CHECK (ordinal >= 0),
    CONSTRAINT upload_chunks_offset_valid CHECK (byte_offset >= 0)
);

CREATE TABLE version_chunks (
    version_id uuid NOT NULL REFERENCES file_versions (id) ON DELETE CASCADE,
    file_id uuid NOT NULL REFERENCES files (id) ON DELETE RESTRICT,
    ordinal integer NOT NULL,
    chunk_id uuid NOT NULL,
    byte_offset bigint NOT NULL,
    PRIMARY KEY (version_id, ordinal),
    FOREIGN KEY (chunk_id, file_id) REFERENCES chunks (id, file_id) ON DELETE RESTRICT,
    CONSTRAINT version_chunks_ordinal_valid CHECK (ordinal >= 0),
    CONSTRAINT version_chunks_offset_valid CHECK (byte_offset >= 0)
);
CREATE INDEX version_chunks_chunk_idx ON version_chunks (chunk_id);

CREATE TABLE archive_chunk_backup_refs (
    backup_id uuid NOT NULL REFERENCES archive_backups (backup_id) ON DELETE CASCADE,
    chunk_id uuid NOT NULL,
    archive_object_key text NOT NULL CHECK (char_length(archive_object_key) BETWEEN 1 AND 1024),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (backup_id, chunk_id),
    UNIQUE (backup_id, archive_object_key)
);
CREATE INDEX archive_chunk_backup_refs_object_idx ON archive_chunk_backup_refs (archive_object_key);

CREATE FUNCTION craftcloud_chunk_refcount() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        UPDATE chunks SET refcount = refcount + 1, state = 'committed' WHERE id = NEW.chunk_id;
        RETURN NEW;
    END IF;
    UPDATE chunks SET refcount = GREATEST(refcount - 1, 0),
        state = CASE WHEN refcount <= 1 THEN 'orphaned' ELSE state END
        WHERE id = OLD.chunk_id;
    RETURN OLD;
END $$;
CREATE TRIGGER version_chunks_refcount_insert AFTER INSERT ON version_chunks
    FOR EACH ROW EXECUTE FUNCTION craftcloud_chunk_refcount();
CREATE TRIGGER version_chunks_refcount_delete AFTER DELETE ON version_chunks
    FOR EACH ROW EXECUTE FUNCTION craftcloud_chunk_refcount();

CREATE FUNCTION craftcloud_upload_chunk_lifecycle() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        UPDATE chunks SET state = CASE WHEN refcount = 0 THEN 'staged' ELSE state END
        WHERE id = NEW.chunk_id;
        RETURN NEW;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM upload_chunks WHERE chunk_id = OLD.chunk_id)
       AND NOT EXISTS (SELECT 1 FROM version_chunks WHERE chunk_id = OLD.chunk_id) THEN
        UPDATE chunks SET state = 'orphaned' WHERE id = OLD.chunk_id;
    END IF;
    RETURN OLD;
END $$;
CREATE TRIGGER upload_chunks_lifecycle_insert AFTER INSERT ON upload_chunks
    FOR EACH ROW EXECUTE FUNCTION craftcloud_upload_chunk_lifecycle();
CREATE TRIGGER upload_chunks_lifecycle_delete AFTER DELETE ON upload_chunks
    FOR EACH ROW EXECUTE FUNCTION craftcloud_upload_chunk_lifecycle();

CREATE FUNCTION craftcloud_upload_chunks_close() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.state IN ('committed', 'aborted', 'expired', 'failed')
       AND OLD.state IS DISTINCT FROM NEW.state THEN
        DELETE FROM upload_chunks WHERE upload_id = NEW.id;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER upload_sessions_chunks_close AFTER UPDATE OF state ON upload_sessions
    FOR EACH ROW EXECUTE FUNCTION craftcloud_upload_chunks_close();

CREATE VIEW chunked_version_physical_usage AS
    SELECT file_id, SUM(size_bytes) AS physical_bytes
    FROM chunks WHERE state = 'committed' GROUP BY file_id;
