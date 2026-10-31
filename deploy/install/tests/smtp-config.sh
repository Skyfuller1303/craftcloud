#!/bin/sh
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd -P)
. "$ROOT/deploy/install/smtp-overrides.sh"
SMTP_TMP=$(mktemp -d)
trap 'rm -rf -- "$SMTP_TMP"' EXIT HUP INT TERM
config=$SMTP_TMP/config
: >"$config"
[ -z "$(smtp_overrides "$config")" ]
printf '%s\n' 'CRAFTCLOUD_MAIL_TRANSPORT=smtp' >"$config"
[ "$(smtp_overrides "$config")" = '-f compose.smtp.yaml' ]
printf '%s\n' 'CRAFTCLOUD_SMTP_USERNAME=synthetic-user' >>"$config"
if smtp_overrides "$config" >/dev/null 2>&1; then exit 1; fi
printf '%s\n' 'CRAFTCLOUD_SMTP_PASSWORD_SOURCE=/synthetic/password' >>"$config"
[ "$(smtp_overrides "$config")" = '-f compose.smtp.yaml -f compose.smtp-auth.yaml' ]

# Graph certificate and client-secret modes.
cat >"$config" <<'EOF'
CRAFTCLOUD_MAIL_TRANSPORT=graph
CRAFTCLOUD_GRAPH_CERT_SOURCE=/synthetic/cert
CRAFTCLOUD_GRAPH_KEY_SOURCE=/synthetic/key
EOF
[ "$(smtp_overrides "$config")" = '-f compose.graph.yaml -f compose.graph-certificate.yaml' ]
printf '%s\n' 'CRAFTCLOUD_GRAPH_CLIENT_SECRET_SOURCE=/synthetic/secret' >>"$config"
if smtp_overrides "$config" >/dev/null 2>&1; then exit 1; fi
sed -i '/CRAFTCLOUD_GRAPH_CERT_SOURCE/d; /CRAFTCLOUD_GRAPH_KEY_SOURCE/d' "$config"
[ "$(smtp_overrides "$config")" = '-f compose.graph.yaml -f compose.graph-secret.yaml' ]

# A Graph secret next to SMTP OAuth settings fails with a message, not an unset-variable error.
cat >"$config" <<'EOF'
CRAFTCLOUD_MAIL_TRANSPORT=graph
CRAFTCLOUD_GRAPH_CLIENT_SECRET_SOURCE=/synthetic/secret
CRAFTCLOUD_SMTP_AUTH=xoauth2
CRAFTCLOUD_SMTP_OAUTH_PROVIDER=google
EOF
if smtp_overrides "$config" >"$SMTP_TMP/out" 2>"$SMTP_TMP/err"; then exit 1; fi
! grep -qi 'unbound' "$SMTP_TMP/err"
[ -s "$SMTP_TMP/err" ]

# Microsoft and Google OAuth credential modes.
cat >"$config" <<'EOF'
CRAFTCLOUD_MAIL_TRANSPORT=smtp
CRAFTCLOUD_SMTP_USERNAME=mailbox@example.test
CRAFTCLOUD_SMTP_AUTH=xoauth2
CRAFTCLOUD_SMTP_OAUTH_PROVIDER=microsoft
CRAFTCLOUD_SMTP_OAUTH_TENANT_ID=tenant
CRAFTCLOUD_SMTP_OAUTH_CLIENT_ID=client
CRAFTCLOUD_SMTP_OAUTH_CERT_SOURCE=/synthetic/cert
CRAFTCLOUD_SMTP_OAUTH_KEY_SOURCE=/synthetic/key
EOF
[ "$(smtp_overrides "$config")" = '-f compose.smtp.yaml -f compose.smtp-oauth-microsoft-certificate.yaml' ]
printf '%s\n' 'CRAFTCLOUD_SMTP_OAUTH_CLIENT_SECRET_SOURCE=/synthetic/secret' >>"$config"
if smtp_overrides "$config" >/dev/null 2>&1; then exit 1; fi
sed -i '/CRAFTCLOUD_SMTP_OAUTH_CERT_SOURCE/d; /CRAFTCLOUD_SMTP_OAUTH_KEY_SOURCE/d' "$config"
[ "$(smtp_overrides "$config")" = '-f compose.smtp.yaml -f compose.smtp-oauth-microsoft-secret.yaml' ]
sed -i 's/microsoft/google/' "$config"
if smtp_overrides "$config" >/dev/null 2>&1; then exit 1; fi
sed -i '/CRAFTCLOUD_SMTP_OAUTH_CLIENT_SECRET_SOURCE/d' "$config"
printf '%s\n' 'CRAFTCLOUD_SMTP_OAUTH_SERVICE_ACCOUNT_SOURCE=/synthetic/service.json' >>"$config"
[ "$(smtp_overrides "$config")" = '-f compose.smtp.yaml -f compose.smtp-oauth-google-service.yaml' ]

# Reject orphan, missing, and conflicting credential pairs.
sed -i '/CRAFTCLOUD_SMTP_OAUTH_PROVIDER/d' "$config"
if smtp_overrides "$config" >/dev/null 2>&1; then exit 1; fi
cat >"$config" <<'EOF'
CRAFTCLOUD_MAIL_TRANSPORT=smtp
CRAFTCLOUD_SMTP_USERNAME=mailbox@example.test
CRAFTCLOUD_SMTP_OAUTH_PROVIDER=google
CRAFTCLOUD_SMTP_OAUTH_CLIENT_SECRET_SOURCE=/synthetic/secret
CRAFTCLOUD_SMTP_PASSWORD_SOURCE=/synthetic/password
EOF
if smtp_overrides "$config" >/dev/null 2>&1; then exit 1; fi
cat >"$config" <<'EOF'
CRAFTCLOUD_MAIL_TRANSPORT=graph
CRAFTCLOUD_GRAPH_CERT_SOURCE=/synthetic/cert
EOF
if smtp_overrides "$config" >/dev/null 2>&1; then exit 1; fi
printf '%s\n' 'CRAFTCLOUD_MAIL_TRANSPORT=invalid' >"$config"
if smtp_overrides "$config" >/dev/null 2>&1; then exit 1; fi

# Missing selected overlay disables mail on old releases, with no auth downgrade.
mkdir "$SMTP_TMP/old" "$SMTP_TMP/new"
full='-f compose.smtp.yaml -f compose.smtp-oauth-google-service.yaml'
[ -z "$(smtp_release_files "$SMTP_TMP/old" "$full" 0.0.9 2>/dev/null)" ]
smtp_release_files "$SMTP_TMP/old" "$full" 0.0.9 2>&1 >/dev/null | grep -F 'no compose.smtp.yaml' >/dev/null
[ -z "$(smtp_release_files "$SMTP_TMP/old" '' 0.0.9)" ]
: >"$SMTP_TMP/new/compose.smtp.yaml"
[ -z "$(smtp_release_files "$SMTP_TMP/new" "$full" 0.1.0 2>/dev/null)" ]
: >"$SMTP_TMP/new/compose.smtp-oauth-google-service.yaml"
[ "$(smtp_release_files "$SMTP_TMP/new" "$full" 0.1.0)" = "$full" ]
# Missing authentication overlay disables mail instead of falling back to a relay.
legacy='-f compose.smtp.yaml -f compose.smtp-auth.yaml'
[ -z "$(smtp_release_files "$SMTP_TMP/new" "$legacy" 0.1.0 2>/dev/null)" ]
# Explicit password mode must never select an unauthenticated relay.
cat >"$config" <<'EOF'
CRAFTCLOUD_MAIL_TRANSPORT=smtp
CRAFTCLOUD_SMTP_AUTH=password
EOF
if smtp_overrides "$config" >/dev/null 2>&1; then exit 1; fi
cat >>"$config" <<'EOF'
CRAFTCLOUD_SMTP_USERNAME=mailbox@example.test
CRAFTCLOUD_SMTP_PASSWORD_SOURCE=/synthetic/password
EOF
[ "$(smtp_overrides "$config")" = '-f compose.smtp.yaml -f compose.smtp-auth.yaml' ]
printf '%s\n' 'SMTP overlay tests passed'
