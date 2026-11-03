//! Forward-only migrations.
//!
//! Migrations are embedded from `migrations/` and applied only by an explicit
//! call to [`run`] (the `craftcloud-server migrate` subcommand). Services call
//! [`verify`] at startup and refuse to run against a database that is not
//! fully migrated or whose applied migrations differ from this build.

use std::borrow::Cow;
use std::collections::BTreeMap;

use sqlx::{
    PgConnection, PgPool, Row,
    migrate::{Migrate, MigrateError, Migrator},
};

use crate::{DbError, SchemaProblem};

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// A migration applied by [`run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// Migration version number.
    pub version: i64,
    /// Description derived from the file name.
    pub description: String,
}

/// Versions embedded in this build, ascending.
#[must_use]
pub fn embedded_versions() -> Vec<i64> {
    MIGRATOR.iter().map(|m| m.version).collect()
}

/// A migrator containing only embedded migrations up to and including
/// `version`. Intended for upgrade tests.
#[must_use]
pub fn migrator_up_to(version: i64) -> Migrator {
    let migrations: Vec<_> = MIGRATOR
        .iter()
        .filter(|m| m.version <= version)
        .cloned()
        .collect();
    Migrator {
        migrations: Cow::Owned(migrations),
        ..Migrator::DEFAULT
    }
}

/// Applies all pending embedded migrations under an advisory lock and returns
/// those applied by this call.
///
/// # Errors
///
/// [`DbError::Migration`] if an applied migration differs from the embedded
/// one, the database is newer than this build, or applying fails.
pub async fn run(pool: &PgPool) -> Result<Vec<Applied>, DbError> {
    run_with(pool, &MIGRATOR).await
}

/// Whether `name` is usable as the runtime role name: a plain unquoted
/// identifier (letters, digits, underscore; not starting with a digit).
#[must_use]
pub fn valid_role_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name.len() <= 63
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Why [`grant_runtime_role`] refused or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantError {
    /// Not a plain role name.
    InvalidName,
    /// The runtime role is the role that runs the migration: it would lose
    /// (or keep) rights that belong to the owner.
    SameAsOwner,
    /// The role could undo the audit guard: superuser, a member of the table
    /// owner or of the schema owner (which includes the database owner).
    Privileged(&'static str),
    /// The role does not exist.
    UnknownRole,
    /// After the grants the role lacks a needed privilege or still has a
    /// forbidden one; most often the migrating role does not own the tables
    /// (migrate once with the role that owns the schema, see the deployment guide).
    PrivilegesNotApplied(String),
    /// A database failure.
    Db(DbError),
}

impl std::fmt::Display for GrantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName => write!(f, "the runtime role name is not a plain identifier"),
            Self::SameAsOwner => write!(
                f,
                "the runtime role is the role that runs the migration; use a separate owner role for CRAFTCLOUD_MIGRATE_DATABASE_URL_FILE"
            ),
            Self::Privileged(why) => write!(f, "the runtime role must not be {why}"),
            Self::UnknownRole => write!(f, "the runtime role does not exist"),
            Self::PrivilegesNotApplied(what) => write!(
                f,
                "the grants did not take effect ({what}); the migrating role must own the tables and functions"
            ),
            Self::Db(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for GrantError {}

impl From<sqlx::Error> for GrantError {
    fn from(error: sqlx::Error) -> Self {
        Self::Db(DbError::from(error))
    }
}

/// Gives the runtime role the data privileges the server needs, and no more
/// (run by the owner role after [`run`]; idempotent, safe to repeat):
/// `SELECT`/`INSERT`/`UPDATE`/`DELETE` on the application tables, `SELECT`
/// only on the migration history, only `SELECT`/`INSERT`/`UPDATE` on
/// `audit_events` (never `DELETE` or `TRUNCATE`, so old events can only go
/// through `audit_purge`), sequence usage, and `EXECUTE` on `audit_purge`.
/// The role is not made owner of anything.
///
/// Nothing is changed for a role that could undo the guard (superuser, member
/// of the table or schema owner), for the migrating role itself, or for an
/// unknown role. Afterwards the privileges are checked one by one; a missing
/// needed or a present forbidden privilege fails the call.
///
/// # Errors
///
/// [`GrantError`].
pub async fn grant_runtime_role(pool: &PgPool, role: &str) -> Result<(), GrantError> {
    if !valid_role_name(role) {
        return Err(GrantError::InvalidName);
    }
    let me: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(pool)
        .await?;
    if me == role {
        return Err(GrantError::SameAsOwner);
    }
    let facts: Option<(bool, bool, bool)> = sqlx::query_as(
        "SELECT r.rolsuper, \
                COALESCE(pg_has_role(r.oid, c.relowner, 'MEMBER'), false), \
                COALESCE(pg_has_role(r.oid, n.nspowner, 'MEMBER'), false) \
         FROM pg_roles r \
         LEFT JOIN pg_class c ON c.oid = to_regclass('public.audit_events') \
         LEFT JOIN pg_namespace n ON n.nspname = 'public' \
         WHERE r.rolname = $1",
    )
    .bind(role)
    .fetch_optional(pool)
    .await?;
    match facts {
        None => return Err(GrantError::UnknownRole),
        Some((true, _, _)) => return Err(GrantError::Privileged("a superuser")),
        Some((_, true, _)) => {
            return Err(GrantError::Privileged(
                "a member of the role that owns the audit log table",
            ));
        }
        Some((_, _, true)) => {
            return Err(GrantError::Privileged(
                "a member of the role that owns the schema (or the database)",
            ));
        }
        Some(_) => {}
    }
    let quoted = format!("\"{role}\"");
    let statements = [
        format!("GRANT USAGE ON SCHEMA public TO {quoted}"),
        format!("GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO {quoted}"),
        format!("REVOKE INSERT, UPDATE, DELETE ON _sqlx_migrations FROM {quoted}"),
        format!("REVOKE DELETE, TRUNCATE ON audit_events FROM {quoted}"),
        format!("GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO {quoted}"),
        format!("GRANT EXECUTE ON FUNCTION audit_purge(timestamptz, integer) TO {quoted}"),
    ];
    let mut tx = pool.begin().await?;
    for statement in &statements {
        sqlx::query(statement).execute(&mut *tx).await?;
    }
    tx.commit().await?;

    // A GRANT by a role that does not own the objects only warns; check.
    let missing: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname || ':' || p \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         CROSS JOIN LATERAL unnest(CASE c.relname \
             WHEN '_sqlx_migrations' THEN ARRAY['SELECT'] \
             WHEN 'audit_events' THEN ARRAY['SELECT', 'INSERT', 'UPDATE'] \
             ELSE ARRAY['SELECT', 'INSERT', 'UPDATE', 'DELETE'] END) AS p \
         WHERE n.nspname = 'public' AND c.relkind = 'r' \
           AND NOT has_table_privilege($1, c.oid, p) \
         UNION ALL \
         SELECT 'audit_purge:EXECUTE' \
         WHERE NOT has_function_privilege($1, 'audit_purge(timestamptz, integer)', 'EXECUTE')",
    )
    .bind(role)
    .fetch_all(pool)
    .await?;
    if !missing.is_empty() {
        return Err(GrantError::PrivilegesNotApplied(format!(
            "missing {}",
            missing.join(", ")
        )));
    }
    let forbidden: Vec<String> = sqlx::query_scalar(
        "SELECT p FROM unnest(ARRAY['DELETE', 'TRUNCATE', 'TRIGGER', 'REFERENCES']) AS p \
         WHERE has_table_privilege($1, 'public.audit_events', p) \
         UNION ALL \
         SELECT 'MODIFY MIGRATIONS' FROM unnest(ARRAY['INSERT', 'UPDATE', 'DELETE']) AS p \
         WHERE has_table_privilege($1, 'public._sqlx_migrations', p)",
    )
    .bind(role)
    .fetch_all(pool)
    .await?;
    if !forbidden.is_empty() {
        return Err(GrantError::PrivilegesNotApplied(format!(
            "still allowed: {}",
            forbidden.join(", ")
        )));
    }
    Ok(())
}

/// Whether the append-only guard of `audit_events` is only advisory for the
/// connected role: it owns the table, is a member of the table owner or of
/// the schema owner (this includes the database owner), or is a superuser.
/// `None` if the table does not exist yet.
///
/// # Errors
///
/// Database failures only.
pub async fn audit_guard_is_advisory(pool: &PgPool) -> Result<Option<bool>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(pg_has_role(current_user, c.relowner, 'MEMBER'), false) \
             OR COALESCE(pg_has_role(current_user, n.nspowner, 'MEMBER'), false) \
             OR COALESCE((SELECT rolsuper FROM pg_roles WHERE rolname = current_user), false) \
         FROM pg_class c, pg_namespace n \
         WHERE c.oid = to_regclass('public.audit_events') AND n.nspname = 'public'",
    )
    .fetch_optional(pool)
    .await?)
}

/// Like [`run`] with an explicit migrator (for upgrade tests).
///
/// Holds sqlx's advisory lock for the whole run and releases it on every
/// path, so concurrent migrators are serialized and exactly one of them
/// applies (and reports) each migration.
///
/// # Errors
///
/// See [`run`].
pub async fn run_with(pool: &PgPool, migrator: &Migrator) -> Result<Vec<Applied>, DbError> {
    let mut pooled = pool.acquire().await?;
    let conn: &mut PgConnection = &mut pooled;
    conn.lock().await.map_err(describe)?;
    let result = apply_pending(conn, migrator).await;
    // The lock is session-scoped; release it before the connection returns
    // to the pool, also after a failure.
    let _ = conn.unlock().await;
    result
}

async fn apply_pending(
    conn: &mut PgConnection,
    migrator: &Migrator,
) -> Result<Vec<Applied>, DbError> {
    conn.ensure_migrations_table().await.map_err(describe)?;
    if let Some(version) = conn.dirty_version().await.map_err(describe)? {
        return Err(describe(MigrateError::Dirty(version)));
    }
    let applied_rows = conn.list_applied_migrations().await.map_err(describe)?;
    if let Some(unknown) = applied_rows
        .iter()
        .find(|row| !migrator.version_exists(row.version))
    {
        return Err(describe(MigrateError::VersionMissing(unknown.version)));
    }
    let known: BTreeMap<i64, Vec<u8>> = applied_rows
        .into_iter()
        .map(|row| (row.version, row.checksum.into_owned()))
        .collect();

    let mut applied = Vec::new();
    for migration in migrator.iter() {
        match known.get(&migration.version) {
            Some(checksum) if checksum.as_slice() != migration.checksum.as_ref() => {
                return Err(describe(MigrateError::VersionMismatch(migration.version)));
            }
            Some(_) => {}
            None => {
                conn.apply(migration).await.map_err(describe)?;
                applied.push(Applied {
                    version: migration.version,
                    description: migration.description.to_string(),
                });
            }
        }
    }
    Ok(applied)
}

/// Maps a sqlx migration error to a message without driver or SQL text.
fn describe(error: MigrateError) -> DbError {
    DbError::Migration(match error {
        MigrateError::VersionMismatch(v) => {
            format!("applied migration {v} was modified (checksum mismatch)")
        }
        MigrateError::VersionMissing(v) => {
            format!("database has migration {v} which this build does not know")
        }
        MigrateError::Dirty(v) => format!("migration {v} is recorded as failed"),
        MigrateError::Execute(_) => "a migration statement failed".to_owned(),
        _ => "migration could not be applied".to_owned(),
    })
}

/// Checks that the schema matches this build exactly: every embedded
/// migration applied successfully with an identical checksum and nothing
/// unknown applied.
///
/// # Errors
///
/// [`DbError::Schema`] with the first problem found, or a connection error.
pub async fn verify(pool: &PgPool) -> Result<(), DbError> {
    verify_with(pool, &MIGRATOR).await
}

/// Like [`verify`] with an explicit migrator.
///
/// # Errors
///
/// See [`verify`].
pub async fn verify_with(pool: &PgPool, migrator: &Migrator) -> Result<(), DbError> {
    if !bookkeeping_table_exists(pool).await? {
        return Err(DbError::Schema(SchemaProblem::NotMigrated));
    }
    let rows = sqlx::query("SELECT version, checksum, success FROM _sqlx_migrations")
        .fetch_all(pool)
        .await?;
    let mut applied: BTreeMap<i64, (Vec<u8>, bool)> = BTreeMap::new();
    for row in rows {
        applied.insert(
            row.try_get("version")?,
            (row.try_get("checksum")?, row.try_get("success")?),
        );
    }

    if let Some(v) = applied
        .keys()
        .find(|v| !migrator.iter().any(|m| m.version == **v))
    {
        return Err(DbError::Schema(SchemaProblem::UnknownApplied(*v)));
    }
    let mut pending = Vec::new();
    for migration in migrator.iter() {
        match applied.get(&migration.version) {
            None => pending.push(migration.version),
            Some((_, false)) => {
                return Err(DbError::Schema(SchemaProblem::Dirty(migration.version)));
            }
            Some((checksum, true)) if checksum.as_slice() != migration.checksum.as_ref() => {
                return Err(DbError::Schema(SchemaProblem::ChecksumMismatch(
                    migration.version,
                )));
            }
            Some(_) => {}
        }
    }
    if pending.is_empty() {
        Ok(())
    } else if pending.len() == migrator.iter().count() {
        Err(DbError::Schema(SchemaProblem::NotMigrated))
    } else {
        Err(DbError::Schema(SchemaProblem::Pending(pending)))
    }
}

async fn bookkeeping_table_exists(pool: &PgPool) -> Result<bool, DbError> {
    Ok(
        sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
            .fetch_one(pool)
            .await?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_migrations_are_ordered_and_complete() {
        // Migration 19 adds email delivery state, 20 adds app access, 22 adds
        // archive tiers, 23 adds instance setup/settings, 25 adds per-file chunks,
        // and 26 adds temporary chunked-upload manifests.
        let versions = embedded_versions();
        assert!(versions.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(versions.first(), Some(&1));
        assert_eq!(versions.last(), Some(&26));
        assert!((1..=23).all(|version| versions.contains(&version)));
        // Migration 24 belongs to AC-14 and may be absent in this standalone
        // worktree; the integrated branch contains both allocated migrations.
        assert!(versions.contains(&25));
        assert!(versions.contains(&26));
    }

    #[test]
    fn migrator_up_to_filters() {
        assert_eq!(migrator_up_to(1).iter().count(), 1);
        assert_eq!(migrator_up_to(0).iter().count(), 0);
        assert_eq!(migrator_up_to(15).iter().count(), 15);
        assert_eq!(migrator_up_to(16).iter().count(), 16);
        assert_eq!(migrator_up_to(17).iter().count(), 17);
        assert_eq!(migrator_up_to(18).iter().count(), 18);
        assert_eq!(migrator_up_to(19).iter().count(), 19);
        assert_eq!(migrator_up_to(20).iter().count(), 20);
        assert_eq!(migrator_up_to(21).iter().count(), 21);
        assert_eq!(migrator_up_to(22).iter().count(), 22);
        assert_eq!(migrator_up_to(23).iter().count(), 23);
        let latest_count = embedded_versions().len();
        assert_eq!(migrator_up_to(26).iter().count(), latest_count);
        assert_eq!(migrator_up_to(99).iter().count(), latest_count);
    }
}
