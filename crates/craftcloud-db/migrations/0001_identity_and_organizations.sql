-- 0001: identity and organizations.
-- Purpose: users, organizations and organization membership (CC-F04 basis).
-- Tenant scope: `users` are instance-wide identities; `organizations` is the
-- tenant root; `organization_members` is tenant-bound by `org_id`.
-- Restore note: migrations are forward-only. Take a backup (pg_dump) before
-- applying; restoring that backup is the rollback path. No cascading deletes:
-- every foreign key is ON DELETE RESTRICT (soft delete arrives later).

CREATE TABLE users (
    id uuid PRIMARY KEY,
    normalized_email text NOT NULL UNIQUE,
    display_name text NOT NULL,
    password_hash text NULL,
    instance_role text NULL,
    disabled_at timestamptz NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT users_email_normalized CHECK (lower(normalized_email) = normalized_email),
    CONSTRAINT users_email_length CHECK (char_length(normalized_email) BETWEEN 3 AND 320),
    CONSTRAINT users_display_name_length CHECK (char_length(display_name) BETWEEN 1 AND 100),
    CONSTRAINT users_instance_role_valid CHECK (instance_role IS NULL OR instance_role IN ('operator'))
);

CREATE TABLE organizations (
    id uuid PRIMARY KEY,
    slug text NOT NULL UNIQUE,
    name text NOT NULL,
    owner_user_id uuid NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT organizations_slug_valid CHECK (slug ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    CONSTRAINT organizations_name_length CHECK (char_length(name) BETWEEN 1 AND 100)
);

CREATE TABLE organization_members (
    org_id uuid NOT NULL REFERENCES organizations (id) ON DELETE RESTRICT,
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    role text NOT NULL,
    status text NOT NULL,
    PRIMARY KEY (org_id, user_id),
    CONSTRAINT organization_members_role_valid CHECK (role IN ('owner', 'admin', 'member')),
    CONSTRAINT organization_members_status_valid CHECK (status IN ('invited', 'active', 'disabled'))
);

CREATE INDEX organization_members_user_idx ON organization_members (user_id);
CREATE INDEX organizations_owner_idx ON organizations (owner_user_id);
