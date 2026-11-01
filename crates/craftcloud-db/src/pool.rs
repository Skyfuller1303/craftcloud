//! Connection pool creation.

use std::str::FromStr;
use std::time::Duration;

use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};

use crate::DbError;

/// Oldest PostgreSQL major version this build supports (Ubuntu 24.04 ships 16).
pub const MIN_POSTGRES_MAJOR: u32 = 16;
/// Newest PostgreSQL major version this build has been tested with.
pub const MAX_POSTGRES_MAJOR: u32 = 18;

/// Major version from `server_version_num` (for example `180006` is 18.6 and
/// `160014` is 16.14). Returns `None` for a negative number.
#[must_use]
pub fn postgres_major(server_version_num: i64) -> Option<u32> {
    if server_version_num < 0 {
        return None;
    }
    u32::try_from(server_version_num / 10_000).ok()
}

/// Accepts PostgreSQL 16, 17 and 18 and refuses every other major version.
///
/// # Errors
///
/// [`DbError::UnsupportedServer`] for a version outside the supported range.
pub fn ensure_supported_major(major: u32) -> Result<(), DbError> {
    if (MIN_POSTGRES_MAJOR..=MAX_POSTGRES_MAJOR).contains(&major) {
        Ok(())
    } else {
        Err(DbError::UnsupportedServer { major })
    }
}

/// Asks the server for its version and refuses unsupported majors, so that an
/// old or untested database fails at start with a clear message instead of
/// later with a confusing SQL error.
///
/// # Errors
///
/// [`DbError::UnsupportedServer`] for an unsupported version, other
/// [`DbError`] values if the query fails.
pub async fn check_server_version(pool: &PgPool) -> Result<(), DbError> {
    let number: i64 = sqlx::query_scalar("SELECT current_setting('server_version_num')::bigint")
        .fetch_one(pool)
        .await?;
    match postgres_major(number) {
        Some(major) => ensure_supported_major(major),
        None => Err(DbError::Internal),
    }
}

/// Runs the cheapest possible query: proves that a connection can be taken
/// from the pool and that the database answers. Callers add their own timeout.
///
/// # Errors
///
/// [`DbError`] if no connection is available or the query fails.
pub async fn ping(pool: &PgPool) -> Result<(), DbError> {
    sqlx::query("SELECT 1").execute(pool).await?;
    Ok(())
}

/// Connects a pool to `url`.
///
/// The URL contains credentials: it is never logged, and errors never echo
/// it. For remote databases use `sslmode=verify-full` (with `sslrootcert` if
/// the CA is private) in the URL.
///
/// # Errors
///
/// [`DbError::InvalidUrl`] for an unparsable URL, [`DbError::Connection`] if
/// no connection can be established within `connect_timeout`.
pub async fn connect(
    url: &str,
    max_connections: u32,
    connect_timeout: Duration,
) -> Result<PgPool, DbError> {
    let options = PgConnectOptions::from_str(url).map_err(|_| DbError::InvalidUrl)?;
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(connect_timeout)
        .connect_with(options)
        .await
        .map_err(DbError::from)?;
    check_server_version(&pool).await?;
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn major_version_comes_from_server_version_num() {
        assert_eq!(postgres_major(180_006), Some(18));
        assert_eq!(postgres_major(170_000), Some(17));
        assert_eq!(postgres_major(160_014), Some(16));
        assert_eq!(postgres_major(150_010), Some(15));
        assert_eq!(postgres_major(-1), None);
    }

    #[test]
    fn versions_16_to_18_are_accepted_and_others_refused() {
        for major in [16, 17, 18] {
            assert!(ensure_supported_major(major).is_ok(), "{major}");
        }
        for major in [9, 14, 15, 19] {
            assert_eq!(
                ensure_supported_major(major),
                Err(DbError::UnsupportedServer { major })
            );
        }
    }

    #[test]
    fn refusal_names_the_version_and_the_supported_range() {
        let text = DbError::UnsupportedServer { major: 15 }.to_string();
        assert!(text.contains("15"), "{text}");
        assert!(text.contains("16 to 18"), "{text}");
    }
}
