-- 0014: reserved bytes include the blobs of aborted or expired stored uploads
-- (CC-P03-09, SR-01).
-- Purpose: until now aborting or expiring a `stored` upload released its
-- reservation at once while the object stayed on disk. From this version the
-- bytes of every `orphaned` blob keep counting in `reserved_bytes` until the
-- garbage collection claims the blob. Databases written by the old rule hold
-- orphaned blobs whose reservation was already released, so the counters are
-- recomputed here from the rows: open upload sessions (created, receiving,
-- stored) plus orphaned blobs, the same formula as the usage reconciliation.
-- Data model: data only, no table, column or constraint changes, so a backup of
-- schema 13 and one of schema 14 have the same table set. Grants are unaffected.
-- Restore note: forward-only; back up before applying. The statements are
-- idempotent: running the same recomputation again changes nothing.

UPDATE workspaces w SET reserved_bytes =
    COALESCE((SELECT sum(s.declared_size) FROM upload_sessions s
               WHERE s.workspace_id = w.id
                 AND s.state IN ('created', 'receiving', 'stored')), 0)
  + COALESCE((SELECT sum(b.size_bytes) FROM blobs b
               WHERE b.workspace_id = w.id AND b.state = 'orphaned'), 0);

UPDATE organizations o SET reserved_bytes =
    COALESCE((SELECT sum(w.reserved_bytes) FROM workspaces w WHERE w.org_id = o.id), 0);
