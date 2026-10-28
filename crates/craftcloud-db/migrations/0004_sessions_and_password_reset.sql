-- 0004: web sessions and password reset tokens (CC-F02).
-- Purpose: server-side sessions for the cookie-based web login and
-- single-use password reset links issued by an operator or the offline CLI.
-- Tenant scope: instance-wide identity data (a user may belong to several
-- organizations), not tenant-bound. Only SHA-256 hashes of the bearer tokens
-- are stored; the raw tokens exist only in the cookie / reset link. The CSRF
-- token is derived from the raw session token and therefore needs no column.
-- No client IP address and no user agent are stored (data minimisation,
-- ADR-0016).
-- Restore note: forward-only; back up before applying and restore the backup
-- to roll back. Restoring an older backup invalidates sessions created after
-- it, which is the safe direction. All foreign keys are ON DELETE RESTRICT.

CREATE TABLE sessions (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    token_hash bytea NOT NULL UNIQUE,
    created_at timestamptz NOT NULL,
    last_seen_at timestamptz NOT NULL,
    absolute_expires_at timestamptz NOT NULL,
    revoked_at timestamptz NULL,
    CONSTRAINT sessions_token_hash_length CHECK (octet_length(token_hash) = 32),
    CONSTRAINT sessions_expiry_after_creation CHECK (absolute_expires_at > created_at)
);

CREATE INDEX sessions_user_idx ON sessions (user_id);
CREATE INDEX sessions_absolute_expiry_idx ON sessions (absolute_expires_at);
-- Cleanup of revoked sessions only needs the (few) revoked rows.
CREATE INDEX sessions_revoked_idx ON sessions (revoked_at) WHERE revoked_at IS NOT NULL;

CREATE TABLE password_reset_tokens (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    token_hash bytea NOT NULL UNIQUE,
    created_by_user_id uuid NULL REFERENCES users (id) ON DELETE RESTRICT,
    created_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    used_at timestamptz NULL,
    CONSTRAINT password_reset_token_hash_length CHECK (octet_length(token_hash) = 32),
    CONSTRAINT password_reset_expiry_after_creation CHECK (expires_at > created_at)
);

CREATE INDEX password_reset_tokens_user_idx ON password_reset_tokens (user_id);
CREATE INDEX password_reset_tokens_expiry_idx ON password_reset_tokens (expires_at);
CREATE INDEX password_reset_tokens_used_idx ON password_reset_tokens (used_at) WHERE used_at IS NOT NULL;
