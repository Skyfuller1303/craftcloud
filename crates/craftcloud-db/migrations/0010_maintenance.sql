-- 0010: retention purge, blob garbage collection and integrity verification
-- (CC-F10, CC-P02-06, CC-P02-07).
-- Purpose: the maintenance job deletes expired trash and old versions, removes
-- blobs that nothing references any more and re-hashes stored objects.
-- - blobs.state gets 'deleting': a blob that garbage collection has claimed
--   (under the organization lock, after re-checking that nothing references it)
--   and whose object is being removed from storage. Nothing may reference a
--   'deleting' blob; a crashed run is completed by the next one.
-- - blobs.verified_at: last time the object was re-read and its SHA-256 compared
--   (NULL = never), the verification scan works oldest first.
-- - maintenance_runs: one row per finished run (counts and error codes only, no
--   tenant data); the newest rows are kept for the operator endpoint.
-- - storage_binding: the identity of the object store this database belongs to
--   (one row). The same identifier is written to a marker file in the store
--   directory; maintenance only deletes or quarantines objects while both agree,
--   so a wrong or fresh database can never make reconciliation wipe a store.
-- Restore note: forward-only; back up before applying. A 'deleting' blob row is
-- only a marker for work in progress; the data it describes is already gone or
-- about to be.

ALTER TABLE blobs DROP CONSTRAINT blobs_state_valid;
ALTER TABLE blobs ADD CONSTRAINT blobs_state_valid
    CHECK (state IN ('staged', 'committed', 'orphaned', 'corrupt', 'deleting'));

ALTER TABLE blobs ADD COLUMN verified_at timestamptz NULL;

-- Verification scan: committed and corrupt blobs, never verified first.
CREATE INDEX blobs_verify_idx ON blobs (verified_at NULLS FIRST, created_at)
    WHERE state IN ('committed', 'corrupt');

-- Trash scans of the purge step.
CREATE INDEX files_trash_idx ON files (deleted_at) WHERE deleted_at IS NOT NULL;
CREATE INDEX projects_trash_idx ON projects (deleted_at) WHERE deleted_at IS NOT NULL;

CREATE TABLE storage_binding (
    id boolean PRIMARY KEY DEFAULT true CHECK (id),
    store_id uuid NOT NULL,
    bound_at timestamptz NOT NULL,
    -- Raised by every maintenance run that may delete something, and copied to
    -- the marker file afterwards. A marker with a higher epoch than the
    -- database means the database was restored from an older state.
    epoch bigint NOT NULL DEFAULT 0,
    CONSTRAINT storage_binding_epoch_non_negative CHECK (epoch >= 0)
);

CREATE TABLE maintenance_runs (
    id bigserial PRIMARY KEY,
    -- 'scheduled' (server loop) or 'cli' (craftcloud-server admin maintenance).
    triggered_by text NOT NULL,
    started_at timestamptz NOT NULL,
    finished_at timestamptz NOT NULL,
    duration_ms bigint NOT NULL,
    -- Counts per step and error codes; never names, paths, hashes or e-mails.
    report jsonb NOT NULL,
    CONSTRAINT maintenance_runs_triggered_by_valid CHECK (triggered_by IN ('scheduled', 'cli')),
    CONSTRAINT maintenance_runs_duration_non_negative CHECK (duration_ms >= 0)
);
