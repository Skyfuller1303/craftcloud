#!/bin/bash
# OPS-02 installation tests 1 to 5, run INSIDE a debian:trixie container that
# has the Docker socket of Docker Desktop and shares the path /var/lib/docker/ops02-it with
# the Docker host (so that bind mounts of the Compose stack resolve). The driver
# run-ops02-integration.sh prepares the container and calls this script:
#
#   ops02-cases.sh setup            install the tools in the container
#   ops02-cases.sh case1|case2|case3|case4|case5
#
# Environment: V1, V2 (versions), DIST (directory with the release files), SRC
# (the repository), OUT (log directory). Ports are above 18000. All Docker
# resources carry the prefix ops02- and are removed at the end of each case.
set -euo pipefail

V1=${V1:?}
V2=${V2:?}
DIST=${DIST:-/dist}
SRC=${SRC:-/src}
BASE=/var/lib/docker/ops02-it
OUT=${OUT:-$BASE/logs}
mkdir -p "$BASE" "$OUT"

say() { printf '== %s\n' "$*"; }
die() { printf 'FAILED: %s\n' "$*" >&2; exit 1; }
need_file() { [ -f "$1" ] || die "missing $1"; }

setup() {
    export DEBIAN_FRONTEND=noninteractive
    printf '#!/bin/sh\nexit 101\n' >/usr/sbin/policy-rc.d
    chmod +x /usr/sbin/policy-rc.d
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends docker-cli docker-compose nginx apache2 curl openssl python3 zstd ca-certificates procps >/dev/null
    rm -f /etc/nginx/sites-enabled/default
    a2dissite -q 000-default >/dev/null 2>&1 || true
    # Apache stops without any Listen line: keep one harmless loopback listener.
    echo 'Listen 127.0.0.1:18999' >/etc/apache2/ports.conf
    a2enmod -q proxy proxy_http proxy_wstunnel headers rewrite ssl >/dev/null
    docker version --format 'docker server {{.Server.Version}} {{.Server.Arch}}'
    docker compose version --short
    nginx -v 2>&1
    apache2 -v | head -1
}

# extract_release <version> <dir>: the installer comes from the release archive itself.
extract_release() {
    rm -rf "$2"
    mkdir -p "$2"
    (cd "$DIST" && sha256sum --check --ignore-missing SHA256SUMS >/dev/null) || die "SHA256SUMS check failed"
    tar -xzf "$DIST/craftcloud-$1.tar.gz" -C "$2"
}

# installer_env <case> : exports the variables that put an instance below $BASE/<case>
installer_env() {
    export CRAFTCLOUD_SKIP_SYSTEMD=1
    export CRAFTCLOUD_INSTALL_ROOT=$BASE/$1/opt CRAFTCLOUD_CONFIG_DIR=$BASE/$1/etc CRAFTCLOUD_DATA_DIR=$BASE/$1/data
    export CRAFTCLOUD_PROJECT_NAME=ops02-$1 CRAFTCLOUD_IMAGE_PREFIX=craftcloud:
    export CRAFTCLOUD_SYSTEMD_DIR=$BASE/$1/systemd
}

ctl() { # ctl <case> <args...>
    c=$1; shift
    installer_env "$c"
    sh "$BASE/$c/opt/current/deploy/install/craftcloud-ctl" "$@"
}

wait_ready() { # wait_ready <url>
    for _ in $(seq 1 60); do
        if curl -ksf --max-time 5 "$1" >/dev/null 2>&1; then return 0; fi
        sleep 2
    done
    die "no readiness at $1"
}

no_leftovers() { # no_leftovers <project prefix>
    left=$(docker ps -a --format '{{.Names}}' | grep -E "^$1" || true)
    [ -z "$left" ] || die "containers left: $left"
}

secret_scan() { # secret_scan <case> <log files...>
    c=$1; shift
    envf=$BASE/$c/etc/craftcloud.env
    [ -f "$envf" ] || return 0
    leaks=0
    for key in CC_SECRET_DB_OWNER_PASSWORD CC_SECRET_DB_RUNTIME_PASSWORD CC_SECRET_BOOTSTRAP CC_SECRET_METRICS_TOKEN CC_SECRET_TWO_FACTOR_KEY CC_SECRET_DB_SUPERUSER_PASSWORD; do
        value=$(sed -n "s/^$key=//p" "$envf" | tail -n 1)
        [ "${#value}" -ge 20 ] || continue
        for f in "$@"; do [ -f "$f" ] && grep -F -- "$value" "$f" >/dev/null && { echo "LEAK: $key in $f"; leaks=$((leaks + 1)); }; done
    done
    fig=$(cat "$BASE/$c/etc/figcraft.key" 2>/dev/null || true)
    if [ -n "$fig" ]; then
        for f in "$@"; do [ -f "$f" ] && grep -F -- "$fig" "$f" >/dev/null && { echo "LEAK: figcraft key in $f"; leaks=$((leaks + 1)); }; done
    fi
    [ "$leaks" -eq 0 ] || die "secret scan found $leaks leak(s)"
    echo "secret scan: no secret of $c in $*"
}

case1() {
    say "case 1: --proxy caddy --db bundled --image-archive (no local build): status, backup, upgrade, rollback, uninstall"
    rm -f "$OUT"/case1-*.log "$OUT"/c1-*.log
    c=c1
    extract_release "$V1" "$BASE/$c-rel"
    installer_env "$c"
    arch=$(docker version --format '{{.Server.Arch}}')
    log=$OUT/case1-install.log
    sh "$BASE/$c-rel/craftcloud-$V1/deploy/install/install.sh" --non-interactive --domain localhost --acme-email ops@example.test \
        --proxy caddy --db bundled --http-port 18180 --https-port 18543 --data-dir "$CRAFTCLOUD_DATA_DIR" \
        --image-archive "$DIST/craftcloud-$V1-image-$arch.tar.zst" "$DIST/craftcloud-$V1.tar.gz" 2>&1 | tee "$log"
    if grep -E 'compose.* build ' "$OUT/case1-install.log" >/dev/null; then die "a local build ran"; fi
    docker images --format '{{.Repository}}:{{.Tag}}' | grep -qx "craftcloud:$V1" || die "image craftcloud:$V1 missing"
    wait_ready https://localhost:18543/health/ready
    status=$(curl -ks https://localhost:18543/api/v1/setup/status)
    echo "setup status: $status"
    case $status in *'"setup_required":true'*) ;; *) die "unexpected setup status" ;; esac
    ctl "$c" status 2>&1 | tee "$OUT/case1-status.log"
    ctl "$c" backup 2>&1 | tee "$OUT/case1-backup.log"
    grep -q 'Backup created' "$OUT/case1-backup.log" || die "backup failed"
    # Upgrade with the second prebuilt image
    ctl "$c" upgrade "$DIST/craftcloud-$V2.tar.gz" --image-archive "$DIST/craftcloud-$V2-image-$arch.tar.zst" 2>&1 | tee "$OUT/case1-upgrade.log"
    [ "$(readlink -f "$BASE/$c/opt/current")" = "$BASE/$c/opt/releases/$V2" ] || die "upgrade did not switch the release"
    grep -E 'compose.* build ' "$OUT/case1-upgrade.log" >/dev/null && die "a local build ran during upgrade"
    wait_ready https://localhost:18543/health/ready
    docker ps --format '{{.Image}}' --filter "label=com.docker.compose.project=ops02-$c" | grep -q "craftcloud:$V2" || die "server does not run craftcloud:$V2"
    ctl "$c" rollback 2>&1 | tee "$OUT/case1-rollback.log"
    [ "$(readlink -f "$BASE/$c/opt/current")" = "$BASE/$c/opt/releases/$V1" ] || die "rollback did not switch back"
    wait_ready https://localhost:18543/health/ready
    docker ps --format '{{.Image}}' --filter "label=com.docker.compose.project=ops02-$c" | grep -q "craftcloud:$V1" || die "server does not run craftcloud:$V1 after rollback"
    secret_scan "$c" "$OUT"/case1-*.log
    ctl "$c" uninstall --purge --confirm-purge 2>&1 | tee "$OUT/case1-uninstall.log"
    [ ! -e "$BASE/$c/opt" ] && [ ! -e "$BASE/$c/etc" ] && [ ! -e "$BASE/$c/data" ] || die "files left after purge"
    no_leftovers "ops02-$c"
    rm -rf "$BASE/$c" "$BASE/$c-rel"
    say "case 1 passed"
}


# start_argv_poller <secret>: a container in the host PID namespace of the Docker VM
# that scans the command line of EVERY process (all containers included) for the
# secret and records hits in $BASE/argv-hits. A positive control proves that it sees
# a secret that is an argument.
start_argv_poller() {
    printf '%s\n' "$1" >"$BASE/argv-pattern"
    chmod 0600 "$BASE/argv-pattern"
    : >"$BASE/argv-hits"
    docker rm -f ops02-argv-poll >/dev/null 2>&1 || true
    docker run -d --name ops02-argv-poll --pid host --cpus 1 -v "$BASE:$BASE" debian:trixie sh -c \
        "while :; do for f in /proc/[0-9]*/cmdline; do if tr '\\0' ' ' <\$f 2>/dev/null | grep -qF -f $BASE/argv-pattern; then echo \$f >>$BASE/argv-hits; fi; done; sleep 0.02; done" >/dev/null
    sleep 2
    docker run --rm debian:trixie sh -c 'sleep 3' "$1" >/dev/null
    [ -s "$BASE/argv-hits" ] || die "the argument scanner did not see a secret that is an argument (control failed)"
    : >"$BASE/argv-hits"
    echo "argument scanner works (positive control)"
}
stop_argv_poller() {
    docker rm -f ops02-argv-poll >/dev/null 2>&1 || true
    if [ -s "$BASE/argv-hits" ]; then die "the administrator password was an argument of a process: $(head -n 3 "$BASE/argv-hits")"; fi
    echo "no process ever had the administrator password in its arguments"
    rm -f "$BASE/argv-pattern" "$BASE/argv-hits"
}

# token_requests <https-base> <http-base>: requests with tokens in path and query,
# against the proxy (login page, API, FigCraft path, HTTP redirect, 404).
token_requests() {
    for url in "$1/api/v1/invitations/TOKENSECRET123?reset=TOKENQUERY456" "$1/figcraft/TOKENSECRET123?join=TOKENQUERY456" \
        "$1/admin/TOKENSECRET123?x=TOKENQUERY456" "$2/p/TOKENSECRET123?x=TOKENQUERY456" "$1/nothing/TOKENSECRET123"; do
        curl -ks -o /dev/null --max-time 10 "$url" || true
    done
    sleep 1
}
# no_token_in_logs <dirs...>: neither token appears in any file below the directories.
no_token_in_logs() {
    if grep -r -I -l -e TOKENSECRET123 -e TOKENQUERY456 "$@" 2>/dev/null; then die "a token from a request path or query is in a web server log (files above)"; fi
    echo "no token from a request path or query in any log below: $*"
}

make_certificate() { # make_certificate <dir> <subjectAltName>
    mkdir -p "$1"
    openssl req -x509 -newkey rsa:2048 -nodes -days 2 -keyout "$1/key.pem" -out "$1/cert.pem" -subj /CN=localhost \
        -addext "subjectAltName=$2" >/dev/null 2>&1
    chmod 0644 "$1/cert.pem"; chmod 0600 "$1/key.pem"
}

proxy_case() { # proxy_case <nginx|apache> <case>
    kind=$1; c=$2
    # Start with empty web server logs so that the token search below proves something.
    find /var/log/nginx /var/log/apache2 -type f -exec truncate -s 0 {} + 2>/dev/null || true
    extract_release "$V1" "$BASE/$c-rel"
    installer_env "$c"
    arch=$(docker version --format '{{.Server.Arch}}')
    make_certificate "$BASE/$c-tls" 'DNS:localhost,IP:127.0.0.1,IP:127.0.0.2'
    if [ "$kind" = nginx ]; then
        nginx
        sleep 1
    else
        apache2ctl start || die "apache did not start"
        sleep 1
    fi
    sh "$BASE/$c-rel/craftcloud-$V1/deploy/install/install.sh" --non-interactive --domain localhost --acme-email ops@example.test \
        --proxy "$kind" --proxy-install --proxy-cert "$BASE/$c-tls/cert.pem" --proxy-key "$BASE/$c-tls/key.pem" \
        --http-port 18080 --https-port 18443 --local-port 18081 --local-figcraft-port 18082 \
        --data-dir "$CRAFTCLOUD_DATA_DIR" --image-archive "$DIST/craftcloud-$V1-image-$arch.tar.zst" "$DIST/craftcloud-$V1.tar.gz" 2>&1 | tee "$OUT/$c-install.log"
    grep -q "$kind is configured with" "$OUT/$c-install.log" || die "installer did not configure $kind"
    ps -e -o comm | grep -q '^caddy$' && die "a Caddy process runs"
    docker ps --format '{{.Names}}' | grep -q "ops02-$c-caddy" && die "a Caddy container runs"
    ss=$(docker ps --format '{{.Ports}}' --filter "label=com.docker.compose.project=ops02-$c" | tr '\n' ' ')
    echo "published ports: $ss"
    case $ss in *0.0.0.0:*|*"[::]:"*) die "a CraftCloud port is published on all interfaces: $ss" ;; esac
}

case2() {
    say "case 2: --proxy nginx --proxy-install (self-signed certificate): login page, 100 MiB upload, WebSocket, forged X-Forwarded-For"
    rm -f "$OUT"/case2-*.log "$OUT"/c2-*.log
    c=c2
    proxy_case nginx "$c"
    python3 -I "$SRC/deploy/install/tests/integration/ops02_e2e.py" --base https://127.0.0.1:18443 --public-url https://localhost:18443 \
        --bootstrap-file "$BASE/$c/etc/bootstrap-token" --upload-mib 100 --figcraft-port 18082 --second-address 127.0.0.2 --hsts 2>&1 | tee "$OUT/case2-e2e.log"
    grep -q '^not ok' "$OUT/case2-e2e.log" && die "e2e failed"
    token_requests https://127.0.0.1:18443 http://127.0.0.1:18080
    no_token_in_logs /var/log/nginx
    nginx -s quit || true
    secret_scan "$c" "$OUT"/case2-*.log "$OUT/$c-install.log"
    ctl "$c" uninstall --purge --confirm-purge >"$OUT/case2-uninstall.log" 2>&1
    no_leftovers "ops02-$c"
    rm -rf "$BASE/$c" "$BASE/$c-rel" "$BASE/$c-tls"
    rm -f /etc/nginx/conf.d/craftcloud.conf
    say "case 2 passed"
}

case3() {
    say "case 3: --proxy apache --proxy-install: login page and WebSocket"
    rm -f "$OUT"/case3-*.log "$OUT"/c3-*.log
    c=c3
    proxy_case apache "$c"
    python3 -I "$SRC/deploy/install/tests/integration/ops02_e2e.py" --base https://127.0.0.1:18443 --public-url https://localhost:18443         --bootstrap-file "$BASE/$c/etc/bootstrap-token" --upload-mib 10 --figcraft-port 18082 --second-address 127.0.0.2 --hsts 2>&1 | tee "$OUT/case3-e2e.log"
    grep -q '^not ok' "$OUT"/case3-*.log && die "apache e2e failed"
    token_requests https://127.0.0.1:18443 http://127.0.0.1:18080
    ls -l /var/log/apache2/ | tee "$OUT/case3-logs.txt"
    no_token_in_logs /var/log/apache2
    apache2ctl stop || true
    secret_scan "$c" "$OUT"/case3-*.log "$OUT/$c-install.log"
    ctl "$c" uninstall --purge --confirm-purge >"$OUT/case3-uninstall.log" 2>&1
    no_leftovers "ops02-$c"
    rm -rf "$BASE/$c" "$BASE/$c-rel" "$BASE/$c-tls"
    rm -f /etc/apache2/sites-available/craftcloud.conf /etc/apache2/sites-enabled/craftcloud.conf
    apache2ctl stop >/dev/null 2>&1 || true
    sleep 1
    rm -f /var/log/apache2/craftcloud-error.log
    say "case 3 passed"
}

start_pg16() { # start_pg16 <name> <port>   TLS with an IP certificate for the docker0 gateway
    gw=$(docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}')
    d=$BASE/pg-$1
    rm -rf "$d"; mkdir -p "$d"
    # A real CA and a server certificate with an IP address (sslmode=verify-full needs both).
    openssl req -x509 -newkey rsa:2048 -nodes -days 2 -keyout "$d/ca.key" -out "$d/ca.pem" -subj /CN=ops02-ca         -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign >/dev/null 2>&1
    openssl req -newkey rsa:2048 -nodes -keyout "$d/key.pem" -out "$d/server.csr" -subj /CN=localhost >/dev/null 2>&1
    printf 'subjectAltName=IP:%s,DNS:localhost
basicConstraints=CA:FALSE
' "$gw" >"$d/ext.cnf"
    openssl x509 -req -in "$d/server.csr" -CA "$d/ca.pem" -CAkey "$d/ca.key" -CAcreateserial -out "$d/cert.pem" -days 2 -extfile "$d/ext.cnf" >/dev/null 2>&1
    chmod 0644 "$d/ca.pem" "$d/cert.pem"
    chown 999:999 "$d/key.pem"; chmod 0600 "$d/key.pem"
    pw=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')
    echo "$pw" >"$d/pw"; chmod 0600 "$d/pw"
    docker run -d --name "$1" --network host -e POSTGRES_PASSWORD="$pw" -v "$d:/tls:ro" --memory 1g postgres:16.15 \
        postgres -c port="$2" -c ssl=on -c ssl_cert_file=/tls/cert.pem -c ssl_key_file=/tls/key.pem -c fsync=off >/dev/null
    for _ in $(seq 1 40); do
        docker exec "$1" pg_isready -h 127.0.0.1 -p "$2" -U postgres >/dev/null 2>&1 && break
        sleep 1
    done
    docker exec "$1" pg_isready -h 127.0.0.1 -p "$2" -U postgres >/dev/null || die "PostgreSQL 16 did not start"
    docker exec "$1" psql -U postgres -p "$2" -tAc "select current_setting('server_version')" | tee "$d/version"
    PG_GATEWAY=$gw
}

case4() {
    say "case 4: --db external against PostgreSQL 16; MariaDB detection"
    rm -f "$OUT"/case4-*.log "$OUT"/c4-*.log
    c=c4
    arch=$(docker version --format '{{.Server.Arch}}')
    # MariaDB: detection message only
    mdb=ops02-mariadb
    docker run -d --name "$mdb" --network host -e MARIADB_ROOT_PASSWORD="$(head -c 12 /dev/urandom | od -An -tx1 | tr -d ' \n')" --memory 512m mariadb:10.11 >/dev/null
    for _ in $(seq 1 30); do ss=$(cat /proc/net/tcp /proc/net/tcp6 2>/dev/null | awk '$4=="0A" && toupper($2) ~ /:0CEA$/' | wc -l); [ "$ss" -gt 0 ] && break; sleep 2; done
    extract_release "$V1" "$BASE/$c-rel"
    installer_env "$c"
    sh "$BASE/$c-rel/craftcloud-$V1/deploy/install/install.sh" --dry-run --non-interactive --domain localhost --proxy caddy \
        --http-port 18180 --https-port 18543 --data-dir "$CRAFTCLOUD_DATA_DIR" "$DIST/craftcloud-$V1.tar.gz" 2>&1 | tee "$OUT/case4-mariadb.log"
    grep -q 'MySQL or MariaDB was detected' "$OUT/case4-mariadb.log" || die "MariaDB was not detected"
    grep -q 'does not support MySQL, MariaDB' "$OUT/case4-mariadb.log" || die "unsupported message missing"
    docker rm -f "$mdb" >/dev/null
    rm -rf "$BASE/$c"
    # PostgreSQL 16 as the external database
    start_pg16 ops02-pg16 18432
    d=$BASE/pg-ops02-pg16
    admin_pw=$(cat "$d/pw")
    # The database must exist for the connection check of the administrator URL.
    printf 'postgres://%s:18432/craftcloud?sslmode=verify-full&sslrootcert=%s/ca.pem\n' "$PG_GATEWAY" "$d" >"$BASE/c4-db.url"
    printf 'postgres://postgres:%s@%s:18432/postgres?sslmode=verify-full&sslrootcert=%s/ca.pem\n' "$admin_pw" "$PG_GATEWAY" "$d" >"$BASE/c4-admin.url"
    chmod 0600 "$BASE/c4-admin.url" "$BASE/c4-db.url"
    # Without the administrator URL: the installer stops and writes the SQL
    sh "$BASE/$c-rel/craftcloud-$V1/deploy/install/install.sh" --non-interactive --domain localhost --acme-email ops@example.test \
        --db external --database-url-file "$BASE/c4-db.url" --proxy caddy --http-port 18180 --https-port 18543 \
        --data-dir "$CRAFTCLOUD_DATA_DIR" --image-archive "$DIST/craftcloud-$V1-image-$arch.tar.zst" "$DIST/craftcloud-$V1.tar.gz" 2>&1 | tee "$OUT/case4-roles.log"
    grep -q 'needs the two CraftCloud roles' "$OUT/case4-roles.log" || die "role instructions missing"
    [ -f "$BASE/$c/etc/external-db-roles.sql" ] || die "role SQL missing"
    docker exec -i ops02-pg16 psql -U postgres -p 18432 -v ON_ERROR_STOP=1 -q <"$BASE/$c/etc/external-db-roles.sql" || die "the generated role SQL failed"
    sh "$BASE/$c-rel/craftcloud-$V1/deploy/install/install.sh" --non-interactive --domain localhost --acme-email ops@example.test \
        --db external --database-url-file "$BASE/c4-db.url" --db-roles-ready --proxy caddy --http-port 18180 --https-port 18543 \
        --data-dir "$CRAFTCLOUD_DATA_DIR" --image-archive "$DIST/craftcloud-$V1-image-$arch.tar.zst" "$DIST/craftcloud-$V1.tar.gz" 2>&1 | tee "$OUT/case4-install.log"
    wait_ready https://localhost:18543/health/ready
    docker ps -a --format '{{.Names}}' | grep -E "^ops02-$c-db" && die "a bundled db container exists"
    count=$(docker exec ops02-pg16 psql -U postgres -p 18432 -d craftcloud -tAc 'select count(*) from _sqlx_migrations')
    echo "migrations applied in the external database: $count"
    [ "$count" -gt 0 ] || die "no migrations in the external database"
    docker exec ops02-pg16 psql -U postgres -p 18432 -d craftcloud -tAc "select rolname from pg_roles where rolname like 'craftcloud_%' order by 1"
    ctl "$c" status 2>&1 | tee "$OUT/case4-status.log"
    ctl "$c" backup 2>&1 | tee "$OUT/case4-backup.log"
    grep -q 'Backup created' "$OUT/case4-backup.log" || die "backup against the external database failed"
    secret_scan "$c" "$OUT"/case4-*.log
    if grep -F -e "$PG_GATEWAY" -e "$admin_pw" "$OUT"/case4-*.log >/dev/null; then die "database address or administrator password printed"; fi
    ctl "$c" uninstall --purge --confirm-purge >"$OUT/case4-uninstall.log" 2>&1
    docker rm -f ops02-pg16 >/dev/null
    no_leftovers "ops02-$c"
    rm -rf "$BASE/$c" "$BASE/$c-rel" "$BASE/pg-ops02-pg16" "$BASE/c4-db.url" "$BASE/c4-admin.url"
    # The administrator URL path is covered by case 4b below.
    say "case 4 passed"
}

case4b() {
    say "case 4b: --db-admin-url-file (roles created by the installer in a container)"
    rm -f "$OUT"/case4b-*.log "$OUT"/c4b-*.log
    c=c4b
    rm -rf "$BASE/$c"
    arch=$(docker version --format '{{.Server.Arch}}')
    extract_release "$V1" "$BASE/$c-rel"
    installer_env "$c"
    start_pg16 ops02-pg16b 18434
    d=$BASE/pg-ops02-pg16b
    admin_pw=$(cat "$d/pw")
    printf 'postgres://%s:18434/craftcloud?sslmode=verify-full&sslrootcert=%s/ca.pem\n' "$PG_GATEWAY" "$d" >"$BASE/c4b-db.url"
    printf 'postgres://postgres:%s@%s:18434/postgres?sslmode=verify-full&sslrootcert=%s/ca.pem\n' "$admin_pw" "$PG_GATEWAY" "$d" >"$BASE/c4b-admin.url"
    chmod 0600 "$BASE/c4b-admin.url" "$BASE/c4b-db.url"
    start_argv_poller "$admin_pw"
    sh "$BASE/$c-rel/craftcloud-$V1/deploy/install/install.sh" --non-interactive --domain localhost --acme-email ops@example.test \
        --db external --database-url-file "$BASE/c4b-db.url" --db-admin-url-file "$BASE/c4b-admin.url" --proxy caddy --http-port 18180 --https-port 18543 \
        --data-dir "$CRAFTCLOUD_DATA_DIR" --image-archive "$DIST/craftcloud-$V1-image-$arch.tar.zst" "$DIST/craftcloud-$V1.tar.gz" 2>&1 | tee "$OUT/case4b-install.log"
    stop_argv_poller | tee "$OUT/case4b-argv.log"
    [ "${PIPESTATUS[0]}" -eq 0 ] || die "argument scan failed"
    grep -q 'the external database is PostgreSQL 16' "$OUT/case4b-install.log" || die "version of the external database not reported"
    wait_ready https://localhost:18543/health/ready
    if grep -F -e "$PG_GATEWAY" -e "$admin_pw" "$OUT"/case4b-*.log >/dev/null; then die "database address or administrator password printed"; fi
    secret_scan "$c" "$OUT"/case4b-*.log
    ctl "$c" uninstall --purge --confirm-purge >"$OUT/case4b-uninstall.log" 2>&1
    docker rm -f ops02-pg16b >/dev/null
    no_leftovers "ops02-$c"
    rm -rf "$BASE/$c" "$BASE/$c-rel" "$BASE/pg-ops02-pg16b" "$BASE/c4b-db.url" "$BASE/c4b-admin.url"
    say "case 4b passed"
}

case5() {
    say "case 5: docker run all-in-one against an external PostgreSQL 16; docker stop"
    rm -f "$OUT"/case5-*.log "$OUT"/c5-*.log
    arch=$(docker version --format '{{.Server.Arch}}')
    docker load -i "$DIST/craftcloud-$V1-image-$arch.tar.zst" >/dev/null 2>&1 || zstd -dc "$DIST/craftcloud-$V1-image-$arch.tar.zst" | docker load >/dev/null
    image=craftcloud:$V1
    # The runtime image stays shell-less: no /bin/sh, no busybox, no entrypoint script.
    for shell in /bin/sh /bin/bash /busybox/sh /usr/bin/sh; do
        if docker run --rm --entrypoint "$shell" "$image" -c true >/dev/null 2>&1; then die "the image contains a shell: $shell"; fi
    done
    echo "image has no shell (/bin/sh, /bin/bash, /busybox/sh, /usr/bin/sh all fail to start)"
    [ "$(docker image inspect "$image" --format '{{json .Config.Entrypoint}}')" = '["/usr/local/bin/craftcloud-server"]' ] || die "entrypoint is not the exec-form binary"
    docker image inspect "$image" --format '{{json .Config.Healthcheck.Test}}' | grep -q '"CMD"' || die "healthcheck is not exec-form"
    start_pg16 ops02-pg16c 18433
    d=$BASE/pg-ops02-pg16c
    s=$BASE/c5-secrets
    rm -rf "$s"; mkdir -p "$s"
    owner_pw=$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')
    runtime_pw=$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')
    docker exec -i ops02-pg16c psql -U postgres -p 18433 -v ON_ERROR_STOP=1 -q <<SQL
CREATE ROLE craftcloud_owner LOGIN PASSWORD '$owner_pw';
CREATE ROLE craftcloud_runtime LOGIN PASSWORD '$runtime_pw';
CREATE DATABASE craftcloud OWNER craftcloud_owner;
\\connect craftcloud
GRANT CONNECT ON DATABASE craftcloud TO craftcloud_runtime;
GRANT USAGE ON SCHEMA public TO craftcloud_runtime;
SQL
    q="sslmode=verify-full&sslrootcert=/run/secrets/ca.pem"
    printf 'postgres://craftcloud_owner:%s@%s:18433/craftcloud?%s' "$owner_pw" "$PG_GATEWAY" "$q" >"$s/db_owner_url"
    printf 'postgres://craftcloud_runtime:%s@%s:18433/craftcloud?%s' "$runtime_pw" "$PG_GATEWAY" "$q" >"$s/db_runtime_url"
    head -c 32 /dev/urandom | base64 | tr -d '\n' >"$s/two_factor_key"
    head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' >"$s/figcraft_key"
    cp "$d/ca.pem" "$s/ca.pem"
    chown -R 65532:65532 "$s"; chmod 0500 "$s"; chmod 0400 "$s"/*
    gw=$PG_GATEWAY
    # The schema first (role migrate), then the single container.
    docker run --rm --name ops02-aio-migrate -v "$s:/run/secrets:ro" \
        -e CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE=/run/secrets/db_owner_url -e CRAFTCLOUD_DATABASE_URL_FILE=/run/secrets/db_runtime_url \
        -e CRAFTCLOUD_DATABASE_RUNTIME_ROLE=craftcloud_runtime "$image" migrate 2>&1 | tee "$OUT/case5-migrate.log"
    docker run -d --name ops02-aio --stop-timeout 35 -p 127.0.0.1:18084:8080 -p 127.0.0.1:18085:47900 \
        -e CRAFTCLOUD_PUBLIC_URL=https://localhost:18543 -e CRAFTCLOUD_TRUSTED_PROXIES="$gw/32" \
        -e CRAFTCLOUD_DATABASE_URL_FILE=/run/secrets/db_runtime_url -e CRAFTCLOUD_SECRET_KEY_FILE=/run/secrets/two_factor_key \
        -e CRAFTCLOUD_FIGCRAFT_KEY_FILE=/run/secrets/figcraft_key \
        -v "$s:/run/secrets:ro" -v ops02-aio-store:/var/lib/craftcloud/store -v ops02-aio-figcraft:/var/lib/craftcloud/figcraft \
        "$image" all-in-one >/dev/null
    for _ in $(seq 1 60); do curl -sf --max-time 3 http://127.0.0.1:18084/health/ready >/dev/null 2>&1 && break; sleep 2; done
    curl -sf http://127.0.0.1:18084/health/ready >/dev/null || { docker logs ops02-aio 2>&1 | tail -20; die "all-in-one server is not ready"; }
    echo "server ready; processes in the container:"
    docker top ops02-aio -eo pid,args | tee "$OUT/case5-top.log"
    grep -q 'craftcloud-server' "$OUT/case5-top.log" || die "server process missing"
    grep -q 'figcraft-server' "$OUT/case5-top.log" || die "figcraft process missing"
    python3 -I "$SRC/deploy/install/tests/integration/ops02_e2e.py" --direct-websocket 18085 2>&1 | tee "$OUT/case5-ws.log"
    grep -q '^not ok' "$OUT/case5-ws.log" && die "FigCraft server in all-in-one does not answer a WebSocket handshake"
    started=$(date +%s)
    docker stop ops02-aio >/dev/null
    elapsed=$(( $(date +%s) - started ))
    code=$(docker inspect ops02-aio --format '{{.State.ExitCode}}')
    docker logs ops02-aio >"$OUT/case5-container.log" 2>&1
    echo "docker stop took ${elapsed}s, exit code $code"
    [ "$elapsed" -lt 35 ] || die "stop took $elapsed s"
    [ "$code" = 0 ] || die "exit code $code"
    grep -q '"child":"server".*stopped\|child=server.*stopped\|"stopped"' "$OUT/case5-container.log" || echo "(no explicit stopped lines)"
    if grep -F -e "$owner_pw" -e "$runtime_pw" -e "$(cat "$s/figcraft_key")" "$OUT"/case5-*.log >/dev/null; then die "a secret appears in the logs"; fi
    docker rm -f ops02-aio >/dev/null 2>&1 || true
    docker volume rm ops02-aio-store ops02-aio-figcraft >/dev/null
    docker rm -f ops02-pg16c >/dev/null
    rm -rf "$s" "$d"
    say "case 5 passed (stop ${elapsed}s)"
}


case6() {
    say "case 6: --db external together with --proxy nginx: one routable network, two real client addresses keep separate limits"
    rm -f "$OUT"/case6-*.log "$OUT"/c6-*.log
    c=c6
    rm -rf "$BASE/$c"
    arch=$(docker version --format '{{.Server.Arch}}')
    extract_release "$V1" "$BASE/$c-rel"
    installer_env "$c"
    make_certificate "$BASE/$c-tls" 'DNS:localhost,IP:127.0.0.1,IP:127.0.0.2'
    start_pg16 ops02-pg16d 18435
    d=$BASE/pg-ops02-pg16d
    admin_pw=$(cat "$d/pw")
    printf 'postgres://%s:18435/craftcloud?sslmode=verify-full&sslrootcert=%s/ca.pem\n' "$PG_GATEWAY" "$d" >"$BASE/c6-db.url"
    printf 'postgres://postgres:%s@%s:18435/postgres?sslmode=verify-full&sslrootcert=%s/ca.pem\n' "$admin_pw" "$PG_GATEWAY" "$d" >"$BASE/c6-admin.url"
    chmod 0600 "$BASE/c6-admin.url" "$BASE/c6-db.url"
    nginx
    sleep 1
    sh "$BASE/$c-rel/craftcloud-$V1/deploy/install/install.sh" --non-interactive --domain localhost --acme-email ops@example.test \
        --db external --database-url-file "$BASE/c6-db.url" --db-admin-url-file "$BASE/c6-admin.url" \
        --proxy nginx --proxy-install --proxy-cert "$BASE/$c-tls/cert.pem" --proxy-key "$BASE/$c-tls/key.pem" \
        --http-port 18080 --https-port 18443 --local-port 18081 --local-figcraft-port 18082 \
        --data-dir "$CRAFTCLOUD_DATA_DIR" --image-archive "$DIST/craftcloud-$V1-image-$arch.tar.zst" "$DIST/craftcloud-$V1.tar.gz" 2>&1 | tee "$OUT/c6-install.log"
    nets=$(docker inspect "ops02-$c-server-1" --format '{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{end}}')
    echo "server networks: $nets"
    case $nets in *dbegress*) die "the server is on dbegress next to the proxy network: $nets" ;; esac
    case $nets in *_proxy*) ;; *) die "the server is not on the proxy network: $nets" ;; esac
    routable=0
    for n in $nets; do [ "$(docker network inspect "$n" --format '{{.Internal}}')" = false ] && routable=$((routable + 1)); done
    [ "$routable" -eq 1 ] || die "the server has $routable routable networks, expected exactly one"
    for n in $nets; do
        docker network inspect "$n" --format '{{.Name}} internal={{.Internal}} subnet={{range .IPAM.Config}}{{.Subnet}}{{end}}' | tee -a "$OUT/case6-networks.log"
    done
    python3 -I "$SRC/deploy/install/tests/integration/ops02_e2e.py" --base https://127.0.0.1:18443 --public-url https://localhost:18443 \
        --bootstrap-file "$BASE/$c/etc/bootstrap-token" --upload-mib 10 --figcraft-port 18082 --second-address 127.0.0.2 --hsts 2>&1 | tee "$OUT/case6-e2e.log"
    grep -q '^not ok' "$OUT/case6-e2e.log" && die "e2e failed (external database with an own proxy)"
    grep -q 'a different real client address has its own limit' "$OUT/case6-e2e.log" || die "the second source address did not get its own limit"
    nginx -s quit || true
    if grep -F -e "$PG_GATEWAY" -e "$admin_pw" "$OUT"/c6-install.log "$OUT"/case6-*.log >/dev/null; then die "database address or administrator password printed"; fi
    secret_scan "$c" "$OUT"/case6-*.log "$OUT/c6-install.log"
    ctl "$c" uninstall --purge --confirm-purge >"$OUT/case6-uninstall.log" 2>&1
    docker rm -f ops02-pg16d >/dev/null
    no_leftovers "ops02-$c"
    rm -rf "$BASE/$c" "$BASE/$c-rel" "$BASE/$c-tls" "$BASE/pg-ops02-pg16d" "$BASE/c6-db.url" "$BASE/c6-admin.url"
    rm -f /etc/nginx/conf.d/craftcloud.conf
    say "case 6 passed"
}

case7() {
    say "case 7: --proxy apache --proxy-install with a configuration that fails the Apache configuration test: Apache stays up, the site is not enabled"
    rm -f "$OUT"/case7-*.log "$OUT"/c7-*.log
    c=c7
    rm -rf "$BASE/$c"
    rm -f /etc/apache2/sites-available/craftcloud.conf /etc/apache2/sites-enabled/craftcloud.conf
    # The test container has no init process: a stopped Apache leaves a zombie and a stale pid file.
    apache2ctl stop >/dev/null 2>&1 || true
    sleep 1
    rmdir /var/log/apache2/craftcloud-error.log 2>/dev/null || true
    rm -f /var/run/apache2/apache2.pid /var/log/apache2/craftcloud-error.log
    arch=$(docker version --format '{{.Server.Arch}}')
    extract_release "$V1" "$BASE/$c-rel"
    installer_env "$c"
    make_certificate "$BASE/$c-bad" 'DNS:localhost,IP:127.0.0.1'
    apache2ctl start || die "apache did not start"
    sleep 1
    # Forced configuration failure: a wrapper around apachectl that fails `configtest` as soon as the
    # CraftCloud site is enabled and passes everything else (modules, graceful, real Apache) through.
    cat >"$BASE/apachectl-guard" <<'GUARD'
#!/bin/sh
if [ "$1" = configtest ] && [ -e /etc/apache2/sites-enabled/craftcloud.conf ]; then
    echo 'forced configtest failure (OPS-02 test)' >&2
    exit 1
fi
exec apachectl "$@"
GUARD
    chmod +x "$BASE/apachectl-guard"
    before=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 http://127.0.0.1:18999/ || true)
    echo "Apache answers on its existing listener before: $before"
    [ "$before" != 000 ] || die "the existing Apache listener does not answer"
    if CRAFTCLOUD_APACHECTL="$BASE/apachectl-guard" sh "$BASE/$c-rel/craftcloud-$V1/deploy/install/install.sh" --non-interactive --domain localhost --acme-email ops@example.test \
        --proxy apache --proxy-install --proxy-cert "$BASE/$c-bad/cert.pem" --proxy-key "$BASE/$c-bad/key.pem" \
        --http-port 18080 --https-port 18443 --local-port 18081 --local-figcraft-port 18082 \
        --data-dir "$CRAFTCLOUD_DATA_DIR" --image-archive "$DIST/craftcloud-$V1-image-$arch.tar.zst" "$DIST/craftcloud-$V1.tar.gz" >"$OUT/c7-install.log" 2>&1; then
        die "the installation must fail when the Apache configuration test fails"
    fi
    tail -3 "$OUT/c7-install.log"
    grep -q 'the change was undone' "$OUT/c7-install.log" || die "undo message missing"
    [ ! -e /etc/apache2/sites-enabled/craftcloud.conf ] && [ ! -L /etc/apache2/sites-enabled/craftcloud.conf ] || die "the site is still enabled"
    [ ! -e /etc/apache2/sites-available/craftcloud.conf ] || die "the new site file is still there"
    apachectl configtest 2>&1 | tail -1 | tee "$OUT/case7-configtest.log"
    grep -q 'Syntax OK' "$OUT/case7-configtest.log" || die "the restored Apache configuration does not pass the test"
    after=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 http://127.0.0.1:18999/ || true)
    echo "Apache answers on its existing listener after: $after"
    [ "$after" != 000 ] || die "Apache stopped answering"
    pgrep -x apache2 >/dev/null || die "no apache2 process runs"
    apache2ctl stop || true
    ctl "$c" uninstall --purge --confirm-purge >"$OUT/case7-uninstall.log" 2>&1
    no_leftovers "ops02-$c"
    rm -rf "$BASE/$c" "$BASE/$c-rel" "$BASE/$c-bad"
    say "case 7 passed"
}

cleanup_images() {
    for v in "$V1" "$V2"; do docker rmi "craftcloud:$v" >/dev/null 2>&1 || true; done
    docker rmi "craftcloud:$V1-" >/dev/null 2>&1 || true
}

case ${1:-} in
    setup) setup ;;
    case1) case1 ;;
    case2) case2 ;;
    case3) case3 ;;
    case4) case4 ;;
    case4b) case4b ;;
    case5) case5 ;;
    case6) case6 ;;
    case7) case7 ;;
    images-cleanup) cleanup_images ;;
    *) echo "usage: $0 setup|case1|case2|case3|case4|case4b|case5|case6|case7|images-cleanup" >&2; exit 2 ;;
esac
