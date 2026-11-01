#!/bin/sh
# OPS-02 installer tests with a mocked Docker (no containers are started):
# database detection and choice, external database URL rules, proxy choice,
# proxy templates and --proxy-install, image archives, and the rule that no
# output contains a secret. Run as root (the installer insists), for example in
# a debian container:  sh deploy/install/tests/ops02-config.sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd -P)
TMP=${TMPDIR:-/tmp}/craftcloud-ops02-test-$$
trap 'if [ "${KEEP_TEST:-0}" = 1 ]; then echo "test artifacts: $TMP"; else rm -rf "$TMP"; fi' EXIT HUP INT TERM
mkdir -p "$TMP/bin" "$TMP/output" "$TMP/procnet" "$TMP/proc"
[ "$(id -u)" -eq 0 ] || { echo 'ops02-config: run as root' >&2; exit 1; }

ok() { printf 'ok - %s\n' "$1"; }
die() { printf 'not ok - %s\n' "$1" >&2; exit 1; }

# --- mocks --------------------------------------------------------------------
cat >"$TMP/bin/docker" <<'DOCKER'
#!/bin/sh
printf '%s\n' "$*" >>"${DOCKER_LOG:-/dev/null}"
case "$*" in
    *"${DOCKER_FAIL:-@@never@@}"*) exit 1 ;;
    'compose version --short') echo 2.24.4 ;;
    'version --format {{.Server.Version}}') echo 25.0.0 ;;
    'version --format {{.Server.Arch}}') echo "${MOCK_ARCH:-amd64}" ;;
    'image inspect craftcloud:'*'--format'*) echo "${MOCK_IMAGE_ARCH:-amd64}" ;;
    'image inspect --format {{.Architecture}} '*) echo "${MOCK_IMAGE_ARCH:-amd64}" ;;
    *'exec -T server /usr/local/bin/craftcloud-server healthcheck'*) exit 0 ;;
    *'-At -c'*) echo "${MOCK_PG_NUM:-160004}" ;;
esac
exit 0
DOCKER
cat >"$TMP/bin/df" <<'DF'
#!/bin/sh
printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/test 20000000 100 19999900 1%% /\n'
DF
cat >"$TMP/bin/systemctl" <<'SYSTEMCTL'
#!/bin/sh
printf '%s\n' "$*" >>"${SYSTEMCTL_LOG:-/dev/null}"
case "$1" in is-active) exit 1 ;; esac
exit 0
SYSTEMCTL
cat >"$TMP/bin/nginx" <<'NGINX'
#!/bin/sh
printf 'nginx %s\n' "$*" >>"${NGINX_LOG:-/dev/null}"
case "$1" in -t) [ "${NGINX_FAIL:-0}" != 1 ] || exit 1 ;; esac
exit 0
NGINX
cat >"$TMP/bin/apachectl" <<'APACHECTL'
#!/bin/sh
enabled=no
[ ! -e "${CRAFTCLOUD_APACHE_SITES_ENABLED:-/nonexistent}/craftcloud.conf" ] || enabled=yes
printf 'apachectl %s enabled=%s\n' "$*" "$enabled" >>"${NGINX_LOG:-/dev/null}"
case "$1" in
    -M) printf '%s\n' ${APACHE_MODULES:-proxy_module proxy_http_module proxy_wstunnel_module headers_module rewrite_module ssl_module} ;;
    configtest) [ "${APACHE_FAIL:-0}" != 1 ] || exit 1 ;;
esac
exit 0
APACHECTL
cat >"$TMP/bin/a2ensite" <<'A2ENSITE'
#!/bin/sh
printf 'a2ensite %s\n' "$*" >>"${NGINX_LOG:-/dev/null}"
[ "${A2ENSITE_FAIL:-0}" != 1 ] || exit 1
ln -sf "$CRAFTCLOUD_APACHE_SITES_AVAILABLE/$1.conf" "$CRAFTCLOUD_APACHE_SITES_ENABLED/$1.conf"
A2ENSITE
cat >"$TMP/bin/a2dissite" <<'A2DISSITE'
#!/bin/sh
printf 'a2dissite %s\n' "$*" >>"${NGINX_LOG:-/dev/null}"
rm -f "$CRAFTCLOUD_APACHE_SITES_ENABLED/$1.conf"
A2DISSITE
mkdir -p "$TMP/certbot-bin"
cat >"$TMP/certbot-bin/certbot" <<'CERTBOT'
#!/bin/sh
# Records whether the HTTP site was already enabled when certbot ran, then creates the certificate.
enabled=no
[ ! -e "${CRAFTCLOUD_APACHE_SITES_ENABLED:-/nonexistent}/craftcloud.conf" ] || enabled=yes
printf 'certbot %s site_enabled=%s\n' "$*" "$enabled" >>"${NGINX_LOG:-/dev/null}"
domain=
while [ "$#" -gt 0 ]; do [ "$1" != -d ] || domain=$2; shift; done
mkdir -p "/etc/letsencrypt/live/$domain"
printf cert >"/etc/letsencrypt/live/$domain/fullchain.pem"
printf key >"/etc/letsencrypt/live/$domain/privkey.pem"
CERTBOT
chmod +x "$TMP/bin/"* "$TMP/certbot-bin/certbot"

# A release built from the real deploy directory (compose files, templates, scripts).
make_release() {
    rel=$TMP/source/craftcloud-$1
    rm -rf "${rel:?}"
    mkdir -p "$rel/deploy"
    cp -r "$ROOT/deploy/compose" "$ROOT/deploy/proxy" "$ROOT/deploy/install" "$rel/deploy/"
    rm -rf "$rel/deploy/install/tests"
    printf '{"version":"%s","commit":"%s"}\n' "$1" "$2" >"$rel/CRAFTCLOUD-RELEASE.json"
    (cd "$TMP/source" && tar -czf "$TMP/output/craftcloud-$1.tar.gz" "craftcloud-$1")
    (cd "$TMP/output" && sha256sum "craftcloud-$1.tar.gz" >SHA256SUMS)
}
make_release 0.3.0 0300000

# fake_listeners <port...>: a /proc/net/tcp with LISTEN sockets on these ports.
fake_listeners() {
    {
        printf '  sl  local_address rem_address   st tx_queue rx_queue\n'
        n=0
        for p in "$@"; do
            printf '  %s: 00000000:%04X 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0\n' "$n" "$p"
            n=$((n + 1))
        done
    } >"$TMP/procnet/tcp"
}
fake_process() { mkdir -p "$TMP/proc/$2"; printf '%s\n' "$1" >"$TMP/proc/$2/comm"; }
reset_host() {
    rm -rf "$TMP/proc" "$TMP/procnet"
    mkdir -p "$TMP/proc" "$TMP/procnet"
    for table in tcp tcp6 udp udp6; do : >"$TMP/procnet/$table"; done
}
reset_host

# install <name> <args...>: a fresh instance under $TMP/<name>; output in $TMP/<name>.log
install_into() {
    name=$1; shift
    rm -rf "${TMP:?}/${name:?}"
    run_installer "$name" "$@"
}
run_installer() {
    name=$1; shift
    PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/$name/opt" CRAFTCLOUD_CONFIG_DIR="$TMP/$name/etc" \
    CRAFTCLOUD_DATA_DIR="$TMP/$name/data" CRAFTCLOUD_PROJECT_NAME="craftcloud-ops02-$name" CRAFTCLOUD_IMAGE_PREFIX=craftcloud:t- \
    CRAFTCLOUD_SYSTEMD_DIR="$TMP/$name/systemd" DOCKER_LOG="$TMP/$name.docker.log" \
    CRAFTCLOUD_PROC_NET="$TMP/procnet" CRAFTCLOUD_PROC_DIR="$TMP/proc" \
    sh "$ROOT/deploy/install/install.sh" --non-interactive --domain cloud.example.test --acme-email ops@example.test \
        --data-dir "$TMP/$name/data" "$@" "$TMP/output/craftcloud-0.3.0.tar.gz" >"$TMP/$name.log" 2>&1
}
expect_fail() { # expect_fail <name> <message fragment> <installer args...>
    name=$1; msg=$2; shift 2
    if install_into "$name" "$@"; then die "$name: installer unexpectedly succeeded"; fi
    grep -F -- "$msg" "$TMP/$name.log" >/dev/null || { cat "$TMP/$name.log" >&2; die "$name: message '$msg' missing"; }
}
# Directory layout: CONFIG_DIR is $TMP/<name>/etc, craftcloud.env lives inside.
envfile() { printf '%s\n' "$TMP/$1/etc/craftcloud.env"; }
envget() { sed -n "s/^$2=//p" "$(envfile "$1")" | tail -n 1; }

# --- 1. database detection (auto) ---------------------------------------------
fake_listeners 5432
install_into detect-pg --proxy caddy --http-port 28300 --https-port 28301 || { cat "$TMP/detect-pg.log"; die 'auto with a host PostgreSQL must continue with the bundled database'; }
grep -F 'a PostgreSQL server (version not determined) was detected' "$TMP/detect-pg.log" >/dev/null || die 'PostgreSQL detection message missing'
grep -F -- '--db external --database-url-file' "$TMP/detect-pg.log" >/dev/null || die 'external hint missing'
[ "$(envget detect-pg CRAFTCLOUD_DB)" = bundled ] || die 'non-interactive auto must not pick the host PostgreSQL'
ok 'host PostgreSQL is reported, not used without --db external'

printf '#!/bin/sh\nprintf "16 main 5432 online postgres /var/lib/postgresql/16/main /var/log/x.log\\n"\n' >"$TMP/bin/pg_lsclusters"
chmod +x "$TMP/bin/pg_lsclusters"
install_into detect-pg16 --proxy caddy --http-port 28300 --https-port 28301
grep -F 'PostgreSQL 16 was detected' "$TMP/detect-pg16.log" >/dev/null || die 'PostgreSQL 16 version not reported'
rm -f "$TMP/bin/pg_lsclusters"
ok 'detected version is reported'

fake_listeners 3306
install_into detect-mysql --proxy caddy --http-port 28300 --https-port 28301
grep -F 'MySQL or MariaDB was detected' "$TMP/detect-mysql.log" >/dev/null || die 'MariaDB detection message missing'
grep -F 'does not support MySQL, MariaDB' "$TMP/detect-mysql.log" >/dev/null || die 'unsupported message missing'
[ "$(envget detect-mysql CRAFTCLOUD_DB)" = bundled ] || die 'MariaDB host must fall back to the bundled PostgreSQL'
ok 'MySQL or MariaDB is explained and the bundled PostgreSQL is offered'
reset_host

# --- 2. external database rules -------------------------------------------------
expect_fail ext-nofile '--db external needs --database-url-file' --db external --proxy caddy --http-port 28300 --https-port 28301
echo 'postgres://craftcloud:hunter2secret@db.internal.example:5432/craftcloud?sslmode=verify-full' >"$TMP/url-cred"
expect_fail ext-cred 'must not contain a user name or password' --db external --database-url-file "$TMP/url-cred" --proxy caddy --http-port 28300 --https-port 28301
if grep -F -e hunter2secret -e db.internal.example "$TMP/ext-cred.log" >/dev/null; then die 'the URL (credential or host) was echoed'; fi
echo 'postgres://db.internal.example:5432/craftcloud' >"$TMP/url-remote-plain"
expect_fail ext-plain 'sslmode=verify-full' --db external --database-url-file "$TMP/url-remote-plain" --proxy caddy --http-port 28300 --https-port 28301
echo 'postgres://db.internal.example:5432/craftcloud?sslmode=require' >"$TMP/url-remote-require"
expect_fail ext-require 'sslmode=verify-full' --db external --database-url-file "$TMP/url-remote-require" --proxy caddy --http-port 28300 --https-port 28301
echo 'postgres://localhost:5432/craftcloud' >"$TMP/url-local"
expect_fail ext-local 'needs --db-allow-insecure-local' --db external --database-url-file "$TMP/url-local" --proxy caddy --http-port 28300 --https-port 28301
echo 'postgres://db.internal.example:5432/craftcloud?sslmode=verify-full' >"$TMP/url-remote"
expect_fail ext-remote-insecure 'applies only to a database on this host' --db external --database-url-file "$TMP/url-remote" --db-allow-insecure-local --proxy caddy --http-port 28300 --https-port 28301
echo 'postgres://db.internal.example:5432/craftcloud?sslmode=verify-full&sslcert=/x' >"$TMP/url-extra"
expect_fail ext-extra 'only sslmode and sslrootcert' --db external --database-url-file "$TMP/url-extra" --proxy caddy --http-port 28300 --https-port 28301
echo 'mysql://db.internal.example:3306/craftcloud' >"$TMP/url-mysql"
expect_fail ext-mysql 'only PostgreSQL is supported' --db external --database-url-file "$TMP/url-mysql" --proxy caddy --http-port 28300 --https-port 28301
echo 'postgres://db.internal.example:5432/craft-cloud;drop' >"$TMP/url-name"
expect_fail ext-name 'database name' --db external --database-url-file "$TMP/url-name" --proxy caddy --http-port 28300 --https-port 28301
ok 'external database URLs: credentials, plaintext remote, odd queries, other databases are refused without echoing the URL'

printf 'dummy ca\n' >"$TMP/ca.pem"
echo "postgres://db.internal.example:5432/craftcloud?sslmode=verify-full&sslrootcert=$TMP/ca.pem" >"$TMP/url-ca"
install_into ext-ca --db external --database-url-file "$TMP/url-ca" --proxy caddy --http-port 28300 --https-port 28301 || { cat "$TMP/ext-ca.log"; die 'external database with a private CA failed'; }
grep -F 'needs the two CraftCloud roles' "$TMP/ext-ca.log" >/dev/null || die 'role instructions missing'
[ -f "$TMP/ext-ca/etc/db-ca.pem" ] || die 'CA file not copied'
[ "$(stat -c '%a' "$TMP/ext-ca/etc/external-db-roles.sql")" = 600 ] || die 'role SQL must be private'
[ "$(envget ext-ca CRAFTCLOUD_DB)" = external ] || die 'external not recorded'
[ "$(envget ext-ca CRAFTCLOUD_DB_TARGET)" = db.internal.example:5432/craftcloud ] || die 'target not recorded'
[ "$(envget ext-ca CRAFTCLOUD_DB_QUERY)" = '?sslmode=verify-full&sslrootcert=/run/db-ca/ca.pem' ] || die 'query not rewritten to the container path'
[ "$(envget ext-ca CRAFTCLOUD_DB_CA)" = 1 ] || die 'CA flag missing'
[ "$(envget ext-ca CC_SECRET_DB_SUPERUSER_PASSWORD)" = external-database-unused ] || die 'no superuser password may be generated'
if grep -E -- ' (build|up|run) ' "$TMP/ext-ca.docker.log" >/dev/null 2>&1; then die 'the stack must not start before the roles exist'; fi
grep -F "CREATE ROLE craftcloud_owner" "$TMP/ext-ca/etc/external-db-roles.sql" >/dev/null || die 'role SQL incomplete'
owner_pw=$(envget ext-ca CC_SECRET_DB_OWNER_PASSWORD)
grep -F "$owner_pw" "$TMP/ext-ca/etc/external-db-roles.sql" >/dev/null || die 'role SQL must carry the generated owner password'
if grep -F -e "$owner_pw" -e db.internal.example "$TMP/ext-ca.log" "$TMP/ext-ca.docker.log" >/dev/null; then die 'role password or database host printed'; fi
ok 'external database: SQL written privately, nothing started, no superuser secret, nothing leaked'

run_installer ext-ca --db external --database-url-file "$TMP/url-ca" --db-roles-ready --proxy caddy --http-port 28300 --https-port 28301 || { cat "$TMP/ext-ca.log"; die 'second run with --db-roles-ready failed'; }
[ ! -e "$TMP/ext-ca/etc/external-db-roles.sql" ] || die 'role SQL must be removed once the roles exist'
grep -F -- '-f compose.install.yaml -f compose.figcraft.yaml -f compose.external-db.yaml -f compose.external-db-ca.yaml' "$TMP/ext-ca.docker.log" >/dev/null || { cat "$TMP/ext-ca.docker.log"; die 'external database overlays missing from the Compose calls'; }
if grep -F db.internal.example "$TMP/ext-ca.log" "$TMP/ext-ca.docker.log" >/dev/null; then die 'database host printed'; fi
ok 'external database: rerun continues with the overlays'

# admin URL file: the SQL is run in a container, the URL is mounted and never printed
echo 'postgres://admin:adminsecret99@db.internal.example:5432/postgres?sslmode=verify-full' >"$TMP/admin-url"
echo 'postgres://db.internal.example:5432/craftcloud?sslmode=verify-full' >"$TMP/url-admin"
install_into ext-admin --db external --database-url-file "$TMP/url-admin" --db-admin-url-file "$TMP/admin-url" --proxy caddy --http-port 28300 --https-port 28301 || { cat "$TMP/ext-admin.log"; die 'external database with an administrator URL failed'; }
grep -F 'the external database is PostgreSQL 16' "$TMP/ext-admin.log" >/dev/null || die 'version of the external database not reported'
grep -F -- '--entrypoint /bin/sh' "$TMP/ext-admin.docker.log" | grep -F 'target=/run/admin-url' >/dev/null || die 'administrator URL must be mounted'
if grep -F -e adminsecret99 -e db.internal.example "$TMP/ext-admin.log" "$TMP/ext-admin.docker.log" >/dev/null; then die 'administrator credentials or host printed'; fi
ok 'administrator URL file: roles created in a container, credentials not printed'
export MOCK_PG_NUM=150005
expect_fail ext-old 'PostgreSQL 15 is not supported' --db external --database-url-file "$TMP/url-admin" --db-admin-url-file "$TMP/admin-url" --proxy caddy --http-port 28300 --https-port 28301
unset MOCK_PG_NUM
ok 'PostgreSQL 15 is refused'

# --- 3. proxy choice ------------------------------------------------------------
reset_host
fake_listeners 80 443
fake_process nginx 101
install_into proxy-auto --http-port 80 --https-port 443 --local-port 28310 --local-figcraft-port 28311 || { cat "$TMP/proxy-auto.log"; die 'auto with running nginx failed'; }
grep -F 'a running nginx web server was detected' "$TMP/proxy-auto.log" >/dev/null || die 'nginx detection message missing'
[ "$(envget proxy-auto CRAFTCLOUD_PROXY)" = nginx ] || die 'nginx not chosen'
grep -F -- 'compose.local-proxy.yaml -f compose.local-proxy-figcraft.yaml' "$TMP/proxy-auto.docker.log" >/dev/null || die 'local proxy overlays missing'
if grep -F 'port 80 is already in use' "$TMP/proxy-auto.log" >/dev/null; then die 'busy public ports must not block an own-proxy installation'; fi
for f in craftcloud.nginx.conf craftcloud.apache.conf; do [ -f "$TMP/proxy-auto/etc/proxy/$f" ] || die "template $f missing"; done
ok 'running nginx is detected; templates are written; ports 80/443 are not required'

nginx_conf=$TMP/proxy-auto/etc/proxy/craftcloud.nginx.conf
grep -F 'server_name cloud.example.test;' "$nginx_conf" >/dev/null || die 'domain not rendered'
grep -F 'proxy_pass http://127.0.0.1:28310;' "$nginx_conf" >/dev/null || die 'upstream port not rendered'
grep -F 'proxy_pass http://127.0.0.1:28311;' "$nginx_conf" >/dev/null || die 'FigCraft port not rendered'
grep -F 'client_max_body_size 5385486336;' "$nginx_conf" >/dev/null || die 'body limit (5 GiB + 16 MiB) not rendered'
grep -F 'proxy_request_buffering off;' "$nginx_conf" >/dev/null || die 'upload buffering must be off'
grep -F 'proxy_set_header X-Forwarded-For   $remote_addr;' "$nginx_conf" >/dev/null || die 'X-Forwarded-For must be replaced, not appended'
if grep -F 'proxy_add_x_forwarded_for' "$nginx_conf" >/dev/null; then die 'client X-Forwarded-For must not be appended'; fi
grep -F 'proxy_set_header X-Forwarded-Host  "";' "$nginx_conf" >/dev/null || die 'X-Forwarded-Host must be cleared'
grep -F 'Strict-Transport-Security' "$nginx_conf" >/dev/null || die 'HSTS missing'
grep -F 'location /figcraft/' "$nginx_conf" >/dev/null || die 'FigCraft path missing'
grep -F 'ssl_certificate     /etc/letsencrypt/live/cloud.example.test/fullchain.pem;' "$nginx_conf" >/dev/null || die 'certificate path not rendered'
if grep -q '@[A-Z_]*@' "$nginx_conf" "$TMP/proxy-auto/etc/proxy/craftcloud.apache.conf"; then die 'unreplaced placeholder'; fi
apache_conf=$TMP/proxy-auto/etc/proxy/craftcloud.apache.conf
grep -F 'LimitRequestBody 0' "$apache_conf" >/dev/null || die 'Apache cannot limit above 2 GiB; 0 expected'
grep -F 'ProxyAddHeaders Off' "$apache_conf" >/dev/null || die 'ProxyAddHeaders must be off'
grep -F 'RequestHeader set X-Forwarded-For "%{REMOTE_ADDR}s"' "$apache_conf" >/dev/null || die 'Apache X-Forwarded-For'
grep -F 'RequestHeader unset X-Forwarded-Host' "$apache_conf" >/dev/null || die 'Apache X-Forwarded-Host'
grep -F 'upgrade=websocket' "$apache_conf" >/dev/null || die 'Apache WebSocket upgrade missing'
ok 'templates: domain, ports, body limit, header rules, WebSocket, HSTS'

# the trusted proxy network is rendered by Compose from the local-proxy overlay
grep -F 'CRAFTCLOUD_TRUSTED_PROXIES' "$ROOT/deploy/compose/compose.local-proxy.yaml" >/dev/null || die 'trusted proxy setting missing from the overlay'

reset_host
fake_listeners 80 443
expect_fail proxy-other 'not recognized as nginx or Apache' --http-port 80 --https-port 443
reset_host
fake_listeners 28320
expect_fail proxy-localbusy 'local port 28320 is already in use' --proxy none --local-port 28320 --local-figcraft-port 28321
reset_host
install_into proxy-free --http-port 28300 --https-port 28301
[ "$(envget proxy-free CRAFTCLOUD_PROXY)" = caddy ] || die 'free ports and no web server must keep the bundled Caddy'
grep -F -- 'local-proxy' "$TMP/proxy-free.docker.log" >/dev/null && die 'Caddy installation must not use the local proxy overlay'
ok 'free ports keep Caddy; busy ports without a known server ask for a choice'

# --- 4. --proxy-install nginx ------------------------------------------------------
printf 'cert\n' >"$TMP/test.crt"; printf 'key\n' >"$TMP/test.key"
nginx_args="--proxy nginx --http-port 28330 --https-port 28331 --local-port 28332 --local-figcraft-port 28333 --proxy-install --proxy-cert $TMP/test.crt --proxy-key $TMP/test.key"
mkdir -p "$TMP/nginx-conf"
install_env() {
    CRAFTCLOUD_NGINX_BIN="$TMP/bin/nginx" CRAFTCLOUD_NGINX_CONF_FILE="$TMP/nginx-conf/craftcloud.conf" CRAFTCLOUD_RELOAD_CMD="$TMP/bin/nginx -s reload" \
    NGINX_LOG="$TMP/nginx.log" "$@"
}
: >"$TMP/nginx.log"
echo 'previous operator file' >"$TMP/nginx-conf/craftcloud.conf"
export NGINX_FAIL=1
# shellcheck disable=SC2086
if install_env install_into nginx-fail $nginx_args; then die 'a failing nginx test must fail the installation'; fi
grep -F 'configuration test failed' "$TMP/nginx-fail.log" >/dev/null || die 'failure message missing'
[ "$(cat "$TMP/nginx-conf/craftcloud.conf")" = 'previous operator file' ] || die 'the previous file was not restored'
if grep -F 'nginx -s reload' "$TMP/nginx.log" >/dev/null; then die 'nginx was reloaded after a failed test'; fi
ls "$TMP/nginx-fail/etc/proxy/backup"/craftcloud.conf.* >/dev/null 2>&1 || die 'no backup of the previous file'
ok 'nginx: failed test restores the previous file, saves a backup and does not reload'
unset NGINX_FAIL
rm -rf "$TMP/nginx-ok"
# shellcheck disable=SC2086
install_env install_into nginx-ok $nginx_args || { cat "$TMP/nginx-ok.log"; die 'nginx installation failed'; }
cmp "$TMP/nginx-conf/craftcloud.conf" "$TMP/nginx-ok/etc/proxy/craftcloud.nginx.conf" || die 'installed file differs from the rendered template'
grep -F 'nginx -s reload' "$TMP/nginx.log" >/dev/null || die 'nginx was not reloaded after a successful test'
grep -F "ssl_certificate     $TMP/test.crt;" "$TMP/nginx-conf/craftcloud.conf" >/dev/null || die 'own certificate not used'
ok 'nginx: configuration installed after a successful test, then reloaded'

rm -rf "$TMP/nginx-nocert"
# shellcheck disable=SC2086
if install_env install_into nginx-nocert --proxy nginx --http-port 28330 --https-port 28331 --local-port 28332 --local-figcraft-port 28333 --proxy-install; then die 'no certificate must stop --proxy-install'; fi
grep -F 'no TLS certificate at' "$TMP/nginx-nocert.log" >/dev/null || die 'certificate message missing'
rm -rf "$TMP/nginx-certbot"
if install_env install_into nginx-certbot --proxy nginx --http-port 28330 --https-port 28331 --local-port 28332 --local-figcraft-port 28333 --proxy-install --certbot; then die 'missing certbot must stop --certbot'; fi
grep -F 'apt install certbot python3-certbot-nginx' "$TMP/nginx-certbot.log" >/dev/null || die 'certbot package hint missing'
grep -F 'does not install it' "$TMP/nginx-certbot.log" >/dev/null || die 'certbot must not be installed automatically'
ok 'nginx: missing certificate or certbot is reported, certbot is never installed'

# --- 5. --proxy-install apache -----------------------------------------------------
apache_args="--proxy apache --http-port 28340 --https-port 28341 --local-port 28342 --local-figcraft-port 28343 --proxy-install --proxy-cert $TMP/test.crt --proxy-key $TMP/test.key"
mkdir -p "$TMP/apache/available" "$TMP/apache/enabled"
apache_env() {
    (
        export CRAFTCLOUD_APACHECTL="$TMP/bin/apachectl" CRAFTCLOUD_APACHE_SITES_AVAILABLE="$TMP/apache/available" CRAFTCLOUD_APACHE_SITES_ENABLED="$TMP/apache/enabled"
        export CRAFTCLOUD_RELOAD_CMD="$TMP/bin/apachectl graceful" NGINX_LOG="$TMP/nginx.log"
        "$@"
    )
}
rm -rf "$TMP/apache-mod"
# shellcheck disable=SC2086
if (export APACHE_MODULES='proxy_module headers_module'; apache_env install_into apache-mod $apache_args); then die 'missing Apache modules must stop the installation'; fi
grep -F 'Apache modules missing: proxy_http proxy_wstunnel rewrite ssl' "$TMP/apache-mod.log" >/dev/null || { cat "$TMP/apache-mod.log"; die 'missing modules not named'; }
rm -rf "$TMP/apache-ok"
# shellcheck disable=SC2086
apache_env install_into apache-ok $apache_args || { cat "$TMP/apache-ok.log"; die 'Apache installation failed'; }
cmp "$TMP/apache/available/craftcloud.conf" "$TMP/apache-ok/etc/proxy/craftcloud.apache.conf" || die 'Apache file differs from the rendered template'
grep -F 'apachectl graceful' "$TMP/nginx.log" >/dev/null || die 'Apache was not reloaded'
ok 'Apache: modules are checked and named, configuration installed after configtest'

# --- 6. image archive ------------------------------------------------------------------
make_release 0.3.1 0310000
printf 'image bytes\n' >"$TMP/output/craftcloud-0.3.1-image-amd64.tar.zst"
(cd "$TMP/output" && sha256sum craftcloud-0.3.1.tar.gz craftcloud-0.3.1-image-amd64.tar.zst >SHA256SUMS)
run_archive() {
    name=$1; shift
    rm -rf "${TMP:?}/${name:?}"
    PATH="$TMP/bin:$PATH" CRAFTCLOUD_SKIP_SYSTEMD=1 CRAFTCLOUD_INSTALL_ROOT="$TMP/$name/opt" CRAFTCLOUD_CONFIG_DIR="$TMP/$name/etc" \
    CRAFTCLOUD_DATA_DIR="$TMP/$name/data" CRAFTCLOUD_PROJECT_NAME="craftcloud-ops02-$name" CRAFTCLOUD_IMAGE_PREFIX=craftcloud-server:t- \
    CRAFTCLOUD_PROC_NET="$TMP/procnet" CRAFTCLOUD_PROC_DIR="$TMP/proc" DOCKER_LOG="$TMP/$name.docker.log" \
    sh "$ROOT/deploy/install/install.sh" --non-interactive --domain cloud.example.test --data-dir "$TMP/$name/data" \
        --proxy caddy --http-port 28350 --https-port 28351 "$@" "$TMP/output/craftcloud-0.3.1.tar.gz" >"$TMP/$name.log" 2>&1
}
run_archive img-ok --image-archive "$TMP/output/craftcloud-0.3.1-image-amd64.tar.zst" || { cat "$TMP/img-ok.log"; die 'installation with an image archive failed'; }
grep -F 'load -i' "$TMP/img-ok.docker.log" >/dev/null || die 'docker load not called'
grep -F 'tag craftcloud:0.3.1 craftcloud-server:t-0.3.1' "$TMP/img-ok.docker.log" >/dev/null || die 'image not tagged for the installation'
if grep -E ' build ' "$TMP/img-ok.docker.log" >/dev/null; then die 'no local build may run when an image archive is used'; fi
ok 'image archive: checksum verified, loaded, tagged, no local build'
printf 'tampered\n' >"$TMP/output/craftcloud-0.3.1-image-amd64.tar.zst"
if run_archive img-bad --image-archive "$TMP/output/craftcloud-0.3.1-image-amd64.tar.zst"; then die 'a tampered image archive must be refused'; fi
grep -F 'archive checksum mismatch' "$TMP/img-bad.log" >/dev/null || die 'checksum mismatch not reported'
cp "$TMP/output/craftcloud-0.3.1-image-amd64.tar.zst" "$TMP/output/craftcloud-0.3.1-image-arm64.tar.zst"
(cd "$TMP/output" && sha256sum craftcloud-0.3.1.tar.gz craftcloud-0.3.1-image-arm64.tar.zst >SHA256SUMS)
if run_archive img-arch --image-archive "$TMP/output/craftcloud-0.3.1-image-arm64.tar.zst"; then die 'an image for another architecture must be refused'; fi
grep -F 'must be named craftcloud-0.3.1-image-amd64.tar.zst' "$TMP/img-arch.log" >/dev/null || die 'architecture message missing'
ok 'image archive: tampered or foreign-architecture files are refused'

# --- 7. nothing printed contains a secret --------------------------------------------------
leaks=0
for instance in ext-ca ext-admin nginx-ok apache-ok proxy-auto img-ok; do
    envfile_path=$(envfile "$instance")
    [ -f "$envfile_path" ] || continue
    for key in CC_SECRET_DB_OWNER_PASSWORD CC_SECRET_DB_RUNTIME_PASSWORD CC_SECRET_BOOTSTRAP CC_SECRET_METRICS_TOKEN CC_SECRET_TWO_FACTOR_KEY; do
        value=$(sed -n "s/^$key=//p" "$envfile_path" | tail -n 1)
        [ "${#value}" -ge 20 ] || continue
        for log in "$TMP/$instance.log" "$TMP/$instance.docker.log"; do
            [ -f "$log" ] || continue
            if grep -F -- "$value" "$log" >/dev/null; then echo "$instance: $key appears in $log" >&2; leaks=$((leaks + 1)); fi
        done
    done
    fig=$(cat "$TMP/$instance/etc/figcraft.key" 2>/dev/null || true)
    if [ -n "$fig" ]; then
        for log in "$TMP/$instance.log" "$TMP/$instance.docker.log"; do
            if [ -f "$log" ] && grep -F -- "$fig" "$log" >/dev/null; then echo "$instance: FigCraft key appears in $log" >&2; leaks=$((leaks + 1)); fi
        done
    fi
done
[ "$leaks" -eq 0 ] || die "secrets found in installer output ($leaks)"
ok 'secret scan: no generated secret, FigCraft key, administrator credential or external database host in any installer or Docker call log'

# --- 8. craftcloud-ctl follows the recorded choices -----------------------------------------
ctl_env() {
    name=$1; shift
    PATH="$TMP/bin:$PATH" CRAFTCLOUD_INSTALL_ROOT="$TMP/$name/opt" CRAFTCLOUD_CONFIG_DIR="$TMP/$name/etc" CRAFTCLOUD_DATA_DIR="$TMP/$name/data" \
    CRAFTCLOUD_PROJECT_NAME="craftcloud-ops02-$name" DOCKER_LOG="$TMP/$name.ctl.log" "$@"
}
: >"$TMP/nginx-ok.ctl.log"
ctl_env nginx-ok sh "$TMP/nginx-ok/opt/current/deploy/install/craftcloud-ctl" stop >/dev/null 2>&1 || die 'ctl stop failed'
grep -F -- 'compose.local-proxy.yaml' "$TMP/nginx-ok.ctl.log" >/dev/null || die 'ctl must use the local proxy overlay for an nginx installation'
ctl_env nginx-ok sh "$TMP/nginx-ok/opt/current/deploy/install/craftcloud-ctl" status >"$TMP/nginx-ok.status.log" 2>&1 || die 'ctl status failed'
grep -F 'reverse proxy: nginx' "$TMP/nginx-ok.status.log" >/dev/null || die 'status must name the proxy'
: >"$TMP/ext-ca.ctl.log"
ctl_env ext-ca sh "$TMP/ext-ca/opt/current/deploy/install/craftcloud-ctl" stop >/dev/null 2>&1 || die 'ctl stop (external database) failed'
grep -F -- 'compose.external-db.yaml -f compose.external-db-ca.yaml' "$TMP/ext-ca.ctl.log" >/dev/null || die 'ctl must use the external database overlays'
# A release without the overlay must not silently start the bundled services.
rm -f "$TMP/ext-ca/opt/releases/0.3.0/deploy/compose/compose.external-db.yaml" "$TMP/ext-ca/opt/releases/0.3.1/deploy/compose/compose.external-db.yaml"
if ctl_env ext-ca sh "$TMP/ext-ca/opt/current/deploy/install/craftcloud-ctl" start >"$TMP/ext-ca.start.log" 2>&1; then die 'start with a missing overlay must fail'; fi
grep -F 'has no compose.external-db.yaml' "$TMP/ext-ca.start.log" >/dev/null || { cat "$TMP/ext-ca.start.log"; die 'missing overlay message'; }
ok 'craftcloud-ctl: overlays follow the recorded choices; a missing overlay stops instead of starting the bundled services'


# --- 9. review fixes -------------------------------------------------------------------------
(cd "$TMP/output" && sha256sum craftcloud-0.3.0.tar.gz >>SHA256SUMS)
# M4/M5: Apache - the site is enabled inside the guarded change, a failure undoes it.
[ -L "$TMP/apache/enabled/craftcloud.conf" ] || die 'the Apache site was not enabled by a2ensite'
awk '/a2ensite/ {en=1} /configtest enabled=/ {if (!en) bad=1; if ($0 !~ /enabled=yes/) bad=1} END {exit bad}' "$TMP/nginx.log" || die 'a2ensite must run before configtest, and the site must be enabled while it is tested'
ok 'Apache: site enabled before the configuration test'
rm -f "$TMP/apache/enabled/craftcloud.conf" "$TMP/apache/available/craftcloud.conf"
: >"$TMP/nginx.log"
if (export APACHE_FAIL=1; apache_env install_into apache-fail $apache_args); then die 'a failing Apache configtest must fail the installation'; fi
grep -F 'the change was undone' "$TMP/apache-fail.log" >/dev/null || die 'undo message missing'
[ ! -e "$TMP/apache/enabled/craftcloud.conf" ] && [ ! -L "$TMP/apache/enabled/craftcloud.conf" ] || die 'the site stays enabled after a failed test'
[ ! -e "$TMP/apache/available/craftcloud.conf" ] || die 'the new site file stays after a failed test'
grep -F 'a2dissite craftcloud' "$TMP/nginx.log" >/dev/null || die 'a2dissite not called'
if grep -F 'graceful' "$TMP/nginx.log" >/dev/null; then die 'Apache was reloaded although the test failed'; fi
ok 'Apache: a failed configtest disables the site, restores the file and does not reload'
: >"$TMP/nginx.log"
if (export A2ENSITE_FAIL=1; apache_env install_into apache-a2fail $apache_args); then die 'a failing a2ensite must fail the installation'; fi
[ ! -L "$TMP/apache/enabled/craftcloud.conf" ] && [ ! -e "$TMP/apache/available/craftcloud.conf" ] || die 'a failed a2ensite left files behind'
ok 'Apache: a failed a2ensite leaves nothing behind'

# certbot: the HTTP site is enabled and reloaded BEFORE certbot runs
rm -rf /etc/letsencrypt/live/cloud.example.test
: >"$TMP/nginx.log"
# shellcheck disable=SC2086
PATH="$TMP/certbot-bin:$PATH" apache_env install_into apache-certbot --proxy apache --http-port 28340 --https-port 28341 --local-port 28342 --local-figcraft-port 28343 --proxy-install --certbot || { cat "$TMP/apache-certbot.log"; die 'certbot flow failed'; }
awk '/certbot certonly --apache/ {c=NR; if ($0 !~ /site_enabled=yes/) bad=1} /a2ensite/ {if (!a) a=NR} /apachectl graceful/ {if (!g) g=NR} END {exit (bad || !c || !a || !g || a > c || g > c)}' "$TMP/nginx.log" || { cat "$TMP/nginx.log"; die 'certbot ran before the HTTP site was enabled and reloaded'; }
rm -rf /etc/letsencrypt/live/cloud.example.test
ok 'certbot: the HTTP site is enabled and loaded before certbot certonly'

# M2: administrator URL rules
echo 'postgres://admin:adminsecret99@other.example:5432/postgres?sslmode=verify-full' >"$TMP/admin-otherhost"
expect_fail adm-host 'must name the same host' --db external --database-url-file "$TMP/url-admin" --db-admin-url-file "$TMP/admin-otherhost" --proxy caddy --http-port 28300 --https-port 28301
echo 'postgres://admin:adminsecret99@db.internal.example:5432/postgres?sslmode=require' >"$TMP/admin-require"
expect_fail adm-ssl 'sslmode=verify-full' --db external --database-url-file "$TMP/url-admin" --db-admin-url-file "$TMP/admin-require" --proxy caddy --http-port 28300 --https-port 28301
echo 'postgres://admin:adminsecret99@db.internal.example:5433/postgres?sslmode=verify-full' >"$TMP/admin-port"
expect_fail adm-port 'same port' --db external --database-url-file "$TMP/url-admin" --db-admin-url-file "$TMP/admin-port" --proxy caddy --http-port 28300 --https-port 28301
echo 'postgres://admin:adm%40secret@db.internal.example:5432/postgres?sslmode=verify-full' >"$TMP/admin-pct"
expect_fail adm-pct 'not percent-decoded' --db external --database-url-file "$TMP/url-admin" --db-admin-url-file "$TMP/admin-pct" --proxy caddy --http-port 28300 --https-port 28301
echo 'postgres://db.internal.example:5432/postgres?sslmode=verify-full' >"$TMP/admin-nouser"
expect_fail adm-nouser 'user name and a password' --db external --database-url-file "$TMP/url-admin" --db-admin-url-file "$TMP/admin-nouser" --proxy caddy --http-port 28300 --https-port 28301
for f in adm-host adm-ssl adm-port adm-pct adm-nouser; do
    if grep -F -e adminsecret99 -e adm%40secret -e other.example "$TMP/$f.log" >/dev/null; then die "$f: the administrator URL was echoed"; fi
done
ok 'administrator URL: same host and port as the application URL, verify-full for remote, no echo'

# M3: the password never reaches an argument; it travels in a PGPASSFILE on a tmpfs
install_into adm-run --db external --database-url-file "$TMP/url-admin" --db-admin-url-file "$TMP/admin-url" --proxy caddy --http-port 28300 --https-port 28301 || { cat "$TMP/adm-run.log"; die 'administrator run failed'; }
grep -F -- '--tmpfs /run/pw' "$TMP/adm-run.docker.log" >/dev/null || die 'the administrator container has no tmpfs for the password file'
grep -F 'PGPASSFILE=/run/pw/pgpass' "$TMP/adm-run.docker.log" >/dev/null || die 'the administrator script does not use a PGPASSFILE'
grep -F 'psql --no-psqlrc -v ON_ERROR_STOP=1 --quiet -f /run/roles.sql' "$TMP/adm-run.docker.log" >/dev/null || die 'psql command line missing'
if grep -F -e adminsecret99 -e db.internal.example "$TMP/adm-run.docker.log" "$TMP/adm-run.log" >/dev/null; then die 'the administrator password or host is part of a docker argument or of the output'; fi
ok 'administrator run: no password and no URL in any argument, PGPASSFILE on a tmpfs'

# L1 / L2 / L3: names, loopback with verify-full, logging of the role passwords
echo 'postgres://localhost:5432/craftcloud?sslmode=verify-full' >"$TMP/url-lo-vf"
expect_fail l1-localhost 'cannot work from a container' --db external --database-url-file "$TMP/url-lo-vf" --proxy caddy --http-port 28300 --https-port 28301
echo 'postgres://host.docker.internal:5432/craftcloud?sslmode=verify-full' >"$TMP/url-hdi-vf"
install_into l1-hdi --db external --database-url-file "$TMP/url-hdi-vf" --proxy caddy --http-port 28300 --https-port 28301 || { cat "$TMP/l1-hdi.log"; die 'host.docker.internal with verify-full must be accepted'; }
[ "$(envget l1-hdi CRAFTCLOUD_DB_TARGET)" = host.docker.internal:5432/craftcloud ] || die 'the name must be kept'
ok 'verify-full: a loopback name is refused, host.docker.internal is kept'
for bad in Craftcloud 1db craft-cloud postgres template1 user select 'a;b' aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; do
    echo "postgres://db.internal.example:5432/$bad?sslmode=verify-full" >"$TMP/url-name2"
    if install_into l2-name --db external --database-url-file "$TMP/url-name2" --proxy caddy --http-port 28300 --https-port 28301; then die "database name $bad was accepted"; fi
done
ok 'database names: upper case, leading digit, hyphen, reserved words, system databases and 64+ characters are refused'
roles_sql=$TMP/ext-ca/etc/external-db-roles.sql
[ -f "$roles_sql" ] || { install_into l3-sql --db external --database-url-file "$TMP/url-ca" --proxy caddy --http-port 28300 --https-port 28301; roles_sql=$TMP/l3-sql/etc/external-db-roles.sql; }
grep -F "SET log_statement = 'none';" "$roles_sql" >/dev/null || die 'log_statement is not switched off'
grep -F "SET log_min_error_statement = 'panic';" "$roles_sql" >/dev/null || die 'log_min_error_statement is not switched off'
grep -F '\if :ops_super' "$roles_sql" >/dev/null || die 'the logging switch must be skipped with a warning for non-superusers'
grep -F "format('CREATE DATABASE %I" "$roles_sql" >/dev/null || die 'the database name must be quoted with %I'
awk '/log_statement/ {l=NR} /ALTER ROLE/ {if (!a) a=NR} END {exit !(l && a && l < a)}' "$roles_sql" || die 'logging must be switched off before ALTER ROLE'
ok 'role SQL: logging off before the passwords, warning path, database name quoted'

# L4: rollback to a release that lacks an overlay changes nothing
rel=$TMP/nginx-ok/opt/releases
cp -r "$rel/0.3.0" "$rel/0.2.9"
rm -f "$rel/0.2.9/deploy/compose/compose.local-proxy.yaml"
printf '%s\n' "$rel/0.2.9" >"$TMP/nginx-ok/etc/previous-release"
image_before=$(envget nginx-ok CRAFTCLOUD_IMAGE)
link_before=$(readlink "$TMP/nginx-ok/opt/current")
if ctl_env nginx-ok sh "$TMP/nginx-ok/opt/current/deploy/install/craftcloud-ctl" rollback >"$TMP/rollback-missing.log" 2>&1; then die 'rollback to a release without the overlay must fail'; fi
grep -F 'has no compose.local-proxy.yaml' "$TMP/rollback-missing.log" >/dev/null || die 'rollback message missing'
[ "$(envget nginx-ok CRAFTCLOUD_IMAGE)" = "$image_before" ] || die 'rollback changed the image setting before the check'
[ "$(readlink "$TMP/nginx-ok/opt/current")" = "$link_before" ] || die 'rollback switched the symlink before the check'
ok 'rollback: the target release is checked before anything changes'

# L5: a leftover craftcloud-figcraft image setting is ignored
printf 'CRAFTCLOUD_FIGCRAFT_IMAGE=craftcloud-figcraft:abc\n' >>"$TMP/nginx-ok/etc/craftcloud.env"
ctl_env nginx-ok sh "$TMP/nginx-ok/opt/current/deploy/install/craftcloud-ctl" stop >"$TMP/l5.log" 2>&1 || die 'ctl stop failed'
grep -F 'retired craftcloud-figcraft image' "$TMP/l5.log" >/dev/null || die 'no warning for the retired image'
if grep -q '^CRAFTCLOUD_FIGCRAFT_IMAGE=' "$TMP/nginx-ok/etc/craftcloud.env"; then die 'the retired image setting was not removed'; fi
ok 'CRAFTCLOUD_FIGCRAFT_IMAGE naming craftcloud-figcraft is ignored with a warning'

# L8: status defaults for an environment file without the OPS-02 keys
grep -v -e '^CRAFTCLOUD_DB=' -e '^CRAFTCLOUD_PROXY=' "$TMP/proxy-free/etc/craftcloud.env" >"$TMP/env.old" && cp "$TMP/env.old" "$TMP/proxy-free/etc/craftcloud.env"
ctl_env proxy-free sh "$TMP/proxy-free/opt/current/deploy/install/craftcloud-ctl" status >"$TMP/l8.log" 2>&1 || die 'ctl status failed'
grep -F 'Database: bundled; reverse proxy: caddy' "$TMP/l8.log" >/dev/null || die 'status defaults missing'
ok 'status prints the defaults bundled and caddy'

# M1: one routable network per mode
install_into m1-extproxy --db external --database-url-file "$TMP/url-admin" --db-admin-url-file "$TMP/admin-url" --proxy nginx --local-port 28350 --local-figcraft-port 28351 || { cat "$TMP/m1-extproxy.log"; die 'external database with own proxy failed'; }
grep -F -- 'compose.external-db.yaml' "$TMP/m1-extproxy.docker.log" | grep -F -- 'compose.external-db-proxy.yaml' >/dev/null || die 'external database with an own proxy must use the proxy network overlay'
if grep -F -- 'compose.external-db-egress.yaml' "$TMP/m1-extproxy.docker.log" >/dev/null; then die 'dbegress must not be added next to the proxy network'; fi
grep -F -- 'compose.external-db-egress.yaml' "$TMP/ext-ca.docker.log" >/dev/null || die 'external database with Caddy must use the egress overlay'
ok 'external database: proxy network with an own proxy, dbegress with Caddy'

echo 'ops02-config: passed'
