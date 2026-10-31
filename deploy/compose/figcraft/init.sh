#!/bin/sh
# Provision only this companion's key and configuration. Never print a secret.
case $- in *x*) set +x ;; esac
set -eu
umask 077
key=$(cat /run/secrets/figcraft_key)
case "$key" in
    *[!A-Fa-f0-9]*|'') echo 'figcraft-init: key must contain 64 random hexadecimal characters' >&2; exit 1 ;;
esac
[ "${#key}" = 64 ] || { echo 'figcraft-init: invalid key length' >&2; exit 1; }
# Values become TOML strings. Reject quotes, control characters and credentials.
case "${FIGCRAFT_PUBLIC_URL:-}" in
    wss://*) ;;
    *) echo 'figcraft-init: public URL must use wss' >&2; exit 1 ;;
esac
case "$FIGCRAFT_PUBLIC_URL" in
    *[!A-Za-z0-9:/._~%\[\]-]*|*'@'*|*'?'*|*'#'*) echo 'figcraft-init: invalid public URL' >&2; exit 1 ;;
esac
install -d -m 0700 -o 65532 -g 65532 /data /shared-secret /config
# Do not replace directory inodes: running services mount these exact volumes.
for directory in /data /shared-secret; do
    printf '%s\n' "$key" > "$directory/.server.key.new"
    chown 65532:65532 "$directory/.server.key.new"
    chmod 0400 "$directory/.server.key.new"
    mv -f "$directory/.server.key.new" "$directory/server.key"
done
cat > /config/.server.toml.new <<EOF
listen = "0.0.0.0:47900"
data = "/data"
public_url = "$FIGCRAFT_PUBLIC_URL"
open_publish = false
max_plugins = 0
max_file_mb = 64
max_files = 1000
max_links = 1000
max_connections = 128
max_people = 64
EOF
chown 65532:65532 /config/.server.toml.new
chmod 0400 /config/.server.toml.new
mv -f /config/.server.toml.new /config/server.toml
echo 'FigCraft companion configured'
