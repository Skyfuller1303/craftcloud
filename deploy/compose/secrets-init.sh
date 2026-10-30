#!/bin/sh
case $- in *x*) set +x ;; esac
# One-shot secret distribution for the Compose stack (service `secrets-init`).
#
# Compose cannot hand secrets from .env to read-only containers, and secret
# files from the host would keep the host owner (the server refuses secret files
# that are readable by anybody but its own user). So this container, which has no
# network and runs for a second, writes every secret as its own file into the
# `secrets` volume, one directory per consuming service, owned by the user that
# reads it (mode 0400, directory 0500). Each service then mounts only its own
# directory, read-only, at /run/secrets.
#
#   db        root   superuser password (PostgreSQL entrypoint)
#   db-init   999    the three passwords
#   migrate   65532  owner and runtime database URL
#   server    65532  runtime database URL; bootstrap and metrics token only when
#                    the compose.setup.yaml / compose.metrics.yaml override asks
#
# It never prints a secret. The two database URLs are built here from the two
# role passwords, so a URL and its password cannot diverge: .env holds one value
# per role. Every run rewrites all files, so rotating a value in .env and running
# `docker compose up -d --force-recreate migrate server` is enough.
#
# The per-service directories are bind-mounted (volume subpath) into running
# containers, so they must never be deleted or recreated: that would give the
# directory a new inode and leave the running container with an empty
# /run/secrets. Each file is therefore written to a temporary file in the same
# directory and moved over the old one; only files that no longer belong (for
# example the bootstrap token after setup) are removed, by name.
set -eu
umask 077

OUT=/out
SRC=/run/secrets

# Installations can use separate Compose volumes instead of volume subpaths.
out_dir() {
    case "$1" in
        db) printf '%s\n' "${OUT_DB:-$OUT/db}" ;;
        db-init) printf '%s\n' "${OUT_DB_INIT:-$OUT/db-init}" ;;
        migrate) printf '%s\n' "${OUT_MIGRATE:-$OUT/migrate}" ;;
        server) printf '%s\n' "${OUT_SERVER:-$OUT/server}" ;;
        *) exit 1 ;;
    esac
}

# Role names and database: keep in sync with db-init.sh and xtask/src/compose.rs.
OWNER_ROLE=craftcloud_owner
RUNTIME_ROLE=craftcloud_runtime
DATABASE=craftcloud

# The URLs embed the passwords unescaped, so only URL-safe characters are allowed.
url_safe() {
    case "$1" in
        "" | *[!A-Za-z0-9._~-]*)
            echo "secrets-init: the $2 contains characters outside A-Z a-z 0-9 . _ ~ - (or is empty); it is used inside a database URL" >&2
            exit 1
            ;;
    esac
}

owner_password=$(cat "$SRC/db_owner_password")
runtime_password=$(cat "$SRC/db_runtime_password")
url_safe "$owner_password" "owner password"
url_safe "$runtime_password" "runtime password"

# put <uid> <service> <name> < content  (content on stdin, written without newline change)
put() {
    uid=$1
    service=$2
    name=$3
    dir=$(out_dir "$service")
    install -d -m 0500 -o "$uid" -g "$uid" "$dir"
    tmp="$dir/.new.$name"
    cat >"$tmp"
    chown "$uid:$uid" "$tmp"
    chmod 0400 "$tmp"
    mv -f "$tmp" "$dir/$name"
}

# prune <service> <name> ...: remove every file in the directory not listed.
prune() {
    service=$1
    shift
    dir=$(out_dir "$service")
    for path in "$dir"/* "$dir"/.[!.]*; do
        [ -e "$path" ] || continue
        keep=0
        for name in "$@"; do
            [ "$path" = "$dir/$name" ] && keep=1
        done
        [ "$keep" = 1 ] || rm -f -- "$path"
    done
}

copy() { put "$1" "$2" "$3" <"$SRC/$3"; }

copy 0 db db_superuser_password
prune db db_superuser_password

copy 999 db-init db_superuser_password
copy 999 db-init db_owner_password
copy 999 db-init db_runtime_password
prune db-init db_superuser_password db_owner_password db_runtime_password

printf 'postgres://%s:%s@db:5432/%s' "$OWNER_ROLE" "$owner_password" "$DATABASE" | put 65532 migrate db_owner_url
printf 'postgres://%s:%s@db:5432/%s' "$RUNTIME_ROLE" "$runtime_password" "$DATABASE" | put 65532 migrate db_runtime_url
prune migrate db_owner_url db_runtime_url

printf 'postgres://%s:%s@db:5432/%s' "$RUNTIME_ROLE" "$runtime_password" "$DATABASE" | put 65532 server db_runtime_url
server_files="db_runtime_url"
if [ "${COPY_BOOTSTRAP_TOKEN:-0}" = 1 ]; then
    copy 65532 server bootstrap_token
    server_files="$server_files bootstrap_token"
fi
if [ "${COPY_METRICS_TOKEN:-0}" = 1 ]; then
    copy 65532 server metrics_token
    server_files="$server_files metrics_token"
fi
# shellcheck disable=SC2086 # intentional word splitting of fixed names
prune server $server_files

echo "secrets distributed"
