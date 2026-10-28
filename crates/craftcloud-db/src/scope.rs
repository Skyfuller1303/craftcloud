//! Tenant scope token for repository calls.

use craftcloud_core::policy::{Action, Permit};
use uuid::Uuid;

use crate::DbError;

/// Limits repository calls to one organization (tenant) **and** to the kind of
/// access that was permitted.
///
/// A scope can only be built from a [`Permit`], and a permit only comes out of
/// an `Allow` decision of the policy engine (`craftcloud_core::policy::can`).
/// There is no other constructor, so a repository call for an organization is
/// impossible without a positive policy decision for that organization. The
/// scope remembers the permitted action: every repository function that
/// changes data requires the scope of exactly its own action (`MemberAdd` for
/// adding a member, `MemberRemove` for removing one, ...), so neither a read
/// permit nor the permit of another action can drive a write. Every tenant-bound
/// repository function filters by the scope's `org_id` in SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrgScope {
    org_id: Uuid,
    action: Action,
}

impl OrgScope {
    /// The scope of an `Allow` decision. `None` if the permitted action was
    /// not organization-scoped (instance-level actions).
    #[must_use]
    pub fn from_permit(permit: &Permit) -> Option<Self> {
        permit.org_id().map(|org_id| Self {
            org_id,
            action: permit.action(),
        })
    }

    /// The organization this scope is limited to.
    #[must_use]
    pub fn org_id(&self) -> Uuid {
        self.org_id
    }

    /// The action the policy permitted.
    #[must_use]
    pub fn action(&self) -> Action {
        self.action
    }

    /// Whether the permitted action changes data.
    #[must_use]
    pub fn is_write(&self) -> bool {
        self.action.is_write()
    }

    /// Requires the scope to stem from exactly `action`: every writing
    /// repository function names the action it implements, so a permit for one
    /// action (or a read permit) cannot drive another operation.
    pub(crate) fn require(&self, action: Action) -> Result<(), DbError> {
        if self.action == action {
            Ok(())
        } else {
            Err(DbError::Internal)
        }
    }
}
