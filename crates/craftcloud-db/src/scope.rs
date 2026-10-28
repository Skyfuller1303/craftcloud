//! Tenant scope token for repository calls.

use uuid::Uuid;

/// Proof-by-convention that the caller has already authorized access to one
/// organization (tenant).
///
/// **Warning:** this type performs no authorization itself. Construct it only
/// after the caller's membership/permission in `org_id` has been verified by
/// the policy layer (arrives with PR-006). Never build it from a client
/// supplied identifier alone. Every tenant-bound repository function takes
/// `&OrgScope` and filters by its `org_id` in SQL, so a wrong id can never
/// reach another tenant's rows, but a wrongly *authorized* scope still opens
/// that whole tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrgScope {
    org_id: Uuid,
}

impl OrgScope {
    /// Creates a scope for `org_id`. The caller asserts that access to this
    /// organization has been authorized (see the type documentation).
    #[must_use]
    pub fn caller_has_authorized(org_id: Uuid) -> Self {
        Self { org_id }
    }

    /// The organization this scope is limited to.
    #[must_use]
    pub fn org_id(&self) -> Uuid {
        self.org_id
    }
}
