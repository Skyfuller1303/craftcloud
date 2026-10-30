//! Invitations (CC-F03).
//!
//! An invitation is its own record, bound to an organization and an e-mail
//! address; it becomes an organization (and optionally workspace) membership
//! only on acceptance. Membership rows with status `invited` are **not** used
//! for invitations. Only the SHA-256 hash of the secret token is stored.
//!
//! Creation and revocation run in an [`OrgTx`] (organization row lock, fresh
//! roles, policy decision). Acceptance ([`accept`]) re-evaluates the policy
//! with the **inviter's current** roles, so a stale invitation of a demoted or
//! disabled inviter can never escalate privileges.
//!
//! Lock order of acceptance: organization row, then users rows (inviter,
//! target account), then the invitation row. This is compatible with the
//! account paths (users first, they never lock organization rows) and with
//! membership changes (organization first, then users).

use craftcloud_core::policy::{
    Action, Actor, InvitationSpec, OrgMembership, OrgRole, Resource, WorkspaceMembership,
    WorkspaceRole, can,
};
use sqlx::{FromRow, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::audit::{AuditCtx, Detail, NewEvent};
use crate::members::OrgTx;
use crate::organizations::{AfterKey, clamp_page};
use crate::{DbError, OrgScope};

/// Maximum number of open (pending, unexpired) invitations per organization.
pub const MAX_PENDING_PER_ORG: i64 = 1000;

/// A stored invitation (never contains the token).
#[derive(Debug, Clone, FromRow)]
pub struct Invitation {
    /// Identifier.
    pub id: Uuid,
    /// Organization.
    pub org_id: Uuid,
    /// Invited (normalized) e-mail address.
    pub email: String,
    /// Organization role granted (`admin` or `member`).
    pub org_role: String,
    /// Workspace granted, if any.
    pub workspace_id: Option<Uuid>,
    /// Workspace role granted, if any.
    pub workspace_role: Option<String>,
    /// Who created it.
    pub created_by: Uuid,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Expiry time.
    pub expires_at: OffsetDateTime,
    /// Acceptance time.
    pub accepted_at: Option<OffsetDateTime>,
    /// Revocation time.
    pub revoked_at: Option<OffsetDateTime>,
    /// Time the inviter's rights stopped covering it.
    pub invalidated_at: Option<OffsetDateTime>,
}

impl Invitation {
    /// Status at `now`: `accepted`, `revoked`, `invalid`, `expired` or `pending`.
    #[must_use]
    pub fn status(&self, now: OffsetDateTime) -> &'static str {
        if self.accepted_at.is_some() {
            "accepted"
        } else if self.revoked_at.is_some() {
            "revoked"
        } else if self.invalidated_at.is_some() {
            "invalid"
        } else if self.expires_at <= now {
            "expired"
        } else {
            "pending"
        }
    }

    fn spec(&self, workspace_admin_invites_enabled: bool) -> Option<InvitationSpec> {
        let org_role = OrgRole::parse(&self.org_role)?;
        let workspace = if org_role == OrgRole::Guest {
            None
        } else {
            match (self.workspace_id, self.workspace_role.as_deref()) {
                (Some(id), Some(role)) => Some((id, WorkspaceRole::parse(role)?)),
                _ => None,
            }
        };
        Some(InvitationSpec {
            org_role,
            workspace,
            guest_workspace: if org_role == OrgRole::Guest {
                self.workspace_id
            } else {
                None
            },
            workspace_admin_invites_enabled,
            created_by: Some(self.created_by),
        })
    }
}

const COLUMNS: &str = "id, org_id, normalized_email AS email, org_role, workspace_id, workspace_role, \
     created_by_user_id AS created_by, created_at, expires_at, accepted_at, revoked_at, invalidated_at";

/// Input for [`OrgTx::invitation_create`].
#[derive(Debug, Clone)]
pub struct NewInvitation {
    /// Normalized e-mail address of the invitee.
    pub email: String,
    /// Organization role granted.
    pub org_role: OrgRole,
    /// Workspace and role granted, if any.
    pub workspace: Option<(Uuid, WorkspaceRole)>,
    /// Guest invitation context, without a workspace role.
    pub guest_workspace: Option<Uuid>,
    /// SHA-256 of the secret token.
    pub token_hash: [u8; 32],
    /// The inviter.
    pub created_by: Uuid,
    /// Creation time.
    pub now: OffsetDateTime,
    /// Expiry time.
    pub expires_at: OffsetDateTime,
}

/// Result of [`OrgTx::invitation_create`].
#[derive(Debug, Clone)]
pub enum CreateInvitationOutcome {
    /// Stored.
    Created(Box<Invitation>),
    /// An open invitation for this address already exists (revoke it first).
    Pending,
    /// The organization has reached [`MAX_PENDING_PER_ORG`] open invitations.
    LimitReached,
    /// The workspace does not exist in this organization.
    WorkspaceMissing,
}

impl OrgTx {
    /// Fetches an invitation of this organization (any status), read under
    /// the organization lock.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn invitation_get(&mut self, id: Uuid) -> Result<Option<Invitation>, DbError> {
        Ok(sqlx::query_as::<_, Invitation>(&format!(
            "SELECT {COLUMNS} FROM invitations WHERE id = $1 AND org_id = $2"
        ))
        .bind(id)
        .bind(self.org_id)
        .fetch_optional(&mut *self.tx)
        .await?)
    }

    /// Builds the policy spec of a stored invitation with the current value of
    /// the organization setting.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn invitation_spec(
        &mut self,
        invitation: &Invitation,
    ) -> Result<Option<InvitationSpec>, DbError> {
        let enabled = self.workspace_admin_invites_enabled().await?;
        Ok(invitation.spec(enabled))
    }

    /// Creates an invitation: expired open invitations of the same address are
    /// closed first, then the per-organization cap and the one-open-invitation
    /// rule are checked, then the invitation is stored.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn invitation_create(
        &mut self,
        scope: &OrgScope,
        new: &NewInvitation,
    ) -> Result<CreateInvitationOutcome, DbError> {
        self.check(scope, Action::InvitationCreate)?;
        if let Some(workspace_id) = new.workspace.map(|(id, _)| id).or(new.guest_workspace)
            && !self.workspace_exists(workspace_id).await?
        {
            return Ok(CreateInvitationOutcome::WorkspaceMissing);
        }
        sqlx::query(
            "UPDATE invitations SET invalidated_at = $3 \
             WHERE org_id = $1 AND normalized_email = $2 AND accepted_at IS NULL \
               AND revoked_at IS NULL AND invalidated_at IS NULL AND expires_at <= $3",
        )
        .bind(self.org_id)
        .bind(&new.email)
        .bind(new.now)
        .execute(&mut *self.tx)
        .await?;
        let open: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM invitations WHERE org_id = $1 AND accepted_at IS NULL \
             AND revoked_at IS NULL AND invalidated_at IS NULL AND expires_at > $2",
        )
        .bind(self.org_id)
        .bind(new.now)
        .fetch_one(&mut *self.tx)
        .await?;
        if open >= MAX_PENDING_PER_ORG {
            return Ok(CreateInvitationOutcome::LimitReached);
        }
        let duplicate: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM invitations WHERE org_id = $1 AND normalized_email = $2 \
             AND accepted_at IS NULL AND revoked_at IS NULL AND invalidated_at IS NULL)",
        )
        .bind(self.org_id)
        .bind(&new.email)
        .fetch_one(&mut *self.tx)
        .await?;
        if duplicate {
            return Ok(CreateInvitationOutcome::Pending);
        }
        let inserted = sqlx::query_as::<_, Invitation>(&format!(
            "INSERT INTO invitations (id, org_id, normalized_email, org_role, workspace_id, \
                 workspace_role, token_hash, created_by_user_id, created_at, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING {COLUMNS}"
        ))
        .bind(Uuid::new_v4())
        .bind(self.org_id)
        .bind(&new.email)
        .bind(new.org_role.as_str())
        .bind(new.workspace.map(|(id, _)| id).or(new.guest_workspace))
        .bind(new.workspace.map(|(_, role)| role.as_str()))
        .bind(new.token_hash.as_slice())
        .bind(new.created_by)
        .bind(new.now)
        .bind(new.expires_at)
        .fetch_one(&mut *self.tx)
        .await;
        match inserted {
            Ok(invitation) => Ok(CreateInvitationOutcome::Created(Box::new(invitation))),
            // The unique index is the backstop for the check above.
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                Ok(CreateInvitationOutcome::Pending)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Revokes an open invitation. Returns whether one was revoked.
    ///
    /// # Errors
    ///
    /// Database failures only.
    pub async fn invitation_revoke(
        &mut self,
        scope: &OrgScope,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DbError> {
        self.check(scope, Action::InvitationRevoke)?;
        let revoked = sqlx::query(
            "UPDATE invitations SET revoked_at = $3 \
             WHERE id = $1 AND org_id = $2 AND accepted_at IS NULL AND revoked_at IS NULL \
               AND invalidated_at IS NULL",
        )
        .bind(id)
        .bind(self.org_id)
        .bind(now)
        .execute(&mut *self.tx)
        .await?
        .rows_affected();
        Ok(revoked == 1)
    }
}

/// Lists the invitations of the scoped organization (all statuses) ordered by
/// `(created_at, id)`, one page after `after`. The keyset holds no personal
/// data.
///
/// # Errors
///
/// Database failures only.
pub async fn list(
    pool: &PgPool,
    scope: &OrgScope,
    limit: u32,
    after: Option<AfterKey>,
) -> Result<Vec<Invitation>, DbError> {
    Ok(sqlx::query_as::<_, Invitation>(&format!(
        "SELECT {COLUMNS} FROM invitations WHERE org_id = $1 \
           AND ($2::timestamptz IS NULL OR (created_at, id) > ($2::timestamptz, $3::uuid)) \
         ORDER BY created_at, id LIMIT $4"
    ))
    .bind(scope.org_id())
    .bind(after.map(|(at, _)| at))
    .bind(after.map(|(_, id)| id))
    .bind(clamp_page(limit))
    .fetch_all(pool)
    .await?)
}

/// What an open invitation looks like to the acceptance endpoint.
#[derive(Debug, Clone)]
pub struct Peek {
    /// Invitation id.
    pub invitation_id: Uuid,
    /// Organization.
    pub org_id: Uuid,
    /// The invited (normalized) address.
    pub email: String,
}

/// Looks up an open (pending, unexpired) invitation by token hash without
/// locking. Unknown, used, revoked, invalidated and expired tokens are all
/// `None`.
///
/// # Errors
///
/// Database failures only.
pub async fn peek(
    pool: &PgPool,
    token_hash: &[u8; 32],
    now: OffsetDateTime,
) -> Result<Option<Peek>, DbError> {
    let row: Option<(Uuid, Uuid, String)> = sqlx::query_as(
        "SELECT id, org_id, normalized_email FROM invitations \
         WHERE token_hash = $1 AND accepted_at IS NULL AND revoked_at IS NULL \
           AND invalidated_at IS NULL AND expires_at > $2",
    )
    .bind(token_hash.as_slice())
    .bind(now)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(invitation_id, org_id, email)| Peek {
        invitation_id,
        org_id,
        email,
    }))
}

/// Who accepts.
#[derive(Debug, Clone)]
pub enum AcceptBy {
    /// Anonymous caller creating the account for the invited address.
    NewAccount {
        /// Display name of the new account.
        display_name: String,
        /// Argon2id PHC hash of the chosen password.
        password_hash: String,
    },
    /// The authenticated account with this id.
    Existing {
        /// The caller's account.
        session_user: Uuid,
    },
}

/// Result of [`accept`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptOutcome {
    /// Unknown, expired, used, revoked, invalidated (stale inviter) or the
    /// target account is disabled: one uniform answer.
    Invalid,
    /// An account exists for the address but the caller is not logged in.
    LoginRequired,
    /// The caller is logged in as a different account.
    EmailMismatch,
    /// Organization-only invitation for an existing active member; the
    /// invitation is **not** consumed.
    AlreadyMember,
    /// Accepted.
    Accepted {
        /// The account that joined.
        user_id: Uuid,
        /// The organization.
        org_id: Uuid,
        /// The workspace joined, if the invitation had one.
        workspace_id: Option<Uuid>,
        /// Whether the account was created by this acceptance.
        account_created: bool,
        /// The consumed invitation.
        invitation_id: Uuid,
        /// The user who created the invitation.
        inviter_id: Uuid,
        /// Whether the workspace grant was recorded (not lowered).
        workspace_role_applied: bool,
    },
}

/// Accepts an invitation in one transaction (see the module docs for the lock
/// order). The inviter's current rights are re-evaluated; if they no longer
/// cover the invitation it is marked invalid (persisted) and the answer is
/// [`AcceptOutcome::Invalid`].
///
/// # Errors
///
/// Database failures only; nothing changes on failure.
pub async fn accept(
    pool: &PgPool,
    token_hash: &[u8; 32],
    by: &AcceptBy,
    now: OffsetDateTime,
) -> Result<AcceptOutcome, DbError> {
    accept_inner(pool, token_hash, by, now, None).await
}

/// Like [`accept`], and records the `invitation.accept` audit event (actor:
/// the accepting account, which may be created by this very call) in the same
/// transaction. An invitation that cannot be accepted records nothing, so a
/// replayed acceptance never produces a second event.
///
/// # Errors
///
/// Database failures; a failing audit insert accepts nothing.
pub async fn accept_audited(
    pool: &PgPool,
    token_hash: &[u8; 32],
    by: &AcceptBy,
    now: OffsetDateTime,
    audit: &AuditCtx,
) -> Result<AcceptOutcome, DbError> {
    accept_inner(pool, token_hash, by, now, Some(audit)).await
}

async fn accept_inner(
    pool: &PgPool,
    token_hash: &[u8; 32],
    by: &AcceptBy,
    now: OffsetDateTime,
    audit: Option<&AuditCtx>,
) -> Result<AcceptOutcome, DbError> {
    // 1. Organization of the invitation (unlocked read), then the org lock.
    let head: Option<(Uuid, Uuid, String, Uuid)> = sqlx::query_as(
        "SELECT id, org_id, normalized_email, created_by_user_id FROM invitations WHERE token_hash = $1",
    )
    .bind(token_hash.as_slice())
    .fetch_optional(pool)
    .await?;
    let Some((invitation_id, org_id, email, inviter_id)) = head else {
        return Ok(AcceptOutcome::Invalid);
    };
    let Some(mut otx) = OrgTx::begin(pool, org_id).await? else {
        return Ok(AcceptOutcome::Invalid);
    };

    // 2. Users: inviter and target account (share locks).
    let inviter_disabled: Option<bool> =
        sqlx::query_scalar("SELECT disabled_at IS NOT NULL FROM users WHERE id = $1 FOR SHARE")
            .bind(inviter_id)
            .fetch_optional(&mut *otx.tx)
            .await?;
    let target: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT id, disabled_at IS NOT NULL FROM users WHERE normalized_email = $1 FOR SHARE",
    )
    .bind(&email)
    .fetch_optional(&mut *otx.tx)
    .await?;

    // 3. The invitation row, only if still open.
    let invitation: Option<Invitation> = sqlx::query_as::<_, Invitation>(&format!(
        "SELECT {COLUMNS} FROM invitations \
         WHERE id = $1 AND accepted_at IS NULL AND revoked_at IS NULL \
           AND invalidated_at IS NULL AND expires_at > $2 FOR UPDATE"
    ))
    .bind(invitation_id)
    .bind(now)
    .fetch_optional(&mut *otx.tx)
    .await?;
    let Some(invitation) = invitation else {
        return Ok(AcceptOutcome::Invalid);
    };

    // 4. Who is accepting.
    let (account, created) = match (&target, by) {
        (Some((_, true)), _) => return Ok(AcceptOutcome::Invalid),
        (Some((id, false)), AcceptBy::Existing { session_user }) if id == session_user => {
            (*id, false)
        }
        (Some(_), AcceptBy::Existing { .. }) | (None, AcceptBy::Existing { .. }) => {
            return Ok(AcceptOutcome::EmailMismatch);
        }
        (Some(_), AcceptBy::NewAccount { .. }) => return Ok(AcceptOutcome::LoginRequired),
        (None, AcceptBy::NewAccount { .. }) => (Uuid::new_v4(), true),
    };

    // 5. Re-evaluate the inviter's current rights.
    let inviter_ok = match inviter_disabled {
        Some(false) => inviter_still_allowed(&mut otx, org_id, inviter_id, &invitation).await?,
        _ => false,
    };
    if !inviter_ok {
        sqlx::query("UPDATE invitations SET invalidated_at = $2 WHERE id = $1")
            .bind(invitation_id)
            .bind(now)
            .execute(&mut *otx.tx)
            .await?;
        if let Some(audit) = audit {
            // The link died because its creator lost the right to invite: the
            // state change is recorded (no actor: nobody acted on purpose).
            otx.audit(
                &AuditCtx {
                    actor_id: None,
                    request_id: audit.request_id.clone(),
                },
                NewEvent::new("invitation.invalidate")
                    .target("invitation", invitation_id)
                    .workspace_opt(invitation.workspace_id)
                    .detail("inviter_id", Detail::Id(inviter_id)),
            )
            .await?;
        }
        otx.commit().await?;
        return Ok(AcceptOutcome::Invalid);
    }

    // 6. Effects.
    let Some(org_role) = OrgRole::parse(&invitation.org_role) else {
        return Ok(AcceptOutcome::Invalid);
    };
    let invitation_workspace_id = invitation.workspace_id;
    let workspace = if org_role == OrgRole::Guest {
        None
    } else {
        match (
            invitation.workspace_id,
            invitation.workspace_role.as_deref(),
        ) {
            (Some(id), Some(role)) => WorkspaceRole::parse(role).map(|r| (id, r)),
            _ => None,
        }
    };
    if created {
        let AcceptBy::NewAccount {
            display_name,
            password_hash,
        } = by
        else {
            return Ok(AcceptOutcome::Invalid);
        };
        let inserted = sqlx::query(
            "INSERT INTO users \
                 (id, normalized_email, display_name, password_hash, created_via_invitation_id) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(account)
        .bind(&email)
        .bind(display_name)
        .bind(password_hash)
        .bind(invitation_id)
        .execute(&mut *otx.tx)
        .await;
        match inserted {
            Ok(_) => {}
            // Another invitation created the account in the meantime.
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                return Ok(AcceptOutcome::LoginRequired);
            }
            Err(error) => return Err(error.into()),
        }
        sqlx::query(
            "INSERT INTO organization_members (org_id, user_id, role, status) VALUES ($1, $2, $3, 'active')",
        )
        .bind(org_id)
        .bind(account)
        .bind(org_role.as_str())
        .execute(&mut *otx.tx)
        .await?;
    } else {
        let existing: Option<(String, String)> = sqlx::query_as(
            "SELECT role, status FROM organization_members WHERE org_id = $1 AND user_id = $2",
        )
        .bind(org_id)
        .bind(account)
        .fetch_optional(&mut *otx.tx)
        .await?;
        match existing {
            Some((existing_role, status)) if status == "active" => {
                if existing_role == "guest" {
                    // An older invitation may carry a workspace role. Existing guests never gain it.
                    if workspace.is_some() {
                        return Ok(AcceptOutcome::AlreadyMember);
                    }
                }
                if workspace.is_none() {
                    // Nothing to grant: keep the invitation pending and revocable.
                    return Ok(AcceptOutcome::AlreadyMember);
                }
                // Existing active member: only the workspace part applies and the
                // organization role is never changed.
            }
            Some((role, _)) if role == "owner" => return Ok(AcceptOutcome::AlreadyMember),
            Some(_) => {
                sqlx::query(
                    "UPDATE organization_members SET role = $3, status = 'active', created_at = now() \
                     WHERE org_id = $1 AND user_id = $2",
                )
                .bind(org_id)
                .bind(account)
                .bind(org_role.as_str())
                .execute(&mut *otx.tx)
                .await?;
            }
            None => {
                sqlx::query(
                    "INSERT INTO organization_members (org_id, user_id, role, status) VALUES ($1, $2, $3, 'active')",
                )
                .bind(org_id)
                .bind(account)
                .bind(org_role.as_str())
                .execute(&mut *otx.tx)
                .await?;
            }
        }
    }
    let mut workspace_role_applied = false;
    if let Some((workspace_id, role)) = workspace {
        // Never lowers an existing, higher workspace role.
        let applied = sqlx::query(
            "INSERT INTO workspace_members (org_id, workspace_id, user_id, role) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (workspace_id, user_id) DO UPDATE SET role = EXCLUDED.role \
             WHERE (CASE EXCLUDED.role WHEN 'admin' THEN 3 WHEN 'editor' THEN 2 ELSE 1 END) \
                 > (CASE workspace_members.role WHEN 'admin' THEN 3 WHEN 'editor' THEN 2 ELSE 1 END)",
        )
        .bind(org_id)
        .bind(workspace_id)
        .bind(account)
        .bind(role.as_str())
        .execute(&mut *otx.tx)
        .await?
        .rows_affected();
        workspace_role_applied = applied == 1;
    }
    sqlx::query("UPDATE invitations SET accepted_at = $2, accepted_by_user_id = $3 WHERE id = $1")
        .bind(invitation_id)
        .bind(now)
        .bind(account)
        .execute(&mut *otx.tx)
        .await?;
    if let Some(audit) = audit {
        let by = AuditCtx {
            actor_id: Some(account),
            request_id: audit.request_id.clone(),
        };
        otx.audit(
            &by,
            NewEvent::new("invitation.accept")
                .target("invitation", invitation_id)
                .workspace_opt(workspace.map(|(id, _)| id))
                .detail("account_created", Detail::Flag(created))
                .detail(
                    "workspace_role_applied",
                    Detail::Flag(workspace_role_applied),
                ),
        )
        .await?;
    }
    otx.commit().await?;
    Ok(AcceptOutcome::Accepted {
        user_id: account,
        org_id,
        workspace_id: invitation_workspace_id,
        account_created: created,
        invitation_id,
        inviter_id,
        workspace_role_applied,
    })
}

/// Whether the inviter, with their roles **now**, may still create this
/// invitation under the current organization setting.
async fn inviter_still_allowed(
    otx: &mut OrgTx,
    org_id: Uuid,
    inviter_id: Uuid,
    invitation: &Invitation,
) -> Result<bool, DbError> {
    let Some(org_role) = otx.role_of(inviter_id).await? else {
        return Ok(false);
    };
    let workspace = match invitation.workspace_id {
        Some(workspace_id) => otx
            .workspace_role_of(workspace_id, inviter_id)
            .await?
            .map(|role| WorkspaceMembership { workspace_id, role }),
        None => None,
    };
    let actor = Actor {
        user_id: inviter_id,
        instance_operator: false,
        membership: Some(OrgMembership {
            org_id,
            role: org_role,
        }),
        workspace,
    };
    let enabled = otx.workspace_admin_invites_enabled().await?;
    let Some(spec) = invitation.spec(enabled) else {
        return Ok(false);
    };
    Ok(can(
        &actor,
        Action::InvitationCreate,
        &Resource::Invitation {
            org_id,
            spec: Some(spec),
        },
    )
    .is_allow())
}
