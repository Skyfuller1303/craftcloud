-- 0013: index for "newest instance-level event of an action" (CC-P03-08).
-- Purpose: the admin health view asks for the newest successful
-- `backup.created` event (`WHERE action = ... AND org_id IS NULL`). Without an
-- index that is a scan of the whole audit log; this partial index over the
-- instance-level events answers it from its first entries.
-- Data model: index only, no table or column changes, so a backup of schema 12
-- and one of schema 13 have the same table set. Grants are unaffected (indexes
-- need none) and the append-only triggers are untouched.
-- Restore note: forward-only; the index is rebuilt by running the migration.

CREATE INDEX audit_events_instance_action_idx
    ON audit_events (action, created_at DESC)
    WHERE org_id IS NULL;
