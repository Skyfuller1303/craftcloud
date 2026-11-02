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
pub mod app_access;
pub mod archive_intent;
pub mod archive_recall;
pub mod audit;
pub mod device_sessions;
pub mod email_outbox;
pub mod email_verification;
mod error;
pub mod figcraft;
pub mod files;
pub mod grants;
pub mod instance_settings;
pub mod invitations;
pub mod locks;
pub mod mail_security;
pub mod maintenance;
pub mod members;
pub mod migrate;
pub mod oidc;
pub mod organizations;
pub mod overview;
pub mod password_reset;
mod pool;
pub mod projects;
pub mod recovery;
pub mod scim;
mod scope;
pub mod sessions;
pub mod setup;
pub mod two_factor;
pub mod uploads;
pub mod users;
pub mod versions;
pub mod workspaces;

pub use error::{DbError, SchemaProblem};
pub use pool::{
    MAX_POSTGRES_MAJOR, MIN_POSTGRES_MAJOR, check_server_version, connect, ensure_supported_major,
    ping, postgres_major,
};
pub use scope::OrgScope;

pub use sqlx::PgPool;

/// Organization SSO session policy.
pub mod sso;

/// Bounded OIDC login state.
pub mod oidc_state;
