-- AC-11: organization seat caps and per-member Craft app assignments.
ALTER TABLE organizations
    ADD COLUMN seat_limit integer NULL CHECK (seat_limit IS NULL OR seat_limit >= 0),
    ADD COLUMN app_access_mode text NOT NULL DEFAULT 'all'
        CHECK (app_access_mode IN ('all', 'assigned'));

CREATE TABLE member_app_access (
    org_id uuid NOT NULL,
    user_id uuid NOT NULL,
    app text NOT NULL CHECK (app ~ '^[a-z0-9-]{1,32}$'),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (org_id, user_id, app),
    FOREIGN KEY (org_id, user_id) REFERENCES organization_members(org_id, user_id) ON DELETE CASCADE
);
CREATE INDEX member_app_access_app_idx ON member_app_access(org_id, app, user_id);

-- Preserve the credential origin of newly issued FigCraft links. Existing
-- links are unknown-origin and are revoked conservatively on app entitlement loss.
ALTER TABLE figcraft_links
    ADD COLUMN source_kind text NOT NULL DEFAULT 'legacy'
        CHECK (source_kind IN ('legacy', 'app', 'launcher', 'cli', 'browser')),
    ADD COLUMN source_app text NULL
        CHECK (source_app IS NULL OR source_app ~ '^[a-z0-9-]{1,32}$');
ALTER TABLE figcraft_links ADD CONSTRAINT figcraft_links_source_kind_matches_app_check
    CHECK ((source_kind = 'app') = (source_app IS NOT NULL));
DROP INDEX figcraft_links_one_active_idx;
CREATE UNIQUE INDEX figcraft_links_one_source_active_idx
    ON figcraft_links(file_id, user_id, access, source_kind, COALESCE(source_app, ''))
    WHERE revoked_at IS NULL;

ALTER TABLE figcraft_outbox DROP CONSTRAINT figcraft_outbox_reason_check;
ALTER TABLE figcraft_outbox ADD CONSTRAINT figcraft_outbox_reason_check
    CHECK (reason IN ('grant_revoked', 'role_changed', 'membership_removed',
        'account_disabled', 'file_deleted', 'security_changed', 'unpublish')
        OR reason ~ '^app_access_revoked:(legacy|app:[a-z0-9-]{1,32})$');
