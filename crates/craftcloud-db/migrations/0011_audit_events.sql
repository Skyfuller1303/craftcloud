-- 0011: audit events (CC-F11, CC-P03-05).
-- Purpose: a durable, append-only record of security-relevant actions: who
-- (actor), did what (stable action code), to which object (target type + id),
-- in which organization and workspace, with which result and request id.
-- Tenant scope: `org_id` is the tenant of the event; NULL marks an
-- instance-level event (operator, login, storage binding). Rows deliberately
-- carry no foreign keys: audit history must outlive the objects it describes
-- and the retention purge must never be blocked by (or cascade into) tenant
-- data.
-- Data minimisation (ADR-0016): only identifiers, enum-like codes and counters.
-- `details` is a flat JSON object with scalar values; the application builds
-- it from typed values (ids, codes, numbers, booleans), never from names,
-- e-mail addresses, tokens, passwords, file content or client addresses. No
-- IP address and no user agent column exists.
-- Append-only: triggers reject every UPDATE except the monotonic counter bump
-- of an aggregated row (repeated denials or login failures of one account in
-- one 15-minute window share one row, see `agg_key`), reject every DELETE
-- except inside `audit_purge` (the retention function used by the maintenance
-- job) and reject TRUNCATE. There is no application code path that updates or
-- deletes audit rows besides these two.
-- Restore note: forward-only; back up before applying and restore the backup
-- to roll back. Dropping the table loses the audit history.

CREATE TABLE audit_events (
    id uuid PRIMARY KEY,
    org_id uuid NULL,
    actor_id uuid NULL,
    -- Stable dotted code such as 'member.add'; never free text.
    action text NOT NULL,
    target_type text NULL,
    target_id uuid NULL,
    workspace_id uuid NULL,
    result text NOT NULL,
    request_id text NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    -- Aggregation: number of occurrences folded into this row and the time of
    -- the latest one (1 / created_at for ordinary events).
    event_count integer NOT NULL DEFAULT 1,
    last_at timestamptz NOT NULL DEFAULT now(),
    -- Non-NULL only for aggregated rows: '<action>:<result>:<actor|->:<org|->:<target|->:<window>'.
    agg_key text NULL,
    details jsonb NOT NULL DEFAULT '{}'::jsonb,
    CONSTRAINT audit_events_action_valid CHECK (action ~ '^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*){0,4}$' AND char_length(action) <= 64),
    CONSTRAINT audit_events_target_type_valid CHECK (target_type IS NULL OR target_type ~ '^[a-z][a-z0-9_]{0,31}$'),
    CONSTRAINT audit_events_result_valid CHECK (result IN ('success', 'denied', 'failed')),
    CONSTRAINT audit_events_request_id_valid CHECK (request_id IS NULL OR request_id ~ '^[A-Za-z0-9._-]{1,64}$'),
    CONSTRAINT audit_events_count_positive CHECK (event_count >= 1),
    CONSTRAINT audit_events_details_object CHECK (jsonb_typeof(details) = 'object' AND octet_length(details::text) <= 2048),
    CONSTRAINT audit_events_agg_key_length CHECK (agg_key IS NULL OR char_length(agg_key) <= 200)
);

-- One aggregated row per key (the upsert target).
CREATE UNIQUE INDEX audit_events_agg_key_idx ON audit_events (agg_key) WHERE agg_key IS NOT NULL;
-- Organization listing, newest first, with a stable tiebreak on the id.
CREATE INDEX audit_events_org_time_idx ON audit_events (org_id, created_at DESC, id DESC)
    WHERE org_id IS NOT NULL;
-- Instance-level listing.
CREATE INDEX audit_events_instance_time_idx ON audit_events (created_at DESC, id DESC)
    WHERE org_id IS NULL;
-- Filters of the organization listing.
CREATE INDEX audit_events_org_actor_idx ON audit_events (org_id, actor_id, created_at DESC, id DESC)
    WHERE org_id IS NOT NULL AND actor_id IS NOT NULL;
CREATE INDEX audit_events_org_workspace_idx ON audit_events (org_id, workspace_id, created_at DESC, id DESC)
    WHERE org_id IS NOT NULL AND workspace_id IS NOT NULL;
CREATE INDEX audit_events_org_target_idx ON audit_events (org_id, target_id, created_at DESC, id DESC)
    WHERE org_id IS NOT NULL AND target_id IS NOT NULL;
CREATE INDEX audit_events_org_action_idx ON audit_events (org_id, action text_pattern_ops, created_at DESC)
    WHERE org_id IS NOT NULL;
-- Retention purge.
CREATE INDEX audit_events_created_idx ON audit_events (created_at, id);

CREATE FUNCTION audit_events_guard() RETURNS trigger AS $$
BEGIN
    IF TG_OP = 'UPDATE' THEN
        -- Only the counter of an aggregated row may move, and only upwards.
        IF OLD.agg_key IS NOT NULL
           AND NEW.agg_key IS NOT DISTINCT FROM OLD.agg_key
           AND NEW.id = OLD.id
           AND NEW.org_id IS NOT DISTINCT FROM OLD.org_id
           AND NEW.actor_id IS NOT DISTINCT FROM OLD.actor_id
           AND NEW.action = OLD.action
           AND NEW.target_type IS NOT DISTINCT FROM OLD.target_type
           AND NEW.target_id IS NOT DISTINCT FROM OLD.target_id
           AND NEW.workspace_id IS NOT DISTINCT FROM OLD.workspace_id
           AND NEW.result = OLD.result
           AND NEW.request_id IS NOT DISTINCT FROM OLD.request_id
           AND NEW.created_at = OLD.created_at
           AND NEW.details = OLD.details
           AND NEW.event_count > OLD.event_count
           AND NEW.last_at >= OLD.last_at THEN
            RETURN NEW;
        END IF;
        RAISE EXCEPTION 'audit_events rows are append-only' USING ERRCODE = 'restrict_violation';
    END IF;
    IF TG_OP = 'DELETE' THEN
        IF current_setting('craftcloud.audit_purge', true) = 'on' THEN
            RETURN OLD;
        END IF;
        RAISE EXCEPTION 'audit_events rows can only be removed by audit_purge' USING ERRCODE = 'restrict_violation';
    END IF;
    RAISE EXCEPTION 'audit_events cannot be truncated' USING ERRCODE = 'restrict_violation';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER audit_events_no_update_delete
    BEFORE UPDATE OR DELETE ON audit_events
    FOR EACH ROW EXECUTE FUNCTION audit_events_guard();
CREATE TRIGGER audit_events_no_truncate
    BEFORE TRUNCATE ON audit_events
    FOR EACH STATEMENT EXECUTE FUNCTION audit_events_guard();

-- Retention: deletes at most `batch_size` events created before `cutoff` and
-- returns how many. The cutoff is computed by the caller from the database
-- clock; the function only lifts the delete guard for its own transaction.
CREATE FUNCTION audit_purge(cutoff timestamptz, batch_size integer) RETURNS bigint AS $$
DECLARE
    removed bigint;
BEGIN
    PERFORM set_config('craftcloud.audit_purge', 'on', true);
    WITH doomed AS (
        SELECT id FROM audit_events
        WHERE created_at < cutoff
        ORDER BY created_at, id
        LIMIT GREATEST(batch_size, 1)
    ), gone AS (
        DELETE FROM audit_events e USING doomed d WHERE e.id = d.id RETURNING 1
    )
    SELECT count(*) INTO removed FROM gone;
    PERFORM set_config('craftcloud.audit_purge', 'off', true);
    RETURN removed;
END;
$$ LANGUAGE plpgsql;
