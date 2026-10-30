-- 0017: account TOTP, one-use recovery hashes and short-lived login challenges.

ALTER TABLE organizations
    ADD COLUMN require_two_factor boolean NOT NULL DEFAULT false;

CREATE TABLE user_two_factor (
    user_id uuid PRIMARY KEY REFERENCES users (id) ON DELETE RESTRICT,
    secret_ciphertext bytea NOT NULL,
    enabled_at timestamptz NULL,
    last_accepted_step bigint NULL,
    failed_attempts integer NOT NULL DEFAULT 0,
    pending_session_id uuid NULL,
    pending_session_kind text NULL,
    pending_expires_at timestamptz NULL,
    created_at timestamptz NOT NULL,
    CONSTRAINT user_two_factor_ciphertext_length CHECK (octet_length(secret_ciphertext) BETWEEN 30 AND 256),
    CONSTRAINT user_two_factor_failed_attempts_nonnegative CHECK (failed_attempts >= 0),
    CONSTRAINT user_two_factor_step_nonnegative CHECK (last_accepted_step IS NULL OR last_accepted_step >= 0),
    CONSTRAINT user_two_factor_pending_shape CHECK (
        (enabled_at IS NOT NULL AND pending_session_id IS NULL AND pending_session_kind IS NULL AND pending_expires_at IS NULL)
        OR (enabled_at IS NULL AND pending_session_id IS NOT NULL AND pending_session_kind IN ('browser','device') AND pending_expires_at IS NOT NULL)
    )
);

CREATE TABLE user_two_factor_recovery_codes (
    user_id uuid NOT NULL REFERENCES user_two_factor (user_id) ON DELETE CASCADE,
    salt bytea NOT NULL,
    code_hash bytea NOT NULL,
    created_at timestamptz NOT NULL,
    used_at timestamptz NULL,
    PRIMARY KEY (user_id, salt),
    CONSTRAINT user_two_factor_recovery_salt_length CHECK (octet_length(salt) = 16),
    CONSTRAINT user_two_factor_recovery_hash_length CHECK (octet_length(code_hash) = 32)
);

CREATE TABLE two_factor_login_challenges (
    id_hash bytea PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    password_hash_digest bytea NOT NULL,
    login_kind text NOT NULL,
    client_address text NOT NULL,
    device_client_kind text NULL,
    device_client_id text NULL,
    device_name text NULL,
    attempts smallint NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    used_at timestamptz NULL,
    CONSTRAINT two_factor_challenge_id_hash_length CHECK (octet_length(id_hash) = 32),
    CONSTRAINT two_factor_challenge_password_digest_length CHECK (octet_length(password_hash_digest) = 32),
    CONSTRAINT two_factor_challenge_login_kind CHECK (login_kind IN ('browser', 'device')),
    CONSTRAINT two_factor_challenge_device_shape CHECK (
        (login_kind = 'browser' AND device_client_kind IS NULL AND device_client_id IS NULL AND device_name IS NULL)
        OR (login_kind = 'device' AND device_client_kind IN ('app', 'launcher', 'cli')
            AND device_client_id ~ '^[a-z0-9-]{1,32}$' AND device_name IS NOT NULL AND char_length(device_name) BETWEEN 1 AND 64)
    ),
    CONSTRAINT two_factor_challenge_attempts CHECK (attempts BETWEEN 0 AND 5),
    CONSTRAINT two_factor_challenge_expiry CHECK (expires_at > created_at AND expires_at <= created_at + interval '5 minutes')
);

CREATE INDEX two_factor_login_challenges_expiry_idx ON two_factor_login_challenges (expires_at);

