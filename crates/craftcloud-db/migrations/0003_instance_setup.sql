-- 0003: instance setup marker.
-- Purpose: records that the one-time instance bootstrap (CC-F01) has been
-- completed and which user became the first instance operator.
-- Tenant scope: instance-wide singleton, not tenant-bound.
-- Restore note: forward-only; back up before applying and restore the backup
-- to roll back. The singleton is enforced by the primary key on a boolean
-- that must be true, so a second row can never exist. The operator reference
-- is DEFERRABLE INITIALLY DEFERRED so the setup transaction can claim the
-- singleton row first (serializing concurrent attempts) and create the user
-- afterwards; the reference is verified at commit.

CREATE TABLE instance_setup (
    id boolean PRIMARY KEY DEFAULT true,
    completed_at timestamptz NOT NULL DEFAULT now(),
    operator_user_id uuid NOT NULL
        REFERENCES users (id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT instance_setup_singleton CHECK (id)
);
