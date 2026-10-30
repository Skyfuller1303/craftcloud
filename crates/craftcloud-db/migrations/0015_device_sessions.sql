-- 0015: desktop device sessions and refresh-token replay history (AC-01).
-- Device credentials remain separate from browser cookie sessions. Only
-- SHA-256 token hashes are stored; raw credentials are returned once.
-- Restore must not revive sessions or refresh credentials.

CREATE TABLE device_sessions (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    name varchar(64) NOT NULL,
    client_kind text NOT NULL CHECK (client_kind IN ('app', 'launcher', 'cli')),
    client_id varchar(32) NOT NULL CHECK (client_id ~ '^[a-z0-9-]{1,32}$'),
    refresh_hash bytea NOT NULL UNIQUE,
    refresh_family uuid NOT NULL,
    access_hash bytea NULL UNIQUE,
    access_expires_at timestamptz NULL,
    created_at timestamptz NOT NULL,
    last_used_at timestamptz NOT NULL,
    idle_expires_at timestamptz NOT NULL,
    absolute_expires_at timestamptz NOT NULL,
    revoked_at timestamptz NULL,
    revoke_reason text NULL,
    CONSTRAINT device_sessions_refresh_hash_length CHECK (octet_length(refresh_hash) = 32),
    CONSTRAINT device_sessions_access_hash_length CHECK (access_hash IS NULL OR octet_length(access_hash) = 32),
    CONSTRAINT device_sessions_expiry_order CHECK (
        idle_expires_at > created_at AND absolute_expires_at > created_at
        AND idle_expires_at <= absolute_expires_at
    ),
    CONSTRAINT device_sessions_access_pair CHECK (
        (access_hash IS NULL) = (access_expires_at IS NULL)
    ),
    CONSTRAINT device_sessions_revoke_reason CHECK (
        revoke_reason IS NULL OR revoke_reason IN ('refresh_reuse', 'logout', 'user_revoked', 'security')
    )
);

CREATE INDEX device_sessions_user_idx ON device_sessions (user_id, created_at DESC, id);
CREATE INDEX device_sessions_expiry_idx ON device_sessions (idle_expires_at, absolute_expires_at);
CREATE INDEX device_sessions_revoked_idx ON device_sessions (revoked_at) WHERE revoked_at IS NOT NULL;
CREATE INDEX device_sessions_family_idx ON device_sessions (refresh_family);

CREATE TABLE device_refresh_history (
    device_session_id uuid NOT NULL REFERENCES device_sessions (id) ON DELETE CASCADE,
    refresh_hash bytea NOT NULL UNIQUE,
    rotated_at timestamptz NOT NULL,
    PRIMARY KEY (device_session_id, refresh_hash),
    CONSTRAINT device_refresh_history_hash_length CHECK (octet_length(refresh_hash) = 32)
);

CREATE INDEX device_refresh_history_rotated_idx ON device_refresh_history (rotated_at);
