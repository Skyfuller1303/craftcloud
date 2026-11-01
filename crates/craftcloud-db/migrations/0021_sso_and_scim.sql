-- 0021: organization-owned OIDC and SCIM (AC-10).
-- Forward-only. Rollback requires a pre-migration backup restored in isolation.
CREATE TABLE organization_sso (
 org_id uuid PRIMARY KEY REFERENCES organizations(id) ON DELETE RESTRICT,
 discovery_url text NOT NULL CHECK (octet_length(discovery_url) BETWEEN 1 AND 2048),
 client_id text NOT NULL CHECK (octet_length(client_id) BETWEEN 1 AND 256),
 client_secret_ciphertext bytea NOT NULL CHECK (octet_length(client_secret_ciphertext) BETWEEN 30 AND 8192),
 allowed_domains text[] NOT NULL CHECK (cardinality(allowed_domains) BETWEEN 1 AND 100),
 enabled boolean NOT NULL DEFAULT true,
 jit_enabled boolean NOT NULL DEFAULT false,
 sso_required boolean NOT NULL DEFAULT false,
 trust_mfa boolean NOT NULL DEFAULT true,
 version bigint NOT NULL DEFAULT 1 CHECK(version > 0),
 updated_at timestamptz NOT NULL DEFAULT now()
);
-- An allowed domain is inert until the instance operator approves the claim.
-- A domain can be approved for at most one organization; domains are stored as
-- lower-case ASCII (punycode for internationalized names), validated by the API.
CREATE TABLE sso_domain_claims (
 org_id uuid NOT NULL REFERENCES organization_sso(org_id) ON DELETE CASCADE,
 domain text NOT NULL CHECK(domain = lower(domain) AND octet_length(domain) BETWEEN 3 AND 253),
 status text NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','approved','rejected')),
 requested_at timestamptz NOT NULL DEFAULT now(),
 decided_at timestamptz NULL,
 decided_by uuid NULL REFERENCES users(id) ON DELETE RESTRICT,
 PRIMARY KEY(org_id,domain),
 CHECK((status='pending') = (decided_at IS NULL))
);
CREATE UNIQUE INDEX sso_domain_claims_approved_idx ON sso_domain_claims(lower(domain)) WHERE status='approved';
CREATE INDEX sso_domain_claims_status_idx ON sso_domain_claims(status,requested_at);
CREATE TABLE instance_sso_policy (
 singleton boolean PRIMARY KEY DEFAULT true CHECK(singleton),
 enabled boolean NOT NULL DEFAULT true
);
CREATE TABLE external_identities (
 org_id uuid NOT NULL REFERENCES organizations(id) ON DELETE RESTRICT,
 issuer text NOT NULL CHECK(octet_length(issuer) BETWEEN 1 AND 2048),
 subject text NOT NULL CHECK(octet_length(subject) BETWEEN 1 AND 255),
 user_id uuid NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
 created_at timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY(issuer,subject),
 UNIQUE(org_id,issuer,subject)
);
CREATE INDEX external_identities_user_idx ON external_identities(user_id,org_id);
-- These security-state tables must not be restored from backups.
CREATE TABLE oidc_transactions (
 state_hash bytea PRIMARY KEY CHECK(octet_length(state_hash)=32),
 launch_hash bytea NOT NULL UNIQUE CHECK(octet_length(launch_hash)=32),
 browser_claimed boolean NOT NULL DEFAULT false,
 org_id uuid NOT NULL REFERENCES organizations(id) ON DELETE RESTRICT,
 config_version bigint NOT NULL,
 binding_hash bytea NOT NULL CHECK(octet_length(binding_hash)=32),
 secrets_ciphertext bytea NOT NULL CHECK(octet_length(secrets_ciphertext) BETWEEN 30 AND 16384),
 expires_at timestamptz NOT NULL,
 created_at timestamptz NOT NULL,
 CHECK(expires_at > created_at AND expires_at <= created_at + interval '10 minutes')
);
CREATE INDEX oidc_transactions_expiry_idx ON oidc_transactions(expires_at);
CREATE TABLE oidc_pending_logins (
 token_hash bytea PRIMARY KEY CHECK(octet_length(token_hash)=32),
 org_id uuid NOT NULL REFERENCES organizations(id) ON DELETE RESTRICT,
 user_id uuid NULL REFERENCES users(id) ON DELETE RESTRICT,
 config_version bigint NOT NULL,
 binding_hash bytea NOT NULL CHECK(octet_length(binding_hash)=32),
 payload_ciphertext bytea NOT NULL CHECK(octet_length(payload_ciphertext) BETWEEN 30 AND 32768),
 attempts integer NOT NULL DEFAULT 0 CHECK(attempts BETWEEN 0 AND 5),
 created_at timestamptz NOT NULL,
 expires_at timestamptz NOT NULL,
 CHECK(expires_at > created_at AND expires_at <= created_at + interval '10 minutes')
);
CREATE INDEX oidc_pending_expiry_idx ON oidc_pending_logins(expires_at);
CREATE TABLE sso_session_bindings (
 session_kind text NOT NULL CHECK(session_kind IN ('browser','device')),
 session_id uuid NOT NULL,
 org_id uuid NOT NULL REFERENCES organizations(id) ON DELETE RESTRICT,
 user_id uuid NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
 mfa boolean NOT NULL,
 provider_mfa boolean NOT NULL DEFAULT false,
 -- Exactly one of these mirrors session_id, so purging or deleting the session cascades.
 browser_session_id uuid GENERATED ALWAYS AS (CASE WHEN session_kind='browser' THEN session_id END) STORED REFERENCES sessions(id) ON DELETE CASCADE,
 device_session_id uuid GENERATED ALWAYS AS (CASE WHEN session_kind='device' THEN session_id END) STORED REFERENCES device_sessions(id) ON DELETE CASCADE,
 PRIMARY KEY(session_kind,session_id,org_id)
);
CREATE TABLE organization_session_cutoffs (
 org_id uuid NOT NULL REFERENCES organizations(id) ON DELETE RESTRICT,
 user_id uuid NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
 revoked_before timestamptz NOT NULL,
 PRIMARY KEY(org_id,user_id)
);
CREATE TABLE scim_tokens (
 org_id uuid PRIMARY KEY REFERENCES organizations(id) ON DELETE RESTRICT,
 token_hash bytea NOT NULL CHECK(octet_length(token_hash)=32),
 created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE scim_users (
 org_id uuid NOT NULL REFERENCES organizations(id) ON DELETE RESTRICT,
 id uuid NOT NULL,
 user_id uuid NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
 external_id text NULL CHECK(octet_length(external_id)<=255),
 user_name text NOT NULL CHECK(octet_length(user_name) BETWEEN 3 AND 320),
 display_name text NOT NULL CHECK(char_length(display_name) BETWEEN 1 AND 100),
 active boolean NOT NULL DEFAULT true,
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY(org_id,id), UNIQUE(org_id,user_name), UNIQUE(org_id,user_id)
);
CREATE UNIQUE INDEX scim_users_external_idx ON scim_users(org_id,external_id) WHERE external_id IS NOT NULL;
CREATE TABLE scim_groups (
 org_id uuid NOT NULL REFERENCES organizations(id) ON DELETE RESTRICT,
 id uuid NOT NULL,
 external_id text NULL CHECK(octet_length(external_id)<=255),
 display_name text NOT NULL CHECK(char_length(display_name) BETWEEN 1 AND 100),
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY(org_id,id), UNIQUE(org_id,display_name)
);
CREATE UNIQUE INDEX scim_groups_external_idx ON scim_groups(org_id,external_id) WHERE external_id IS NOT NULL;
CREATE TABLE scim_group_members (
 org_id uuid NOT NULL,
 group_id uuid NOT NULL,
 scim_user_id uuid NOT NULL,
 PRIMARY KEY(org_id,group_id,scim_user_id),
 FOREIGN KEY(org_id,group_id) REFERENCES scim_groups(org_id,id) ON DELETE CASCADE,
 FOREIGN KEY(org_id,scim_user_id) REFERENCES scim_users(org_id,id) ON DELETE CASCADE
);
CREATE TABLE scim_group_mappings (
 org_id uuid NOT NULL,
 group_id uuid NOT NULL,
 workspace_id uuid NOT NULL,
 role text NOT NULL CHECK(role IN ('viewer','editor')),
 PRIMARY KEY(org_id,group_id,workspace_id),
 FOREIGN KEY(org_id,group_id) REFERENCES scim_groups(org_id,id) ON DELETE CASCADE,
 FOREIGN KEY(org_id,workspace_id) REFERENCES workspaces(org_id,id) ON DELETE RESTRICT
);
CREATE TABLE scim_workspace_ownership (
 org_id uuid NOT NULL,
 workspace_id uuid NOT NULL,
 user_id uuid NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
 applied_role text NOT NULL CHECK(applied_role IN ('viewer','editor')),
 PRIMARY KEY(org_id,workspace_id,user_id),
 FOREIGN KEY(org_id,workspace_id) REFERENCES workspaces(org_id,id) ON DELETE RESTRICT
);
