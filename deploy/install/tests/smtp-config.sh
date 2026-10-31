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
printf '%s\n' 'CRAFTCLOUD_MAIL_TRANSPORT=invalid' >"$config"
if smtp_overrides "$config" >/dev/null 2>&1; then exit 1; fi
# Releases from before SMTP support do not ship the overlays.
mkdir "$SMTP_TMP/old" "$SMTP_TMP/new"
full='-f compose.smtp.yaml -f compose.smtp-auth.yaml'
[ -z "$(smtp_release_files "$SMTP_TMP/old" "$full" 0.0.9 2>/dev/null)" ]
smtp_release_files "$SMTP_TMP/old" "$full" 0.0.9 2>&1 >/dev/null | grep -F 'no compose.smtp.yaml' >/dev/null
[ -z "$(smtp_release_files "$SMTP_TMP/old" '' 0.0.9)" ]
: >"$SMTP_TMP/new/compose.smtp.yaml"
[ "$(smtp_release_files "$SMTP_TMP/new" "$full" 0.1.0 2>/dev/null)" = '-f compose.smtp.yaml' ]
: >"$SMTP_TMP/new/compose.smtp-auth.yaml"
[ "$(smtp_release_files "$SMTP_TMP/new" "$full" 0.1.0)" = "$full" ]
printf '%s\n' 'SMTP overlay tests passed'
