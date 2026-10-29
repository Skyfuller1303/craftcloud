-- 0012: audit log guard that holds against the runtime database role (CC-F11).
-- Purpose: 0011 made audit rows append-only with triggers and a session setting
-- that lifted the delete guard. A role that owns the table, or that can set the
-- setting, could bypass that. This migration changes the guard so that it holds
-- when two roles are used:
--   * the OWNER role runs migrations and owns the tables and functions;
--   * the RUNTIME role (the server) gets only data privileges: SELECT, INSERT and
--     UPDATE on `audit_events` (the trigger still pins every column except the
--     counter of aggregated rows), no DELETE, no TRUNCATE, no ownership. It
--     cannot ALTER the table or disable its triggers, and it deletes old events
--     only through `audit_purge`.
-- The privileges of the runtime role are granted by `craftcloud-server migrate`
-- (CRAFTCLOUD_DATABASE_RUNTIME_ROLE), not by this file, because the role name is
-- deployment-specific.
-- Single-role mode (one role does everything, the default of a simple setup)
-- keeps working, but the guard is then advisory: that role owns the table and
-- can disable the triggers. The server says so in its startup log.
-- Restore note: forward-only; back up before applying.

-- DELETE is allowed only for the table owner, i.e. inside `audit_purge` (a
-- SECURITY DEFINER function owned by the owner role). In single-role mode the
-- caller is the owner too; there the purge function raises the session setting
-- as before, so an accidental DELETE is still refused. With two roles the
-- setting is irrelevant: the runtime role is never the table owner.
CREATE OR REPLACE FUNCTION audit_events_guard() RETURNS trigger AS $$
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
        IF current_user = (SELECT pg_catalog.pg_get_userbyid(c.relowner)
                           FROM pg_catalog.pg_class c WHERE c.oid = TG_RELID)
           AND (session_user <> current_user
                OR pg_catalog.current_setting('craftcloud.audit_purge', true) = 'on') THEN
            RETURN OLD;
        END IF;
        RAISE EXCEPTION 'audit_events rows can only be removed by audit_purge' USING ERRCODE = 'restrict_violation';
    END IF;
    RAISE EXCEPTION 'audit_events cannot be truncated' USING ERRCODE = 'restrict_violation';
END;
$$ LANGUAGE plpgsql SET search_path = pg_catalog, public;

-- Retention. The cutoff is clamped to at most `now() - 30 days`, so even a
-- caller that passes 'infinity' cannot delete recent events.
CREATE OR REPLACE FUNCTION audit_purge(cutoff timestamptz, batch_size integer) RETURNS bigint AS $$
DECLARE
    removed bigint;
    effective timestamptz := LEAST(cutoff, pg_catalog.now() - interval '30 days');
BEGIN
    PERFORM pg_catalog.set_config('craftcloud.audit_purge', 'on', true);
    WITH doomed AS (
        SELECT id FROM public.audit_events
        WHERE created_at < effective
        ORDER BY created_at, id
        LIMIT GREATEST(batch_size, 1)
    ), gone AS (
        DELETE FROM public.audit_events e USING doomed d WHERE e.id = d.id RETURNING 1
    )
    SELECT count(*) INTO removed FROM gone;
    PERFORM pg_catalog.set_config('craftcloud.audit_purge', 'off', true);
    RETURN removed;
END;
$$ LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public;

REVOKE ALL ON FUNCTION audit_purge(timestamptz, integer) FROM PUBLIC;

-- The runtime role may insert events but not forge their time or counter: a
-- plain insert gets the database time and a counter of 1. The owner role (the
-- migration role; also test seeding and imports) is exempt.
CREATE OR REPLACE FUNCTION audit_events_insert_guard() RETURNS trigger AS $$
BEGIN
    IF current_user <> (SELECT pg_catalog.pg_get_userbyid(c.relowner)
                        FROM pg_catalog.pg_class c WHERE c.oid = TG_RELID) THEN
        NEW.created_at := pg_catalog.now();
        NEW.last_at := pg_catalog.now();
        NEW.event_count := 1;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql SET search_path = pg_catalog, public;

CREATE TRIGGER audit_events_insert_guard
    BEFORE INSERT ON audit_events
    FOR EACH ROW EXECUTE FUNCTION audit_events_insert_guard();
