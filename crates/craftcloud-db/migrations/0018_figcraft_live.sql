-- 0018: FigCraft live state, personal links and durable remote-operation outbox.
-- Link capabilities are encrypted by the API with the AC-02 server key.
ALTER TABLE file_versions ADD COLUMN actor_kind text NOT NULL DEFAULT 'user'
    CHECK (actor_kind IN ('user', 'figcraft_live'));

CREATE TABLE figcraft_live_files (
    file_id uuid PRIMARY KEY,
    org_id uuid NOT NULL,
    workspace_id uuid NOT NULL,
    figcraft_file_id text NULL,
    published_from_version_id uuid NOT NULL,
    last_snapshot_version_id uuid NULL,
    last_snapshot_at timestamptz NULL,
    last_snapshot_attempt_at timestamptz NULL,
    final_snapshot_done boolean NOT NULL DEFAULT false,
    state text NOT NULL CHECK (state IN ('publishing', 'live', 'unpublishing', 'republish')),
    published_by_user_id uuid NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    FOREIGN KEY (org_id, workspace_id, file_id)
        REFERENCES files(org_id, workspace_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (file_id, published_from_version_id)
        REFERENCES file_versions(file_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (file_id, last_snapshot_version_id)
        REFERENCES file_versions(file_id, id) ON DELETE RESTRICT
);
CREATE INDEX figcraft_live_files_reconcile_idx ON figcraft_live_files(state, updated_at);
CREATE INDEX figcraft_live_files_snapshot_idx ON figcraft_live_files(last_snapshot_at, last_snapshot_attempt_at) WHERE state = 'live';

CREATE TABLE figcraft_links (
    id uuid PRIMARY KEY,
    file_id uuid NOT NULL REFERENCES figcraft_live_files(file_id) ON DELETE RESTRICT,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    access text NOT NULL CHECK (access IN ('edit', 'view')),
    figcraft_link_id text NULL,
    token_ciphertext bytea NULL,
    created_at timestamptz NOT NULL,
    revoked_at timestamptz NULL,
    UNIQUE(file_id, user_id, access, id)
);
CREATE UNIQUE INDEX figcraft_links_one_active_idx
    ON figcraft_links(file_id, user_id, access) WHERE revoked_at IS NULL;

CREATE INDEX figcraft_links_revoked_idx ON figcraft_links(revoked_at) WHERE revoked_at IS NOT NULL;

CREATE TABLE figcraft_outbox (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    file_id uuid NOT NULL REFERENCES figcraft_live_files(file_id) ON DELETE RESTRICT,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    reason text NOT NULL CHECK (reason IN ('grant_revoked', 'role_changed', 'membership_removed', 'account_disabled', 'file_deleted', 'security_changed', 'unpublish')),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    available_at timestamptz NOT NULL,
    claimed_at timestamptz NULL,
    created_at timestamptz NOT NULL,
    UNIQUE(file_id, user_id, reason)
);
CREATE INDEX figcraft_outbox_ready_idx ON figcraft_outbox(available_at, id) WHERE claimed_at IS NULL;
