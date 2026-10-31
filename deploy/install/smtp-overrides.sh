#!/bin/sh
# Emits only fixed Compose arguments, never configuration values or secrets.
# The caller deliberately splits these fixed words before its command.
smtp_overrides() {
    smtp_env=$1
    [ -f "$smtp_env" ] || return 0
    smtp_value() { sed -n "s/^$1=//p" "$smtp_env" | tail -n 1; }
    smtp_transport=$(smtp_value CRAFTCLOUD_MAIL_TRANSPORT)
    smtp_user=$(smtp_value CRAFTCLOUD_SMTP_USERNAME)
    smtp_password_source=$(smtp_value CRAFTCLOUD_SMTP_PASSWORD_SOURCE)
    graph_cert=$(smtp_value CRAFTCLOUD_GRAPH_CERT_SOURCE)
    graph_key=$(smtp_value CRAFTCLOUD_GRAPH_KEY_SOURCE)
    graph_secret=$(smtp_value CRAFTCLOUD_GRAPH_CLIENT_SECRET_SOURCE)
    smtp_auth=$(smtp_value CRAFTCLOUD_SMTP_AUTH)
    oauth_provider=$(smtp_value CRAFTCLOUD_SMTP_OAUTH_PROVIDER)
    oauth_cert=$(smtp_value CRAFTCLOUD_SMTP_OAUTH_CERT_SOURCE)
    oauth_key=$(smtp_value CRAFTCLOUD_SMTP_OAUTH_KEY_SOURCE)
    oauth_secret=$(smtp_value CRAFTCLOUD_SMTP_OAUTH_CLIENT_SECRET_SOURCE)
    oauth_service=$(smtp_value CRAFTCLOUD_SMTP_OAUTH_SERVICE_ACCOUNT_SOURCE)

    case "$smtp_auth" in
        ''|password|none|xoauth2) ;;
        *) printf '%s\n' 'Unsupported SMTP authentication mode' >&2; return 1 ;;
    esac
    if { [ -n "$oauth_provider" ] && [ "$smtp_auth" != xoauth2 ]; } ||
       { [ "$smtp_auth" = xoauth2 ] && [ -z "$oauth_provider" ]; }; then
        printf '%s\n' 'SMTP XOAUTH2 and its provider must be configured together' >&2
        return 1
    fi
    if [ "$smtp_auth" = password ] && { [ -z "$smtp_user" ] || [ -z "$smtp_password_source" ]; }; then
        printf '%s\n' 'SMTP password authentication requires username and password source' >&2
        return 1
    fi
    if [ "$smtp_auth" = none ] && [ -n "$smtp_user$smtp_password_source" ]; then
        printf '%s\n' 'SMTP relay mode conflicts with authentication credentials' >&2
        return 1
    fi
    if [ -z "$oauth_provider" ] && [ -n "$smtp_user" ] && [ -z "$smtp_password_source" ]; then
        printf '%s\n' 'SMTP username and password source must be configured together' >&2
        return 1
    fi
    if [ -z "$oauth_provider" ] && [ -z "$smtp_user" ] && [ -n "$smtp_password_source" ]; then
        printf '%s\n' 'SMTP username and password source must be configured together' >&2
        return 1
    fi
    if { [ -n "$graph_cert" ] && [ -z "$graph_key" ]; } ||
       { [ -z "$graph_cert" ] && [ -n "$graph_key" ]; }; then
        printf '%s\n' 'Graph certificate and key sources must be configured together' >&2
        return 1
    fi
    graph_mode=none
    if [ -n "$graph_cert" ] && [ -n "$graph_key" ]; then graph_mode=certificate; fi
    if [ -n "$graph_secret" ]; then
        if [ "$oauth_provider" = google ] && [ "$oauth_mode" != service ]; then
        printf '%s\n' 'Google SMTP OAuth requires a service-account file' >&2
        return 1
    fi
    [ "$graph_mode" = none ] || { printf '%s\n' 'Graph certificate and client secret are conflicting credentials' >&2; return 1; }
        graph_mode=secret
    fi
    if [ "$graph_mode" != none ] && [ "$smtp_transport" != graph ]; then
        printf '%s\n' 'Graph credentials require CRAFTCLOUD_MAIL_TRANSPORT=graph' >&2
        return 1
    fi
    if [ "$smtp_transport" = graph ] && [ "$graph_mode" = none ]; then
        printf '%s\n' 'Graph transport requires certificate or client secret credentials' >&2
        return 1
    fi
    case "$oauth_provider" in
        '')
            if [ -n "$oauth_cert$oauth_key$oauth_secret$oauth_service" ]; then
                printf '%s\n' 'SMTP OAuth credentials require an OAuth provider' >&2
                return 1
            fi
            oauth_mode=none
            ;;
        microsoft|google)
            if [ -n "$oauth_cert" ] && [ -z "$oauth_key" ] || [ -z "$oauth_cert" ] && [ -n "$oauth_key" ]; then
                printf '%s\n' 'SMTP OAuth certificate and key sources must be configured together' >&2
                return 1
            fi
            oauth_mode=none
            if [ -n "$oauth_cert" ] && [ -n "$oauth_key" ]; then oauth_mode=certificate; fi
            if [ -n "$oauth_secret" ]; then
                [ "$oauth_mode" = none ] || { printf '%s\n' 'SMTP OAuth certificate and client secret are conflicting credentials' >&2; return 1; }
                oauth_mode=secret
            fi
            if [ -n "$oauth_service" ]; then
                [ "$oauth_provider" = google ] || { printf '%s\n' 'SMTP OAuth service account is supported only for Google' >&2; return 1; }
                [ "$oauth_mode" = none ] || { printf '%s\n' 'SMTP OAuth service account conflicts with other credentials' >&2; return 1; }
                oauth_mode=service
            fi
            [ "$oauth_mode" != none ] || { printf '%s\n' 'SMTP OAuth provider requires certificate, client secret, or Google service account credentials' >&2; return 1; }
            [ -z "$smtp_password_source" ] || { printf '%s\n' 'SMTP OAuth conflicts with password credentials' >&2; return 1; }
            [ -n "$smtp_user" ] || { printf '%s\n' 'SMTP OAuth requires SMTP username mailbox' >&2; return 1; }
            [ "$smtp_transport" = smtp ] || { printf '%s\n' 'SMTP OAuth requires CRAFTCLOUD_MAIL_TRANSPORT=smtp' >&2; return 1; }
            ;;
        *) printf '%s\n' 'Unsupported SMTP OAuth provider in craftcloud.env' >&2; return 1 ;;
    esac
    if [ "$oauth_provider" = google ] && [ "$oauth_mode" != service ]; then
        printf '%s\n' 'Google SMTP OAuth requires a service-account file' >&2
        return 1
    fi
    [ "$graph_mode" = none ] || { [ -z "$oauth_provider$smtp_user$smtp_password_source" ] || { printf '%s\n' 'Graph credentials conflict with SMTP credentials' >&2; return 1; }; }
    case "$smtp_transport" in
        '') [ "$oauth_mode" = none ] && [ "$graph_mode" = none ] || { printf '%s\n' 'Mail credentials require a mail transport' >&2; return 1; }; return 0 ;;
        smtp)
            printf '%s' '-f compose.smtp.yaml'
            if [ "$oauth_mode" != none ]; then
                printf '%s' " -f compose.smtp-oauth-$oauth_provider-$oauth_mode.yaml"
            elif [ -n "$smtp_password_source" ]; then
                printf '%s' ' -f compose.smtp-auth.yaml'
            fi
            ;;
        graph)
            printf '%s' '-f compose.graph.yaml'
            printf '%s' " -f compose.graph-$graph_mode.yaml"
            ;;
        *) printf '%s\n' 'Unsupported mail transport in craftcloud.env' >&2; return 1 ;;
    esac
}

# Keeps only overlays that release actually ships. Missing selected overlay
# disables mail for rollback; never changes authentication mode.
smtp_release_files() {
    rel_dir=$1; rel_list=$2; rel_version=$3
    [ -n "$rel_list" ] || return 0
    for overlay in ${rel_list#-f }; do
        case "$overlay" in -f) continue ;; esac
        overlay=${overlay#-f }
        if [ ! -f "$rel_dir/$overlay" ]; then
            printf '%s\n' "craftcloud-ctl: warning: release $rel_version has no $overlay; running it without mail delivery" >&2
            return 0
        fi
    done
    printf '%s' "$rel_list"
}
