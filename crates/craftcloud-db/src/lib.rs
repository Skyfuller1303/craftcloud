//! CraftCloud PostgreSQL layer: schema migrations and tenant-scoped
//! repositories.
//!
//! Design rules (see ADR-0012):
//! - runtime queries only (`sqlx::query*`); no compile-time query macros, so
//!   building never needs a database;
//! - forward-only migrations, applied only by an explicit call to
//!   [`migrate::run`]; services use [`migrate::verify`] at startup;
//! - every tenant-bound repository function takes an [`OrgScope`] and filters
//!   by `org_id` in SQL;
//! - raw database error text never leaves this crate ([`DbError`]).
//!
//! This crate does not read environment variables or files; the server edge
//! supplies the connection URL.

pub mod accounts;
pub mod audit;
pub mod device_sessions;
mod error;
pub mod files;
pub mod grants;
pub mod invitations;
pub mod locks;
pub mod maintenance;
pub mod members;
pub mod migrate;
pub mod organizations;
pub mod password_reset;
mod pool;
pub mod projects;
pub mod recovery;
mod scope;
pub mod sessions;
pub mod setup;
pub mod two_factor;
pub mod uploads;
pub mod users;
pub mod versions;
pub mod workspaces;

pub use error::{DbError, SchemaProblem};
pub use pool::{connect, ping};
pub use scope::OrgScope;

pub use sqlx::PgPool;
