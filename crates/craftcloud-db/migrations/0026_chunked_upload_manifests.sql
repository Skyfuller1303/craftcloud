-- 0026: temporary client manifests for AC-15 delta uploads.
-- Manifests are upload-session state, never durable version or backup state.

CREATE TABLE upload_chunk_manifests (
    upload_id uuid PRIMARY KEY REFERENCES upload_sessions (id) ON DELETE RESTRICT,
    file_id uuid NOT NULL REFERENCES files (id) ON DELETE RESTRICT,
    created_at timestamptz NOT NULL
);

CREATE TABLE upload_chunk_manifest_entries (
    upload_id uuid NOT NULL REFERENCES upload_chunk_manifests (upload_id) ON DELETE CASCADE,
    ordinal integer NOT NULL,
    byte_offset bigint NOT NULL,
    size_bytes bigint NOT NULL,
    sha256 bytea NOT NULL,
    digest bytea NOT NULL,
    PRIMARY KEY (upload_id, ordinal),
    CONSTRAINT upload_chunk_manifest_ordinal_valid CHECK (ordinal BETWEEN 0 AND 32767),
    CONSTRAINT upload_chunk_manifest_offset_valid CHECK (byte_offset >= 0),
    CONSTRAINT upload_chunk_manifest_size_valid CHECK (size_bytes BETWEEN 1 AND 4194304),
    CONSTRAINT upload_chunk_manifest_sha_len CHECK (octet_length(sha256) = 32),
    CONSTRAINT upload_chunk_manifest_digest_len CHECK (octet_length(digest) = 32)
);

CREATE FUNCTION craftcloud_upload_chunk_manifests_close() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.state IN ('committed', 'aborted', 'expired', 'failed')
       AND OLD.state IS DISTINCT FROM NEW.state THEN
        DELETE FROM upload_chunk_manifests WHERE upload_id = NEW.id;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER upload_sessions_chunk_manifests_close AFTER UPDATE OF state ON upload_sessions
    FOR EACH ROW EXECUTE FUNCTION craftcloud_upload_chunk_manifests_close();
