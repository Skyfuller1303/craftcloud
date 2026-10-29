//! One-shot instance setup (CC-F01): creates the first instance operator and
//! organization atomically, exactly once.
//!
//! The setup transaction first claims the singleton `instance_setup` row.
//! Concurrent attempts block on that row's primary key until the winner
//! commits (they then see `AlreadyCompleted`) or rolls back (one of them then
//! becomes the winner). The operator reference on that row is deferred, so
//! user, organization and membership are created afterwards in the same
//! transaction; any failure rolls everything back.

use std::fmt;

use sqlx::PgPool;
use uuid::Uuid;

use crate::DbError;
use crate::audit::{AuditCtx, Detail, NewEvent, record_in};

/// Validated inputs for [`complete`]. The caller validates and normalizes
/// every field and hashes the password beforehand.
#[derive(Debug, Clone)]
pub struct NewInstance {
    /// Normalized (lowercase) operator e-mail address.
    pub operator_email: String,
    /// Operator display name.
    pub operator_display_name: String,
    /// Argon2id PHC string; never the password itself.
    pub operator_password_hash: String,
    /// Organization display name.
    pub organization_name: String,
    /// Organization slug.
    pub organization_slug: String,
}

/// Identifiers created by [`complete`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Created {
    /// The new instance operator.
    pub user_id: Uuid,
    /// The new organization.
    pub organization_id: Uuid,
}

/// Why [`complete`] did not create anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupError {
    /// Setup was already completed.
    AlreadyCompleted,
    /// A database error; nothing was persisted.
    Db(DbError),
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyCompleted => f.write_str("instance setup was already completed"),
            Self::Db(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for SetupError {}

impl From<sqlx::Error> for SetupError {
    fn from(error: sqlx::Error) -> Self {
        // A serialization failure (SQLSTATE 40001) can only come from a
        // concurrent setup attempt that won; treat it as already completed.
        if let sqlx::Error::Database(db) = &error
            && db.code().as_deref() == Some("40001")
        {
            return Self::AlreadyCompleted;
        }
        Self::Db(DbError::from(error))
    }
}

/// Whether the one-time setup has been completed.
///
/// # Errors
///
/// Database failures only.
pub async fn is_completed(pool: &PgPool) -> Result<bool, DbError> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM instance_setup)")
            .fetch_one(pool)
            .await?,
    )
}

/// Creates the first operator, organization and owner membership and marks
/// the instance as set up, all in one transaction.
///
/// # Errors
///
/// [`SetupError::AlreadyCompleted`] if setup was done before (or a concurrent
/// attempt won); [`SetupError::Db`] otherwise, with nothing persisted.
pub async fn complete(pool: &PgPool, new: &NewInstance) -> Result<Created, SetupError> {
    complete_inner(pool, new, None).await
}

/// Like [`complete`], and records the instance-level `operator.create` event
/// and the `organization.create` event of the first organization (no actor:
/// the operator did not exist before) in the same transaction.
///
/// # Errors
///
/// As [`complete`]; a failing audit insert fails the setup.
pub async fn complete_audited(
    pool: &PgPool,
    new: &NewInstance,
    audit: &AuditCtx,
) -> Result<Created, SetupError> {
    complete_inner(pool, new, Some(audit)).await
}

async fn complete_inner(
    pool: &PgPool,
    new: &NewInstance,
    audit: Option<&AuditCtx>,
) -> Result<Created, SetupError> {
    let user_id = Uuid::new_v4();
    let organization_id = Uuid::new_v4();
    let mut tx = pool.begin().await?;
    // Race semantics must not depend on the server default isolation level.
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *tx)
        .await?;

    // 1. Claim the singleton row first; this serializes concurrent attempts.
    let claimed = sqlx::query(
        "INSERT INTO instance_setup (operator_user_id) VALUES ($1) ON CONFLICT (id) DO NOTHING",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if claimed == 0 {
        // Dropping the transaction rolls it back.
        return Err(SetupError::AlreadyCompleted);
    }

    // 2. Operator, organization and owner membership.
    sqlx::query(
        "INSERT INTO users (id, normalized_email, display_name, password_hash, instance_role) \
         VALUES ($1, $2, $3, $4, 'operator')",
    )
    .bind(user_id)
    .bind(&new.operator_email)
    .bind(&new.operator_display_name)
    .bind(&new.operator_password_hash)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO organizations (id, slug, name, owner_user_id) VALUES ($1, $2, $3, $4)",
    )
    .bind(organization_id)
    .bind(&new.organization_slug)
    .bind(&new.organization_name)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO organization_members (org_id, user_id, role, status) \
         VALUES ($1, $2, 'owner', 'active')",
    )
    .bind(organization_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;

    if let Some(audit) = audit {
        let by = AuditCtx {
            actor_id: Some(user_id),
            request_id: audit.request_id.clone(),
        };
        record_in(
            &mut tx,
            &by,
            &NewEvent::new("operator.create").target("user", user_id),
        )
        .await
        .map_err(SetupError::Db)?;
        record_in(
            &mut tx,
            &by,
            &NewEvent::new("organization.create")
                .org(organization_id)
                .target("organization", organization_id)
                .detail("owner_id", Detail::Id(user_id)),
        )
        .await
        .map_err(SetupError::Db)?;
    }

    // 3. Commit verifies the deferred operator reference.
    tx.commit().await?;
    Ok(Created {
        user_id,
        organization_id,
    })
}
