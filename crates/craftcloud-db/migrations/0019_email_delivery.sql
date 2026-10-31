-- 0019: durable outbound email queue. Capabilities stay encrypted and are
-- excluded from backups; the queue is operational state, not user data.
-- 'suppressed' marks a security notice that the per-recipient limit dropped;
-- it is never delivered and only counted for the operator status.
CREATE TABLE outbound_emails (
    id uuid PRIMARY KEY,
    recipient text NOT NULL CHECK (char_length(recipient) BETWEEN 3 AND 320),
    template text NOT NULL CHECK (template ~ '^[a-z][a-z0-9_]{0,63}$'),
    language text NOT NULL CHECK (language IN ('en', 'de')),
    params jsonb NOT NULL DEFAULT '{}'::jsonb,
    encrypted_token bytea NULL,
    expires_at timestamptz NOT NULL,
    -- Provenance only. Avoid a user-row foreign-key lock that can deadlock
    -- when a notification is queued while another transaction updates that user.
    actor_id uuid NULL,
    is_resend boolean NOT NULL DEFAULT false,
    status text NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued', 'sending', 'sent', 'failed', 'expired', 'suppressed')),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts BETWEEN 0 AND 8),
    created_at timestamptz NOT NULL,
    available_at timestamptz NOT NULL,
    claimed_until timestamptz NULL,
    lease_token uuid NULL,
    sent_at timestamptz NULL,
    failure_code text NULL CHECK (failure_code IS NULL OR failure_code ~ '^[a-z0-9_]{1,64}$'),
    CHECK (
        (status = 'sending' AND claimed_until IS NOT NULL AND lease_token IS NOT NULL)
        OR (status <> 'sending' AND claimed_until IS NULL AND lease_token IS NULL)
    ),
    CHECK (status NOT IN ('sent', 'failed', 'expired', 'suppressed') OR encrypted_token IS NULL)
);
CREATE INDEX outbound_emails_ready_idx ON outbound_emails(available_at, created_at, id)
    WHERE status IN ('queued', 'sending');
CREATE INDEX outbound_emails_recipient_rate_idx ON outbound_emails(recipient, template, created_at);
CREATE INDEX outbound_emails_actor_rate_idx ON outbound_emails(actor_id, created_at)
    WHERE actor_id IS NOT NULL;

ALTER TABLE invitations ADD COLUMN language text NOT NULL DEFAULT 'en'
    CHECK (language IN ('en', 'de'));
ALTER TABLE invitations ADD COLUMN email_delivery boolean NOT NULL DEFAULT false;

ALTER TABLE users ADD COLUMN security_version bigint NOT NULL DEFAULT 0
    CHECK (security_version >= 0);

CREATE TABLE email_verification_tokens (
    token_hash bytea PRIMARY KEY CHECK (octet_length(token_hash) = 32),
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    normalized_email text NOT NULL CHECK (char_length(normalized_email) BETWEEN 3 AND 320),
    created_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    CHECK (expires_at > created_at)
);
CREATE INDEX email_verification_tokens_user_idx ON email_verification_tokens(user_id, created_at);

CREATE TABLE pending_email_changes (
    user_id uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    original_email text NOT NULL CHECK (char_length(original_email) BETWEEN 3 AND 320),
    normalized_email text NOT NULL UNIQUE CHECK (char_length(normalized_email) BETWEEN 3 AND 320),
    token_hash bytea NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
    security_version bigint NOT NULL CHECK (security_version >= 0),
    factor_fingerprint bytea NULL CHECK (factor_fingerprint IS NULL OR octet_length(factor_fingerprint)=32),
    requested_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    CHECK (expires_at > requested_at)
);
