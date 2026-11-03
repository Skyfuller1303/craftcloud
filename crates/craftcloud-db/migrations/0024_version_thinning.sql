-- AC-14: version pins and validated instance-level thinning settings.
ALTER TABLE file_versions ADD COLUMN pinned boolean NOT NULL DEFAULT false;

ALTER TABLE instance_settings
    ADD CONSTRAINT instance_settings_version_thinning CHECK (
        key NOT IN ('versions.keep_all_hours', 'versions.daily_days',
                    'versions.weekly_weeks', 'versions.max_per_file')
        OR CASE
            WHEN jsonb_typeof(value -> 'value') = 'number'
                 AND (value ->> 'value') ~ '^[0-9]{1,5}$'
                 AND (value -> 'previous' IS NULL
                      OR (jsonb_typeof(value -> 'previous') = 'number'
                          AND (value ->> 'previous') ~ '^[0-9]{1,5}$'))
            THEN CASE key
                WHEN 'versions.keep_all_hours' THEN
                    (value ->> 'value')::int BETWEEN 0 AND 168 AND
                    (value -> 'previous' IS NULL OR (value ->> 'previous')::int BETWEEN 0 AND 168)
                WHEN 'versions.daily_days' THEN
                    (value ->> 'value')::int BETWEEN 0 AND 90 AND
                    (value -> 'previous' IS NULL OR (value ->> 'previous')::int BETWEEN 0 AND 90)
                WHEN 'versions.weekly_weeks' THEN
                    (value ->> 'value')::int BETWEEN 0 AND 52 AND
                    (value -> 'previous' IS NULL OR (value ->> 'previous')::int BETWEEN 0 AND 52)
                WHEN 'versions.max_per_file' THEN
                    ((value ->> 'value')::int = 0 OR (value ->> 'value')::int BETWEEN 2 AND 10000) AND
                    (value -> 'previous' IS NULL OR (value ->> 'previous')::int = 0 OR (value ->> 'previous')::int BETWEEN 2 AND 10000)
                ELSE false
            END
            ELSE false
        END
    );
