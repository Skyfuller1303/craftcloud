# OPS-02 helpers shared by install.sh and craftcloud-ctl (POSIX sh, sourced).
# shellcheck disable=SC2034,SC2154 # variables are shared with the including script
#
# Everything here is about three decisions of an installation:
#   * which PostgreSQL it uses: the bundled container or an existing server,
#   * which reverse proxy fronts it: the bundled Caddy, your own nginx or Apache,
#   * which Compose files that makes (ops_overlay_files).
#
# Rules that hold for every function in this file:
#   * Nothing prints a secret: no password, no key, and not the database URL of
#     an external database (not even its host name).
#   * Detection only reads: it never changes the host.
#   * Functions report problems with `fail`, defined by the including script.

OPS_PROC_NET=${CRAFTCLOUD_PROC_NET:-/proc/net}
OPS_PROC=${CRAFTCLOUD_PROC_DIR:-/proc}

# ops_env_get <env-file> <NAME>: last value of NAME, empty if absent.
ops_env_get() {
    sed -n "s/^$2=//p" "$1" 2>/dev/null | tail -n 1
}

# --- Compose overlays ---------------------------------------------------------

# ops_overlay_files <env-file> <compose-dir> <figcraft enabled|disabled>
# Prints the ` -f <file>` words for the database and proxy choices recorded in
# the environment file. A missing overlay is an error and never silently
# dropped: running without it would start the bundled Caddy or database.
# An external database needs one routable network: dbegress with the bundled
# Caddy, the `proxy` network with your own proxy (never two, so that the
# default route and the trusted gateway stay the same network).
ops_overlay_files() {
    ops_db=$(ops_env_get "$1" CRAFTCLOUD_DB)
    ops_proxy=$(ops_env_get "$1" CRAFTCLOUD_PROXY)
    ops_files=
    case ${ops_db:-bundled} in
        bundled) ;;
        external)
            ops_files="compose.external-db.yaml"
            [ "$(ops_env_get "$1" CRAFTCLOUD_DB_CA)" != 1 ] || ops_files="$ops_files compose.external-db-ca.yaml"
            ;;
        *) fail "CRAFTCLOUD_DB must be bundled or external" ;;
    esac
    case ${ops_proxy:-caddy} in
        caddy)
            [ "${ops_db:-bundled}" != external ] || ops_files="$ops_files compose.external-db-egress.yaml"
            ;;
        nginx|apache|none)
            ops_files="$ops_files compose.local-proxy.yaml"
            [ "$3" != enabled ] || ops_files="$ops_files compose.local-proxy-figcraft.yaml"
            [ "${ops_db:-bundled}" != external ] || ops_files="$ops_files compose.external-db-proxy.yaml"
            ;;
        *) fail "CRAFTCLOUD_PROXY must be caddy, nginx, apache or none" ;;
    esac
    for ops_file in $ops_files; do
        [ -f "$2/$ops_file" ] || fail "this release has no $ops_file, which the installation needs (database: ${ops_db:-bundled}, proxy: ${ops_proxy:-caddy}); use a release that contains it"
        printf ' -f %s' "$ops_file"
    done
}

# ops_sanitize_figcraft_image <env-file>: the separate FigCraft image no longer
# exists. A leftover CRAFTCLOUD_FIGCRAFT_IMAGE that still names it (craftcloud-figcraft:...)
# is ignored with a warning, in the environment file and in the environment of
# this process, so that the companion runs from the CraftCloud image.
ops_sanitize_figcraft_image() {
    ops_fig_file=$(ops_env_get "$1" CRAFTCLOUD_FIGCRAFT_IMAGE)
    case $ops_fig_file in
        craftcloud-figcraft:*)
            printf '%s\n' 'craftcloud: warning: CRAFTCLOUD_FIGCRAFT_IMAGE names the retired craftcloud-figcraft image; the setting is ignored and removed from the environment file' >&2
            ops_fig_tmp=$1.tmp.$$
            awk 'index($0, "CRAFTCLOUD_FIGCRAFT_IMAGE=") != 1' "$1" >"$ops_fig_tmp"
            chmod 0600 "$ops_fig_tmp"
            mv -f "$ops_fig_tmp" "$1"
            ;;
    esac
    case ${CRAFTCLOUD_FIGCRAFT_IMAGE:-} in
        craftcloud-figcraft:*)
            printf '%s\n' 'craftcloud: warning: CRAFTCLOUD_FIGCRAFT_IMAGE in the environment names the retired craftcloud-figcraft image; it is ignored' >&2
            unset CRAFTCLOUD_FIGCRAFT_IMAGE
            ;;
    esac
}

# --- Detection ----------------------------------------------------------------

# ops_port_listening <port>: a TCP listener on that port exists on this host.
ops_port_listening() {
    ops_hex=$(printf '%04X' "$1")
    for ops_table in "$OPS_PROC_NET/tcp" "$OPS_PROC_NET/tcp6"; do
        [ -r "$ops_table" ] || continue
        if awk -v port="$ops_hex" 'NR > 1 { split($2, endpoint, ":"); if (toupper(endpoint[2]) == port && $4 == "0A") found = 1 } END { exit !found }' "$ops_table"; then
            return 0
        fi
    done
    return 1
}

# ops_process_running <comm>: a process with that name runs on this host.
ops_process_running() {
    for ops_comm in "$OPS_PROC"/[0-9]*/comm; do
        [ -r "$ops_comm" ] || continue
        [ "$(cat "$ops_comm" 2>/dev/null)" = "$1" ] && return 0
    done
    return 1
}

# ops_unit_active <unit>: a systemd unit is active (false without systemd).
ops_unit_active() {
    command -v systemctl >/dev/null 2>&1 || return 1
    systemctl is-active --quiet "$1" 2>/dev/null
}

# ops_detect_postgres: finds a PostgreSQL server on this host. Sets
# OPS_PG_VERSION to the highest major version found or `unknown`. Returns 1
# when nothing was found.
ops_detect_postgres() {
    OPS_PG_VERSION=
    ops_pg_found=0
    if command -v pg_lsclusters >/dev/null 2>&1; then
        ops_cluster=$(pg_lsclusters --no-header 2>/dev/null | awk '{print $1}' | sort -n | tail -n 1)
        if [ -n "$ops_cluster" ]; then OPS_PG_VERSION=$ops_cluster; ops_pg_found=1; fi
    fi
    if [ -z "$OPS_PG_VERSION" ] && command -v psql >/dev/null 2>&1; then
        ops_psql=$(psql --version 2>/dev/null | sed -n 's/^[^0-9]*\([0-9][0-9]*\)\..*/\1/p' | head -n 1)
        if [ -n "$ops_psql" ]; then OPS_PG_VERSION=$ops_psql; ops_pg_found=1; fi
    fi
    if [ -z "$OPS_PG_VERSION" ]; then
        ops_dirs=$(ls -1 /usr/lib/postgresql 2>/dev/null | sort -n | tail -n 1)
        if [ -n "$ops_dirs" ]; then OPS_PG_VERSION=$ops_dirs; ops_pg_found=1; fi
    fi
    if ops_unit_active postgresql.service; then ops_pg_found=1; fi
    if ops_process_running postgres || ops_process_running postmaster; then ops_pg_found=1; fi
    if ops_port_listening 5432; then ops_pg_found=1; fi
    [ "$ops_pg_found" -eq 1 ] || return 1
    OPS_PG_VERSION=${OPS_PG_VERSION:-unknown}
    return 0
}

# ops_detect_mysql: MySQL or MariaDB on this host (unsupported by CraftCloud).
ops_detect_mysql() {
    ops_port_listening 3306 && return 0
    ops_process_running mysqld && return 0
    ops_process_running mariadbd && return 0
    for ops_unit in mysql.service mariadb.service mysqld.service; do
        ops_unit_active "$ops_unit" && return 0
    done
    return 1
}

# ops_detect_proxy: sets OPS_PROXY_FOUND to nginx, apache, other (80/443 are
# taken by something else) or none.
ops_detect_proxy() {
    OPS_PROXY_FOUND=none
    ops_ports_taken=0
    if ops_port_listening "$1" || ops_port_listening "$2"; then ops_ports_taken=1; fi
    if ops_process_running nginx || ops_unit_active nginx.service; then
        OPS_PROXY_FOUND=nginx
    elif ops_process_running apache2 || ops_process_running httpd || ops_unit_active apache2.service || ops_unit_active httpd.service; then
        OPS_PROXY_FOUND=apache
    elif [ "$ops_ports_taken" -eq 1 ]; then
        OPS_PROXY_FOUND=other
    fi
}

# ops_ask <question> <default y|n>: yes (0) or no (1); the default applies
# without a terminal or on an empty answer.
ops_ask() {
    if [ "${NON_INTERACTIVE:-0}" -eq 1 ] || [ ! -t 0 ]; then
        [ "$2" = y ]
        return
    fi
    if [ "$2" = y ]; then ops_hint='[Y/n]'; else ops_hint='[y/N]'; fi
    printf '%s %s ' "$1" "$ops_hint"
    IFS= read -r ops_answer || ops_answer=
    case $ops_answer in
        [Yy]|[Yy][Ee][Ss]) return 0 ;;
        [Nn]|[Nn][Oo]) return 1 ;;
        *) [ "$2" = y ] ;;
    esac
}

# --- Database choice ----------------------------------------------------------

# ops_choose_db: decides bundled or external from DB_MODE (auto|bundled|external)
# and DB_URL_FILE; sets DB_MODE to bundled or external. Never touches the host.
ops_choose_db() {
    case $DB_MODE in auto|bundled|external) ;; *) fail '--db must be auto, bundled or external' ;; esac
    if [ "$DB_MODE" = bundled ]; then
        [ -z "$DB_URL_FILE" ] || fail '--database-url-file belongs to --db external'
        return 0
    fi
    if [ -n "$DB_URL_FILE" ]; then
        DB_MODE=external
        return 0
    fi
    [ "$DB_MODE" != external ] || fail '--db external needs --database-url-file FILE (a PostgreSQL URL without user and password)'
    # auto, no database file given: look at the host.
    if ops_detect_mysql; then
        printf '%s\n' 'craftcloud-install: MySQL or MariaDB was detected on this host. CraftCloud does not support MySQL, MariaDB, SQLite or SQL Server; it needs PostgreSQL 16, 17 or 18. The bundled PostgreSQL container can be used instead.'
    fi
    if ops_detect_postgres; then
        case $OPS_PG_VERSION in
            unknown) ops_pg_text='a PostgreSQL server (version not determined)' ;;
            *) ops_pg_text="PostgreSQL $OPS_PG_VERSION" ;;
        esac
        printf '%s\n' "craftcloud-install: $ops_pg_text was detected on this host."
        if [ "${NON_INTERACTIVE:-0}" -eq 1 ] || [ ! -t 0 ]; then
            printf '%s\n' 'craftcloud-install: it is not used without an explicit choice. To use it, run again with --db external --database-url-file FILE. Continuing with the bundled PostgreSQL container.'
        elif ops_ask 'Use the existing PostgreSQL server instead of the bundled container?' n; then
            fail 'to use it, create a file with the connection URL without user and password (postgres://HOST:5432/DATABASE?sslmode=verify-full) and run again with --db external --database-url-file FILE'
        fi
    fi
    DB_MODE=bundled
}

# ops_check_db_name <name>: [a-z_][a-z0-9_]{0,62} and not a reserved word. The
# generated SQL also quotes the name with format('%I').
ops_check_db_name() {
    case $1 in
        ''|[!a-z_]*|*[!a-z0-9_]*) fail 'the database name must match [a-z_][a-z0-9_]{0,62} (lower case letters, digits, underscore)' ;;
    esac
    [ "${#1}" -le 63 ] || fail 'the database name is longer than 63 characters'
    # shellcheck disable=SC2194 # the word list is the subject on purpose
    case " all analyse analyze and any array as asc asymmetric both case cast check collate column constraint create current_catalog current_date current_role current_time current_timestamp current_user default deferrable desc distinct do else end except false fetch for foreign from grant group having in initially intersect into lateral leading limit localtime localtimestamp not null offset on only or order placing primary references returning select session_user some symmetric system_user table then to trailing true union unique user using variadic when where window with postgres template0 template1 " in
        *" $1 "*) fail "the database name $1 is a reserved word or a system database" ;;
    esac
}

# ops_parse_db_url <file> <config-dir>: reads the connection target of an
# external database. The file holds one line: postgres://HOST[:PORT]/DATABASE[?QUERY]
# with no user name and no password; the installer creates the two roles and
# their passwords. Sets DB_TARGET (host:port/database for the containers),
# DB_NAME, DB_QUERY (? and the TLS query, possibly empty), DB_LOCAL (1 for a
# database on this host) and DB_CA_SOURCE (a certificate file to copy, or
# empty). Prints no part of the file, not even the host name.
ops_parse_db_url() {
    [ -f "$1" ] && [ ! -L "$1" ] || fail 'the database URL file does not exist or is a symbolic link'
    ops_url=$(head -n 1 "$1" | tr -d '\r')
    [ "$(wc -l <"$1")" -le 1 ] || fail 'the database URL file must contain a single line'
    case $ops_url in
        postgres://*) ops_rest=${ops_url#postgres://} ;;
        postgresql://*) ops_rest=${ops_url#postgresql://} ;;
        *) fail 'the database URL must start with postgres:// (only PostgreSQL is supported)' ;;
    esac
    [ "${#ops_url}" -le 512 ] || fail 'the database URL is too long'
    case $ops_rest in *[[:space:]]*|'') fail 'the database URL contains white space' ;; esac
    case ${ops_rest%%/*} in *@*) fail 'the database URL must not contain a user name or password; the installer creates the roles' ;; esac
    ops_authority=${ops_rest%%/*}
    case $ops_rest in */*) ops_path=${ops_rest#*/} ;; *) fail 'the database URL needs a database name (postgres://HOST:5432/DATABASE)' ;; esac
    ops_database=${ops_path%%\?*}
    case $ops_path in *\?*) ops_query=${ops_path#*\?} ;; *) ops_query= ;; esac
    ops_check_db_name "$ops_database"
    case $ops_authority in
        \[*\]:*) ops_host=${ops_authority%%\]*}]; ops_port=${ops_authority##*\]:} ;;
        \[*\]) ops_host=$ops_authority; ops_port=5432 ;;
        *:*) ops_host=${ops_authority%%:*}; ops_port=${ops_authority#*:} ;;
        *) ops_host=$ops_authority; ops_port=5432 ;;
    esac
    case $ops_host in ''|*[!]A-Za-z0-9.:_[-]*) fail 'the database host name is not valid' ;; esac
    case $ops_host in /*|*%*) fail 'Unix sockets are not supported with containers; use a TCP address' ;; esac
    case $ops_port in ''|*[!0-9]*) fail 'the database port must be numeric' ;; esac
    [ "$ops_port" -ge 1 ] && [ "$ops_port" -le 65535 ] || fail 'the database port is out of range'
    # Only the TLS settings are accepted in the query.
    ops_sslmode=
    DB_CA_SOURCE=
    ops_new_query=
    ops_old_ifs=$IFS
    IFS='&'
    # shellcheck disable=SC2086 # the query is intentionally split at '&'
    set -- $ops_query
    IFS=$ops_old_ifs
    for ops_pair in "$@"; do
        case $ops_pair in
            sslmode=*)
                ops_sslmode=${ops_pair#sslmode=}
                case $ops_sslmode in disable|allow|prefer|require|verify-ca|verify-full) ;; *) fail 'unknown sslmode in the database URL' ;; esac
                ops_new_query="$ops_new_query&sslmode=$ops_sslmode"
                ;;
            sslrootcert=*)
                DB_CA_SOURCE=${ops_pair#sslrootcert=}
                case $DB_CA_SOURCE in /*) ;; *) fail 'sslrootcert must be an absolute path' ;; esac
                case $DB_CA_SOURCE in *[!A-Za-z0-9/._-]*) fail 'sslrootcert contains unsupported characters' ;; esac
                [ -f "$DB_CA_SOURCE" ] && [ -r "$DB_CA_SOURCE" ] || fail 'the sslrootcert file does not exist or is not readable'
                ops_new_query="$ops_new_query&sslrootcert=/run/db-ca/ca.pem"
                ;;
            '') ;;
            *) fail 'the database URL query may contain only sslmode and sslrootcert' ;;
        esac
    done
    ops_host=$(printf '%s' "$ops_host" | tr 'A-Z' 'a-z')
    DB_HOST_ORIG=$ops_host
    DB_PORT_ORIG=$ops_port
    DB_SSLMODE=$ops_sslmode
    case $ops_host in
        localhost|127.*|\[::1\]|host.docker.internal) ops_local=1 ;;
        *) ops_local=0 ;;
    esac
    if [ "$ops_local" -eq 1 ]; then
        if [ "$ops_sslmode" = verify-full ]; then
            # The certificate is checked against the name the container connects to.
            # A loopback name would have to be rewritten to the Docker host name,
            # which the certificate does not contain, so only that name is allowed.
            [ "$ops_host" = host.docker.internal ] || fail 'sslmode=verify-full with localhost or a loopback address cannot work from a container (the name is rewritten to host.docker.internal); use host.docker.internal as the host name, issue the certificate for it, and put it in the URL'
        elif [ "${DB_ALLOW_INSECURE_LOCAL:-0}" -ne 1 ]; then
            fail 'a database on this host without sslmode=verify-full needs --db-allow-insecure-local'
        fi
        # The containers reach the Docker host through its gateway name (extra_hosts
        # host.docker.internal:host-gateway in compose.external-db.yaml).
        ops_host=host.docker.internal
    else
        [ "$ops_sslmode" = verify-full ] || fail 'a database on another host needs sslmode=verify-full in the URL (and sslrootcert for a private CA)'
        [ "${DB_ALLOW_INSECURE_LOCAL:-0}" -ne 1 ] || fail '--db-allow-insecure-local applies only to a database on this host, not to a database on another host'
    fi
    DB_TARGET=$ops_host:$ops_port/$ops_database
    DB_NAME=$ops_database
    DB_QUERY=
    [ -z "$ops_new_query" ] || DB_QUERY="?${ops_new_query#&}"
    DB_LOCAL=$ops_local
}

# ops_write_role_sql <file> <database> <owner-password> <runtime-password>
# Writes the SQL that creates the two application roles and the database
# (idempotent, same rules as deploy/compose/db-init.sh). The file contains the
# role passwords: mode 0600, never printed. Before the passwords are set, a
# superuser session switches statement logging off for itself so that the
# ALTER ROLE statements (which carry the passwords) do not reach the server log;
# a session that may not do that prints a warning and continues. The password is
# sent as text, as ALTER ROLE ... PASSWORD does; the client library (libpq) is
# not asked to hash it, so use TLS (the installer requires it for remote servers).
ops_write_role_sql() {
    ops_check_db_name "$2"
    ops_sql_tmp=$1.tmp.$$
    {
        printf '%s\n' '-- CraftCloud roles for an existing PostgreSQL server. Run as an administrator:'
        printf '%s\n' '--   psql -v ON_ERROR_STOP=1 -f <this file> postgres'
        printf '%s\n' '-- The file contains the generated role passwords: keep it private and delete it afterwards.'
        printf '%s\n' "SELECT rolsuper AS ops_super FROM pg_roles WHERE rolname = current_user \\gset"
        printf '%s\n' '\if :ops_super'
        printf '%s\n' "SET log_statement = 'none';"
        printf '%s\n' "SET log_min_error_statement = 'panic';"
        printf '%s\n' 'SET log_min_duration_statement = -1;'
        printf '%s\n' '\else'
        printf '%s\n' "\\echo 'warning: this session is not a superuser and cannot switch statement logging off; the role passwords may appear in the server log if log_statement or log_min_error_statement record them. Change the passwords afterwards or turn logging down.'"
        printf '%s\n' '\endif'
        printf '%s\n' "SELECT 'CREATE ROLE craftcloud_owner LOGIN' WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'craftcloud_owner')\\gexec"
        printf '%s\n' "SELECT 'CREATE ROLE craftcloud_runtime LOGIN' WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'craftcloud_runtime')\\gexec"
        printf '%s\n' "ALTER ROLE craftcloud_owner   LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD '$3';"
        printf '%s\n' "ALTER ROLE craftcloud_runtime LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD '$4';"
        printf '%s\n' "\\set dbname '$2'"
        printf '%s\n' "SELECT format('CREATE DATABASE %I OWNER craftcloud_owner', :'dbname') WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = :'dbname') \\gexec"
        printf '%s\n' "SELECT format('ALTER DATABASE %I OWNER TO craftcloud_owner', :'dbname') \\gexec"
        printf '%s\n' "SELECT format('REVOKE ALL ON DATABASE %I FROM PUBLIC', :'dbname') \\gexec"
        printf '%s\n' "SELECT format('GRANT CONNECT ON DATABASE %I TO craftcloud_runtime', :'dbname') \\gexec"
        printf '%s\n' "\\connect $2"
        printf '%s\n' 'ALTER SCHEMA public OWNER TO craftcloud_owner;'
        printf '%s\n' 'REVOKE CREATE ON SCHEMA public FROM PUBLIC;'
        printf '%s\n' 'GRANT USAGE ON SCHEMA public TO craftcloud_runtime;'
    } >"$ops_sql_tmp"
    chmod 0600 "$ops_sql_tmp"
    mv -f "$ops_sql_tmp" "$1"
}

# ops_host_class <host>: prints loopback or remote.
ops_host_class() {
    case $1 in
        localhost|127.*|\[::1\]|host.docker.internal) printf '%s\n' loopback ;;
        *) printf '%s\n' remote ;;
    esac
}

# ops_check_admin_url <admin-url-file>: the administrator URL follows the same
# rules as the application URL. Same host and port as --database-url-file, a
# remote server needs sslmode=verify-full, a server on this host needs it or
# --db-allow-insecure-local. It needs a user and a password; the password may
# not contain @ : / ? # % \ or white space (it is not percent-decoded). Prints
# nothing from the file.
ops_check_admin_url() {
    [ -f "$1" ] && [ ! -L "$1" ] || fail 'the administrator URL file does not exist or is a symbolic link'
    [ "$(wc -l <"$1")" -le 1 ] || fail 'the administrator URL file must contain a single line'
    ops_au=$(head -n 1 "$1" | tr -d '\r')
    case $ops_au in
        postgres://*) ops_ar=${ops_au#postgres://} ;;
        postgresql://*) ops_ar=${ops_au#postgresql://} ;;
        *) fail 'the administrator URL must start with postgres://' ;;
    esac
    case $ops_ar in *[[:space:]]*|'') fail 'the administrator URL contains white space' ;; esac
    case $ops_ar in *@*@*) fail 'the administrator password may not contain @ (it is not percent-decoded)' ;; esac
    case $ops_ar in *@*) ;; *) fail 'the administrator URL needs a user name and a password (postgres://USER:PASSWORD@HOST:PORT/postgres)' ;; esac
    ops_acred=${ops_ar%%@*}
    ops_aafter=${ops_ar#*@}
    case $ops_acred in *:*) ;; *) fail 'the administrator URL needs a password' ;; esac
    ops_auser=${ops_acred%%:*}
    ops_apass=${ops_acred#*:}
    case $ops_auser in ''|*[!A-Za-z0-9._-]*) fail 'the administrator user name may contain only letters, digits, dot, underscore and hyphen' ;; esac
    case $ops_apass in ''|*[:/?#%\\]*) fail 'the administrator password may not contain : / ? # % or a backslash (it is not percent-decoded)' ;; esac
    ops_aauth=${ops_aafter%%/*}
    case $ops_aafter in */*) ops_apath=${ops_aafter#*/} ;; *) fail 'the administrator URL needs a database name' ;; esac
    case $ops_aauth in
        \[*\]:*) ops_ahost=${ops_aauth%%\]*}]; ops_aport=${ops_aauth##*\]:} ;;
        \[*\]) ops_ahost=$ops_aauth; ops_aport=5432 ;;
        *:*) ops_ahost=${ops_aauth%%:*}; ops_aport=${ops_aauth#*:} ;;
        *) ops_ahost=$ops_aauth; ops_aport=5432 ;;
    esac
    ops_ahost=$(printf '%s' "$ops_ahost" | tr 'A-Z' 'a-z')
    case $ops_ahost in ''|*[!]A-Za-z0-9.:_[-]*) fail 'the administrator host name is not valid' ;; esac
    case $ops_aport in ''|*[!0-9]*) fail 'the administrator port must be numeric' ;; esac
    ops_aq=
    case $ops_apath in *\?*) ops_aq=${ops_apath#*\?} ;; esac
    ops_asslmode=
    ops_old_ifs=$IFS
    IFS='&'
    # shellcheck disable=SC2086 # the query is intentionally split at '&'
    set -- $ops_aq
    IFS=$ops_old_ifs
    for ops_apair in "$@"; do
        case $ops_apair in
            sslmode=*) ops_asslmode=${ops_apair#sslmode=} ;;
            sslrootcert=*) ;;
            '') ;;
            *) fail 'the administrator URL query may contain only sslmode and sslrootcert' ;;
        esac
    done
    # Same server as the application URL.
    if [ "$(ops_host_class "$ops_ahost")" = loopback ] && [ "$(ops_host_class "${DB_HOST_ORIG:-}")" = loopback ]; then
        :
    else
        [ "$ops_ahost" = "${DB_HOST_ORIG:-}" ] || fail 'the administrator URL must name the same host as --database-url-file'
    fi
    [ "$ops_aport" = "${DB_PORT_ORIG:-}" ] || fail 'the administrator URL must use the same port as --database-url-file'
    if [ "$(ops_host_class "$ops_ahost")" = loopback ]; then
        [ "$ops_asslmode" = verify-full ] || [ "${DB_ALLOW_INSECURE_LOCAL:-0}" -eq 1 ] || fail 'a database on this host needs sslmode=verify-full in the administrator URL, or --db-allow-insecure-local'
    else
        [ "$ops_asslmode" = verify-full ] || fail 'a database on another host needs sslmode=verify-full in the administrator URL'
    fi
}

# ops_admin_ca_args <admin-url-file>: `--mount` arguments for the private
# certificate authority named by sslrootcert in the administrator URL, if any.
# The path is used for the mount only and never printed.
ops_admin_ca_args() {
    ops_ca=$(sed -n 's/.*[?&]sslrootcert=\([^&[:space:]]*\).*/\1/p' "$1" | head -n 1)
    case $ops_ca in
        '') ;;
        /*)
            case $ops_ca in *[!A-Za-z0-9/._-]*) fail 'sslrootcert in the administrator URL contains unsupported characters' ;; esac
            [ -f "$ops_ca" ] || fail 'the sslrootcert file of the administrator URL does not exist'
            printf '%s\n' "--mount" "type=bind,source=$ops_ca,target=$ops_ca,readonly"
            ;;
        *) fail 'sslrootcert in the administrator URL must be an absolute path' ;;
    esac
}

# The script that runs INSIDE the container. It splits the administrator URL into
# the libpq variables PGHOST, PGPORT, PGUSER, PGDATABASE, PGSSLMODE and
# PGSSLROOTCERT and writes the password to a mode 0600 PGPASSFILE on a tmpfs, so
# that neither the URL nor the password is ever an argument of a process (only
# shell builtins touch it). The script itself contains no secret.
OPS_ADMIN_ENV_SCRIPT='set -eu
umask 077
u=$(cat /run/admin-url)
rest=${u#*://}
cred=${rest%%@*}
after=${rest#*@}
PGUSER=${cred%%:*}
pw=${cred#*:}
hostport=${after%%/*}
pathq=${after#*/}
PGDATABASE=${pathq%%\?*}
q=
case $pathq in *\?*) q=${pathq#*\?} ;; esac
case $hostport in
    \[*\]:*) h=${hostport%%\]*}; PGHOST=${h#\[}; PGPORT=${hostport##*\]:} ;;
    \[*\]) h=${hostport%%\]*}; PGHOST=${h#\[}; PGPORT=5432 ;;
    *:*) PGHOST=${hostport%%:*}; PGPORT=${hostport#*:} ;;
    *) PGHOST=$hostport; PGPORT=5432 ;;
esac
export PGUSER PGDATABASE PGHOST PGPORT
IFS="&"
for pair in $q; do
    case $pair in
        sslmode=*) PGSSLMODE=${pair#sslmode=}; export PGSSLMODE ;;
        sslrootcert=*) PGSSLROOTCERT=${pair#sslrootcert=}; export PGSSLROOTCERT ;;
    esac
done
unset IFS
printf "*:*:*:*:%s\n" "$pw" >/run/pw/pgpass
PGPASSFILE=/run/pw/pgpass
export PGPASSFILE
unset u rest cred after pw q
trap "rm -f /run/pw/pgpass" EXIT
'

# ops_run_admin_sql <admin-url-file> <sql-file> <postgres-image>: runs the role
# SQL with the administrator URL the operator gave, inside a container of the
# pinned PostgreSQL image (no psql needed on the host). The URL file is mounted,
# never passed on a command line; the password reaches psql only through a
# PGPASSFILE on a tmpfs. Warnings of the SQL file are shown, nothing else.
ops_run_admin_sql() {
    ops_check_admin_url "$1"
    ops_ca_args=$(ops_admin_ca_args "$1")
    ops_out=$(
        # shellcheck disable=SC2086 # the mount arguments are plain paths and are intentionally split
        docker run --rm -i --network host --add-host host.docker.internal:host-gateway --tmpfs /run/pw:mode=0700 $ops_ca_args \
            --mount "type=bind,source=$1,target=/run/admin-url,readonly" \
            --mount "type=bind,source=$2,target=/run/roles.sql,readonly" \
            --entrypoint /bin/sh "$3" -c "$OPS_ADMIN_ENV_SCRIPT"'psql --no-psqlrc -v ON_ERROR_STOP=1 --quiet -f /run/roles.sql'
    ) || fail 'running the role setup with the administrator URL failed (host, credentials or TLS); no detail is printed on purpose'
    printf '%s\n' "$ops_out" | grep '^warning:' | sed 's/^/craftcloud-install: /' >&2 || true
}

# ops_admin_db_major <admin-url-file> <postgres-image>: major version of the
# server behind the administrator URL, printed on stdout.
ops_admin_db_major() {
    ops_check_admin_url "$1"
    ops_ca_args=$(ops_admin_ca_args "$1")
    # shellcheck disable=SC2086 # the mount arguments are plain paths and are intentionally split
    docker run --rm --network host --add-host host.docker.internal:host-gateway --tmpfs /run/pw:mode=0700 $ops_ca_args \
        --mount "type=bind,source=$1,target=/run/admin-url,readonly" \
        --entrypoint /bin/sh "$2" -c "$OPS_ADMIN_ENV_SCRIPT"'psql --no-psqlrc -At -c "SHOW server_version_num"' 2>/dev/null \
        | awk '{ print int($1 / 10000) }'
}

# --- Proxy choice -------------------------------------------------------------

# ops_choose_proxy: decides caddy, nginx, apache or none from PROXY_MODE and
# the host. Sets PROXY_MODE. HTTP_PORT and HTTPS_PORT are the public ports.
ops_choose_proxy() {
    case $PROXY_MODE in auto|caddy|nginx|apache|none) ;; *) fail '--proxy must be auto, caddy, nginx, apache or none' ;; esac
    if [ "$PROXY_MODE" != auto ]; then return 0; fi
    ops_detect_proxy "$HTTP_PORT" "$HTTPS_PORT"
    case $OPS_PROXY_FOUND in
        nginx|apache)
            printf '%s\n' "craftcloud-install: a running $OPS_PROXY_FOUND web server was detected. CraftCloud will use it as reverse proxy instead of the bundled Caddy; only configuration templates are written unless --proxy-install is given."
            PROXY_MODE=$OPS_PROXY_FOUND
            ;;
        other)
            fail "port $HTTP_PORT or $HTTPS_PORT is used by a service that is not recognized as nginx or Apache. Choose: --proxy none (your own proxy, templates only), --proxy nginx or --proxy apache, or other public ports with --http-port and --https-port"
            ;;
        *) PROXY_MODE=caddy ;;
    esac
}

# --- Proxy templates ----------------------------------------------------------

OPS_DEFAULT_MAX_FILE_BYTES=5368709120
OPS_BODY_RESERVE_BYTES=16777216

# ops_render_proxy_template <template> <output> <domain> <upstream-port> <figcraft-port>
#   <http-port> <https-port> <tls-cert> <tls-key> <max-file-bytes>
ops_render_proxy_template() {
    ops_max=$((${10} + OPS_BODY_RESERVE_BYTES))
    ops_apache_limit=$ops_max
    # Apache cannot limit above 2 GiB - 1; 0 means no limit there and CraftCloud enforces its own.
    [ "$ops_max" -le 2147483647 ] || ops_apache_limit=0
    ops_suffix=
    [ "$7" = 443 ] || ops_suffix=:$7
    # Apache needs a Listen line for a port that ports.conf does not already open.
    ops_listen_http='# Port 80 is opened by the ports.conf of your Apache.'
    ops_listen_https='# Port 443 is opened by the ports.conf of your Apache.'
    [ "$6" = 80 ] || ops_listen_http="Listen $6"
    [ "$7" = 443 ] || ops_listen_https="Listen $7"
    # Debian and Ubuntu define APACHE_LOG_DIR; other layouts use the logs directory of the server root.
    ops_error_log='logs/craftcloud-error.log'
    [ -d "${CRAFTCLOUD_APACHE_SITES_AVAILABLE:-/etc/apache2/sites-available}" ] && ops_error_log='${APACHE_LOG_DIR}/craftcloud-error.log'
    ops_render_tmp=$2.tmp.$$
    sed \
        -e "s|@DOMAIN@|$3|g" \
        -e "s|@UPSTREAM_PORT@|$4|g" \
        -e "s|@FIGCRAFT_PORT@|$5|g" \
        -e "s|@HTTP_PORT@|$6|g" \
        -e "s|@HTTPS_PORT@|$7|g" \
        -e "s|@PUBLIC_PORT_SUFFIX@|$ops_suffix|g" \
        -e "s|@TLS_CERT@|$8|g" \
        -e "s|@TLS_KEY@|$9|g" \
        -e "s|@APACHE_ERROR_LOG@|$ops_error_log|g" \
        -e "s|@APACHE_LISTEN_HTTP@|$ops_listen_http|g" \
        -e "s|@APACHE_LISTEN_HTTPS@|$ops_listen_https|g" \
        -e "s|@MAX_BODY@|$ops_max|g" \
        -e "s|@APACHE_LIMIT@|$ops_apache_limit|g" \
        "$1" >"$ops_render_tmp"
    if grep -q '@[A-Z_]*@' "$ops_render_tmp"; then
        rm -f -- "$ops_render_tmp"
        fail "proxy template $1 has an unreplaced placeholder"
    fi
    chmod 0644 "$ops_render_tmp"
    mv -f "$ops_render_tmp" "$2"
}

# ops_certbot_hint <nginx|apache>: Debian/Ubuntu package names for the hint.
ops_certbot_hint() {
    case $1 in
        nginx) printf '%s\n' 'apt install certbot python3-certbot-nginx' ;;
        apache) printf '%s\n' 'apt install certbot python3-certbot-apache' ;;
    esac
}

# ops_apache_missing_modules <apachectl>: names of required modules that are not loaded.
ops_apache_missing_modules() {
    ops_loaded=$("$1" -M 2>/dev/null) || ops_loaded=
    for ops_module in proxy_module proxy_http_module proxy_wstunnel_module headers_module rewrite_module ssl_module; do
        case $ops_loaded in *"$ops_module"*) ;; *) printf '%s ' "${ops_module%_module}" ;; esac
    done
}

# ops_apache_sites_available / ops_apache_sites_enabled: Debian layout directories.
ops_apache_sites_available() { printf '%s\n' "${CRAFTCLOUD_APACHE_SITES_AVAILABLE:-/etc/apache2/sites-available}"; }
ops_apache_sites_enabled() { printf '%s\n' "${CRAFTCLOUD_APACHE_SITES_ENABLED:-/etc/apache2/sites-enabled}"; }

# ops_proxy_reload <nginx|apache>: reloads the web server (CRAFTCLOUD_RELOAD_CMD is a test hook).
ops_proxy_reload() {
    if [ -n "${CRAFTCLOUD_RELOAD_CMD:-}" ]; then
        # shellcheck disable=SC2086 # test hook
        $CRAFTCLOUD_RELOAD_CMD
    elif [ "$1" = nginx ]; then
        if ops_unit_active nginx.service; then systemctl reload nginx
        else "${CRAFTCLOUD_NGINX_BIN:-nginx}" -s reload; fi
    else
        if ops_unit_active apache2.service; then systemctl reload apache2
        elif ops_unit_active httpd.service; then systemctl reload httpd
        else "${CRAFTCLOUD_APACHECTL:-apachectl}" graceful; fi
    fi
}

# ops_proxy_apply <nginx|apache> <config-file> <rendered-file> <backup-dir>
# One guarded change: back up the previous file, install the rendered one, for
# Apache on the Debian layout also enable the site (a2ensite), test the whole
# configuration, and only then reload. Any failure (test or reload) undoes
# exactly what this call did (restores the previous file or removes the new one,
# a2dissite for a site it enabled), checks that the restored configuration still
# passes the test, and fails. Uses CRAFTCLOUD_NGINX_BIN / CRAFTCLOUD_APACHECTL.
ops_proxy_apply() {
    ops_kind=$1; ops_target=$2; ops_new=$3; ops_backup=$4
    mkdir -p "$ops_backup"
    chmod 0700 "$ops_backup"
    ops_had=0
    ops_saved=
    ops_enabled_by_us=0
    if [ -e "$ops_target" ]; then
        ops_had=1
        ops_saved=$ops_backup/$(basename -- "$ops_target").$(date -u +%Y%m%dT%H%M%SZ)
        cp -p -- "$ops_target" "$ops_saved"
        printf '%s\n' "craftcloud-install: saved the previous $ops_target as $ops_saved"
    fi
    cp -- "$ops_new" "$ops_target"
    chmod 0644 "$ops_target"
    case $ops_kind in
        nginx) ops_test="${CRAFTCLOUD_NGINX_BIN:-nginx} -t" ;;
        apache) ops_test="${CRAFTCLOUD_APACHECTL:-apachectl} configtest" ;;
    esac
    ops_apply_failure=
    if [ "$ops_kind" = apache ] && [ "$ops_target" = "$(ops_apache_sites_available)/craftcloud.conf" ] && [ ! -e "$(ops_apache_sites_enabled)/craftcloud.conf" ]; then
        if a2ensite craftcloud >/dev/null 2>&1; then ops_enabled_by_us=1; else ops_apply_failure='a2ensite craftcloud failed'; fi
    fi
    if [ -z "$ops_apply_failure" ]; then
        # shellcheck disable=SC2086 # the command and its argument are intentionally split
        $ops_test >/dev/null 2>&1 || ops_apply_failure="the $ops_kind configuration test failed with the CraftCloud configuration"
    fi
    if [ -z "$ops_apply_failure" ]; then
        ops_proxy_reload "$ops_kind" >/dev/null 2>&1 || ops_apply_failure="reloading $ops_kind failed"
    fi
    if [ -n "$ops_apply_failure" ]; then
        if [ "$ops_enabled_by_us" -eq 1 ]; then a2dissite craftcloud >/dev/null 2>&1 || true; fi
        if [ "$ops_had" -eq 1 ]; then cp -p -- "$ops_saved" "$ops_target"; else rm -f -- "$ops_target"; fi
        # shellcheck disable=SC2086
        if $ops_test >/dev/null 2>&1; then
            ops_restored='the restored configuration passes the test'
            # A reload that failed may have left the server on the new file: reload the restored state.
            ops_proxy_reload "$ops_kind" >/dev/null 2>&1 || ops_restored="$ops_restored, but reloading it failed: check the web server"
        else
            ops_restored="the restored configuration does NOT pass the test either: check the web server yourself (run '$ops_test')"
        fi
        fail "$ops_apply_failure; the change was undone ($ops_restored) (run '$ops_test' to see the error)"
    fi
}

# --- Image archive ------------------------------------------------------------

# ops_load_image_archive <archive> <version> <image-reference>
# Loads a prebuilt image archive of this release after checking the SHA-256
# entry in SHA256SUMS beside it (verify_archive_checksum of the caller), the
# file name (version and machine architecture) and the loaded image, and tags
# it with the reference the installation uses. Needs docker.
ops_load_image_archive() {
    [ -f "$1" ] && [ ! -L "$1" ] || fail 'image archive not found'
    ops_arch=$(docker version --format '{{.Server.Arch}}' 2>/dev/null) || fail 'cannot determine the architecture of the Docker engine'
    ops_name=$(basename -- "$1")
    [ "$ops_name" = "craftcloud-$2-image-$ops_arch.tar.zst" ] || fail "the image archive for this release and this machine must be named craftcloud-$2-image-$ops_arch.tar.zst"
    verify_archive_checksum "$1"
    printf '%s\n' "craftcloud-install: loading the prebuilt image $ops_name"
    if command -v zstd >/dev/null 2>&1; then
        zstd -dc -- "$1" | docker load >/dev/null || fail 'cannot load the image archive'
    else
        docker load -i "$1" >/dev/null || fail 'cannot load the image archive (install the zstd package if Docker cannot read .zst files)'
    fi
    ops_loaded=craftcloud:$(image_tag "$2")
    docker image inspect "$ops_loaded" >/dev/null 2>&1 || fail "the image archive did not provide $ops_loaded"
    ops_loaded_arch=$(docker image inspect --format '{{.Architecture}}' "$ops_loaded" 2>/dev/null || true)
    [ "$ops_loaded_arch" = "$ops_arch" ] || fail "the image archive is for $ops_loaded_arch, this machine is $ops_arch"
    [ "$ops_loaded" = "$3" ] || docker tag "$ops_loaded" "$3" || fail 'cannot tag the loaded image'
}

# --- Proxy templates and installation -----------------------------------------

# ops_proxy_write_templates <compose-dir>: renders both templates for this
# installation into $CONFIG_DIR/proxy. Uses DOMAIN, HTTP_PORT, HTTPS_PORT,
# LOCAL_PORT, LOCAL_FIGCRAFT_PORT, TLS_CERT and TLS_KEY (set by the caller).
ops_proxy_write_templates() {
    ops_tpl_dir=$1/../proxy
    [ -f "$ops_tpl_dir/craftcloud.nginx.conf" ] && [ -f "$ops_tpl_dir/craftcloud.apache.conf" ] || fail 'this release has no proxy templates (deploy/proxy)'
    mkdir -p "$CONFIG_DIR/proxy"
    chmod 0700 "$CONFIG_DIR/proxy"
    ops_max_bytes=$(ops_env_get "$env_file" CRAFTCLOUD_MAX_FILE_SIZE_BYTES)
    case $ops_max_bytes in ''|*[!0-9]*) ops_max_bytes=$OPS_DEFAULT_MAX_FILE_BYTES ;; esac
    for ops_kind in nginx apache; do
        ops_render_proxy_template "$ops_tpl_dir/craftcloud.$ops_kind.conf" "$CONFIG_DIR/proxy/craftcloud.$ops_kind.conf" \
            "$DOMAIN" "$LOCAL_PORT" "$LOCAL_FIGCRAFT_PORT" "$HTTP_PORT" "$HTTPS_PORT" "$TLS_CERT" "$TLS_KEY" "$ops_max_bytes"
    done
}

# ops_apache_target: path of the Apache site file (Debian layout or conf.d).
ops_apache_target() {
    if [ -n "${CRAFTCLOUD_APACHE_CONF_FILE:-}" ]; then printf '%s\n' "$CRAFTCLOUD_APACHE_CONF_FILE"
    elif [ -d "$(ops_apache_sites_available)" ]; then printf '%s\n' "$(ops_apache_sites_available)/craftcloud.conf"
    else printf '%s\n' /etc/httpd/conf.d/craftcloud.conf; fi
}

ops_nginx_target() {
    printf '%s\n' "${CRAFTCLOUD_NGINX_CONF_FILE:-/etc/nginx/conf.d/craftcloud.conf}"
}

# ops_http_only_config <nginx|apache> <file>: a minimal port-80 site that lets
# certbot answer the HTTP challenge before the TLS site exists.
ops_http_only_config() {
    case $1 in
        nginx)
            printf 'server {\n    listen %s;\n    listen [::]:%s;\n    server_name %s;\n    location / { return 404; }\n}\n' "$HTTP_PORT" "$HTTP_PORT" "$DOMAIN" >"$2"
            ;;
        apache)
            printf '<VirtualHost *:%s>\n    ServerName %s\n</VirtualHost>\n' "$HTTP_PORT" "$DOMAIN" >"$2"
            ;;
    esac
}

# ops_proxy_install: the --proxy-install flow for nginx and Apache. It changes
# the web server configuration only here: a backup first, a configuration test
# before the reload, the previous state restored when the test fails.
ops_proxy_install() {
    [ "$(id -u)" -eq 0 ] || fail 'run as root to install the proxy configuration'
    case $PROXY_MODE in
        nginx)
            command -v "${CRAFTCLOUD_NGINX_BIN:-nginx}" >/dev/null 2>&1 || fail 'nginx is not installed (apt install nginx)'
            ops_target=$(ops_nginx_target)
            ;;
        apache)
            command -v "${CRAFTCLOUD_APACHECTL:-apachectl}" >/dev/null 2>&1 || fail 'Apache is not installed (apt install apache2)'
            ops_missing=$(ops_apache_missing_modules "${CRAFTCLOUD_APACHECTL:-apachectl}")
            [ -z "$ops_missing" ] || fail "Apache modules missing: $ops_missing(on Debian and Ubuntu: a2enmod $ops_missing; the installer does not enable modules itself)"
            ops_target=$(ops_apache_target)
            ;;
        *) fail '--proxy-install applies to --proxy nginx or --proxy apache' ;;
    esac
    ops_backup=$CONFIG_DIR/proxy/backup
    if [ ! -f "$TLS_CERT" ] || [ ! -f "$TLS_KEY" ]; then
        [ "$CERTBOT" -eq 1 ] || fail "no TLS certificate at $TLS_CERT; give --proxy-cert and --proxy-key, or use --certbot to obtain one"
        command -v certbot >/dev/null 2>&1 || fail "certbot is not installed. On Debian and Ubuntu: $(ops_certbot_hint "$PROXY_MODE"). The installer does not install it."
        ops_http_only_config "$PROXY_MODE" "$CONFIG_DIR/proxy/http-only.conf"
        ops_proxy_apply "$PROXY_MODE" "$ops_target" "$CONFIG_DIR/proxy/http-only.conf" "$ops_backup"
        rm -f -- "$CONFIG_DIR/proxy/http-only.conf"
        ops_certbot_args="certonly --$PROXY_MODE -d $DOMAIN"
        if [ -n "$ACME_EMAIL_GIVEN" ]; then ops_certbot_args="$ops_certbot_args -m $ACME_EMAIL"; fi
        if [ "$NON_INTERACTIVE" -eq 1 ]; then
            [ -n "$ACME_EMAIL_GIVEN" ] || fail '--certbot with --non-interactive needs --acme-email (certbot is then run with --agree-tos)'
            ops_certbot_args="$ops_certbot_args --non-interactive --agree-tos"
        fi
        printf '%s\n' "craftcloud-install: running: certbot $ops_certbot_args"
        # shellcheck disable=SC2086 # the argument list is intentionally split
        certbot $ops_certbot_args || fail 'certbot failed; the HTTP-only site stays in place. Fix the cause (DNS, firewall on port 80) and run the installer again'
        [ -f "$TLS_CERT" ] && [ -f "$TLS_KEY" ] || fail "certbot did not create $TLS_CERT; use --proxy-cert and --proxy-key with the files it created"
    fi
    ops_proxy_apply "$PROXY_MODE" "$ops_target" "$CONFIG_DIR/proxy/craftcloud.$PROXY_MODE.conf" "$ops_backup"
    printf '%s\n' "craftcloud-install: $PROXY_MODE is configured with $ops_target and was reloaded after a successful test."
}

# ops_proxy_instructions: what the operator does when the installer did not
# install the configuration.
ops_proxy_instructions() {
    case $PROXY_MODE in
        nginx)
            printf '%s\n' "Reverse proxy nginx: the template is $CONFIG_DIR/proxy/craftcloud.nginx.conf (certificate: $TLS_CERT)."
            printf '%s\n' "  1. Get a certificate: certbot certonly --nginx -d $DOMAIN   (package: $(ops_certbot_hint nginx)), or use your own files and edit the two ssl_ lines."
            printf '%s\n' "  2. Copy the template: cp $CONFIG_DIR/proxy/craftcloud.nginx.conf /etc/nginx/conf.d/craftcloud.conf"
            printf '%s\n' '  3. Test and reload: nginx -t && systemctl reload nginx'
            printf '%s\n' '  (The installer can do this itself with --proxy-install, after a backup and a configuration test.)'
            ;;
        apache)
            printf '%s\n' "Reverse proxy Apache: the template is $CONFIG_DIR/proxy/craftcloud.apache.conf (certificate: $TLS_CERT)."
            printf '%s\n' '  1. Modules: a2enmod proxy proxy_http proxy_wstunnel headers rewrite ssl'
            printf '%s\n' "  2. Get a certificate: certbot certonly --apache -d $DOMAIN   (package: $(ops_certbot_hint apache)), or use your own files and edit the two SSLCertificate lines."
            printf '%s\n' "  3. Copy the template: cp $CONFIG_DIR/proxy/craftcloud.apache.conf /etc/apache2/sites-available/craftcloud.conf && a2ensite craftcloud"
            printf '%s\n' '  4. Test and reload: apachectl configtest && systemctl reload apache2'
            printf '%s\n' '  (The installer can do this itself with --proxy-install, after a backup and a configuration test.)'
            ;;
        none)
            printf '%s\n' "Own proxy: CraftCloud listens on 127.0.0.1:$LOCAL_PORT and the FigCraft server on 127.0.0.1:$LOCAL_FIGCRAFT_PORT (path /figcraft/)."
            printf '%s\n' "  Templates for nginx and Apache are in $CONFIG_DIR/proxy as a starting point."
            printf '%s\n' '  Your proxy must terminate TLS, set X-Forwarded-For to the real client address and X-Forwarded-Proto to https, and must not forward X-Forwarded-Host.'
            ;;
    esac
}
