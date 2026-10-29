//! Which tables a backup contains, and in which order they are restored.
//!
//! The list is explicit on purpose: a table added by a later migration must be
//! classified here (the database tests fail until it is), so a new table can
//! never silently be missing from backups. Every table carries the migration
//! version that introduced it, so the table set of a backup is a function of
//! its migration version: a backup of schema N is restored with the tables
//! that exist at N, and a later migration that adds a table does not break the
//! restore of older backups (restore, then migrate).

use crate::error::{BackupError, ErrorKind};

/// The oldest migration version a backup can have: version 11 creates the audit
/// log, which is part of every backup. Older schemas are refused with a clear
/// message instead of an opaque database error.
pub const MIN_BACKUP_MIGRATION: i64 = 11;

/// A table in the backup.
#[derive(Debug, Clone, Copy)]
pub struct TableSpec {
    /// Table name in schema `public`.
    pub name: &'static str,
    /// The migration version that created the table.
    pub since: i64,
    /// A column with a foreign key that closes a cycle with a later table.
    /// The restore loads the table with this column cleared and sets it after
    /// all tables are in (the table's primary key is `id`).
    pub deferred_column: Option<&'static str>,
}

const fn table(name: &'static str, since: i64) -> TableSpec {
    TableSpec {
        name,
        since,
        deferred_column: None,
    }
}

/// Restore order: every table comes after the tables it references, except
/// for the deferred columns (`users` <-> `invitations`, `files` <->
/// `file_versions`).
pub const BACKUP_TABLES: &[TableSpec] = &[
    TableSpec {
        name: "users",
        since: 1,
        deferred_column: Some("created_via_invitation_id"),
    },
    table("organizations", 1),
    table("workspaces", 2),
    table("invitations", 6),
    table("organization_members", 1),
    table("workspace_members", 2),
    table("instance_setup", 3),
    table("projects", 7),
    TableSpec {
        name: "files",
        since: 7,
        deferred_column: Some("head_version_id"),
    },
    table("blobs", 8),
    table("upload_sessions", 8),
    table("file_versions", 9),
    table("audit_events", 11),
];

/// Tables that are deliberately not in a backup, with the migration version
/// that created them, and the reason.
///
/// - `sessions`: every session is invalid after a restore (users log in again);
///   a stolen backup must not contain usable session records.
/// - `password_reset_tokens`: single-use credentials; issue new links.
/// - `storage_binding`: the restored store gets a fresh identity.
/// - `maintenance_runs`: a run log that the new instance rebuilds.
pub const EXCLUDED_WITH_VERSION: &[(&str, i64)] = &[
    ("sessions", 4),
    ("password_reset_tokens", 4),
    ("storage_binding", 10),
    ("maintenance_runs", 10),
];

/// The names of [`EXCLUDED_WITH_VERSION`].
pub const EXCLUDED_TABLES: &[&str] = &[
    "sessions",
    "password_reset_tokens",
    "storage_binding",
    "maintenance_runs",
];

/// The migration bookkeeping table: recreated by running the migrations.
pub const MIGRATIONS_TABLE: &str = "_sqlx_migrations";

/// Whether `name` is one of the backed-up tables.
#[must_use]
pub fn is_backup_table(name: &str) -> bool {
    BACKUP_TABLES.iter().any(|t| t.name == name)
}

/// The tables of `specs` that exist at migration `version`, in order.
#[must_use]
pub fn select(specs: &[TableSpec], version: i64) -> Vec<&TableSpec> {
    specs.iter().filter(|t| t.since <= version).collect()
}

/// The backed-up tables at `version`, in restore order.
///
/// # Errors
///
/// [`ErrorKind::Format`] when `version` is older than [`MIN_BACKUP_MIGRATION`].
pub fn backup_tables_for(version: i64) -> Result<Vec<&'static TableSpec>, BackupError> {
    if version < MIN_BACKUP_MIGRATION {
        return Err(BackupError::new(
            ErrorKind::Format,
            format!(
                "migration version {version} is older than the first one with backup support \
                 ({MIN_BACKUP_MIGRATION}, which adds the audit log); such a database cannot be backed up or restored"
            ),
        ));
    }
    Ok(select(BACKUP_TABLES, version))
}

/// The deliberately excluded tables that exist at `version`.
#[must_use]
pub fn excluded_tables_for(version: i64) -> Vec<&'static str> {
    EXCLUDED_WITH_VERSION
        .iter()
        .filter(|(_, since)| *since <= version)
        .map(|(name, _)| *name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_plain_identifiers_and_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for table in BACKUP_TABLES {
            assert!(
                table
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{}",
                table.name
            );
            assert!(seen.insert(table.name), "duplicate {}", table.name);
        }
        for name in EXCLUDED_TABLES {
            assert!(!is_backup_table(name));
        }
    }

    #[test]
    fn the_excluded_lists_agree() {
        let names: Vec<&str> = EXCLUDED_WITH_VERSION.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, EXCLUDED_TABLES);
    }

    #[test]
    fn the_table_set_follows_the_migration_version() {
        // Every current table exists from the first supported version on.
        assert_eq!(
            backup_tables_for(MIN_BACKUP_MIGRATION).unwrap().len(),
            BACKUP_TABLES.len()
        );
        assert_eq!(backup_tables_for(13).unwrap().len(), BACKUP_TABLES.len());
        assert_eq!(excluded_tables_for(13), EXCLUDED_TABLES);
        // Older schemas are refused with a clear reason.
        let error = backup_tables_for(MIN_BACKUP_MIGRATION - 1).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Format);
        assert!(error.message.contains("backup support"), "{error}");
    }

    #[test]
    fn a_table_added_by_a_later_migration_is_only_expected_from_that_version() {
        // A synthetic future table: backups of older schemas must not expect it.
        let specs = [
            table("users", 1),
            table("audit_events", 11),
            table("future_things", 14),
        ];
        let at = |version| {
            select(&specs, version)
                .iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(at(13), ["users", "audit_events"]);
        assert_eq!(at(14), ["users", "audit_events", "future_things"]);
        assert_eq!(at(10), ["users"]);
    }
}
