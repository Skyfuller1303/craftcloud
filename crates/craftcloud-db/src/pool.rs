//! Connection pool creation.

use std::str::FromStr;
use std::time::Duration;

use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};

use crate::DbError;

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
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(connect_timeout)
        .connect_with(options)
        .await
        .map_err(DbError::from)
}
