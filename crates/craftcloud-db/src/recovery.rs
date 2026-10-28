//! Break-glass recovery for the offline `craftcloud-server admin
//! set-org-owner` tool.
//!
//! These functions deliberately bypass the policy engine: they are the
//! documented last resort of the host operator when an organization has no
//! enabled owner left (for example after the only owner's account had to be
//! disabled). They are not reachable from the HTTP API; the server binary only
//! calls them from the command line, which requires access to the database
//! URL secret file, i.e. host access.

use sqlx::PgPool;
use uuid::Uuid;

use crate::DbError;

/// Result of [`set_org_owner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOwnerOutcome {
    /// The user was added to the organization as owner.
    Added {
        /// The organization.
        org_id: Uuid,
        /// The user.
        user_id: Uuid,
    },
    /// An existing membership (any status) was turned into an active owner.
    Promoted {
        /// The organization.
        org_id: Uuid,
        /// The user.
        user_id: Uuid,
    },
    /// The user already is an active owner.
    AlreadyOwner {
        /// The organization.
        org_id: Uuid,
        /// The user.
        user_id: Uuid,
    },
    /// No organization with that slug.
    OrgNotFound,
    /// No account with that e-mail address.
    UserNotFound,
    /// The account is disabled.
    UserDisabled,
}

/// Makes an existing, enabled user an active owner of the organization with
/// the given slug (adding or promoting as needed, reactivating any status).
/// Serialized with all membership changes through the organization row lock.
///
/// # Errors
///
/// Database failures only; nothing changes on failure.
pub async fn set_org_owner(
    pool: &PgPool,
    org_slug: &str,
    normalized_email: &str,
) -> Result<SetOwnerOutcome, DbError> {
    let mut tx = pool.begin().await?;
    let org: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM organizations WHERE slug = $1 FOR UPDATE")
            .bind(org_slug)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(org_id) = org else {
        return Ok(SetOwnerOutcome::OrgNotFound);
    };
    let user: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT id, disabled_at IS NOT NULL FROM users WHERE normalized_email = $1 FOR SHARE",
    )
    .bind(normalized_email)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((user_id, disabled)) = user else {
        return Ok(SetOwnerOutcome::UserNotFound);
    };
    if disabled {
        return Ok(SetOwnerOutcome::UserDisabled);
    }
    let existing: Option<(String, String)> = sqlx::query_as(
        "SELECT role, status FROM organization_members WHERE org_id = $1 AND user_id = $2",
    )
    .bind(org_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let outcome = match existing {
        Some((role, status)) if role == "owner" && status == "active" => {
            return Ok(SetOwnerOutcome::AlreadyOwner { org_id, user_id });
        }
        Some(_) => {
            sqlx::query(
                "UPDATE organization_members SET role = 'owner', status = 'active' \
                 WHERE org_id = $1 AND user_id = $2",
            )
            .bind(org_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
            SetOwnerOutcome::Promoted { org_id, user_id }
        }
        None => {
            sqlx::query(
                "INSERT INTO organization_members (org_id, user_id, role, status) \
                 VALUES ($1, $2, 'owner', 'active')",
            )
            .bind(org_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
            SetOwnerOutcome::Added { org_id, user_id }
        }
    };
    tx.commit().await?;
    Ok(outcome)
}
