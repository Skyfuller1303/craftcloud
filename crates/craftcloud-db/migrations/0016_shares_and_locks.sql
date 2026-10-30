-- 0016: object grants, guests and edit leases (AC-03).
-- Grants and leases are scoped to the organization and workspace. Leases are
-- deliberately ephemeral and are excluded from backups.
ALTER TABLE files ADD CONSTRAINT files_org_workspace_id_unique UNIQUE (org_id, workspace_id, id);
ALTER TABLE files ADD CONSTRAINT files_org_id_unique UNIQUE (org_id, id);

ALTER TABLE organization_members DROP CONSTRAINT organization_members_role_valid;
ALTER TABLE organization_members ADD CONSTRAINT organization_members_role_valid
    CHECK (role IN ('owner', 'admin', 'member', 'guest'));

ALTER TABLE invitations DROP CONSTRAINT invitations_org_role_valid;
ALTER TABLE invitations ADD CONSTRAINT invitations_org_role_valid
    CHECK (org_role IN ('admin', 'member', 'guest'));
ALTER TABLE invitations DROP CONSTRAINT invitations_workspace_pair;
ALTER TABLE invitations ADD CONSTRAINT invitations_workspace_pair CHECK (
    (org_role = 'guest' AND workspace_role IS NULL)
    OR (org_role <> 'guest' AND ((workspace_id IS NULL) = (workspace_role IS NULL)))
);

CREATE TABLE grants (
    id uuid PRIMARY KEY,
    org_id uuid NOT NULL REFERENCES organizations (id) ON DELETE RESTRICT,
    workspace_id uuid NOT NULL,
    target_kind text NOT NULL,
    target_id uuid NOT NULL,
    project_id uuid NULL,
    file_id uuid NULL,
    grantee_user_id uuid NOT NULL,
    role text NOT NULL,
    created_by_user_id uuid NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    created_at timestamptz NOT NULL,
    revoked_at timestamptz NULL,
    version bigint NOT NULL DEFAULT 1,
    CONSTRAINT grants_target_kind CHECK (target_kind IN ('project', 'file')),
    CONSTRAINT grants_role CHECK (role IN ('viewer', 'editor')),
    CONSTRAINT grants_version_positive CHECK (version >= 1),
    CONSTRAINT grants_target_shape CHECK (
        (target_kind = 'project' AND project_id IS NOT NULL AND project_id = target_id AND file_id IS NULL)
        OR (target_kind = 'file' AND file_id IS NOT NULL AND file_id = target_id AND project_id IS NULL)
    ),
    CONSTRAINT grants_workspace_fk FOREIGN KEY (org_id, workspace_id)
        REFERENCES workspaces (org_id, id) ON DELETE RESTRICT,
    CONSTRAINT grants_project_fk FOREIGN KEY (org_id, workspace_id, project_id)
        REFERENCES projects (org_id, workspace_id, id) ON DELETE RESTRICT,
    CONSTRAINT grants_file_fk FOREIGN KEY (org_id, workspace_id, file_id)
        REFERENCES files (org_id, workspace_id, id) ON DELETE RESTRICT,
    CONSTRAINT grants_grantee_fk FOREIGN KEY (grantee_user_id)
        REFERENCES users (id) ON DELETE RESTRICT
);
CREATE UNIQUE INDEX grants_one_active_per_target_user_idx
    ON grants (org_id, target_kind, target_id, grantee_user_id) WHERE revoked_at IS NULL;
CREATE INDEX grants_user_created_idx ON grants (grantee_user_id, created_at, id)
    WHERE revoked_at IS NULL;
CREATE INDEX grants_workspace_user_idx ON grants (org_id, workspace_id, grantee_user_id)
    WHERE revoked_at IS NULL;

CREATE TABLE file_locks (
    file_id uuid PRIMARY KEY REFERENCES files (id) ON DELETE RESTRICT,
    org_id uuid NOT NULL,
    holder_user_id uuid NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    holder_session_kind text NOT NULL,
    holder_session_id uuid NOT NULL,
    client_id text NOT NULL,
    acquired_at timestamptz NOT NULL,
    refreshed_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    CONSTRAINT file_locks_session_kind CHECK (holder_session_kind IN ('browser', 'device')),
    CONSTRAINT file_locks_client_id CHECK (client_id ~ '^[a-z0-9-]{1,32}$'),
    CONSTRAINT file_locks_times CHECK (acquired_at <= refreshed_at AND refreshed_at < expires_at),
    CONSTRAINT file_locks_org_file_fk FOREIGN KEY (org_id, file_id)
        REFERENCES files (org_id, id) ON DELETE RESTRICT
);
CREATE INDEX file_locks_expiry_idx ON file_locks (expires_at);


