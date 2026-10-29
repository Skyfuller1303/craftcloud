-- 0006: workspace API support and invitations (CC-F03, CC-F04).
-- Purpose: workspace versions for ETag/If-Match, the organization setting that
-- lets workspace admins invite, join times of workspace members (cursor order
-- without personal data) and the `invitations` table.
-- Tenant scope: every table here is tenant-bound by `org_id`; invitations
-- reference workspaces through the composite key (org_id, workspace_id), so an
-- invitation can never point at a workspace of another organization.
-- Restore note: forward-only; back up before applying and restore the backup
-- to roll back. Invitations store only a SHA-256 hash of the secret token.
-- Membership statuses are not used for invitations: invitations are their own
-- table and become organization/workspace memberships only on acceptance.

ALTER TABLE workspaces ADD COLUMN version bigint NOT NULL DEFAULT 1;
ALTER TABLE workspaces ADD CONSTRAINT workspaces_version_positive CHECK (version >= 1);
CREATE INDEX workspaces_org_created_idx ON workspaces (org_id, created_at, id);

ALTER TABLE workspace_members ADD COLUMN created_at timestamptz NOT NULL DEFAULT now();
CREATE INDEX workspace_members_ws_joined_idx ON workspace_members (workspace_id, created_at, user_id);

ALTER TABLE organizations
    ADD COLUMN workspace_admin_invites_enabled boolean NOT NULL DEFAULT true;

CREATE TABLE invitations (
    id uuid PRIMARY KEY,
    org_id uuid NOT NULL REFERENCES organizations (id) ON DELETE RESTRICT,
    normalized_email text NOT NULL,
    org_role text NOT NULL,
    workspace_id uuid NULL,
    workspace_role text NULL,
    token_hash bytea NOT NULL UNIQUE,
    created_by_user_id uuid NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    created_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    accepted_at timestamptz NULL,
    accepted_by_user_id uuid NULL REFERENCES users (id) ON DELETE RESTRICT,
    revoked_at timestamptz NULL,
    -- Set when the inviter's rights no longer cover the invitation at acceptance time.
    invalidated_at timestamptz NULL,
    CONSTRAINT invitations_email_normalized CHECK (lower(normalized_email) = normalized_email),
    CONSTRAINT invitations_email_length CHECK (char_length(normalized_email) BETWEEN 3 AND 320),
    CONSTRAINT invitations_org_role_valid CHECK (org_role IN ('admin', 'member')),
    CONSTRAINT invitations_workspace_role_valid
        CHECK (workspace_role IS NULL OR workspace_role IN ('admin', 'editor', 'viewer')),
    CONSTRAINT invitations_workspace_pair CHECK ((workspace_id IS NULL) = (workspace_role IS NULL)),
    CONSTRAINT invitations_token_hash_length CHECK (octet_length(token_hash) = 32),
    CONSTRAINT invitations_expiry_after_creation CHECK (expires_at > created_at),
    CONSTRAINT invitations_workspace_fk FOREIGN KEY (org_id, workspace_id)
        REFERENCES workspaces (org_id, id) ON DELETE RESTRICT
);

-- At most one open invitation per organization and e-mail address. Expired
-- but unclosed rows are closed (invalidated_at) by the creating transaction
-- before it inserts a new one.
CREATE UNIQUE INDEX invitations_one_open_per_email_idx
    ON invitations (org_id, normalized_email)
    WHERE accepted_at IS NULL AND revoked_at IS NULL AND invalidated_at IS NULL;
CREATE INDEX invitations_org_created_idx ON invitations (org_id, created_at, id);
CREATE INDEX invitations_email_idx ON invitations (normalized_email);

-- Account provenance (no e-mail delivery exists yet, so addresses are not
-- verified). email_verified_at stays NULL for every account until a
-- verification flow exists; created_via_invitation_id records which invitation
-- created an account, so origin organization and inviter stay derivable for the
-- instance operator.
ALTER TABLE users
    ADD COLUMN email_verified_at timestamptz NULL,
    ADD COLUMN created_via_invitation_id uuid NULL REFERENCES invitations (id) ON DELETE RESTRICT;
CREATE INDEX users_created_idx ON users (created_at, id);
