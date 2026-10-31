#!/bin/sh
# Emits only fixed Compose arguments, never configuration values or secrets.
# The caller deliberately splits these fixed words before its command.
smtp_overrides() {
    smtp_env=$1
    [ -f "$smtp_env" ] || return 0
    smtp_transport=$(sed -n 's/^CRAFTCLOUD_MAIL_TRANSPORT=//p' "$smtp_env" | tail -n 1)
    smtp_user=$(sed -n 's/^CRAFTCLOUD_SMTP_USERNAME=//p' "$smtp_env" | tail -n 1)
    smtp_password_source=$(sed -n 's/^CRAFTCLOUD_SMTP_PASSWORD_SOURCE=//p' "$smtp_env" | tail -n 1)
    if { [ -n "$smtp_user" ] && [ -z "$smtp_password_source" ]; } ||
       { [ -z "$smtp_user" ] && [ -n "$smtp_password_source" ]; }; then
        printf '%s\n' 'SMTP username and password source must be configured together' >&2
        return 1
    fi
    case "$smtp_transport" in
        '') return 0 ;;
        smtp)
            printf '%s' '-f compose.smtp.yaml'
            if [ -n "$smtp_password_source" ]; then
                printf '%s' ' -f compose.smtp-auth.yaml'
            fi
            ;;
        *) printf '%s\n' 'Unsupported mail transport in craftcloud.env' >&2; return 1 ;;
    esac
}
# smtp_release_files <release-compose-dir> <overrides from smtp_overrides> <version>
# Keeps only overlays that the release actually ships, so a rollback to a
# release from before SMTP support runs without mail instead of failing.
smtp_release_files() {
    rel_dir=$1; rel_list=$2; rel_version=$3
    [ -n "$rel_list" ] || return 0
    if [ ! -f "$rel_dir/compose.smtp.yaml" ]; then
        printf '%s\n' "craftcloud-ctl: warning: release $rel_version has no compose.smtp.yaml; running it without SMTP delivery" >&2
        return 0
    fi
    case $rel_list in
        *compose.smtp-auth.yaml*)
            if [ ! -f "$rel_dir/compose.smtp-auth.yaml" ]; then
                printf '%s\n' "craftcloud-ctl: warning: release $rel_version has no compose.smtp-auth.yaml; running it without SMTP authentication" >&2
                printf '%s' '-f compose.smtp.yaml'
                return 0
            fi
            ;;
    esac
    printf '%s' "$rel_list"
}
