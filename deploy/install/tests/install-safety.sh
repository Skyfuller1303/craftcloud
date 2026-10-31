#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd -P)
TMP=${TMPDIR:-/tmp}/craftcloud-ops01-test-$$
trap 'if [ "${KEEP_TEST:-0}" = 1 ]; then echo "test artifacts: $TMP"; else rm -rf "$TMP"; fi' EXIT HUP INT TERM
mkdir -p "$TMP/bin" "$TMP/source/craftcloud-0.1.0/deploy/compose/caddy" "$TMP/output"

cat >"$TMP/bin/docker" <<'DOCKER'
#!/bin/sh
printf '%s\n' "$*" >>"${DOCKER_LOG:-/dev/null}"
case "$*" in
    *"${DOCKER_FAIL:-@@never@@}"*) exit 1 ;;
    'compose version --short') echo 2.24.0 ;;
    'version --format {{.Server.Version}}') echo 25.0.0 ;;
    *'exec -T caddy wget -qO- http://server:8080/api/v1/setup/status'*) echo '{"setup_required":false}' ;;
    *'exec -T server /usr/local/bin/craftcloud-server healthcheck'*) exit 0 ;;
esac
exit 0
DOCKER
cat >"$TMP/bin/df" <<'DF'
#!/bin/sh
printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/test 20000000 100 19999900 1%% /\n'
DF
chmod +x "$TMP/bin/docker" "$TMP/bin/df"
printf '%s\n' 'services: {}' >"$TMP/source/craftcloud-0.1.0/deploy/compose/compose.yaml"
printf '%s\n' 'services: {}' >"$TMP/source/craftcloud-0.1.0/deploy/compose/compose.install.yaml"
printf '%s\n' 'services: {}' >"$TMP/source/craftcloud-0.1.0/deploy/compose/compose.figcraft.yaml"
printf '%s\n' 'localhost { import tls-{$CRAFTCLOUD_TLS_MODE}.caddy }' >"$TMP/source/craftcloud-0.1.0/deploy/compose/caddy/Caddyfile"
printf '%s\n' 'tls /etc/caddy/fullchain.pem /etc/caddy/private.key' >"$TMP/source/craftcloud-0.1.0/deploy/compose/caddy/tls-custom.caddy"
printf '{"version":"0.1.0","commit":"0123456"}\n' >"$TMP/source/craftcloud-0.1.0/CRAFTCLOUD-RELEASE.json"
(cd "$TMP/source" && tar -czf "$TMP/output/craftcloud-0.1.0.tar.gz" craftcloud-0.1.0)
(cd "$TMP/output" && sha256sum craftcloud-0.1.0.tar.gz >SHA256SUMS)

PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_IMAGE_PREFIX=craftcloud-server:ops01- CRAFTCLOUD_INSTALL_ROOT="$TMP/opt/craftcloud" \
CRAFTCLOUD_CONFIG_DIR="$TMP/etc/craftcloud" CRAFTCLOUD_DATA_DIR="$TMP/data" \
CRAFTCLOUD_SYSTEMD_DIR="$TMP/systemd" CRAFTCLOUD_PROJECT_NAME=craftcloud-ops01 DOCKER_LOG="$TMP/docker.log" \
sh -x "$ROOT/deploy/install/install.sh" --non-interactive --domain localhost \
  --acme-email test@example.invalid --http-port 28180 --https-port 28543 \
  --data-dir "$TMP/data" "$TMP/output/craftcloud-0.1.0.tar.gz" >"$TMP/install.log" 2>&1

[ "$(stat -c '%a' "$TMP/etc/craftcloud/craftcloud.env")" = 600 ]
[ "$(stat -c '%a' "$TMP/etc/craftcloud/bootstrap-token")" = 600 ]
[ "$(stat -c '%a' "$TMP/opt/craftcloud/.craftcloud-installation")" = 600 ]
[ "$(stat -c '%a' "$TMP/etc/craftcloud/.craftcloud-installation")" = 600 ]
[ "$(stat -c '%a' "$TMP/data/.craftcloud-installation")" = 600 ]
[ "$(sed -n 's/^CRAFTCLOUD_IMAGE=//p' "$TMP/etc/craftcloud/craftcloud.env")" = 'craftcloud-server:ops01-0.1.0' ]
[ "$(stat -c '%u:%g' "$TMP/data/postgres")" = 999:999 ]
[ "$(stat -c '%u:%g' "$TMP/data/storage")" = 65532:65532 ]
[ "$(stat -c '%u:%g' "$TMP/data/backups")" = 65532:65532 ]
[ -f "$TMP/etc/craftcloud/caddy/tls-custom.caddy" ]
[ "$(sed -n 's/^CRAFTCLOUD_PUBLIC_URL=//p' "$TMP/etc/craftcloud/craftcloud.env")" = 'https://localhost:28543' ]
[ -L "$TMP/opt/craftcloud/current" ]
# The 2FA encryption key is generated once: Base64 of 32 bytes, kept on rerun.
two_factor_key=$(sed -n 's/^CC_SECRET_TWO_FACTOR_KEY=//p' "$TMP/etc/craftcloud/craftcloud.env")
[ "${#two_factor_key}" -eq 44 ]
[ "$(printf '%s' "$two_factor_key" | base64 -d | wc -c)" -eq 32 ]
[ -f "$TMP/etc/craftcloud/caddy/.shipped-sha256" ]
cp "$TMP/etc/craftcloud/bootstrap-token" "$TMP/bootstrap-before"
PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_IMAGE_PREFIX=craftcloud-server:ops01- CRAFTCLOUD_INSTALL_ROOT="$TMP/opt/craftcloud" \
CRAFTCLOUD_CONFIG_DIR="$TMP/etc/craftcloud" CRAFTCLOUD_DATA_DIR="$TMP/data" \
CRAFTCLOUD_SYSTEMD_DIR="$TMP/systemd" CRAFTCLOUD_PROJECT_NAME=craftcloud-ops01 DOCKER_LOG="$TMP/docker.log" \
sh -x "$ROOT/deploy/install/install.sh" --non-interactive --domain localhost \
  --acme-email test@example.invalid --http-port 28180 --https-port 28543 \
  --data-dir "$TMP/data" "$TMP/output/craftcloud-0.1.0.tar.gz" >"$TMP/reinstall.log" 2>&1
cmp "$TMP/bootstrap-before" "$TMP/etc/craftcloud/bootstrap-token"
[ "$(sed -n 's/^CC_SECRET_TWO_FACTOR_KEY=//p' "$TMP/etc/craftcloud/craftcloud.env")" = "$two_factor_key" ]
[ "$(grep -c '^CC_SECRET_TWO_FACTOR_KEY=' "$TMP/etc/craftcloud/craftcloud.env")" -eq 1 ]

# FigCraft companion: enabled by default. Every Compose call of the installer
# carries the overlay, the key file is private, and the key never reaches a log.
figcraft_env="$TMP/etc/craftcloud/craftcloud.env"
[ "$(sed -n 's/^CRAFTCLOUD_FIGCRAFT=//p' "$figcraft_env")" = enabled ]
[ "$(sed -n 's/^CRAFTCLOUD_FIGCRAFT_SECRET_FILE=//p' "$figcraft_env")" = "$TMP/etc/craftcloud/figcraft.key" ]
[ "$(sed -n 's/^CRAFTCLOUD_FIGCRAFT_PUBLIC_URL=//p' "$figcraft_env")" = 'wss://localhost:28543/figcraft/' ]
[ "$(stat -c '%a' "$TMP/etc/craftcloud/figcraft.key")" = 600 ]
figcraft_key=$(cat "$TMP/etc/craftcloud/figcraft.key")
[ "${#figcraft_key}" -eq 64 ]
case $figcraft_key in *[!0-9a-f]*) echo 'FigCraft key is not hexadecimal' >&2; exit 1 ;; esac
[ -f "$TMP/etc/craftcloud/caddy/optional/figcraft.caddy" ]
compose_calls=$(grep -c -- '--project-name craftcloud-ops01 ' "$TMP/docker.log")
[ "$compose_calls" -gt 0 ]
# Every call that names compose.yaml also names the overlay (and the build includes the service).
if grep -- '-f compose.yaml ' "$TMP/docker.log" | grep -v -- '-f compose.figcraft.yaml' >/dev/null; then
    echo 'a Compose call of the default install omitted the FigCraft overlay' >&2
    exit 1
fi
grep -F -- '-f compose.install.yaml -f compose.figcraft.yaml -f compose.setup.yaml up -d' "$TMP/docker.log" >/dev/null
grep -E -- 'build .* migrate server backup restore figcraft$' "$TMP/docker.log" >/dev/null
if grep -F -- "$figcraft_key" "$TMP/install.log" "$TMP/reinstall.log" "$TMP/docker.log" >/dev/null; then
    echo 'the FigCraft key was printed' >&2
    exit 1
fi
[ "$(grep -c '^CRAFTCLOUD_FIGCRAFT=' "$figcraft_env")" -eq 1 ]

# Invalid ACME addresses are rejected (the character class once allowed a range
# that included , / : ; < = > ?). A normal address passes a dry run.
for bad_email in 'a,b@example.org' 'a/b@example.org' 'a:b@example.org' 'a?b@example.org' 'a<b@example.org'; do
    if PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/email/opt" CRAFTCLOUD_CONFIG_DIR="$TMP/email/etc" \
       CRAFTCLOUD_DATA_DIR="$TMP/email/data" sh "$ROOT/deploy/install/install.sh" --dry-run --domain localhost \
       --acme-email "$bad_email" --data-dir "$TMP/email/data" "$TMP/output/craftcloud-0.1.0.tar.gz" >"$TMP/bad-email.log" 2>&1; then
        echo "installer accepted ACME email $bad_email" >&2
        exit 1
    fi
    grep -F 'invalid ACME email' "$TMP/bad-email.log" >/dev/null
done
PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/email/opt" CRAFTCLOUD_CONFIG_DIR="$TMP/email/etc" \
CRAFTCLOUD_DATA_DIR="$TMP/email/data" sh "$ROOT/deploy/install/install.sh" --dry-run --domain localhost \
  --acme-email 'first.last+tag@example-host.org' --data-dir "$TMP/email/data" "$TMP/output/craftcloud-0.1.0.tar.gz" >/dev/null 2>&1

# Helper for the retry tests below: build a release archive from a source tree.
# usage: make_release <version> <commit> [no-metadata]
make_release() {
    mkdir -p "$TMP/source/craftcloud-$1/deploy/compose/caddy"
    printf '%s\n' 'services: {}' >"$TMP/source/craftcloud-$1/deploy/compose/compose.yaml"
    printf '%s\n' 'services: {}' >"$TMP/source/craftcloud-$1/deploy/compose/compose.install.yaml"
    printf '%s\n' 'services: {}' >"$TMP/source/craftcloud-$1/deploy/compose/compose.figcraft.yaml"
    printf '%s\n' 'localhost { import tls-{$CRAFTCLOUD_TLS_MODE}.caddy }' >"$TMP/source/craftcloud-$1/deploy/compose/caddy/Caddyfile"
    printf '%s\n' 'tls /etc/caddy/fullchain.pem /etc/caddy/private.key' >"$TMP/source/craftcloud-$1/deploy/compose/caddy/tls-custom.caddy"
    if [ "${3:-}" != no-metadata ]; then
        printf '{"version":"%s","commit":"%s"}\n' "$1" "$2" >"$TMP/source/craftcloud-$1/CRAFTCLOUD-RELEASE.json"
    fi
    (cd "$TMP/source" && tar -czf "$TMP/output/craftcloud-$1.tar.gz" "craftcloud-$1")
    (cd "$TMP/output" && sha256sum "craftcloud-$1.tar.gz" >SHA256SUMS)
}

# A failed first install (here: the migration fails) leaves no active release; a
# rerun must succeed and reuse the complete release directory.
retry_env() {
    PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_IMAGE_PREFIX=craftcloud-server:retry- CRAFTCLOUD_INSTALL_ROOT="$TMP/retry/opt" \
    CRAFTCLOUD_CONFIG_DIR="$TMP/retry/etc" CRAFTCLOUD_DATA_DIR="$TMP/retry/data" CRAFTCLOUD_PROJECT_NAME=craftcloud-retry \
    DOCKER_LOG="$TMP/docker.log" "$@"
}
make_release 0.2.0 0200000
if retry_env DOCKER_FAIL='run --rm migrate' sh "$ROOT/deploy/install/install.sh" --non-interactive --domain localhost \
   --http-port 28190 --https-port 28553 --data-dir "$TMP/retry/data" "$TMP/output/craftcloud-0.2.0.tar.gz" >"$TMP/retry-first.log" 2>&1; then
    echo 'install with failing migration unexpectedly succeeded' >&2
    exit 1
fi
[ -d "$TMP/retry/opt/releases/0.2.0" ] && [ ! -e "$TMP/retry/opt/current" ]
retry_env sh "$ROOT/deploy/install/install.sh" --non-interactive --domain localhost \
  --http-port 28190 --https-port 28553 --data-dir "$TMP/retry/data" "$TMP/output/craftcloud-0.2.0.tar.gz" >"$TMP/retry-second.log" 2>&1
[ -L "$TMP/retry/opt/current" ]
# An incomplete leftover directory of an interrupted older run is replaced.
rm -f "$TMP/retry/opt/current"
rm -rf "$TMP/retry/opt/releases/0.2.0"
mkdir "$TMP/retry/opt/releases/0.2.0"
retry_env sh "$ROOT/deploy/install/install.sh" --non-interactive --domain localhost \
  --http-port 28190 --https-port 28553 --data-dir "$TMP/retry/data" "$TMP/output/craftcloud-0.2.0.tar.gz" >"$TMP/retry-third.log" 2>&1
[ -f "$TMP/retry/opt/releases/0.2.0/CRAFTCLOUD-RELEASE.json" ]

# An archive without metadata is rejected without leaving a release or staging directory.
make_release 0.2.1 0210000 no-metadata
if PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/bad/opt" CRAFTCLOUD_CONFIG_DIR="$TMP/bad/etc" \
   CRAFTCLOUD_DATA_DIR="$TMP/bad/data" CRAFTCLOUD_PROJECT_NAME=craftcloud-bad DOCKER_LOG="$TMP/docker.log" \
   sh "$ROOT/deploy/install/install.sh" --non-interactive --domain localhost --http-port 28191 --https-port 28554 \
   --data-dir "$TMP/bad/data" "$TMP/output/craftcloud-0.2.1.tar.gz" >"$TMP/bad-archive.log" 2>&1; then
    echo 'installer accepted an archive without metadata' >&2
    exit 1
fi
[ -z "$(ls -A "$TMP/bad/opt/releases")" ]

# An over-long Docker tag fails before any environment file or secret is written.
long_version=0.3.0-$(printf 'a%.0s' $(seq 1 130))
make_release "$long_version" 0300000
if PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/long/opt" CRAFTCLOUD_CONFIG_DIR="$TMP/long/etc" \
   CRAFTCLOUD_DATA_DIR="$TMP/long/data" CRAFTCLOUD_PROJECT_NAME=craftcloud-long DOCKER_LOG="$TMP/docker.log" \
   sh "$ROOT/deploy/install/install.sh" --non-interactive --domain localhost --http-port 28192 --https-port 28555 \
   --data-dir "$TMP/long/data" "$TMP/output/craftcloud-$long_version.tar.gz" >"$TMP/long-version.log" 2>&1; then
    echo 'installer accepted an over-long image tag' >&2
    exit 1
fi
grep -F 'too long for a Docker image tag' "$TMP/long-version.log" >/dev/null
[ ! -e "$TMP/long/etc/craftcloud.env" ]
# Restore the checksum file of the main archive for the tests that follow.
(cd "$TMP/output" && sha256sum craftcloud-0.1.0.tar.gz >SHA256SUMS)
mkdir -p "$TMP/preexisting-data"
printf '%s\n' 'user data remains' >"$TMP/preexisting-data/user.txt"
if PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/untrusted/opt/craftcloud" \
   CRAFTCLOUD_CONFIG_DIR="$TMP/untrusted/etc/craftcloud" CRAFTCLOUD_DATA_DIR="$TMP/preexisting-data" \
   sh "$ROOT/deploy/install/install.sh" --dry-run --domain localhost --data-dir "$TMP/preexisting-data" \
   "$TMP/output/craftcloud-0.1.0.tar.gz" >"$TMP/unmarked-root.log" 2>&1; then
    echo 'installer accepted a nonempty unmarked data directory' >&2
    exit 1
fi
[ "$(cat "$TMP/preexisting-data/user.txt")" = 'user data remains' ]
mkdir -p "$TMP/source/craftcloud-0.1.1/deploy/compose/caddy"
printf '%s\n' 'services: {}' >"$TMP/source/craftcloud-0.1.1/deploy/compose/compose.yaml"
printf '%s\n' 'services: {}' >"$TMP/source/craftcloud-0.1.1/deploy/compose/compose.install.yaml"
printf '%s\n' 'services: {}' >"$TMP/source/craftcloud-0.1.1/deploy/compose/compose.figcraft.yaml"
printf '%s\n' 'localhost { import tls-{$CRAFTCLOUD_TLS_MODE}.caddy }' >"$TMP/source/craftcloud-0.1.1/deploy/compose/caddy/Caddyfile"
printf '%s\n' 'tls /etc/caddy/fullchain.pem /etc/caddy/private.key' >"$TMP/source/craftcloud-0.1.1/deploy/compose/caddy/tls-custom.caddy"
printf '{"version":"0.1.1","commit":"1234567"}\n' >"$TMP/source/craftcloud-0.1.1/CRAFTCLOUD-RELEASE.json"
(cd "$TMP/source" && tar -czf "$TMP/output/craftcloud-0.1.1.tar.gz" craftcloud-0.1.1)
(cd "$TMP/output" && sha256sum craftcloud-0.1.1.tar.gz >SHA256SUMS)
if PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/opt/craftcloud" \
   CRAFTCLOUD_CONFIG_DIR="$TMP/etc/craftcloud" CRAFTCLOUD_DATA_DIR="$TMP/data" \
   CRAFTCLOUD_PROJECT_NAME=craftcloud-ops01 sh "$ROOT/deploy/install/install.sh" --domain localhost \
   --http-port 28180 --https-port 28543 --data-dir "$TMP/data" "$TMP/output/craftcloud-0.1.1.tar.gz" \
   >"$TMP/direct-upgrade.log" 2>&1; then
    echo 'installer bypassed mandatory upgrade backup' >&2
    exit 1
fi
if grep -F 'backup admin backup create' "$TMP/docker.log" >/dev/null; then
    echo 'direct install attempted backup instead of refusing upgrade' >&2
    exit 1
fi
mkdir -p "$TMP/source/craftcloud-1.2.3+build.7/deploy/compose/caddy"
printf '%s\n' 'services: {}' >"$TMP/source/craftcloud-1.2.3+build.7/deploy/compose/compose.yaml"
printf '%s\n' 'services: {}' >"$TMP/source/craftcloud-1.2.3+build.7/deploy/compose/compose.install.yaml"
printf '%s\n' 'services: {}' >"$TMP/source/craftcloud-1.2.3+build.7/deploy/compose/compose.figcraft.yaml"
printf '%s\n' 'localhost { import tls-{$CRAFTCLOUD_TLS_MODE}.caddy }' >"$TMP/source/craftcloud-1.2.3+build.7/deploy/compose/caddy/Caddyfile"
printf '%s\n' 'tls /etc/caddy/fullchain.pem /etc/caddy/private.key' >"$TMP/source/craftcloud-1.2.3+build.7/deploy/compose/caddy/tls-custom.caddy"
printf '{"version":"1.2.3+build.7","commit":"7654321"}\n' >"$TMP/source/craftcloud-1.2.3+build.7/CRAFTCLOUD-RELEASE.json"
(cd "$TMP/source" && tar -czf "$TMP/output/craftcloud-1.2.3+build.7.tar.gz" craftcloud-1.2.3+build.7)
(cd "$TMP/output" && sha256sum craftcloud-1.2.3+build.7.tar.gz >SHA256SUMS)
PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_IMAGE_PREFIX=craftcloud-server:ops01-plus- \
CRAFTCLOUD_INSTALL_ROOT="$TMP/plus/opt/craftcloud" CRAFTCLOUD_CONFIG_DIR="$TMP/plus/etc/craftcloud" \
CRAFTCLOUD_DATA_DIR="$TMP/plus/data" CRAFTCLOUD_PROJECT_NAME=craftcloud-ops01-plus DOCKER_LOG="$TMP/docker.log" \
sh "$ROOT/deploy/install/install.sh" --non-interactive --domain localhost --http-port 28181 --https-port 28544 \
  --data-dir "$TMP/plus/data" "$TMP/output/craftcloud-1.2.3+build.7.tar.gz" >"$TMP/plus-install.log" 2>&1
[ "$(sed -n 's/^CRAFTCLOUD_IMAGE=//p' "$TMP/plus/etc/craftcloud/craftcloud.env")" = 'craftcloud-server:ops01-plus-1.2.3_build.7' ]
mkdir -p "$TMP/source/craftcloud-1.2.4/deploy/compose/caddy"
printf '%s\n' 'services: {}' >"$TMP/source/craftcloud-1.2.4/deploy/compose/compose.yaml"
printf '%s\n' 'services: {}' >"$TMP/source/craftcloud-1.2.4/deploy/compose/compose.install.yaml"
printf '%s\n' 'services: {}' >"$TMP/source/craftcloud-1.2.4/deploy/compose/compose.figcraft.yaml"
printf '%s\n' 'localhost { import tls-{$CRAFTCLOUD_TLS_MODE}.caddy } # v2' >"$TMP/source/craftcloud-1.2.4/deploy/compose/caddy/Caddyfile"
printf '%s\n' 'tls /etc/caddy/fullchain.pem /etc/caddy/private.key # v2' >"$TMP/source/craftcloud-1.2.4/deploy/compose/caddy/tls-custom.caddy"
printf '{"version":"1.2.4","commit":"1234567"}\n' >"$TMP/source/craftcloud-1.2.4/CRAFTCLOUD-RELEASE.json"
(cd "$TMP/source" && tar -czf "$TMP/output/craftcloud-1.2.4.tar.gz" craftcloud-1.2.4)
(cd "$TMP/output" && sha256sum craftcloud-1.2.4.tar.gz >SHA256SUMS)
PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/plus/opt/craftcloud" \
CRAFTCLOUD_CONFIG_DIR="$TMP/plus/etc/craftcloud" CRAFTCLOUD_DATA_DIR="$TMP/plus/data" \
# An older configuration without the 2FA key gets one on upgrade; an operator edit
# of a shipped Caddy file is kept (new version beside it as .new), unmodified shipped
# files are refreshed, and operator-owned certificate files are never touched.
sed -i '/^CC_SECRET_TWO_FACTOR_KEY=/d' "$TMP/plus/etc/craftcloud/craftcloud.env"
printf '%s\n' '# local edit' >>"$TMP/plus/etc/craftcloud/caddy/Caddyfile"
printf '%s' 'operator certificate' >"$TMP/plus/etc/craftcloud/caddy/fullchain.pem"
# Release 1.2.5 has no metadata: the upgrade must fail and leave no release or staging directory.
make_release 1.2.5 1250000 no-metadata
if PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/plus/opt/craftcloud" \
   CRAFTCLOUD_CONFIG_DIR="$TMP/plus/etc/craftcloud" CRAFTCLOUD_DATA_DIR="$TMP/plus/data" \
   sh "$ROOT/deploy/install/craftcloud-ctl" upgrade "$TMP/output/craftcloud-1.2.5.tar.gz" >"$TMP/plus-bad-upgrade.log" 2>&1; then
    echo 'upgrade accepted an archive without metadata' >&2
    exit 1
fi
[ ! -e "$TMP/plus/opt/craftcloud/releases/1.2.5" ]
[ -z "$(find "$TMP/plus/opt/craftcloud/releases" -maxdepth 1 -name '.tmp-*')" ]
# An incomplete directory left by an older interrupted run does not block the upgrade.
mkdir "$TMP/plus/opt/craftcloud/releases/1.2.4"
(cd "$TMP/output" && sha256sum craftcloud-1.2.4.tar.gz >SHA256SUMS)
PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/plus/opt/craftcloud" \
CRAFTCLOUD_CONFIG_DIR="$TMP/plus/etc/craftcloud" CRAFTCLOUD_DATA_DIR="$TMP/plus/data" DOCKER_LOG="$TMP/docker.log" \
sh "$ROOT/deploy/install/craftcloud-ctl" upgrade "$TMP/output/craftcloud-1.2.4.tar.gz" >"$TMP/plus-upgrade.log" 2>&1
# The proxy is recreated once after the refreshed Caddy file.
recreate_after_upgrade=$(grep -c 'force-recreate --no-deps caddy' "$TMP/docker.log")
[ "$recreate_after_upgrade" -ge 1 ]
plus_key=$(sed -n 's/^CC_SECRET_TWO_FACTOR_KEY=//p' "$TMP/plus/etc/craftcloud/craftcloud.env")
[ "${#plus_key}" -eq 44 ]
grep -F 'v2' "$TMP/plus/etc/craftcloud/caddy/tls-custom.caddy" >/dev/null
if grep -F 'v2' "$TMP/plus/etc/craftcloud/caddy/Caddyfile" >/dev/null; then
    echo 'upgrade overwrote an operator-edited Caddy file' >&2
    exit 1
fi
grep -F '# local edit' "$TMP/plus/etc/craftcloud/caddy/Caddyfile" >/dev/null
grep -F 'v2' "$TMP/plus/etc/craftcloud/caddy/Caddyfile.new" >/dev/null
grep -F 'Caddyfile was changed locally' "$TMP/plus-upgrade.log" >/dev/null
[ "$(cat "$TMP/plus/etc/craftcloud/caddy/fullchain.pem")" = 'operator certificate' ]
PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/plus/opt/craftcloud" \
CRAFTCLOUD_CONFIG_DIR="$TMP/plus/etc/craftcloud" CRAFTCLOUD_DATA_DIR="$TMP/plus/data" DOCKER_LOG="$TMP/docker.log" \
sh "$ROOT/deploy/install/craftcloud-ctl" rollback >"$TMP/plus-rollback.log" 2>&1
# The rollback re-syncs Caddy files from the release that becomes active again and
# recreates the proxy a second time.
[ "$(grep -c 'force-recreate --no-deps caddy' "$TMP/docker.log")" -eq "$((recreate_after_upgrade + 1))" ]
if grep -F 'v2' "$TMP/plus/etc/craftcloud/caddy/tls-custom.caddy" >/dev/null; then
    echo 'rollback left the newer Caddy file in place' >&2
    exit 1
fi
grep -F '# local edit' "$TMP/plus/etc/craftcloud/caddy/Caddyfile" >/dev/null
[ "$(cat "$TMP/plus/etc/craftcloud/caddy/fullchain.pem")" = 'operator certificate' ]
[ "$(basename "$(readlink -f "$TMP/plus/opt/craftcloud/current")")" = '1.2.3+build.7' ]
PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/plus/opt/craftcloud" \
CRAFTCLOUD_CONFIG_DIR="$TMP/plus/etc/craftcloud" CRAFTCLOUD_DATA_DIR="$TMP/plus/data" DOCKER_LOG="$TMP/docker.log" \
sh "$ROOT/deploy/install/craftcloud-ctl" uninstall --purge --confirm-purge >/dev/null 2>&1
if grep -E 'CC_SECRET_|bootstrap-token=' "$TMP/install.log" >/dev/null; then
    echo 'installer output/log contains secret material' >&2
    exit 1
fi
for secret in $(sed -n 's/^CC_SECRET_[A-Z_]*=//p' "$TMP/etc/craftcloud/craftcloud.env"); do
    if grep -F "$secret" "$TMP/install.log" >/dev/null; then
        echo 'installer trace exposed a generated secret' >&2
        exit 1
    fi
done
if grep -E 'CC_SECRET_|bootstrap-token=' "$TMP/reinstall.log" >/dev/null; then
    echo 'reinstall output/log contains secret material' >&2
    exit 1
fi
grep -F 'compose.setup.yaml up -d' "$TMP/docker.log" >/dev/null
PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_IMAGE_PREFIX=craftcloud-server:ops01- CRAFTCLOUD_INSTALL_ROOT="$TMP/opt/craftcloud" \
CRAFTCLOUD_CONFIG_DIR="$TMP/etc/craftcloud" CRAFTCLOUD_DATA_DIR="$TMP/data" DOCKER_LOG="$TMP/docker.log" \
sh -x "$ROOT/deploy/install/craftcloud-ctl" setup-complete >"$TMP/setup-complete.log" 2>&1
[ ! -e "$TMP/etc/craftcloud/bootstrap-token" ]
[ "$(sed -n 's/^CC_SECRET_BOOTSTRAP=//p' "$TMP/etc/craftcloud/craftcloud.env")" = '' ]
if grep -E 'CC_SECRET_|bootstrap-token=' "$TMP/setup-complete.log" >/dev/null; then
    echo 'setup-complete output/log contains secret material' >&2
    exit 1
fi

# status reports the key file without printing it; backup names where the key lives.
main_ctl() {
    PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_IMAGE_PREFIX=craftcloud-server:ops01- CRAFTCLOUD_INSTALL_ROOT="$TMP/opt/craftcloud" \
    CRAFTCLOUD_CONFIG_DIR="$TMP/etc/craftcloud" CRAFTCLOUD_DATA_DIR="$TMP/data" DOCKER_LOG="$TMP/docker.log" \
    sh "$ROOT/deploy/install/craftcloud-ctl" "$@"
}
main_ctl status >"$TMP/status.log" 2>&1
grep -F 'Two-factor key: present' "$TMP/status.log" >/dev/null
main_ctl backup >"$TMP/backup.log" 2>&1
grep -F 'does not contain the two-factor encryption key' "$TMP/backup.log" >/dev/null
grep -F "$TMP/etc/craftcloud/craftcloud.env" "$TMP/backup.log" >/dev/null

grep -F 'FigCraft live companion: enabled' "$TMP/status.log" >/dev/null
: >"$TMP/docker.log"
main_ctl backup >"$TMP/backup-figcraft.log" 2>&1
if grep -- '-f compose.yaml ' "$TMP/docker.log" | grep -v -- '-f compose.figcraft.yaml' >/dev/null; then
    echo 'a craftcloud-ctl call omitted the FigCraft overlay' >&2
    exit 1
fi

# Opt-out through craftcloud-ctl: no overlay anywhere afterwards, containers of the
# companion are removed, the key file and data stay for a later enable.
cp "$TMP/etc/craftcloud/figcraft.key" "$TMP/figcraft-key-before"
: >"$TMP/docker.log"
main_ctl figcraft disable >"$TMP/figcraft-disable.log" 2>&1
[ "$(sed -n 's/^CRAFTCLOUD_FIGCRAFT=//p' "$TMP/etc/craftcloud/craftcloud.env")" = disabled ]
grep -F -- 'up -d --remove-orphans' "$TMP/docker.log" >/dev/null
if grep -F -- 'compose.figcraft.yaml' "$TMP/docker.log" >/dev/null; then
    echo 'disabled FigCraft still passed the overlay' >&2
    exit 1
fi
cmp "$TMP/figcraft-key-before" "$TMP/etc/craftcloud/figcraft.key"
: >"$TMP/docker.log"
main_ctl status >"$TMP/status-disabled.log" 2>&1
main_ctl backup >"$TMP/backup-disabled.log" 2>&1
grep -F 'FigCraft live companion: disabled' "$TMP/status-disabled.log" >/dev/null
if grep -F -- 'compose.figcraft.yaml' "$TMP/docker.log" >/dev/null; then
    echo 'a craftcloud-ctl call passed the overlay while FigCraft is disabled' >&2
    exit 1
fi
main_ctl figcraft enable >"$TMP/figcraft-enable.log" 2>&1
[ "$(sed -n 's/^CRAFTCLOUD_FIGCRAFT=//p' "$TMP/etc/craftcloud/craftcloud.env")" = enabled ]
cmp "$TMP/figcraft-key-before" "$TMP/etc/craftcloud/figcraft.key"
grep -F -- 'compose.figcraft.yaml' "$TMP/docker.log" >/dev/null
if main_ctl figcraft bogus >"$TMP/figcraft-bogus.log" 2>&1; then
    echo 'craftcloud-ctl accepted an unknown figcraft subcommand' >&2
    exit 1
fi

# Opt-out at install time: no overlay in any Compose call, no key file, no mount point.
make_release 0.2.3 0230000
(cd "$TMP/output" && sha256sum craftcloud-0.2.3.tar.gz >SHA256SUMS)
: >"$TMP/docker-optout.log"
PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/optout/opt" CRAFTCLOUD_CONFIG_DIR="$TMP/optout/etc" \
CRAFTCLOUD_DATA_DIR="$TMP/optout/data" CRAFTCLOUD_PROJECT_NAME=craftcloud-optout DOCKER_LOG="$TMP/docker-optout.log" \
sh -x "$ROOT/deploy/install/install.sh" --non-interactive --no-figcraft --domain localhost --http-port 28220 --https-port 28221 \
  --data-dir "$TMP/optout/data" "$TMP/output/craftcloud-0.2.3.tar.gz" >"$TMP/optout-install.log" 2>&1
[ "$(sed -n 's/^CRAFTCLOUD_FIGCRAFT=//p' "$TMP/optout/etc/craftcloud.env")" = disabled ]
[ ! -e "$TMP/optout/etc/figcraft.key" ]
if grep -F 'FIGCRAFT_SECRET_FILE' "$TMP/optout/etc/craftcloud.env" >/dev/null; then
    echo 'opt-out installation recorded a FigCraft key path' >&2
    exit 1
fi
grep -F -- '-f compose.yaml -f compose.install.yaml' "$TMP/docker-optout.log" >/dev/null
if grep -F -- 'figcraft' "$TMP/docker-optout.log" >/dev/null; then
    echo 'opt-out installation mentioned FigCraft in a Compose call' >&2
    exit 1
fi
# The setting is sticky: a rerun without the flag keeps it, a contradicting flag is refused.
PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/optout/opt" CRAFTCLOUD_CONFIG_DIR="$TMP/optout/etc" \
CRAFTCLOUD_DATA_DIR="$TMP/optout/data" CRAFTCLOUD_PROJECT_NAME=craftcloud-optout DOCKER_LOG="$TMP/docker-optout.log" \
sh "$ROOT/deploy/install/install.sh" --non-interactive --domain localhost --http-port 28220 --https-port 28221 \
  --data-dir "$TMP/optout/data" "$TMP/output/craftcloud-0.2.3.tar.gz" >"$TMP/optout-rerun.log" 2>&1
[ "$(sed -n 's/^CRAFTCLOUD_FIGCRAFT=//p' "$TMP/optout/etc/craftcloud.env")" = disabled ]
if PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/optout/opt" CRAFTCLOUD_CONFIG_DIR="$TMP/optout/etc" \
   CRAFTCLOUD_DATA_DIR="$TMP/optout/data" CRAFTCLOUD_PROJECT_NAME=craftcloud-optout sh "$ROOT/deploy/install/install.sh" \
   --non-interactive --figcraft --domain localhost --http-port 28220 --https-port 28221 --data-dir "$TMP/optout/data" \
   "$TMP/output/craftcloud-0.2.3.tar.gz" >"$TMP/optout-conflict.log" 2>&1; then
    echo 'installer accepted a FigCraft flag that contradicts the saved setting' >&2
    exit 1
fi
grep -F 'FigCraft setting differs' "$TMP/optout-conflict.log" >/dev/null
# CRAFTCLOUD_FIGCRAFT=disabled in the environment is the same opt-out; other values are rejected.
if PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_FIGCRAFT=maybe CRAFTCLOUD_INSTALL_ROOT="$TMP/badfig/opt" \
   CRAFTCLOUD_CONFIG_DIR="$TMP/badfig/etc" CRAFTCLOUD_DATA_DIR="$TMP/badfig/data" sh "$ROOT/deploy/install/install.sh" \
   --dry-run --domain localhost --data-dir "$TMP/badfig/data" "$TMP/output/craftcloud-0.2.3.tar.gz" >"$TMP/badfig.log" 2>&1; then
    echo 'installer accepted an invalid CRAFTCLOUD_FIGCRAFT value' >&2
    exit 1
fi
grep -F 'must be enabled or disabled' "$TMP/badfig.log" >/dev/null
(cd "$TMP/output" && sha256sum craftcloud-0.1.0.tar.gz >SHA256SUMS)

# Backup, upgrade and rollback exclude each other.
if command -v flock >/dev/null 2>&1; then
    flock "$TMP/etc/craftcloud/.ctl.lock" sleep 4 &
    lock_holder=$!
    sleep 1
    if main_ctl backup >"$TMP/locked-backup.log" 2>&1; then
        echo 'backup ran while the ctl lock was held' >&2
        exit 1
    fi
    grep -F 'another craftcloud-ctl' "$TMP/locked-backup.log" >/dev/null
    # setup-complete (which rewrites the environment file) and uninstall take the same lock.
    for locked_command in setup-complete uninstall; do
        if main_ctl "$locked_command" >"$TMP/locked-$locked_command.log" 2>&1; then
            echo "$locked_command ran while the ctl lock was held" >&2
            exit 1
        fi
        grep -F 'another craftcloud-ctl' "$TMP/locked-$locked_command.log" >/dev/null
    done
    wait "$lock_holder"
    main_ctl backup >"$TMP/unlocked-backup.log" 2>&1
else
    echo 'install-safety: flock missing, lock test skipped' >&2
fi

# Secret scan: no log of this run contains any generated secret value.
for secret in $(sed -n 's/^CC_SECRET_[A-Z_]*=//p' "$TMP/etc/craftcloud/craftcloud.env"); do
    for scanned in "$TMP"/*.log; do
        if grep -F -- "$secret" "$scanned" >/dev/null; then
            echo "a log exposed a generated secret: $scanned" >&2
            exit 1
        fi
    done
done

cp "$TMP/output/SHA256SUMS" "$TMP/output/SHA256SUMS.good"
printf '%064d  other-file.tar.gz\n' 0 >"$TMP/output/SHA256SUMS"
if PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_IMAGE_PREFIX=craftcloud-server:ops01- CRAFTCLOUD_INSTALL_ROOT="$TMP/opt/craftcloud" \
   CRAFTCLOUD_CONFIG_DIR="$TMP/etc/craftcloud" CRAFTCLOUD_DATA_DIR="$TMP/data" \
   sh "$ROOT/deploy/install/install.sh" --dry-run --domain localhost "$TMP/output/craftcloud-0.1.0.tar.gz" >"$TMP/checksum-missing.log" 2>&1; then
    echo 'installer accepted SHA256SUMS without archive entry' >&2
    exit 1
fi
mv "$TMP/output/SHA256SUMS.good" "$TMP/output/SHA256SUMS"

if PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/opt/craftcloud" \
   CRAFTCLOUD_CONFIG_DIR="$TMP/etc/craftcloud" CRAFTCLOUD_DATA_DIR="$TMP/data" \
   sh "$ROOT/deploy/install/craftcloud-ctl" uninstall --purge >"$TMP/purge.log" 2>&1; then
    echo 'unconfirmed non-interactive purge unexpectedly succeeded' >&2
    exit 1
fi
[ -d "$TMP/data" ] && [ -d "$TMP/etc/craftcloud" ]

PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_IMAGE_PREFIX=craftcloud-server:ops01- CRAFTCLOUD_INSTALL_ROOT="$TMP/opt/craftcloud" \
CRAFTCLOUD_CONFIG_DIR="$TMP/etc/craftcloud" CRAFTCLOUD_DATA_DIR="$TMP/data" \
CRAFTCLOUD_SYSTEMD_DIR="$TMP/systemd" CRAFTCLOUD_PROJECT_NAME=craftcloud-ops01 DOCKER_LOG="$TMP/docker.log" \
sh "$ROOT/deploy/install/craftcloud-ctl" uninstall --purge --confirm-purge >"$TMP/purge-confirmed.log" 2>&1
grep -F 'down --volumes' "$TMP/docker.log" >/dev/null
[ ! -e "$TMP/data" ] && [ ! -e "$TMP/etc/craftcloud" ] && [ ! -e "$TMP/opt/craftcloud" ]

# Uninstall must work when the systemd units were never installed (the install stopped
# before the unit step) and must disable only units that exist.
cat >"$TMP/bin/systemctl" <<'SYSTEMCTL'
#!/bin/sh
printf '%s\n' "$*" >>"${SYSTEMCTL_LOG:-/dev/null}"
case "$1" in disable|enable) exit 1 ;; esac
exit 0
SYSTEMCTL
chmod +x "$TMP/bin/systemctl"
make_release 0.2.2 0220000
(cd "$TMP/output" && sha256sum craftcloud-0.2.2.tar.gz >SHA256SUMS)
for instance in none partial; do
    port=28200
    [ "$instance" = none ] || port=28210
    PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/units/$instance/opt" CRAFTCLOUD_CONFIG_DIR="$TMP/units/$instance/etc" \
    CRAFTCLOUD_DATA_DIR="$TMP/units/$instance/data" CRAFTCLOUD_PROJECT_NAME="craftcloud-units-$instance" DOCKER_LOG="$TMP/docker.log" \
    sh "$ROOT/deploy/install/install.sh" --non-interactive --domain localhost --http-port "$port" --https-port "$((port + 1))" \
      --data-dir "$TMP/units/$instance/data" "$TMP/output/craftcloud-0.2.2.tar.gz" >"$TMP/units-$instance-install.log" 2>&1
done
created_systemd_dir=0
if [ ! -d /run/systemd/system ] && mkdir -p /run/systemd/system 2>/dev/null; then created_systemd_dir=1; fi
if [ -d /run/systemd/system ]; then
    mkdir -p "$TMP/units/partial/systemd"
    : >"$TMP/units/partial/systemd/craftcloud.service"
    for instance in none partial; do
        PATH="$TMP/bin:$PATH" CRAFTCLOUD_INSTALL_ROOT="$TMP/units/$instance/opt" CRAFTCLOUD_CONFIG_DIR="$TMP/units/$instance/etc" \
        CRAFTCLOUD_DATA_DIR="$TMP/units/$instance/data" CRAFTCLOUD_PROJECT_NAME="craftcloud-units-$instance" \
        CRAFTCLOUD_SYSTEMD_DIR="$TMP/units/$instance/systemd" DOCKER_LOG="$TMP/docker.log" SYSTEMCTL_LOG="$TMP/systemctl-$instance.log" \
        sh "$ROOT/deploy/install/craftcloud-ctl" uninstall >"$TMP/units-$instance-uninstall.log" 2>&1 || {
            [ "$created_systemd_dir" -eq 0 ] || rmdir /run/systemd/system 2>/dev/null || true
            echo "uninstall failed without installed systemd units ($instance)" >&2
            exit 1
        }
        [ ! -e "$TMP/units/$instance/opt" ]
    done
    [ "$created_systemd_dir" -eq 0 ] || rmdir /run/systemd/system 2>/dev/null || true
    if grep -F 'disable' "$TMP/systemctl-none.log" >/dev/null 2>&1; then
        echo 'uninstall disabled units that were never installed' >&2
        exit 1
    fi
    grep -F 'disable --now craftcloud.service' "$TMP/systemctl-partial.log" >/dev/null
    if grep -F 'craftcloud-backup' "$TMP/systemctl-partial.log" >/dev/null; then
        echo 'uninstall disabled a unit that does not exist' >&2
        exit 1
    fi
else
    echo 'install-safety: cannot create /run/systemd/system, systemd-unit uninstall test skipped' >&2
fi

echo 'install-safety: passed'
