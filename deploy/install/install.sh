#!/bin/sh
set -eu
umask 077

# Disable xtrace before any config or secret can be read, even when invoked as sh -x.
case $- in *x*) set +x ;; esac

SELF_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
INSTALL_ROOT=${CRAFTCLOUD_INSTALL_ROOT:-/opt/craftcloud}
CONFIG_DIR=${CRAFTCLOUD_CONFIG_DIR:-/etc/craftcloud}
SYSTEMD_DIR=${CRAFTCLOUD_SYSTEMD_DIR:-/etc/systemd/system}
PROJECT_ENV=${CRAFTCLOUD_PROJECT_NAME:-}
PROJECT=${PROJECT_ENV:-craftcloud}
DATA_DIR=${CRAFTCLOUD_DATA_DIR:-/var/lib/craftcloud}
DATA_DIR_ENV=${CRAFTCLOUD_DATA_DIR:-}
CADDY_CONFIG_DIR=$CONFIG_DIR/caddy
SKIP_SYSTEMD=${CRAFTCLOUD_SKIP_SYSTEMD:-0}
IMAGE_PREFIX=${CRAFTCLOUD_IMAGE_PREFIX:-}
FIGCRAFT=${CRAFTCLOUD_FIGCRAFT:-enabled}
FIGCRAFT_SET=0
[ -z "${CRAFTCLOUD_FIGCRAFT:-}" ] || FIGCRAFT_SET=1
ARCHIVE=
DOMAIN=
ACME_EMAIL=
HTTP_PORT=80
HTTPS_PORT=443
DOMAIN_SET=0
DATA_DIR_SET=0
HTTP_PORT_SET=0
HTTPS_PORT_SET=0
NON_INTERACTIVE=0
DRY_RUN=0

usage() {
    cat <<'USAGE'
Usage: install.sh [options] <craftcloud-version.tar.gz>
  --domain NAME          Public DNS name
  --acme-email ADDRESS   ACME account email
  --data-dir PATH        Persistent data directory
  --http-port PORT       HTTP host port (default 80)
  --https-port PORT      HTTPS host port (default 443)
  --no-figcraft          Do not run the FigCraft live collaboration companion
                         (enabled by default; same as CRAFTCLOUD_FIGCRAFT=disabled;
                         change later with: craftcloud-ctl figcraft enable|disable)
  --non-interactive      Do not prompt
  --dry-run              Check inputs and print planned actions
USAGE
}
fail() { printf '%s\n' "craftcloud-install: $*" >&2; exit 1; }
systemd_running() {
    [ -d /run/systemd/system ] || [ "$(cat /proc/1/comm 2>/dev/null || true)" = systemd ]
}
safe_path() {
    safe_value=$1
    case $safe_value in /*) ;; *) fail 'installation, config, and data paths must be absolute' ;; esac
    case $safe_value in /|*//*|*/|*/./*|*/../*|*/.|*/..) fail "unsafe path: $safe_value" ;; esac
    case $safe_value in *[!A-Za-z0-9/._-]*) fail 'paths may contain only letters, digits, slash, dot, underscore, and hyphen' ;; esac
    safe_rest=${safe_value#/}
    safe_parent=
    while [ -n "$safe_rest" ]; do
        case $safe_rest in */*) safe_part=${safe_rest%%/*}; safe_rest=${safe_rest#*/} ;; *) safe_part=$safe_rest; safe_rest= ;; esac
        [ -n "$safe_part" ] || fail "unsafe path: $safe_value"
        safe_parent=$safe_parent/$safe_part
        [ ! -L "$safe_parent" ] || fail "path contains a symbolic link: $safe_parent"
    done
}
paths_overlap() {
    case $1/ in "$2/"*) return 0 ;; esac
    case $2/ in "$1/"*) return 0 ;; esac
    return 1
}
valid_semver() {
    printf '%s\n' "$1" | awk '
        function identifiers(value, prerelease, parts, count, i) {
            if (value == "") return 0
            count = split(value, parts, /\./)
            for (i = 1; i <= count; i++) {
                if (parts[i] !~ /^[0-9A-Za-z-]+$/) return 0
                if (prerelease && parts[i] ~ /^[0-9]+$/ && length(parts[i]) > 1 && substr(parts[i], 1, 1) == "0") return 0
            }
            return 1
        }
        {
            value = $0
            plus = index(value, "+")
            if (plus) {
                build = substr(value, plus + 1)
                value = substr(value, 1, plus - 1)
                if (index(build, "+") || !identifiers(build, 0)) exit 1
            }
            dash = index(value, "-")
            if (dash) {
                prerelease = substr(value, dash + 1)
                value = substr(value, 1, dash - 1)
                if (!identifiers(prerelease, 1)) exit 1
            }
            if (split(value, core, /\./) != 3) exit 1
            for (i = 1; i <= 3; i++) {
                if (core[i] !~ /^[0-9]+$/ || (length(core[i]) > 1 && substr(core[i], 1, 1) == "0")) exit 1
            }
        }
    '
}
image_tag() { printf '%s\n' "$1" | sed 's/+/_/'; }
installation_identity() { printf '%s|%s|%s|%s\n' "$INSTALL_ROOT" "$CONFIG_DIR" "$DATA_DIR" "$PROJECT"; }
check_owned_root() {
    owned_root=$1
    owned_marker=$owned_root/.craftcloud-installation
    case $owned_root in /opt|/etc|/var|/var/lib|/home|/root|/usr|/tmp|/srv|/mnt) fail "refusing broad system directory: $owned_root" ;; esac
    [ ! -L "$owned_root" ] || fail "installation root is a symbolic link: $owned_root"
    if [ -e "$owned_root" ]; then
        [ -d "$owned_root" ] || fail "installation path is not a directory: $owned_root"
        [ ! -L "$owned_marker" ] || fail "installation marker is a symbolic link: $owned_marker"
        if [ -f "$owned_marker" ]; then
            [ "$(cat "$owned_marker")" = "$(installation_identity)" ] || fail "installation marker does not match this instance: $owned_root"
        else
            for owned_entry in "$owned_root"/* "$owned_root"/.[!.]* "$owned_root"/..?*; do
                [ -e "$owned_entry" ] || [ -L "$owned_entry" ] || continue
                fail "refusing nonempty unmarked directory: $owned_root"
            done
        fi
    fi
}
write_owned_root_marker() {
    owned_root=$1
    owned_marker=$owned_root/.craftcloud-installation
    if [ ! -e "$owned_marker" ]; then
        printf '%s' "$(installation_identity)" >"$owned_marker"
        chmod 0600 "$owned_marker"
    fi
}
verify_archive_checksum() {
    checksum_file=$(dirname -- "$1")/SHA256SUMS
    [ -f "$checksum_file" ] || fail 'SHA256SUMS missing beside archive'
    checksum_name=$(basename -- "$1")
    checksum_expected=
    checksum_matches=0
    while IFS=' ' read -r checksum_hash checksum_file_name checksum_extra || [ -n "${checksum_hash:-}" ]; do
        case $checksum_file_name in "$checksum_name")
            checksum_matches=$((checksum_matches + 1))
            [ -z "${checksum_extra:-}" ] || fail 'malformed checksum entry'
            [ "${#checksum_hash}" -eq 64 ] || fail 'malformed archive checksum'
            case $checksum_hash in *[!0-9a-fA-F]*) fail 'malformed archive checksum' ;; esac
            checksum_expected=$checksum_hash
            ;;
        esac
    done <"$checksum_file"
    [ "$checksum_matches" -eq 1 ] || fail 'SHA256SUMS must contain exactly one checksum for the archive'
    checksum_actual=$(sha256sum "$1" | awk '{print $1}')
    [ "$checksum_actual" = "$checksum_expected" ] || fail 'archive checksum mismatch'
}

release_complete() {
    # release_complete <release-dir> <version>
    if [ -d "$1/deploy/compose" ] && [ -f "$1/CRAFTCLOUD-RELEASE.json" ]; then return 0; fi
    [ -d "$1/craftcloud-$2/deploy/compose" ] && [ -f "$1/craftcloud-$2/CRAFTCLOUD-RELEASE.json" ]
}
# extract_release <archive> <version> <destination>
# Unpacks into a staging directory next to the destination and renames it only
# after the content is validated, so a failed run never leaves a partial release.
extract_release() {
    ex_archive=$1
    ex_version=$2
    ex_dest=$3
    ex_stage=$INSTALL_ROOT/releases/.tmp-$ex_version.$$
    rm -rf -- "$INSTALL_ROOT/releases/.tmp-$ex_version".*
    rm -rf -- "$ex_stage"
    mkdir "$ex_stage"
    trap 'rm -rf -- "$ex_stage"' 0
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM
    ex_entries=$(tar -tzf "$ex_archive") || fail 'cannot read archive table of contents'
    printf '%s\n' "$ex_entries" | awk -v root="craftcloud-$ex_version" '($0 != root "/" && index($0, root "/") != 1) || /^\// || /(^|\/)\.\.($|\/)/ {bad=1} END {exit bad}' || fail 'archive contains unsafe paths'
    ex_types=$(tar -tvzf "$ex_archive") || fail 'cannot inspect archive entry types'
    printf '%s\n' "$ex_types" | awk 'substr($0,1,1) != "-" && substr($0,1,1) != "d" {bad=1} END {exit bad}' || fail 'archive contains links or special files'
    umask 022
    tar -xzf "$ex_archive" -C "$ex_stage" --no-same-owner --strip-components=1
    umask 077
    release_complete "$ex_stage" "$ex_version" || fail 'release archive is incomplete (deploy/compose or CRAFTCLOUD-RELEASE.json missing)'
    mv -T "$ex_stage" "$ex_dest"
    trap - 0 HUP INT TERM
}
# sync_caddy_config <shipped-dir> <config-dir>
# Refreshes release-shipped Caddy files. Operator-owned files (fullchain.pem,
# private.key, and anything the release does not ship) are never touched. A
# shipped file the operator changed is kept and the new version is written
# beside it as <name>.new. Hashes of the shipped versions are recorded in
# .shipped-sha256 to tell an unmodified file from an edited one.
sync_caddy_config() {
    sync_src=$1
    sync_dest=$2
    sync_manifest=$sync_dest/.shipped-sha256
    sync_next=$sync_manifest.tmp.$$
    : >"$sync_next"
    for sync_file in "$sync_src"/*; do
        [ -f "$sync_file" ] && [ ! -L "$sync_file" ] || continue
        sync_name=${sync_file##*/}
        case $sync_name in fullchain.pem|private.key|*.new) continue ;; esac
        sync_target=$sync_dest/$sync_name
        sync_new=$(sha256sum "$sync_file" | awk '{print $1}')
        sync_old=
        [ ! -f "$sync_manifest" ] || sync_old=$(awk -v n="$sync_name" '$2 == n {print $1}' "$sync_manifest")
        if [ -L "$sync_target" ]; then
            printf '%s\n' "craftcloud: warning: $sync_target is a symbolic link; not refreshed" >&2
            [ -z "$sync_old" ] || printf '%s  %s\n' "$sync_old" "$sync_name" >>"$sync_next"
        elif [ ! -e "$sync_target" ]; then
            cp "$sync_file" "$sync_target"
            printf '%s  %s\n' "$sync_new" "$sync_name" >>"$sync_next"
        else
            sync_cur=$(sha256sum "$sync_target" | awk '{print $1}')
            if [ "$sync_cur" = "$sync_new" ]; then
                rm -f -- "$sync_target.new"
                printf '%s  %s\n' "$sync_new" "$sync_name" >>"$sync_next"
            elif [ -n "$sync_old" ] && [ "$sync_old" = "$sync_cur" ]; then
                cp "$sync_file" "$sync_target.tmp.$$"
                mv -f "$sync_target.tmp.$$" "$sync_target"
                rm -f -- "$sync_target.new"
                printf '%s  %s\n' "$sync_new" "$sync_name" >>"$sync_next"
                printf '%s\n' "craftcloud: refreshed Caddy file $sync_name"
            else
                cp "$sync_file" "$sync_target.new"
                printf '%s\n' "craftcloud: warning: Caddy file $sync_name was changed locally and is kept; the new shipped version is $sync_target.new. Compare and merge it." >&2
                [ -z "$sync_old" ] || printf '%s  %s\n' "$sync_old" "$sync_name" >>"$sync_next"
            fi
        fi
    done
    chmod 0600 "$sync_next"
    mv -f "$sync_next" "$sync_manifest"
}
# Generates the FigCraft management key (64 random hex characters) and prints it.
generate_figcraft_key() {
    gen_fig=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
    [ "${#gen_fig}" -eq 64 ] || fail 'cannot generate FigCraft key'
    printf '%s' "$gen_fig"
}
# env_set <name> <value>: replaces or appends one line of the environment file.
env_set() {
    set_tmp=$env_file.tmp.$$
    { awk -v prefix="$1=" 'index($0, prefix) != 1' "$env_file"; printf '%s=%s\n' "$1" "$2"; } >"$set_tmp"
    chmod 0600 "$set_tmp"
    mv -f "$set_tmp" "$env_file"
}
# Prepares everything the FigCraft companion needs: the management key file
# (created once, mode 0600, never printed and never replaced), its path and the
# public wss URL in the environment file, and the Caddy mount point of the route.
ensure_figcraft_files() {
    fig_key=$CONFIG_DIR/figcraft.key
    [ ! -L "$fig_key" ] || fail 'FigCraft key path must not be a symbolic link'
    if [ ! -e "$fig_key" ]; then
        fig_tmp=$fig_key.tmp.$$
        generate_figcraft_key >"$fig_tmp"
        chmod 0600 "$fig_tmp"
        mv -f "$fig_tmp" "$fig_key"
    else
        [ -f "$fig_key" ] || fail 'FigCraft key path is not a regular file'
        grep -Eq '^[0-9A-Fa-f]{64}$' "$fig_key" || fail "existing $fig_key is not 64 hexadecimal characters; refusing to replace it"
        chmod 0600 "$fig_key"
    fi
    env_set CRAFTCLOUD_FIGCRAFT_SECRET_FILE "$fig_key"
    if ! grep -q '^CRAFTCLOUD_FIGCRAFT_PUBLIC_URL=.' "$env_file"; then
        fig_url=wss://$DOMAIN/figcraft/
        [ "$HTTPS_PORT" = 443 ] || fig_url=wss://$DOMAIN:$HTTPS_PORT/figcraft/
        env_set CRAFTCLOUD_FIGCRAFT_PUBLIC_URL "$fig_url"
    fi
    mkdir -p "$CADDY_CONFIG_DIR/optional"
    chmod 0700 "$CADDY_CONFIG_DIR/optional"
    [ -e "$CADDY_CONFIG_DIR/optional/figcraft.caddy" ] || : >"$CADDY_CONFIG_DIR/optional/figcraft.caddy"
}
# compose_files <with-setup 0|1>: the single place that decides which Compose
# files every invocation uses, so they cannot diverge.
compose_files() {
    printf '%s' '-f compose.yaml -f compose.install.yaml'
    [ "$FIGCRAFT" != enabled ] || printf '%s' ' -f compose.figcraft.yaml'
    # smtp_files contains only fixed -f filenames emitted by smtp_overrides.
    [ -z "${smtp_files:-}" ] || printf ' %s' "$smtp_files"
    [ "$1" != 1 ] || printf '%s' ' -f compose.setup.yaml'
}

# Generates the Base64 two-factor key (32 random bytes) and prints it.
generate_two_factor_key() {
    gen_key=$(head -c 32 /dev/urandom | base64 | tr -d '\n') || fail 'cannot generate two-factor key'
    [ "${#gen_key}" -eq 44 ] || fail 'cannot generate two-factor key'
    printf '%s' "$gen_key"
}

while [ "$#" -gt 0 ]; do
    case $1 in
        --domain) [ "$#" -ge 2 ] || fail '--domain needs a value'; DOMAIN=$2; DOMAIN_SET=1; shift 2 ;;
        --acme-email) [ "$#" -ge 2 ] || fail '--acme-email needs a value'; ACME_EMAIL=$2; shift 2 ;;
        --data-dir) [ "$#" -ge 2 ] || fail '--data-dir needs a value'; DATA_DIR=$2; DATA_DIR_SET=1; shift 2 ;;
        --http-port) [ "$#" -ge 2 ] || fail '--http-port needs a value'; HTTP_PORT=$2; HTTP_PORT_SET=1; shift 2 ;;
        --https-port) [ "$#" -ge 2 ] || fail '--https-port needs a value'; HTTPS_PORT=$2; HTTPS_PORT_SET=1; shift 2 ;;
        --figcraft) FIGCRAFT=enabled; FIGCRAFT_SET=1; shift ;;
        --no-figcraft) FIGCRAFT=disabled; FIGCRAFT_SET=1; shift ;;
        --non-interactive) NON_INTERACTIVE=1; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        --*) fail "unknown option: $1" ;;
        *) [ -z "$ARCHIVE" ] || fail 'only one archive is allowed'; ARCHIVE=$1; shift ;;
    esac
done

[ -n "$ARCHIVE" ] || { usage >&2; exit 2; }
[ -f "$ARCHIVE" ] || fail 'archive not found'
safe_path "$INSTALL_ROOT"
safe_path "$CONFIG_DIR"
safe_path "$DATA_DIR"
safe_path "$CADDY_CONFIG_DIR"
env_file=$CONFIG_DIR/craftcloud.env
if [ -f "$env_file" ]; then
    saved_domain=$(sed -n 's/^CRAFTCLOUD_DOMAIN=//p' "$env_file" | tail -n 1)
    saved_http=$(sed -n 's/^CRAFTCLOUD_HTTP_PORT=//p' "$env_file" | tail -n 1)
    saved_https=$(sed -n 's/^CRAFTCLOUD_HTTPS_PORT=//p' "$env_file" | tail -n 1)
    saved_data=$(sed -n 's/^CRAFTCLOUD_DATA_DIR=//p' "$env_file" | tail -n 1)
    saved_caddy_dir=$(sed -n 's/^CRAFTCLOUD_CADDY_CONFIG_DIR=//p' "$env_file" | tail -n 1)
    saved_image_prefix=$(sed -n 's/^CRAFTCLOUD_IMAGE_PREFIX=//p' "$env_file" | tail -n 1)
    saved_project=$(sed -n 's/^CRAFTCLOUD_PROJECT_NAME=//p' "$env_file" | tail -n 1)
    saved_figcraft=$(sed -n 's/^CRAFTCLOUD_FIGCRAFT=//p' "$env_file" | tail -n 1)
    if [ -n "$saved_figcraft" ]; then
        [ "$FIGCRAFT_SET" -eq 0 ] || [ "$FIGCRAFT" = "$saved_figcraft" ] || fail 'FigCraft setting differs from existing config; use craftcloud-ctl figcraft enable|disable'
        FIGCRAFT=$saved_figcraft
    fi
    if [ -n "$saved_domain" ]; then
        [ "$DOMAIN_SET" -eq 0 ] || [ "$DOMAIN" = "$saved_domain" ] || fail 'domain differs from existing config'
        DOMAIN=$saved_domain
    fi
    if [ -n "$saved_http" ]; then
        [ "$HTTP_PORT_SET" -eq 0 ] || [ "$HTTP_PORT" = "$saved_http" ] || fail 'HTTP port differs from existing config'
        HTTP_PORT=$saved_http
    fi
    if [ -n "$saved_https" ]; then
        [ "$HTTPS_PORT_SET" -eq 0 ] || [ "$HTTPS_PORT" = "$saved_https" ] || fail 'HTTPS port differs from existing config'
        HTTPS_PORT=$saved_https
    fi
    if [ -n "$saved_data" ]; then
        [ "$DATA_DIR_SET" -eq 0 ] || [ "$DATA_DIR" = "$saved_data" ] || fail 'data directory differs from existing config'
        [ -z "$DATA_DIR_ENV" ] || [ "$DATA_DIR_ENV" = "$saved_data" ] || fail 'CRAFTCLOUD_DATA_DIR differs from existing config'
        DATA_DIR=$saved_data
    fi
    [ -z "$saved_caddy_dir" ] || [ "$saved_caddy_dir" = "$CADDY_CONFIG_DIR" ] || fail 'Caddy config directory differs from installed config'
    if [ -n "$saved_project" ]; then
        [ -z "$PROJECT_ENV" ] || [ "$PROJECT_ENV" = "$saved_project" ] || fail 'Compose project name differs from existing config'
        PROJECT=$saved_project
    fi
    if [ -n "$saved_image_prefix" ]; then
        [ -z "$IMAGE_PREFIX" ] || [ "$IMAGE_PREFIX" = "$saved_image_prefix" ] || fail 'image prefix differs from existing config'
        IMAGE_PREFIX=$saved_image_prefix
    fi
fi
case $FIGCRAFT in enabled|disabled) ;; *) fail 'CRAFTCLOUD_FIGCRAFT must be enabled or disabled' ;; esac
[ -n "$IMAGE_PREFIX" ] || IMAGE_PREFIX=craftcloud-server:
[ -n "$IMAGE_PREFIX" ] || fail 'image prefix is required'
case $IMAGE_PREFIX in *[!A-Za-z0-9._:/-]*) fail 'invalid image prefix' ;; esac
[ -n "$IMAGE_PREFIX" ] || fail 'image prefix is required'
case $IMAGE_PREFIX in *[!A-Za-z0-9._:/-]*) fail 'invalid image prefix' ;; esac
[ -n "$DOMAIN" ] || fail '--domain is required for first install'
case $DOMAIN in *[!A-Za-z0-9.-]*|'') fail 'invalid domain' ;; esac
newline='
'
case $DATA_DIR in *:*|*=*|*"$newline"*) fail 'data directory contains unsupported characters' ;; esac
if [ -n "$ACME_EMAIL" ]; then case $ACME_EMAIL in *[!A-Za-z0-9._+@-]*|'') fail 'invalid ACME email' ;; esac; fi
case $PROJECT in *[!A-Za-z0-9_-]*|'') fail 'invalid Compose project name' ;; esac
case $HTTP_PORT:$HTTPS_PORT in *[!0-9:]*|:*|*:) fail 'ports must be numeric' ;; esac
[ "$HTTP_PORT" -ge 1 ] && [ "$HTTP_PORT" -le 65535 ] || fail 'invalid HTTP port'
[ "$HTTPS_PORT" -ge 1 ] && [ "$HTTPS_PORT" -le 65535 ] || fail 'invalid HTTPS port'

need() { command -v "$1" >/dev/null 2>&1 || fail "required command missing: $1"; }
safe_path "$DATA_DIR"
if paths_overlap "$INSTALL_ROOT" "$CONFIG_DIR" || paths_overlap "$INSTALL_ROOT" "$DATA_DIR" || paths_overlap "$CONFIG_DIR" "$DATA_DIR"; then
    fail 'install, config, and data directories must not overlap'
fi
need tar; need sha256sum; need awk; need sed; need docker; need readlink; need df; need chown; need base64; need head; need od; need tr; need grep
if [ "$SKIP_SYSTEMD" = 1 ]; then
    systemd_running && fail 'CRAFTCLOUD_SKIP_SYSTEMD=1 is allowed only outside a systemd host'
else
    need systemctl
    systemd_running || fail 'systemd is not running; set CRAFTCLOUD_SKIP_SYSTEMD=1 only in a test container'
fi
safe_path "$DATA_DIR"
if paths_overlap "$INSTALL_ROOT" "$CONFIG_DIR" || paths_overlap "$INSTALL_ROOT" "$DATA_DIR" || paths_overlap "$CONFIG_DIR" "$DATA_DIR"; then
    fail 'install, config, and data directories must not overlap'
fi
docker compose version --short >/dev/null 2>&1 || fail 'Docker Compose plugin unavailable'
compose_version=$(docker compose version --short | sed 's/^v//')
compose_major=$(printf '%s' "$compose_version" | awk -F. '{print $1+0}')
compose_minor=$(printf '%s' "$compose_version" | awk -F. '{print $2+0}')
[ "$compose_major" -gt 2 ] || { [ "$compose_major" -eq 2 ] && [ "$compose_minor" -ge 24 ]; } || fail 'Docker Compose 2.24 or newer required'
docker_version=$(docker version --format '{{.Server.Version}}' 2>/dev/null) || fail 'Docker Engine unavailable'
docker_major=$(printf '%s' "$docker_version" | awk -F. '{gsub(/[^0-9].*/, "", $1); print $1+0}')
[ "$docker_major" -ge 25 ] || fail 'Docker Engine 25 or newer required'

verify_archive_checksum "$ARCHIVE"
archive_name=$(basename -- "$ARCHIVE")
version=$(printf '%s' "$archive_name" | sed -n 's/^craftcloud-\([0-9][0-9A-Za-z.+-]*\)\.tar\.gz$/\1/p')
[ -n "$version" ] || fail 'archive name must be craftcloud-<version>.tar.gz'
valid_semver "$version" || fail 'release version must be valid SemVer'

if [ -L "$INSTALL_ROOT/current" ]; then
    active_release=$(readlink -f "$INSTALL_ROOT/current" 2>/dev/null || true)
    [ "$active_release" = "$INSTALL_ROOT/releases/$version" ] || fail 'use craftcloud-ctl upgrade for an installed instance'
elif [ -e "$INSTALL_ROOT/current" ]; then
    fail 'current release path exists and is not a symbolic link'
fi
check_owned_root "$INSTALL_ROOT"
check_owned_root "$CONFIG_DIR"
check_owned_root "$DATA_DIR"
safe_path "$DATA_DIR/storage"
safe_path "$DATA_DIR/postgres"
safe_path "$DATA_DIR/backups"
safe_path "$INSTALL_ROOT/releases"

active_config=0
if [ -L "$INSTALL_ROOT/current" ] && [ -f "$env_file" ]; then
    saved_domain=$(sed -n 's/^CRAFTCLOUD_DOMAIN=//p' "$env_file" | tail -n 1)
    saved_http=$(sed -n 's/^CRAFTCLOUD_HTTP_PORT=//p' "$env_file" | tail -n 1)
    saved_https=$(sed -n 's/^CRAFTCLOUD_HTTPS_PORT=//p' "$env_file" | tail -n 1)
    saved_data=$(sed -n 's/^CRAFTCLOUD_DATA_DIR=//p' "$env_file" | tail -n 1)
    saved_project=$(sed -n 's/^CRAFTCLOUD_PROJECT_NAME=//p' "$env_file" | tail -n 1)
    current_real=$(readlink -f "$INSTALL_ROOT/current" 2>/dev/null || true)
    case $current_real in "$INSTALL_ROOT"/releases/*)
        if [ "$saved_domain" = "$DOMAIN" ] && [ "$saved_http" = "$HTTP_PORT" ] && [ "$saved_https" = "$HTTPS_PORT" ] && [ "$saved_data" = "$DATA_DIR" ] && [ "${saved_project:-craftcloud}" = "$PROJECT" ]; then active_config=1; fi
        ;;
    esac
fi

if [ "$DRY_RUN" -eq 0 ]; then
    [ "$(id -u)" -eq 0 ] || fail 'run installer as root'
    [ -r /proc/meminfo ] || fail 'cannot verify available memory'
    mem_kb=$(awk '/MemTotal:/ {print $2}' /proc/meminfo)
    [ "$mem_kb" -ge 2097152 ] || fail 'at least 2 GiB RAM required'
    for target in "$INSTALL_ROOT" "$DATA_DIR"; do
        probe=$target
        while [ ! -d "$probe" ]; do probe=$(dirname -- "$probe"); done
        avail_kb=$(df -Pk "$probe" 2>/dev/null | awk 'NR==2 {print $4}')
        [ "${avail_kb:-0}" -ge 10485760 ] || fail "at least 10 GiB free disk space required on $probe"
    done
    if [ "$active_config" -eq 0 ]; then
        port_in_use() {
            port_hex=$(printf '%04X' "$1")
            protocol=$2
            tables_checked=0
            if [ "$protocol" = tcp ]; then tables='/proc/net/tcp /proc/net/tcp6'
            else tables='/proc/net/udp /proc/net/udp6'; fi
            for table in $tables; do
                [ -r "$table" ] || continue
                tables_checked=1
                if awk -v port="$port_hex" -v proto="$protocol" 'NR > 1 {split($2, endpoint, ":"); if (toupper(endpoint[2]) == port && (proto != "tcp" || $4 == "0A")) found=1} END {exit !found}' "$table"; then return 0; fi
            done
            [ "$tables_checked" -eq 1 ] || fail 'cannot verify whether install ports are free'
            return 1
        }
        port_in_use "$HTTP_PORT" tcp && fail "port $HTTP_PORT is already in use"
        port_in_use "$HTTPS_PORT" tcp && fail "port $HTTPS_PORT is already in use"
        port_in_use "$HTTPS_PORT" udp && fail "UDP port $HTTPS_PORT is already in use"
    fi
    if command -v getent >/dev/null 2>&1 && ! getent ahosts "$DOMAIN" >/dev/null 2>&1; then
        printf '%s\n' "craftcloud-install: warning: DNS lookup failed for $DOMAIN" >&2
    fi
fi

release="$INSTALL_ROOT/releases/$version"
if [ "$DRY_RUN" -eq 1 ]; then
    printf 'install %s to %s; config %s; data %s; project %s\n' "$version" "$release" "$CONFIG_DIR" "$DATA_DIR" "$PROJECT"
    exit 0
fi

release_exists=0
if [ -e "$release" ]; then
    [ ! -L "$release" ] || fail "release path is a symbolic link: $version"
    current_release=$(readlink -f "$INSTALL_ROOT/current" 2>/dev/null || true)
    if [ "$current_release" = "$(readlink -f "$release")" ]; then
        release_exists=1
    elif [ ! -e "$INSTALL_ROOT/current" ] && [ ! -L "$INSTALL_ROOT/current" ]; then
        # A failed first install: nothing is active. Reuse a complete release
        # directory; an incomplete one is a leftover of an interrupted older run.
        if release_complete "$release" "$version"; then
            release_exists=1
        else
            rm -rf -- "$release"
        fi
    else
        fail "release already exists and is not active: $version"
    fi
fi

mkdir -p "$INSTALL_ROOT/releases" "$CONFIG_DIR" "$DATA_DIR/storage" "$DATA_DIR/postgres" "$DATA_DIR/backups"
write_owned_root_marker "$INSTALL_ROOT"
write_owned_root_marker "$CONFIG_DIR"
write_owned_root_marker "$DATA_DIR"
chmod 0755 "$INSTALL_ROOT"
chmod 0700 "$CONFIG_DIR" "$DATA_DIR" "$DATA_DIR/storage" "$DATA_DIR/postgres" "$DATA_DIR/backups"
chown 65532:65532 "$DATA_DIR/storage" "$DATA_DIR/backups"
chown 999:999 "$DATA_DIR/postgres"
if [ "$release_exists" -eq 0 ]; then
    extract_release "$ARCHIVE" "$version" "$release"
fi
if [ -d "$release/deploy/compose" ]; then compose_dir=$release/deploy/compose
elif [ -d "$release/craftcloud-$version/deploy/compose" ]; then compose_dir=$release/craftcloud-$version/deploy/compose
else fail 'release archive has no deploy/compose directory'; fi
metadata=$release/CRAFTCLOUD-RELEASE.json
[ -f "$metadata" ] || metadata=$release/craftcloud-$version/CRAFTCLOUD-RELEASE.json
[ -f "$metadata" ] || fail 'CRAFTCLOUD-RELEASE.json missing'
release_commit=$(sed -n 's/.*"commit"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$metadata")
release_metadata_version=$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$metadata")
[ "$release_metadata_version" = "$version" ] || fail 'archive metadata version mismatch'
[ -n "$release_commit" ] || fail 'archive metadata commit missing'

env_file=$CONFIG_DIR/craftcloud.env
bootstrap_file=$CONFIG_DIR/bootstrap-token
[ ! -L "$env_file" ] || fail 'config file must not be a symbolic link'
[ ! -L "$bootstrap_file" ] || fail 'bootstrap token path must not be a symbolic link'
fresh_config=0
if [ ! -e "$env_file" ]; then
    fresh_config=1
    [ -n "$ACME_EMAIL" ] || ACME_EMAIL=admin@$DOMAIN
    secret() {
        secret_value=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
        [ "${#secret_value}" -eq 64 ] || fail 'cannot read random data'
        printf '%s' "$secret_value"
    }
    # Everything that can fail is checked before the environment file is written.
    version_image_tag=$(image_tag "$version")
    [ "${#version_image_tag}" -le 128 ] || fail 'release version is too long for a Docker image tag'
    public_url=https://$DOMAIN
    [ "$HTTPS_PORT" = 443 ] || public_url=https://$DOMAIN:$HTTPS_PORT
    tls_mode=acme
    [ "$DOMAIN" != localhost ] || tls_mode=internal
    bootstrap_token=$(secret)
    secret_db_superuser=$(secret)
    secret_db_owner=$(secret)
    secret_db_runtime=$(secret)
    secret_metrics=$(secret)
    secret_two_factor=$(generate_two_factor_key)
    env_tmp=$env_file.tmp.$$
    trap 'rm -f -- "$env_tmp"' 0
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM
    {
        printf 'CRAFTCLOUD_DOMAIN=%s\nCRAFTCLOUD_PUBLIC_URL=%s\n' "$DOMAIN" "$public_url"
        printf 'CRAFTCLOUD_HTTP_PORT=%s\nCRAFTCLOUD_HTTPS_PORT=%s\nCRAFTCLOUD_TLS_MODE=%s\nCRAFTCLOUD_ACME_EMAIL=%s\n' "$HTTP_PORT" "$HTTPS_PORT" "$tls_mode" "$ACME_EMAIL"
        printf 'CRAFTCLOUD_DATA_DIR=%s\nCRAFTCLOUD_CONFIG_DIR=%s\nCRAFTCLOUD_PROJECT_NAME=%s\nCRAFTCLOUD_BACKUP_KEEP=%s\n' "$DATA_DIR" "$CONFIG_DIR" "$PROJECT" "${CRAFTCLOUD_BACKUP_KEEP:-7}"
        printf 'CRAFTCLOUD_IMAGE_PREFIX=%s\nCRAFTCLOUD_IMAGE=%s\n' "$IMAGE_PREFIX" "$IMAGE_PREFIX$version_image_tag"
        printf 'CRAFTCLOUD_FIGCRAFT=%s\n' "$FIGCRAFT"
        printf 'CC_SECRET_DB_SUPERUSER_PASSWORD=%s\n' "$secret_db_superuser"
        printf 'CC_SECRET_DB_OWNER_PASSWORD=%s\n' "$secret_db_owner"
        printf 'CC_SECRET_DB_RUNTIME_PASSWORD=%s\n' "$secret_db_runtime"
        printf 'CC_SECRET_BOOTSTRAP=%s\n' "$bootstrap_token"
        printf 'CC_SECRET_METRICS_TOKEN=%s\n' "$secret_metrics"
        printf 'CC_SECRET_TWO_FACTOR_KEY=%s\n' "$secret_two_factor"
    } >"$env_tmp"
    chmod 0600 "$env_tmp"
    mv -f "$env_tmp" "$env_file"
    trap - 0 HUP INT TERM
    printf '%s' "$bootstrap_token" >"$CONFIG_DIR/bootstrap-token"
    chmod 0600 "$CONFIG_DIR/bootstrap-token"
fi
# Configurations from before the two-factor key existed get one; an existing key is kept.
if ! grep -q '^CC_SECRET_TWO_FACTOR_KEY=.' "$env_file"; then
    ensure_key=$(generate_two_factor_key)
    ensure_tmp=$env_file.tmp.$$
    { awk '!/^CC_SECRET_TWO_FACTOR_KEY=/' "$env_file"; printf 'CC_SECRET_TWO_FACTOR_KEY=%s\n' "$ensure_key"; } >"$ensure_tmp"
    chmod 0600 "$ensure_tmp"
    mv -f "$ensure_tmp" "$env_file"
fi
chmod 0600 "$env_file"
if ! grep -q '^CRAFTCLOUD_CADDY_CONFIG_DIR=' "$env_file"; then printf 'CRAFTCLOUD_CADDY_CONFIG_DIR=%s\n' "$CADDY_CONFIG_DIR" >>"$env_file"; fi
# Configurations from before the FigCraft setting existed record the chosen value.
if ! grep -q '^CRAFTCLOUD_FIGCRAFT=' "$env_file"; then env_set CRAFTCLOUD_FIGCRAFT "$FIGCRAFT"; fi
mkdir -p "$CADDY_CONFIG_DIR"
if [ ! -e "$CADDY_CONFIG_DIR/Caddyfile" ]; then
    caddy_empty=1
    for caddy_entry in "$CADDY_CONFIG_DIR"/* "$CADDY_CONFIG_DIR"/.[!.]* "$CADDY_CONFIG_DIR"/..?*; do
        [ -e "$caddy_entry" ] || [ -L "$caddy_entry" ] || continue
        caddy_empty=0
        break
    done
    [ "$caddy_empty" -eq 1 ] || fail 'Caddy config dir has files but no Caddyfile'
fi
# Copies missing shipped files, refreshes unmodified ones and keeps operator edits
# (the new version is written as <name>.new).
sync_caddy_config "$compose_dir/caddy" "$CADDY_CONFIG_DIR"
chmod 0700 "$CADDY_CONFIG_DIR"
if [ "$FIGCRAFT" = enabled ]; then
    [ -f "$compose_dir/compose.figcraft.yaml" ] || fail 'release archive has no compose.figcraft.yaml; use --no-figcraft or a complete release'
    ensure_figcraft_files
fi
if [ -f "$CADDY_CONFIG_DIR/private.key" ]; then chmod 0600 "$CADDY_CONFIG_DIR/private.key"; fi
version_image_tag=$(image_tag "$version")
[ "${#version_image_tag}" -le 128 ] || fail 'release version is too long for a Docker image tag'
export CRAFTCLOUD_IMAGE="$IMAGE_PREFIX$version_image_tag"
export CRAFTCLOUD_DATA_DIR="$DATA_DIR" CRAFTCLOUD_CONFIG_DIR="$CONFIG_DIR" CRAFTCLOUD_CADDY_CONFIG_DIR="$CADDY_CONFIG_DIR"
. "$SELF_DIR/smtp-overrides.sh"
smtp_files=$(smtp_overrides "$env_file")
# compose <with-setup 0|1> <docker compose arguments...>
compose() {
    compose_setup=$1; shift
    compose_list=$(compose_files "$compose_setup")
    # shellcheck disable=SC2086 # the file list is intentionally word-split
    (cd "$compose_dir" && docker compose --project-name "$PROJECT" --env-file "$env_file" $compose_list "$@")
}
build_services='migrate server backup restore'
[ "$FIGCRAFT" != enabled ] || build_services="$build_services figcraft"
# shellcheck disable=SC2086 # the service list is intentionally word-split
compose 0 build --build-arg "CRAFTCLOUD_VERSION=$version" --build-arg "CRAFTCLOUD_COMMIT=$release_commit" $build_services
setup_pending=0
[ ! -f "$CONFIG_DIR/bootstrap-token" ] || setup_pending=1
if [ "$setup_pending" -eq 1 ]; then
    compose 1 run --rm migrate
else
    compose 0 run --rm migrate
fi
rm -f -- "$INSTALL_ROOT/current.new"
ln -s "$release" "$INSTALL_ROOT/current.new"
mv -Tf "$INSTALL_ROOT/current.new" "$INSTALL_ROOT/current"
if [ "$setup_pending" -eq 1 ]; then
    compose 1 up -d
else
    compose 0 up -d
fi
tries=0
until compose "$setup_pending" exec -T server /usr/local/bin/craftcloud-server healthcheck >/dev/null 2>&1; do
    tries=$((tries + 1)); [ "$tries" -lt 60 ] || fail 'readiness check timed out'
    sleep 2
done
if [ "$SKIP_SYSTEMD" != 1 ]; then
    mkdir -p "$SYSTEMD_DIR"
    install -m 0644 "$SELF_DIR/systemd/craftcloud.service" "$SYSTEMD_DIR/craftcloud.service"
    install -m 0644 "$SELF_DIR/systemd/craftcloud-backup.service" "$SYSTEMD_DIR/craftcloud-backup.service"
    install -m 0644 "$SELF_DIR/systemd/craftcloud-backup.timer" "$SYSTEMD_DIR/craftcloud-backup.timer"
    systemctl daemon-reload
    systemctl enable --now craftcloud.service craftcloud-backup.timer
else
    printf '%s\n' 'Systemd setup skipped by CRAFTCLOUD_SKIP_SYSTEMD=1 (test container).'
fi
setup_url=https://$DOMAIN/admin
[ "$HTTPS_PORT" = 443 ] || setup_url=https://$DOMAIN:$HTTPS_PORT/admin
printf 'CraftCloud installed. Setup: %s\n' "$setup_url"
if [ -f "$CONFIG_DIR/bootstrap-token" ]; then printf 'Bootstrap secret file: %s (keep private; do not share its contents in logs)\n' "$CONFIG_DIR/bootstrap-token"; fi
if [ "$FIGCRAFT" = enabled ]; then
    printf 'FigCraft live collaboration: enabled (endpoint under https://%s/figcraft/). Disable with: craftcloud-ctl figcraft disable\n' "$DOMAIN"
else
    printf 'FigCraft live collaboration: disabled. Enable with: craftcloud-ctl figcraft enable\n'
fi
printf 'After setup, run: %s setup-complete\n' "$INSTALL_ROOT/current/deploy/install/craftcloud-ctl"
