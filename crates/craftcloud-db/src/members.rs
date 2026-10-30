//! Organization membership.
//!
//! All changes go through [`OrgTx`], a transaction that first locks the
//! organization row (`SELECT ... FOR UPDATE`). Membership changes and renames
//! of one organization are therefore serialized, which makes the policy
//! decision (made inside the transaction with freshly read roles) and the
//! "last owner" rule race-free.
//!
//! **Owners that count:** the last-owner rule counts only owners whose
//! membership is `active` **and** whose account is not disabled. A change that
//! would leave an organization without such an owner is refused
//! ([`RoleOutcome::LastOwner`], [`RemoveOutcome::LastOwner`]). Disabling an
//! account is deliberately *not* blocked by this rule (a security response must
//! not be blocked); `accounts::set_disabled` reports the organizations it
//! leaves without an enabled owner so an operator can recover them with the
//! offline `set-org-owner` tool.
//!
//! **Membership statuses:** only `active` rows are members. `add` reactivates
//! an `invited`/`disabled` row of a non-owner role (role replaced, status set
//! to `active`); a non-active row that still holds the `owner` role is never
//! reactivated through the API (it answers `AlreadyMember`) and has to be
//! removed first. `remove` deletes a row of any status. `set_role` only
//! applies to active members.
//!
//! Lock order: organization row, then (when needed) users / membership rows.
//! This never conflicts with the account paths (`users` first) because those
//! do not take organization row locks.

use craftcloud_core::policy::{Action, OrgRole};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::audit::{AuditCtx, NewEvent};
use crate::organizations::{AfterKey, Organization, UpdateOutcome, clamp_page};
use crate::{DbError, OrgScope};

/// A member of an organization.
#[derive(Debug, Clone, FromRow)]
pub struct Member {
    /// User id.
    pub user_id: Uuid,
    /// Display name.
    pub display_name: String,
    /// Normalized e-mail address.
    pub email: String,
    /// Role (`owner`, `admin`, `member`).
    pub role: String,
    /// When the user became an active member.
    pub joined_at: OffsetDateTime,
}

impl Member {
    /// The parsed role.
    #[must_use]
    pub fn org_role(&self) -> Option<OrgRole> {
        OrgRole::parse(&self.role)
    }
}

const MEMBER_SELECT: &str = "SELECT u.id AS user_id, u.display_name, u.normalized_email AS email, \
     m.role, m.created_at AS joined_at \
     FROM organization_members m JOIN users u ON u.id = m.user_id";

/// Lists active members of the scoped organization ordered by
/// `(joined_at, user_id)`, one page after `after`. The keyset contains no
/// personal data.
///
/// # Errors
///
/// Database failures only.
pub async fn list(
    pool: &PgPool,
    scope: &OrgScope,
    limit: u32,
    after: Option<AfterKey>,
) -> Result<Vec<Member>, DbError> {
    Ok(sqlx::query_as::<_, Member>(&format!(
        "{MEMBER_SELECT} WHERE m.org_id = $1 AND m.status = 'active' \
           AND ($2::timestamptz IS NULL OR (m.created_at, m.user_id) > ($2::timestamptz, $3::uuid)) \
         ORDER BY m.created_at, m.user_id LIMIT $4"
    ))
    .bind(scope.org_id())
    .bind(after.map(|(at, _)| at))
    .bind(after.map(|(_, id)| id))
    .bind(clamp_page(limit))
    .fetch_all(pool)
    .await?)
}

/// Searches active organization members by display name or normalized e-mail.
pub async fn search(
    pool: &PgPool,
    scope: &OrgScope,
    query: &str,
    limit: u32,
    after: Option<AfterKey>,
) -> Result<Vec<Member>, DbError> {
    Ok(sqlx::query_as::<_, Member>(&format!(
        "{MEMBER_SELECT} WHERE m.org_id=$1 AND m.status='active' AND (u.disabled_at IS NULL) \
         AND (position($2 in lower(u.display_name))>0 OR position($2 in u.normalized_email)>0) \
         AND ($3::timestamptz IS NULL OR (m.created_at,m.user_id)>($3,$4)) ORDER BY m.created_at,m.user_id LIMIT $5"
    ))
    .bind(scope.org_id()).bind(query).bind(after.map(|x|x.0)).bind(after.map(|x|x.1)).bind(clamp_page(limit)).fetch_all(pool).await?)
}

pub async fn can_manage_grants(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_members m WHERE m.org_id=$1 AND m.user_id=$2 AND m.status='active' AND m.role IN ('owner','admin')) OR EXISTS(SELECT 1 FROM workspace_members wm JOIN workspaces w ON w.id=wm.workspace_id AND w.org_id=wm.org_id JOIN organization_members m ON m.org_id=wm.org_id AND m.user_id=wm.user_id AND m.status='active' AND m.role <> 'guest' WHERE wm.org_id=$1 AND wm.user_id=$2 AND wm.role IN ('admin','editor'))")
        .bind(org_id).bind(user_id).fetch_one(pool).await?)
}

/// Result of [`OrgTx::add`].
#[derive(Debug, Clone)]
pub enum AddOutcome {
    /// The user is now an active member (new row or reactivated row).
    Added(Member),
    /// The user is already an active member (or holds a non-active owner row).
    AlreadyMember,
    /// No such user.
    UserNotFound,
    /// The account is disabled.
    UserDisabled,
}

/// Result of [`OrgTx::set_role`].
#[derive(Debug, Clone)]
pub enum RoleOutcome {
    /// The role was changed.
    Changed(Member),
    /// The member already had that role.
    Unchanged(Member),
    /// The user is not an active member.
    NotMember,
    /// The change would leave the organization without an enabled owner.
    LastOwner,
}

/// Result of [`OrgTx::remove`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// The membership row (any status) and the user's workspace grants in this
    /// organization were removed.
    Removed,
    /// The user has no membership row.
    NotMember,
    /// The removal would leave the organization without an enabled owner.
    LastOwner,
}

/// A membership transaction holding the organization row lock.
pub struct OrgTx {
    pub(crate) tx: Transaction<'static, Postgres>,
    pub(crate) org_id: Uuid,
}

impl OrgTx {
    /// Starts the transaction and locks the organization. `None` if the
    /// organization does not exist.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn begin(pool: &PgPool, org_id: Uuid) -> Result<Option<Self>, DbError> {
        let mut tx = pool.begin().await?;
        let found: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM organizations WHERE id = $1 FOR UPDATE")
                .bind(org_id)
                .fetch_optional(&mut *tx)
                .await?;
        Ok(found.map(|_| Self { tx, org_id }))
    }

    /// Fresh role of a user (active memberships only), read under the lock.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn role_of(&mut self, user_id: Uuid) -> Result<Option<OrgRole>, DbError> {
        let role: Option<String> = sqlx::query_scalar(
            "SELECT role FROM organization_members \
             WHERE org_id = $1 AND user_id = $2 AND status = 'active'",
        )
        .bind(self.org_id)
        .bind(user_id)
        .fetch_optional(&mut *self.tx)
        .await?;
        Ok(role.as_deref().and_then(OrgRole::parse))
    }

    /// The stored role of a membership row of **any** status (used to decide
    /// who may clean up invited/disabled rows), read under the lock.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn stored_role_of(&mut self, user_id: Uuid) -> Result<Option<OrgRole>, DbError> {
        let role: Option<String> = sqlx::query_scalar(
            "SELECT role FROM organization_members WHERE org_id = $1 AND user_id = $2",
        )
        .bind(self.org_id)
        .bind(user_id)
        .fetch_optional(&mut *self.tx)
        .await?;
        Ok(role.as_deref().and_then(OrgRole::parse))
    }

    pub(crate) fn check(&self, scope: &OrgScope, action: Action) -> Result<(), DbError> {
        scope.require(action)?;
        if scope.org_id() == self.org_id {
            Ok(())
        } else {
            Err(DbError::Internal)
        }
    }

    async fn member(&mut self, user_id: Uuid) -> Result<Option<Member>, DbError> {
        Ok(sqlx::query_as::<_, Member>(&format!(
            "{MEMBER_SELECT} WHERE m.org_id = $1 AND m.user_id = $2 AND m.status = 'active'"
        ))
        .bind(self.org_id)
        .bind(user_id)
        .fetch_optional(&mut *self.tx)
        .await?)
    }

    /// Number of active owners whose account is enabled. The owners' user rows
    /// are share-locked, so a concurrent `set_disabled` is waited for and its
    /// `disabled_at` is re-read before the count is trusted (lock order:
    /// organization row, then users rows).
    async fn enabled_owners(&mut self) -> Result<usize, DbError> {
        let rows: Vec<Uuid> = sqlx::query_scalar(
            "SELECT u.id FROM organization_members m JOIN users u ON u.id = m.user_id \
             WHERE m.org_id = $1 AND m.role = 'owner' AND m.status = 'active' \
               AND u.disabled_at IS NULL \
             FOR SHARE OF u",
        )
        .bind(self.org_id)
        .fetch_all(&mut *self.tx)
        .await?;
        Ok(rows.len())
    }

    /// Whether giving up `user_id`'s owner role would leave no enabled owner.
    async fn would_orphan(&mut self, user_id: Uuid) -> Result<bool, DbError> {
        let counts: Option<bool> = sqlx::query_scalar(
            "SELECT (m.role = 'owner' AND m.status = 'active' AND u.disabled_at IS NULL) \
             FROM organization_members m JOIN users u ON u.id = m.user_id \
             WHERE m.org_id = $1 AND m.user_id = $2 FOR SHARE OF u",
        )
        .bind(self.org_id)
        .bind(user_id)
        .fetch_optional(&mut *self.tx)
        .await?;
        // Only an owner that currently counts can orphan the organization.
        Ok(counts == Some(true) && self.enabled_owners().await? <= 1)
    }

    /// Adds an existing, enabled user as active member, reactivating an
    /// `invited`/`disabled` non-owner row if there is one.
    ///
    /// # Errors
    ///
    /// Database failures only; `scope` must be a write scope of the
    /// transaction's organization.
    pub async fn add(
        &mut self,
        scope: &OrgScope,
        user_id: Uuid,
        role: OrgRole,
    ) -> Result<AddOutcome, DbError> {
        self.check(scope, Action::MemberAdd)?;
        let disabled: Option<bool> =
            sqlx::query_scalar("SELECT disabled_at IS NOT NULL FROM users WHERE id = $1 FOR SHARE")
                .bind(user_id)
                .fetch_optional(&mut *self.tx)
                .await?;
        match disabled {
            None => return Ok(AddOutcome::UserNotFound),
            Some(true) => return Ok(AddOutcome::UserDisabled),
            Some(false) => {}
        }
        let written = sqlx::query(
            "INSERT INTO organization_members (org_id, user_id, role, status) \
             VALUES ($1, $2, $3, 'active') \
             ON CONFLICT (org_id, user_id) DO UPDATE \
                SET role = EXCLUDED.role, status = 'active', created_at = now() \
                WHERE organization_members.status <> 'active' \
                  AND organization_members.role <> 'owner'",
        )
        .bind(self.org_id)
        .bind(user_id)
        .bind(role.as_str())
        .execute(&mut *self.tx)
        .await?
        .rows_affected();
        if written == 0 {
            return Ok(AddOutcome::AlreadyMember);
        }
        match self.member(user_id).await? {
            Some(member) => Ok(AddOutcome::Added(member)),
            None => Err(DbError::Internal),
        }
    }

    /// Changes the role of an **active** member. Refuses to demote the last
    /// enabled owner.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn set_role(
        &mut self,
        scope: &OrgScope,
        user_id: Uuid,
        new_role: OrgRole,
    ) -> Result<RoleOutcome, DbError> {
        self.check(scope, Action::MemberChangeRole)?;
        let Some(current) = self.member(user_id).await? else {
            return Ok(RoleOutcome::NotMember);
        };
        let current_role = current.org_role();
        if current_role == Some(new_role) {
            return Ok(RoleOutcome::Unchanged(current));
        }
        if current_role == Some(OrgRole::Owner) && self.would_orphan(user_id).await? {
            return Ok(RoleOutcome::LastOwner);
        }
        sqlx::query("DELETE FROM file_locks WHERE org_id = $1 AND holder_user_id = $2")
            .bind(self.org_id)
            .bind(user_id)
            .execute(&mut *self.tx)
            .await?;
        if new_role == OrgRole::Guest {
            sqlx::query("DELETE FROM workspace_members WHERE org_id = $1 AND user_id = $2")
                .bind(self.org_id)
                .bind(user_id)
                .execute(&mut *self.tx)
                .await?;
        }
        sqlx::query(
            "UPDATE organization_members SET role = $3 \
             WHERE org_id = $1 AND user_id = $2 AND status = 'active'",
        )
        .bind(self.org_id)
        .bind(user_id)
        .bind(new_role.as_str())
        .execute(&mut *self.tx)
        .await?;
        match self.member(user_id).await? {
            Some(member) => Ok(RoleOutcome::Changed(member)),
            None => Err(DbError::Internal),
        }
    }

    /// Removes a membership row of any status and the user's workspace grants
    /// in this organization. Refuses to remove the last enabled owner.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn remove(
        &mut self,
        scope: &OrgScope,
        user_id: Uuid,
    ) -> Result<RemoveOutcome, DbError> {
        self.remove_audited(scope, user_id, &crate::audit::AuditCtx::system())
            .await
    }

    /// Removes a membership and attributes resulting device revocations to the caller.
    ///
    /// # Errors
    ///
    /// Database failures only; a failed audit write rolls back with the transaction.
    pub async fn remove_audited(
        &mut self,
        scope: &OrgScope,
        user_id: Uuid,
        audit: &crate::audit::AuditCtx,
    ) -> Result<RemoveOutcome, DbError> {
        self.check(scope, Action::MemberRemove)?;
        if self.stored_role_of(user_id).await?.is_none() {
            return Ok(RemoveOutcome::NotMember);
        }
        // Lock order: organization row, then user rows in id order. `would_orphan`
        // share-locks the owners' user rows, so the target and every active owner
        // of this organization are locked exclusively up front, sorted by id. Two
        // organizations removing the same user, or removing each other's shared
        // owners, then acquire the rows in the same order and cannot deadlock.
        sqlx::query(
            "SELECT u.id FROM users u WHERE u.id = $2 OR u.id IN \
               (SELECT m.user_id FROM organization_members m \
                WHERE m.org_id = $1 AND m.role = 'owner' AND m.status = 'active') \
             ORDER BY u.id FOR UPDATE",
        )
        .bind(self.org_id)
        .bind(user_id)
        .fetch_all(&mut *self.tx)
        .await?;
        if self.would_orphan(user_id).await? {
            return Ok(RemoveOutcome::LastOwner);
        }
        sqlx::query("UPDATE grants SET revoked_at = CURRENT_TIMESTAMP, version = version + 1 WHERE org_id = $1 AND grantee_user_id = $2 AND revoked_at IS NULL")
            .bind(self.org_id)
            .bind(user_id)
            .execute(&mut *self.tx)
            .await?;
        sqlx::query("DELETE FROM file_locks WHERE org_id = $1 AND holder_user_id = $2")
            .bind(self.org_id)
            .bind(user_id)
            .execute(&mut *self.tx)
            .await?;
        sqlx::query("DELETE FROM workspace_members WHERE org_id = $1 AND user_id = $2")
            .bind(self.org_id)
            .bind(user_id)
            .execute(&mut *self.tx)
            .await?;
        sqlx::query("DELETE FROM organization_members WHERE org_id = $1 AND user_id = $2")
            .bind(self.org_id)
            .bind(user_id)
            .execute(&mut *self.tx)
            .await?;
        let memberships: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM organization_members WHERE user_id=$1 AND status='active'",
        )
        .bind(user_id)
        .fetch_one(&mut *self.tx)
        .await?;
        if memberships == 0 {
            let now: OffsetDateTime = sqlx::query_scalar("SELECT now()")
                .fetch_one(&mut *self.tx)
                .await?;
            crate::device_sessions::revoke_all_for_user_tx(&mut self.tx, user_id, now, audit)
                .await?;
        }
        Ok(RemoveOutcome::Removed)
    }

    /// Updates the organization's name and/or the workspace-admin invitation
    /// setting if its version is still `expected_version` (compare and set),
    /// under the same lock as membership changes.
    ///
    /// # Errors
    ///
    /// [`DbError::CheckViolation`] for an invalid name.
    pub async fn update_settings(
        &mut self,
        scope: &OrgScope,
        name: Option<&str>,
        workspace_admin_invites_enabled: Option<bool>,
        expected_version: i64,
    ) -> Result<UpdateOutcome, DbError> {
        self.check(scope, Action::OrganizationUpdate)?;
        let updated = sqlx::query_as::<_, Organization>(
            "UPDATE organizations SET name = COALESCE($2, name), \
                    workspace_admin_invites_enabled = COALESCE($3, workspace_admin_invites_enabled), \
                    version = version + 1 \
             WHERE id = $1 AND version = $4 \
             RETURNING id, slug, name, owner_user_id, version, workspace_admin_invites_enabled, created_at",
        )
        .bind(self.org_id)
        .bind(name)
        .bind(workspace_admin_invites_enabled)
        .bind(expected_version)
        .fetch_optional(&mut *self.tx)
        .await?;
        if let Some(org) = updated {
            return Ok(UpdateOutcome::Updated(org));
        }
        let current: Option<i64> =
            sqlx::query_scalar("SELECT version FROM organizations WHERE id = $1")
                .bind(self.org_id)
                .fetch_optional(&mut *self.tx)
                .await?;
        Ok(match current {
            Some(current_version) => UpdateOutcome::VersionMismatch { current_version },
            None => UpdateOutcome::Gone,
        })
    }

    /// The current display name, read under the lock.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn current_name(&mut self) -> Result<String, DbError> {
        Ok(
            sqlx::query_scalar("SELECT name FROM organizations WHERE id = $1")
                .bind(self.org_id)
                .fetch_one(&mut *self.tx)
                .await?,
        )
    }

    /// The organization setting that lets workspace admins invite.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn workspace_admin_invites_enabled(&mut self) -> Result<bool, DbError> {
        Ok(sqlx::query_scalar(
            "SELECT workspace_admin_invites_enabled FROM organizations WHERE id = $1",
        )
        .bind(self.org_id)
        .fetch_one(&mut *self.tx)
        .await?)
    }

    /// Records an audit event of this organization in the transaction, so it
    /// commits or rolls back together with the change it describes. The event's
    /// tenant is always the transaction's organization. On error the caller
    /// must abandon the transaction (fail closed).
    ///
    /// # Errors
    ///
    /// Database failures.
    pub async fn audit(&mut self, ctx: &AuditCtx, event: NewEvent<'_>) -> Result<(), DbError> {
        crate::audit::record_in(&mut self.tx, ctx, &event.org(self.org_id)).await
    }

    /// Commits the transaction and releases the lock.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn commit(self) -> Result<(), DbError> {
        self.tx.commit().await?;
        Ok(())
    }
}
