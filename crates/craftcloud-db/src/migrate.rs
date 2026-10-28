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
        assert_eq!(embedded_versions(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn migrator_up_to_filters() {
        assert_eq!(migrator_up_to(1).iter().count(), 1);
        assert_eq!(migrator_up_to(0).iter().count(), 0);
        assert_eq!(migrator_up_to(99).iter().count(), 4);
    }
}
