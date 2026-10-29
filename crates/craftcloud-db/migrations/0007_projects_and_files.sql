-- 0007: projects and file metadata (CC-F06).
-- Purpose: a flat list of projects per workspace and of files per project, as
-- metadata only. File contents and versions come with later migrations; the
-- head_version_id column is reserved for them and has no foreign key yet.
-- Tenant scope: projects reference (org_id, workspace_id) of their workspace and
-- files reference (org_id, workspace_id, project_id) of their project through
-- composite foreign keys, so a row can never point into another tenant.
-- Names are untrusted display metadata and are never used as paths. name_key
-- (NFC + case folded, computed by the application) makes names unique among
-- non-deleted rows.
-- Soft delete: deleted_at/deleted_by_user_id mark the trash; nothing is purged.
-- Restore note: forward-only; back up before applying. All foreign keys are
-- ON DELETE RESTRICT, users that created or changed rows cannot be deleted.

CREATE TABLE projects (
    id uuid PRIMARY KEY,
    org_id uuid NOT NULL,
    workspace_id uuid NOT NULL,
    name text NOT NULL,
    name_key text NOT NULL,
    name_hash bytea NOT NULL,
    version bigint NOT NULL DEFAULT 1,
    created_by_user_id uuid NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    created_at timestamptz NOT NULL,
    updated_by_user_id uuid NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    updated_at timestamptz NOT NULL,
    deleted_at timestamptz NULL,
    deleted_by_user_id uuid NULL REFERENCES users (id) ON DELETE RESTRICT,
    CONSTRAINT projects_name_length CHECK (char_length(name) BETWEEN 1 AND 100),
    CONSTRAINT projects_name_key_length CHECK (char_length(name_key) BETWEEN 1 AND 4096),
    CONSTRAINT projects_version_positive CHECK (version >= 1),
    CONSTRAINT projects_deleted_pair CHECK ((deleted_at IS NULL) = (deleted_by_user_id IS NULL)),
    CONSTRAINT projects_workspace_fk FOREIGN KEY (org_id, workspace_id)
        REFERENCES workspaces (org_id, id) ON DELETE RESTRICT,
    CONSTRAINT projects_scope_unique UNIQUE (org_id, workspace_id, id)
);

-- name_hash is SHA-256 of name_key, maintained by a trigger. NFKC keys can exceed
-- the btree row limit, so the unique indexes use the fixed-size hash instead of
-- the key; the application compares the full keys first.
CREATE FUNCTION set_name_hash() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.name_hash := sha256(convert_to(NEW.name_key, 'UTF8'));
    RETURN NEW;
END;
$$;

CREATE TRIGGER projects_name_hash BEFORE INSERT OR UPDATE OF name_key ON projects
    FOR EACH ROW EXECUTE FUNCTION set_name_hash();

-- One live project per name key and workspace.
CREATE UNIQUE INDEX projects_live_name_idx ON projects (workspace_id, name_hash)
    WHERE deleted_at IS NULL;
CREATE INDEX projects_list_idx ON projects (workspace_id, created_at, id);

CREATE TABLE files (
    id uuid PRIMARY KEY,
    org_id uuid NOT NULL,
    workspace_id uuid NOT NULL,
    project_id uuid NOT NULL,
    name text NOT NULL,
    name_key text NOT NULL,
    name_hash bytea NOT NULL,
    app text NOT NULL,
    format text NOT NULL,
    size_bytes bigint NOT NULL DEFAULT 0,
    sha256 bytea NULL,
    -- Set by the first committed version (later migration adds the foreign key).
    head_version_id uuid NULL,
    version bigint NOT NULL DEFAULT 1,
    created_by_user_id uuid NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    created_at timestamptz NOT NULL,
    updated_by_user_id uuid NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    updated_at timestamptz NOT NULL,
    deleted_at timestamptz NULL,
    deleted_by_user_id uuid NULL REFERENCES users (id) ON DELETE RESTRICT,
    CONSTRAINT files_name_length CHECK (octet_length(name) BETWEEN 1 AND 255),
    CONSTRAINT files_name_key_length CHECK (octet_length(name_key) BETWEEN 1 AND 8192),
    CONSTRAINT files_app_valid CHECK (app ~ '^[a-z0-9-]{1,32}$'),
    CONSTRAINT files_format_valid CHECK (format ~ '^[a-z0-9.+-]{1,64}$'),
    CONSTRAINT files_size_non_negative CHECK (size_bytes >= 0),
    CONSTRAINT files_sha256_length CHECK (sha256 IS NULL OR octet_length(sha256) = 32),
    CONSTRAINT files_version_positive CHECK (version >= 1),
    CONSTRAINT files_deleted_pair CHECK ((deleted_at IS NULL) = (deleted_by_user_id IS NULL)),
    CONSTRAINT files_project_fk FOREIGN KEY (org_id, workspace_id, project_id)
        REFERENCES projects (org_id, workspace_id, id) ON DELETE RESTRICT
);

-- One live file per name key and project.
CREATE TRIGGER files_name_hash BEFORE INSERT OR UPDATE OF name_key ON files
    FOR EACH ROW EXECUTE FUNCTION set_name_hash();

CREATE UNIQUE INDEX files_live_name_idx ON files (project_id, name_hash)
    WHERE deleted_at IS NULL;
CREATE INDEX files_list_idx ON files (project_id, created_at, id);
