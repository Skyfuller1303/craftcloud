-- 0005: organization versions and listing indexes (CC-F04).
-- Purpose: `organizations.version` backs optimistic concurrency (ETag /
-- If-Match, ADR-0009) for organization updates; the index supports the
-- keyset-paginated organization lists.
-- Tenant scope: `organizations` is the tenant root; the version is part of
-- the tenant's own data.
-- Restore note: forward-only; back up before applying and restore the backup
-- to roll back. Existing rows start at version 1.

ALTER TABLE organizations ADD COLUMN version bigint NOT NULL DEFAULT 1;
ALTER TABLE organizations ADD CONSTRAINT organizations_version_positive CHECK (version >= 1);

CREATE INDEX organizations_created_idx ON organizations (created_at, id);

-- Join time of a membership: stable keyset order for the member list without
-- putting personal data (names) into pagination cursors. Existing rows get the
-- migration time.
ALTER TABLE organization_members ADD COLUMN created_at timestamptz NOT NULL DEFAULT now();
CREATE INDEX organization_members_org_joined_idx ON organization_members (org_id, created_at, user_id);
