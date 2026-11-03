-- 0027: record per-chunk storage encoding; existing rows remain raw.
ALTER TABLE chunks ADD COLUMN encoding text NOT NULL DEFAULT 'raw';
ALTER TABLE chunks ADD COLUMN stored_bytes bigint NOT NULL DEFAULT 0;
UPDATE chunks SET stored_bytes = size_bytes;
ALTER TABLE chunks ADD CONSTRAINT chunks_encoding_valid CHECK (encoding IN ('raw', 'zstd'));
ALTER TABLE chunks ADD CONSTRAINT chunks_stored_bytes_valid CHECK (stored_bytes >= 1);
ALTER TABLE chunks ADD CONSTRAINT chunks_raw_length_matches CHECK (encoding <> 'raw' OR stored_bytes = size_bytes);

ALTER TABLE instance_settings ADD CONSTRAINT instance_settings_compression_level CHECK (
    key <> 'storage.compression_level' OR CASE
        WHEN jsonb_typeof(value -> 'value') = 'number'
             AND (value ->> 'value') ~ '^(0|[1-9][0-9]?)$'
        THEN (value ->> 'value')::int BETWEEN 0 AND 19
        ELSE false
    END
);

CREATE FUNCTION craftcloud_chunk_stored_bytes_default() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.stored_bytes = 0 THEN
        NEW.stored_bytes := NEW.size_bytes;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER chunks_stored_bytes_default BEFORE INSERT ON chunks
    FOR EACH ROW EXECUTE FUNCTION craftcloud_chunk_stored_bytes_default();

CREATE OR REPLACE VIEW chunked_version_physical_usage AS
    SELECT file_id, SUM(stored_bytes) AS physical_bytes
    FROM chunks WHERE state = 'committed' GROUP BY file_id;

-- Archive objects outlive the row that originally referenced them when a
-- chunk/blob is replaced. Keep cleanup ownership until archive GC can prove
-- that neither live rows nor retained backups still reference the object.
CREATE TABLE retired_archive_objects (
    archive_object_key text PRIMARY KEY CHECK (char_length(archive_object_key) BETWEEN 1 AND 1024),
    org_id uuid NOT NULL,
    workspace_id uuid NOT NULL,
    physical_bytes bigint NOT NULL CHECK (physical_bytes >= 0),
    retired_at timestamptz NOT NULL DEFAULT now(),
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    last_error_code text
);
CREATE INDEX retired_archive_objects_due_idx
    ON retired_archive_objects (next_attempt_at, retired_at);

CREATE FUNCTION retain_replaced_archive_object() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    old_bytes bigint;
BEGIN
    IF OLD.archive_object_key IS NOT NULL
       AND OLD.archive_object_key IS DISTINCT FROM NEW.archive_object_key THEN
        IF TG_TABLE_NAME = 'chunks' THEN
            old_bytes := OLD.stored_bytes;
        ELSE
            old_bytes := OLD.size_bytes;
        END IF;
        INSERT INTO retired_archive_objects (archive_object_key, org_id, workspace_id, physical_bytes)
        VALUES (OLD.archive_object_key, OLD.org_id, OLD.workspace_id, old_bytes)
        ON CONFLICT (archive_object_key) DO UPDATE
            SET retired_at = GREATEST(retired_archive_objects.retired_at, EXCLUDED.retired_at),
                next_attempt_at = GREATEST(retired_archive_objects.next_attempt_at, EXCLUDED.next_attempt_at),
                physical_bytes = GREATEST(retired_archive_objects.physical_bytes, EXCLUDED.physical_bytes);
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER blobs_retain_replaced_archive_object
    AFTER UPDATE OF archive_object_key ON blobs
    FOR EACH ROW EXECUTE FUNCTION retain_replaced_archive_object();
CREATE TRIGGER chunks_retain_replaced_archive_object
    AFTER UPDATE OF archive_object_key ON chunks
    FOR EACH ROW EXECUTE FUNCTION retain_replaced_archive_object();
