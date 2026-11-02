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
    updated_by uuid NULL REFERENCES users(id) ON DELETE SET NULL,
    -- Retention rows hold {"value": days} and, for 24 hours after a shortening,
    -- {"previous": days} (the longer period that keeps applying meanwhile).
    CONSTRAINT instance_settings_retention_days CHECK (
        key NOT LIKE 'retention.%' OR CASE
            WHEN jsonb_typeof(value -> 'value') = 'number'
                 AND (value ->> 'value') ~ '^[0-9]{1,4}$'
                 AND (value -> 'previous' IS NULL
                      OR (jsonb_typeof(value -> 'previous') = 'number'
                          AND (value ->> 'previous') ~ '^[0-9]{1,4}$'))
            THEN (value ->> 'value')::int BETWEEN
                     CASE WHEN key = 'retention.audit_days' THEN 30 ELSE 1 END AND 3650
                 AND (value -> 'previous' IS NULL
                      OR (value ->> 'previous')::int BETWEEN
                         CASE WHEN key = 'retention.audit_days' THEN 30 ELSE 1 END AND 3650)
            ELSE false
        END
    )
);

CREATE INDEX instance_settings_updated_idx ON instance_settings (updated_at);

-- Installations that finished the first-run setup before this migration must
-- not see the wizard banner after the upgrade: every step counts as resolved.
INSERT INTO instance_setup_steps (step, status)
SELECT step, CASE WHEN step = 'summary' THEN 'done' ELSE 'skipped' END
FROM unnest(ARRAY[
    'system_check', 'two_factor', 'email_delivery', 'organization',
    'workspace_project', 'invitations', 'optional_services', 'summary'
]) AS step
WHERE EXISTS (SELECT 1 FROM instance_setup);
