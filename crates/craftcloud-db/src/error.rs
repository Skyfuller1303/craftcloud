//! Typed database errors. Display output is generic and never contains raw
//! driver or SQL error text, connection strings or row data.

use std::fmt;

/// Why the database schema does not match this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaProblem {
    /// No migration has been applied yet.
    NotMigrated,
    /// These migration versions are not applied yet.
    Pending(Vec<i64>),
    /// An applied migration differs from the one embedded in this build.
    ChecksumMismatch(i64),
    /// A migration is recorded as failed or partially applied.
    Dirty(i64),
    /// The database contains a migration this build does not know (the
    /// database is newer than this server).
    UnknownApplied(i64),
}

impl fmt::Display for SchemaProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotMigrated => write!(f, "no migrations have been applied"),
            Self::Pending(versions) => write!(f, "pending migrations: {versions:?}"),
            Self::ChecksumMismatch(v) => {
                write!(
                    f,
                    "applied migration {v} does not match this build (checksum mismatch)"
                )
            }
            Self::Dirty(v) => write!(f, "migration {v} is recorded as failed"),
            Self::UnknownApplied(v) => {
                write!(
                    f,
                    "database has migration {v} which this build does not know"
                )
            }
        }
    }
}

/// Database error with the cases callers need to distinguish.
#[derive(Clone, PartialEq, Eq)]
pub enum DbError {
    /// A UNIQUE or PRIMARY KEY constraint was violated.
    UniqueViolation {
        /// Constraint name, if reported.
        constraint: Option<String>,
    },
    /// A FOREIGN KEY constraint was violated.
    ForeignKeyViolation {
        /// Constraint name, if reported.
        constraint: Option<String>,
    },
    /// A CHECK or NOT NULL constraint was violated.
    CheckViolation {
        /// Constraint name, if reported.
        constraint: Option<String>,
    },
    /// The connection could not be established or was lost. The text is a
    /// coarse category and never contains credentials.
    Connection(String),
    /// The configured database URL is not a valid PostgreSQL URL.
    InvalidUrl,
    /// The PostgreSQL server has a major version this build does not support.
    UnsupportedServer {
        /// Major version reported by the server (16 for 16.4, 18 for 18.6).
        major: u32,
    },
    /// The schema does not match this build.
    Schema(SchemaProblem),
    /// Applying migrations failed. The text names the problem class only.
    Migration(String),
    /// Any other database failure; details are deliberately not kept.
    Internal,
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UniqueViolation { .. } => {
                write!(f, "a record with the same unique value already exists")
            }
            Self::ForeignKeyViolation { .. } => {
                write!(f, "a referenced record does not exist or is still in use")
            }
            Self::CheckViolation { .. } => write!(f, "a value violates a database constraint"),
            Self::Connection(what) => write!(f, "database connection failed: {what}"),
            Self::InvalidUrl => write!(f, "the database URL is not a valid PostgreSQL URL"),
            Self::UnsupportedServer { major } => write!(
                f,
                "unsupported PostgreSQL version {major}: CraftCloud supports PostgreSQL {} to {}",
                crate::pool::MIN_POSTGRES_MAJOR,
                crate::pool::MAX_POSTGRES_MAJOR
            ),
            Self::Schema(problem) => write!(f, "database schema check failed: {problem}"),
            Self::Migration(what) => write!(f, "migration failed: {what}"),
            Self::Internal => write!(f, "internal database error"),
        }
    }
}

// Manual Debug: the variants carry no secrets, but keep output identical to
// Display so logs never grow driver details.
impl fmt::Debug for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UniqueViolation { constraint } => write!(f, "UniqueViolation({constraint:?})"),
            Self::ForeignKeyViolation { constraint } => {
                write!(f, "ForeignKeyViolation({constraint:?})")
            }
            Self::CheckViolation { constraint } => write!(f, "CheckViolation({constraint:?})"),
            other => write!(f, "{other}"),
        }
    }
}

impl std::error::Error for DbError {}

impl DbError {
    /// Constraint name for constraint violations.
    #[must_use]
    pub fn constraint(&self) -> Option<&str> {
        match self {
            Self::UniqueViolation { constraint }
            | Self::ForeignKeyViolation { constraint }
            | Self::CheckViolation { constraint } => constraint.as_deref(),
            _ => None,
        }
    }
}

impl From<sqlx::Error> for DbError {
    fn from(error: sqlx::Error) -> Self {
        use sqlx::error::ErrorKind;
        match error {
            sqlx::Error::Database(db) => {
                let constraint = db.constraint().map(str::to_owned);
                // ON DELETE RESTRICT reports SQLSTATE 23001, which sqlx does
                // not classify as a foreign key violation.
                if db.code().as_deref() == Some("23001") {
                    return Self::ForeignKeyViolation { constraint };
                }
                match db.kind() {
                    ErrorKind::UniqueViolation => Self::UniqueViolation { constraint },
                    ErrorKind::ForeignKeyViolation => Self::ForeignKeyViolation { constraint },
                    ErrorKind::CheckViolation | ErrorKind::NotNullViolation => {
                        Self::CheckViolation { constraint }
                    }
                    _ => Self::Internal,
                }
            }
            sqlx::Error::Io(io) => Self::Connection(format!("network error: {}", io.kind())),
            sqlx::Error::Tls(_) => Self::Connection("TLS error".to_owned()),
            sqlx::Error::PoolTimedOut => {
                Self::Connection("timed out waiting for a connection".to_owned())
            }
            sqlx::Error::PoolClosed => Self::Connection("connection pool is closed".to_owned()),
            sqlx::Error::Configuration(_) => Self::InvalidUrl,
            _ => Self::Internal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_generic() {
        let text = DbError::UniqueViolation {
            constraint: Some("users_normalized_email_key".to_owned()),
        }
        .to_string();
        assert!(!text.contains("users_normalized_email_key"));
        assert!(DbError::Internal.to_string().contains("internal"));
    }

    #[test]
    fn constraint_accessor() {
        let err = DbError::CheckViolation {
            constraint: Some("c".to_owned()),
        };
        assert_eq!(err.constraint(), Some("c"));
        assert_eq!(DbError::Internal.constraint(), None);
    }

    #[test]
    fn non_database_errors_map_without_details() {
        let io = sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "postgres://u:hunter2@h/db",
        ));
        let mapped = DbError::from(io);
        assert!(!mapped.to_string().contains("hunter2"));
        assert!(!format!("{mapped:?}").contains("hunter2"));
        assert_eq!(
            DbError::from(sqlx::Error::PoolTimedOut).to_string(),
            "database connection failed: timed out waiting for a connection"
        );
        assert_eq!(DbError::from(sqlx::Error::RowNotFound), DbError::Internal);
        assert_eq!(
            DbError::from(sqlx::Error::Configuration("x".into())),
            DbError::InvalidUrl
        );
    }
}
