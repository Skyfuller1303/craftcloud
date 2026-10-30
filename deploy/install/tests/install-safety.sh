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
cp "$TMP/etc/craftcloud/bootstrap-token" "$TMP/bootstrap-before"
PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_IMAGE_PREFIX=craftcloud-server:ops01- CRAFTCLOUD_INSTALL_ROOT="$TMP/opt/craftcloud" \
CRAFTCLOUD_CONFIG_DIR="$TMP/etc/craftcloud" CRAFTCLOUD_DATA_DIR="$TMP/data" \
CRAFTCLOUD_SYSTEMD_DIR="$TMP/systemd" CRAFTCLOUD_PROJECT_NAME=craftcloud-ops01 DOCKER_LOG="$TMP/docker.log" \
sh -x "$ROOT/deploy/install/install.sh" --non-interactive --domain localhost \
  --acme-email test@example.invalid --http-port 28180 --https-port 28543 \
  --data-dir "$TMP/data" "$TMP/output/craftcloud-0.1.0.tar.gz" >"$TMP/reinstall.log" 2>&1
cmp "$TMP/bootstrap-before" "$TMP/etc/craftcloud/bootstrap-token"
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
printf '%s\n' 'localhost { import tls-{$CRAFTCLOUD_TLS_MODE}.caddy }' >"$TMP/source/craftcloud-1.2.4/deploy/compose/caddy/Caddyfile"
printf '%s\n' 'tls /etc/caddy/fullchain.pem /etc/caddy/private.key' >"$TMP/source/craftcloud-1.2.4/deploy/compose/caddy/tls-custom.caddy"
printf '{"version":"1.2.4","commit":"1234567"}\n' >"$TMP/source/craftcloud-1.2.4/CRAFTCLOUD-RELEASE.json"
(cd "$TMP/source" && tar -czf "$TMP/output/craftcloud-1.2.4.tar.gz" craftcloud-1.2.4)
(cd "$TMP/output" && sha256sum craftcloud-1.2.4.tar.gz >SHA256SUMS)
PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/plus/opt/craftcloud" \
CRAFTCLOUD_CONFIG_DIR="$TMP/plus/etc/craftcloud" CRAFTCLOUD_DATA_DIR="$TMP/plus/data" \
sh "$ROOT/deploy/install/craftcloud-ctl" upgrade "$TMP/output/craftcloud-1.2.4.tar.gz" >"$TMP/plus-upgrade.log" 2>&1
PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/plus/opt/craftcloud" \
CRAFTCLOUD_CONFIG_DIR="$TMP/plus/etc/craftcloud" CRAFTCLOUD_DATA_DIR="$TMP/plus/data" \
sh "$ROOT/deploy/install/craftcloud-ctl" rollback >"$TMP/plus-rollback.log" 2>&1
[ "$(basename "$(readlink -f "$TMP/plus/opt/craftcloud/current")")" = '1.2.3+build.7' ]
PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/plus/opt/craftcloud" \
CRAFTCLOUD_CONFIG_DIR="$TMP/plus/etc/craftcloud" CRAFTCLOUD_DATA_DIR="$TMP/plus/data" \
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

echo 'install-safety: passed'
