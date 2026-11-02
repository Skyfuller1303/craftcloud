-- AC-13: resumable setup progress and operator-managed instance settings.
ALTER TABLE sessions ADD COLUMN mfa_verified_at timestamptz NULL;
CREATE TABLE instance_setup_steps (
    step text PRIMARY KEY CHECK (step IN (
        'system_check', 'two_factor', 'email_delivery', 'organization',
        'workspace_project', 'invitations', 'optional_services', 'summary'
    )),
    status text NOT NULL CHECK (status IN ('pending', 'done', 'skipped')),
    updated_at timestamptz NOT NULL DEFAULT now(),
    updated_by uuid NULL REFERENCES users(id) ON DELETE SET NULL
);

CREATE TABLE instance_settings (
    key text PRIMARY KEY CHECK (key ~ '^[a-z][a-z0-9_.]{0,95}$'),
    value jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    updated_by uuid NULL REFERENCES users(id) ON DELETE SET NULL
);

CREATE INDEX instance_settings_updated_idx ON instance_settings (updated_at);
