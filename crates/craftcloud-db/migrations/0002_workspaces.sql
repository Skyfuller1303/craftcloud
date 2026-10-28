-- 0002: workspaces and workspace membership.
-- Purpose: workspaces inside an organization and per-workspace roles.
-- Tenant scope: both tables carry `org_id`. Composite foreign keys guarantee
-- that a workspace member belongs to the same organization as the workspace
-- and is a member of that organization.
-- Restore note: forward-only; back up before applying and restore the backup
-- to roll back. All foreign keys are ON DELETE RESTRICT.

CREATE TABLE workspaces (
    id uuid PRIMARY KEY,
    org_id uuid NOT NULL REFERENCES organizations (id) ON DELETE RESTRICT,
    slug text NOT NULL,
    name text NOT NULL,
    quota_bytes bigint NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT workspaces_slug_valid CHECK (slug ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    CONSTRAINT workspaces_name_length CHECK (char_length(name) BETWEEN 1 AND 100),
    CONSTRAINT workspaces_quota_non_negative CHECK (quota_bytes IS NULL OR quota_bytes >= 0),
    CONSTRAINT workspaces_org_slug_unique UNIQUE (org_id, slug),
    CONSTRAINT workspaces_org_id_unique UNIQUE (org_id, id)
);

CREATE TABLE workspace_members (
    org_id uuid NOT NULL,
    workspace_id uuid NOT NULL,
    user_id uuid NOT NULL,
    role text NOT NULL,
    PRIMARY KEY (workspace_id, user_id),
    CONSTRAINT workspace_members_role_valid CHECK (role IN ('admin', 'editor', 'viewer')),
    CONSTRAINT workspace_members_workspace_fk FOREIGN KEY (org_id, workspace_id)
        REFERENCES workspaces (org_id, id) ON DELETE RESTRICT,
    CONSTRAINT workspace_members_org_member_fk FOREIGN KEY (org_id, user_id)
        REFERENCES organization_members (org_id, user_id) ON DELETE RESTRICT
);

CREATE INDEX workspace_members_org_user_idx ON workspace_members (org_id, user_id);
