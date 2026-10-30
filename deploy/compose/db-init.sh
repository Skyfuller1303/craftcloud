#!/bin/sh
case $- in *x*) set +x ;; esac
# One-shot role setup for the Compose stack (service `db-init`). Runs as the
# postgres user of the PostgreSQL image, reads the passwords from secret files
# and never prints them. Idempotent: every run re-applies the passwords and
# attributes, so it also rotates them.
#
# Roles (see docs/06-operations/deployment.md, "Audit-Log"):
#   craftcloud_owner    owns database, schema and tables; used only by `migrate`
#   craftcloud_runtime  used by the server; not a superuser, not a member of the
#                       owner role; `migrate` grants it data privileges only
set -eu

PGUSER=postgres
PGPASSWORD="$(cat /run/secrets/db_superuser_password)"
export PGUSER PGPASSWORD

# POSTGRES_PASSWORD_FILE only applies when the data directory is first
# initialised, so a changed superuser password in .env cannot log in here.
if ! psql --no-psqlrc --quiet --command 'SELECT 1' >/dev/null 2>&1; then
    cat >&2 <<'MSG'
db-init: cannot log in as the PostgreSQL superuser "postgres" (or the database is unreachable).
If CC_SECRET_DB_SUPERUSER_PASSWORD in .env was changed after the first start, the database
still has the old password: the image applies it only when the data directory is created.
Set the old value back in .env, change the password with ALTER ROLE first, then update .env.
See docs/06-operations/deployment.md, "Superuser-Passwort ändern".
MSG
    exit 1
fi

# psql reads the passwords itself (backticks) so they are not on a command line.
psql --no-psqlrc --set ON_ERROR_STOP=1 --quiet <<'SQL'
\set owner_password `cat /run/secrets/db_owner_password`
\set runtime_password `cat /run/secrets/db_runtime_password`

SELECT 'CREATE ROLE craftcloud_owner LOGIN'
 WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'craftcloud_owner')\gexec
SELECT 'CREATE ROLE craftcloud_runtime LOGIN'
 WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'craftcloud_runtime')\gexec

ALTER ROLE craftcloud_owner   LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD :'owner_password';
ALTER ROLE craftcloud_runtime LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD :'runtime_password';

-- The owner role must own what `migrate` creates; the runtime role must not
-- be (or be a member of) any owner.
ALTER DATABASE craftcloud OWNER TO craftcloud_owner;
ALTER SCHEMA public OWNER TO craftcloud_owner;
REVOKE ALL ON DATABASE craftcloud FROM PUBLIC;
GRANT CONNECT ON DATABASE craftcloud TO craftcloud_runtime;
REVOKE CREATE ON SCHEMA public FROM PUBLIC;
GRANT USAGE ON SCHEMA public TO craftcloud_runtime;
SQL

echo "database roles craftcloud_owner and craftcloud_runtime are ready"
