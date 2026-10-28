//! Pure authorization policy: `can(actor, action, resource) -> Decision`.
//!
//! No I/O, no clock, no caching. The caller loads the [`Actor`] (instance
//! operator flag, active organization membership, workspace role) from the
//! database **for every request**, so a role change takes effect on the next
//! request. The table below mirrors `docs/05-security/permission-matrix.md`;
//! anything not listed is denied.
//!
//! # Roles and precedence
//!
//! * The instance operator is **not** an organization role and has no implicit
//!   access to organization data: for organization-scoped resources only the
//!   actor's own active membership counts. An operator who is also a member of
//!   an organization has exactly the rights of that membership.
//! * Organization roles are totally ordered `Owner > Admin > Member`; exactly
//!   one role per user and organization, no merging. `Member` has no
//!   organization-level rights.
//! * Workspace roles (`Admin`, `Editor`, `Viewer`) are per workspace and only
//!   matter for workspace resources. An organization `Owner` or `Admin`
//!   manages every workspace of the organization.
//!
//! # Existence
//!
//! [`Decision::Hidden`] means "answer 404": the actor cannot see that the
//! resource exists (not a member, unknown id). [`Decision::Forbidden`] (403)
//! is only returned to actors who may already see the resource.
//!
//! # Type-level scope
//!
//! The only way to obtain a [`Permit`] is an `Allow` decision from [`can`];
//! the database layer builds its tenant scope from a permit, so a repository
//! call for an organization is impossible without a policy decision.

use uuid::Uuid;

/// Role inside an organization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OrgRole {
    /// Highest role; may assign every role.
    Owner,
    /// Manages members and settings but never touches owners.
    Admin,
    /// No organization-level rights.
    Member,
}

impl OrgRole {
    /// All roles.
    pub const ALL: [OrgRole; 3] = [OrgRole::Owner, OrgRole::Admin, OrgRole::Member];

    /// The stored / wire representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }

    /// Parses the stored / wire representation.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Self::Owner),
            "admin" => Some(Self::Admin),
            "member" => Some(Self::Member),
            _ => None,
        }
    }
}

/// Role inside a workspace (used from PR-007 on).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkspaceRole {
    /// Manages the workspace.
    Admin,
    /// Creates projects, edits and soft-deletes files.
    Editor,
    /// Read-only.
    Viewer,
}

impl WorkspaceRole {
    /// The stored / wire representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Editor => "editor",
            Self::Viewer => "viewer",
        }
    }

    /// Parses the stored / wire representation.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "admin" => Some(Self::Admin),
            "editor" => Some(Self::Editor),
            "viewer" => Some(Self::Viewer),
            _ => None,
        }
    }
}

/// Active membership of the actor in one organization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrgMembership {
    /// The organization.
    pub org_id: Uuid,
    /// The actor's role there.
    pub role: OrgRole,
}

/// Role of the actor in one workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceMembership {
    /// The workspace.
    pub workspace_id: Uuid,
    /// The actor's role there.
    pub role: WorkspaceRole,
}

/// Who is acting. Always loaded from the database for the current request;
/// never cached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    /// The authenticated, enabled account.
    pub user_id: Uuid,
    /// Whether the account is the instance operator.
    pub instance_operator: bool,
    /// Active membership in the organization the request is about, if any.
    pub membership: Option<OrgMembership>,
    /// Role in the workspace the request is about, if any.
    pub workspace: Option<WorkspaceMembership>,
}

/// What the actor wants to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// List the organizations the actor is a member of.
    OrganizationListOwn,
    /// Read an organization.
    OrganizationRead,
    /// Change organization settings (name).
    OrganizationUpdate,
    /// Create an organization (operator).
    OrganizationCreate,
    /// List organization metadata of the whole instance (operator).
    OrganizationListAll,
    /// List the members of an organization.
    MemberList,
    /// Add an existing user as member.
    MemberAdd,
    /// Change the role of a member.
    MemberChangeRole,
    /// Remove a member (or leave).
    MemberRemove,
    /// Disable a user account (operator).
    UserDisable,
    /// Enable a user account (operator).
    UserEnable,
    /// Issue a password reset link (operator).
    PasswordResetIssue,
    /// Void pending password reset links (operator).
    PasswordResetVoid,
    /// Read a workspace.
    WorkspaceRead,
    /// Manage a workspace.
    WorkspaceManage,
}

impl Action {
    /// Whether the action changes data. A scope built from a read permit can
    /// not be used for repository writes.
    #[must_use]
    pub fn is_write(self) -> bool {
        match self {
            Self::OrganizationListOwn
            | Self::OrganizationRead
            | Self::OrganizationListAll
            | Self::MemberList
            | Self::WorkspaceRead => false,
            Self::OrganizationUpdate
            | Self::OrganizationCreate
            | Self::MemberAdd
            | Self::MemberChangeRole
            | Self::MemberRemove
            | Self::UserDisable
            | Self::UserEnable
            | Self::PasswordResetIssue
            | Self::PasswordResetVoid
            | Self::WorkspaceManage => true,
        }
    }

    /// Every action. Tests assert that the policy table covers all of them.
    pub const ALL: [Action; 15] = [
        Action::OrganizationListOwn,
        Action::OrganizationRead,
        Action::OrganizationUpdate,
        Action::OrganizationCreate,
        Action::OrganizationListAll,
        Action::MemberList,
        Action::MemberAdd,
        Action::MemberChangeRole,
        Action::MemberRemove,
        Action::UserDisable,
        Action::UserEnable,
        Action::PasswordResetIssue,
        Action::PasswordResetVoid,
        Action::WorkspaceRead,
        Action::WorkspaceManage,
    ];
}

/// The member a member action is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberTarget {
    /// The member's user id.
    pub user_id: Uuid,
    /// The member's current role.
    pub role: OrgRole,
}

/// What the action is applied to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    /// The instance itself (operator actions, own listings).
    Instance,
    /// An organization.
    Organization {
        /// The organization.
        org_id: Uuid,
    },
    /// Membership data of an organization.
    Member {
        /// The organization.
        org_id: Uuid,
        /// The existing member the action targets; `None` for adding, or if
        /// the target is not a member.
        target: Option<MemberTarget>,
        /// The role being assigned (add, change role).
        role: Option<OrgRole>,
    },
    /// A workspace of an organization.
    Workspace {
        /// The organization.
        org_id: Uuid,
        /// The workspace.
        workspace_id: Uuid,
    },
}

/// Proof of an `Allow` decision. Cannot be constructed outside this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permit {
    org_id: Option<Uuid>,
    action: Action,
}

impl Permit {
    /// The organization the permit is limited to, if the action was
    /// organization-scoped.
    #[must_use]
    pub fn org_id(&self) -> Option<Uuid> {
        self.org_id
    }

    /// The permitted action.
    #[must_use]
    pub fn action(&self) -> Action {
        self.action
    }
}

/// The policy decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Allowed; carries the permit needed for scoped data access.
    Allow(Permit),
    /// The actor may see the resource but not do this (403).
    Forbidden,
    /// The actor must not learn whether the resource exists (404).
    Hidden,
}

impl Decision {
    /// Whether the decision is `Allow`.
    #[must_use]
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow(_))
    }

    /// The permit of an `Allow` decision.
    #[must_use]
    pub fn permit(self) -> Option<Permit> {
        match self {
            Self::Allow(permit) => Some(permit),
            _ => None,
        }
    }
}

fn allow(action: Action, org_id: Option<Uuid>) -> Decision {
    Decision::Allow(Permit { org_id, action })
}

/// Role of the actor in `org_id`, only if the loaded membership is for that
/// organization.
fn role_in(actor: &Actor, org_id: Uuid) -> Option<OrgRole> {
    actor
        .membership
        .filter(|m| m.org_id == org_id)
        .map(|m| m.role)
}

/// Decides whether `actor` may perform `action` on `resource`.
///
/// Deny by default: a combination of action and resource kind that is not
/// listed is `Forbidden`.
#[must_use]
pub fn can(actor: &Actor, action: Action, resource: &Resource) -> Decision {
    use Action as A;
    match (action, resource) {
        // ---- instance level ----
        (A::OrganizationListOwn, Resource::Instance) => allow(action, None),
        (
            A::OrganizationCreate
            | A::OrganizationListAll
            | A::UserDisable
            | A::UserEnable
            | A::PasswordResetIssue
            | A::PasswordResetVoid,
            Resource::Instance,
        ) => {
            if actor.instance_operator {
                allow(action, None)
            } else {
                Decision::Forbidden
            }
        }

        // ---- organization ----
        (A::OrganizationRead, Resource::Organization { org_id }) => match role_in(actor, *org_id) {
            Some(_) => allow(action, Some(*org_id)),
            None => Decision::Hidden,
        },
        (A::OrganizationUpdate, Resource::Organization { org_id }) => {
            match role_in(actor, *org_id) {
                Some(OrgRole::Owner | OrgRole::Admin) => allow(action, Some(*org_id)),
                Some(OrgRole::Member) => Decision::Forbidden,
                None => Decision::Hidden,
            }
        }

        // ---- members ----
        (A::MemberList, Resource::Member { org_id, .. }) => match role_in(actor, *org_id) {
            Some(_) => allow(action, Some(*org_id)),
            None => Decision::Hidden,
        },
        (A::MemberAdd, Resource::Member { org_id, role, .. }) => {
            match (role_in(actor, *org_id), role) {
                (None, _) => Decision::Hidden,
                (Some(OrgRole::Owner), Some(_)) => allow(action, Some(*org_id)),
                // An admin never creates an owner.
                (Some(OrgRole::Admin), Some(OrgRole::Admin | OrgRole::Member)) => {
                    allow(action, Some(*org_id))
                }
                (Some(_), _) => Decision::Forbidden,
            }
        }
        (
            A::MemberChangeRole,
            Resource::Member {
                org_id,
                target,
                role,
            },
        ) => {
            match (role_in(actor, *org_id), target, role) {
                (None, _, _) => Decision::Hidden,
                (Some(OrgRole::Member), _, _) => Decision::Forbidden,
                // Whether the target is a member is only revealed to managers.
                (Some(_), None, _) => Decision::Hidden,
                (Some(_), Some(_), None) => Decision::Forbidden,
                (Some(OrgRole::Owner), Some(_), Some(_)) => allow(action, Some(*org_id)),
                (Some(OrgRole::Admin), Some(target), Some(new_role)) => {
                    // Admins manage members and admins (including themselves)
                    // but never touch owners or promote to owner.
                    if target.role != OrgRole::Owner && *new_role != OrgRole::Owner {
                        allow(action, Some(*org_id))
                    } else {
                        Decision::Forbidden
                    }
                }
            }
        }
        (A::MemberRemove, Resource::Member { org_id, target, .. }) => {
            match (role_in(actor, *org_id), target) {
                (None, _) => Decision::Hidden,
                (Some(OrgRole::Member), None) => Decision::Forbidden,
                (Some(OrgRole::Member), Some(target)) => {
                    // Leaving is allowed for everybody.
                    if target.user_id == actor.user_id {
                        allow(action, Some(*org_id))
                    } else {
                        Decision::Forbidden
                    }
                }
                (Some(_), None) => Decision::Hidden,
                (Some(OrgRole::Owner), Some(_)) => allow(action, Some(*org_id)),
                (Some(OrgRole::Admin), Some(target)) => {
                    if target.role == OrgRole::Owner {
                        Decision::Forbidden
                    } else {
                        allow(action, Some(*org_id))
                    }
                }
            }
        }

        // ---- workspaces (routes arrive with PR-007) ----
        (
            A::WorkspaceRead,
            Resource::Workspace {
                org_id,
                workspace_id,
            },
        ) => {
            let ws_role = actor
                .workspace
                .filter(|w| w.workspace_id == *workspace_id)
                .map(|w| w.role);
            match (role_in(actor, *org_id), ws_role) {
                (None, _) => Decision::Hidden,
                (Some(OrgRole::Owner | OrgRole::Admin), _) | (Some(OrgRole::Member), Some(_)) => {
                    allow(action, Some(*org_id))
                }
                // A plain member must not learn about workspaces they are not in.
                (Some(OrgRole::Member), None) => Decision::Hidden,
            }
        }
        (
            A::WorkspaceManage,
            Resource::Workspace {
                org_id,
                workspace_id,
            },
        ) => {
            let ws_role = actor
                .workspace
                .filter(|w| w.workspace_id == *workspace_id)
                .map(|w| w.role);
            match (role_in(actor, *org_id), ws_role) {
                (None, _) => Decision::Hidden,
                (Some(OrgRole::Owner | OrgRole::Admin), _)
                | (Some(OrgRole::Member), Some(WorkspaceRole::Admin)) => {
                    allow(action, Some(*org_id))
                }
                (Some(OrgRole::Member), Some(WorkspaceRole::Editor | WorkspaceRole::Viewer)) => {
                    Decision::Forbidden
                }
                (Some(OrgRole::Member), None) => Decision::Hidden,
            }
        }

        // Anything else (action applied to the wrong kind of resource) is denied.
        _ => Decision::Forbidden,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const ORG: Uuid = Uuid::from_u128(1);
    const OTHER_ORG: Uuid = Uuid::from_u128(2);
    const WS: Uuid = Uuid::from_u128(10);
    const ME: Uuid = Uuid::from_u128(100);
    const OTHER: Uuid = Uuid::from_u128(101);

    fn actor(role: Option<OrgRole>, operator: bool) -> Actor {
        Actor {
            user_id: ME,
            instance_operator: operator,
            membership: role.map(|role| OrgMembership { org_id: ORG, role }),
            workspace: None,
        }
    }

    fn with_ws(mut actor: Actor, role: WorkspaceRole) -> Actor {
        actor.workspace = Some(WorkspaceMembership {
            workspace_id: WS,
            role,
        });
        actor
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Want {
        Allow,
        Forbidden,
        Hidden,
    }

    fn outcome(decision: Decision) -> Want {
        match decision {
            Decision::Allow(_) => Want::Allow,
            Decision::Forbidden => Want::Forbidden,
            Decision::Hidden => Want::Hidden,
        }
    }

    use Want::{Allow as Y, Forbidden as F, Hidden as H};

    fn org() -> Resource {
        Resource::Organization { org_id: ORG }
    }

    fn member(target: Option<(Uuid, OrgRole)>, role: Option<OrgRole>) -> Resource {
        Resource::Member {
            org_id: ORG,
            target: target.map(|(user_id, role)| MemberTarget { user_id, role }),
            role,
        }
    }

    // Rows: [non-member, Member, Admin, Owner]
    fn check(action: Action, resource: Resource, expected: [Want; 4]) {
        let roles = [
            None,
            Some(OrgRole::Member),
            Some(OrgRole::Admin),
            Some(OrgRole::Owner),
        ];
        for (role, want) in roles.into_iter().zip(expected) {
            for operator in [false, true] {
                // The operator flag never changes organization-scoped decisions.
                let got = outcome(can(&actor(role, operator), action, &resource));
                assert_eq!(
                    got, want,
                    "{action:?} role={role:?} operator={operator} {resource:?}"
                );
            }
        }
    }

    #[test]
    fn organization_read_and_update() {
        check(Action::OrganizationRead, org(), [H, Y, Y, Y]);
        check(Action::OrganizationUpdate, org(), [H, F, Y, Y]);
    }

    #[test]
    fn member_list() {
        check(Action::MemberList, member(None, None), [H, Y, Y, Y]);
    }

    #[test]
    fn member_add_by_assigned_role() {
        check(
            Action::MemberAdd,
            member(None, Some(OrgRole::Member)),
            [H, F, Y, Y],
        );
        check(
            Action::MemberAdd,
            member(None, Some(OrgRole::Admin)),
            [H, F, Y, Y],
        );
        // Only an owner may create owners.
        check(
            Action::MemberAdd,
            member(None, Some(OrgRole::Owner)),
            [H, F, F, Y],
        );
        // Missing role: nothing to assign.
        check(Action::MemberAdd, member(None, None), [H, F, F, F]);
    }

    #[test]
    fn member_change_role_matrix() {
        use OrgRole::{Admin, Member, Owner};
        // target role x new role, rows [non-member, Member, Admin, Owner]
        for target in [Member, Admin] {
            for new in [Member, Admin] {
                check(
                    Action::MemberChangeRole,
                    member(Some((OTHER, target)), Some(new)),
                    [H, F, Y, Y],
                );
            }
            // Admin may not promote to owner; owner may.
            check(
                Action::MemberChangeRole,
                member(Some((OTHER, target)), Some(Owner)),
                [H, F, F, Y],
            );
        }
        // Nobody below owner touches owners (demote, keep, or otherwise).
        for new in [Member, Admin, Owner] {
            check(
                Action::MemberChangeRole,
                member(Some((OTHER, Owner)), Some(new)),
                [H, F, F, Y],
            );
        }
        // Unknown target: only managers get to know (404), members get 403.
        check(
            Action::MemberChangeRole,
            member(None, Some(Member)),
            [H, F, H, H],
        );
        // No role to assign.
        check(
            Action::MemberChangeRole,
            member(Some((OTHER, Member)), None),
            [H, F, F, F],
        );
    }

    #[test]
    fn admin_may_demote_themselves_but_not_become_owner() {
        let admin = actor(Some(OrgRole::Admin), false);
        let me = member(Some((ME, OrgRole::Admin)), Some(OrgRole::Member));
        assert!(can(&admin, Action::MemberChangeRole, &me).is_allow());
        let up = member(Some((ME, OrgRole::Admin)), Some(OrgRole::Owner));
        assert_eq!(
            can(&admin, Action::MemberChangeRole, &up),
            Decision::Forbidden
        );
    }

    #[test]
    fn member_remove_matrix() {
        use OrgRole::{Admin, Member, Owner};
        // Removing somebody else.
        check(
            Action::MemberRemove,
            member(Some((OTHER, Member)), None),
            [H, F, Y, Y],
        );
        check(
            Action::MemberRemove,
            member(Some((OTHER, Admin)), None),
            [H, F, Y, Y],
        );
        check(
            Action::MemberRemove,
            member(Some((OTHER, Owner)), None),
            [H, F, F, Y],
        );
        // Unknown target.
        check(Action::MemberRemove, member(None, None), [H, F, H, H]);
        // Leaving: everybody who is a member may leave (last-owner rule is enforced with the data).
        for role in [Member, Admin, Owner] {
            let me = member(Some((ME, role)), None);
            assert!(
                can(&actor(Some(role), false), Action::MemberRemove, &me).is_allow(),
                "{role:?}"
            );
        }
    }

    #[test]
    fn instance_actions_are_operator_only() {
        for action in [
            Action::OrganizationCreate,
            Action::OrganizationListAll,
            Action::UserDisable,
            Action::UserEnable,
            Action::PasswordResetIssue,
            Action::PasswordResetVoid,
        ] {
            for role in [
                None,
                Some(OrgRole::Member),
                Some(OrgRole::Admin),
                Some(OrgRole::Owner),
            ] {
                assert_eq!(
                    outcome(can(&actor(role, false), action, &Resource::Instance)),
                    F,
                    "{action:?} {role:?}"
                );
                assert_eq!(
                    outcome(can(&actor(role, true), action, &Resource::Instance)),
                    Y,
                    "{action:?} {role:?}"
                );
            }
        }
        // Everybody authenticated lists their own organizations.
        assert!(
            can(
                &actor(None, false),
                Action::OrganizationListOwn,
                &Resource::Instance
            )
            .is_allow()
        );
    }

    #[test]
    fn operator_has_no_implicit_access_to_organization_data() {
        let operator = actor(None, true);
        for (action, resource) in [
            (Action::OrganizationRead, org()),
            (Action::OrganizationUpdate, org()),
            (Action::MemberList, member(None, None)),
            (Action::MemberAdd, member(None, Some(OrgRole::Member))),
            (
                Action::MemberRemove,
                member(Some((OTHER, OrgRole::Member)), None),
            ),
        ] {
            assert_eq!(
                can(&operator, action, &resource),
                Decision::Hidden,
                "{action:?}"
            );
        }
    }

    #[test]
    fn membership_of_another_organization_grants_nothing() {
        let mut a = actor(Some(OrgRole::Owner), false);
        a.membership = Some(OrgMembership {
            org_id: OTHER_ORG,
            role: OrgRole::Owner,
        });
        for (action, resource) in [
            (Action::OrganizationRead, org()),
            (Action::OrganizationUpdate, org()),
            (Action::MemberList, member(None, None)),
            (Action::MemberAdd, member(None, Some(OrgRole::Member))),
            (
                Action::MemberChangeRole,
                member(Some((OTHER, OrgRole::Member)), Some(OrgRole::Admin)),
            ),
            (
                Action::MemberRemove,
                member(Some((OTHER, OrgRole::Member)), None),
            ),
            (
                Action::WorkspaceRead,
                Resource::Workspace {
                    org_id: ORG,
                    workspace_id: WS,
                },
            ),
            (
                Action::WorkspaceManage,
                Resource::Workspace {
                    org_id: ORG,
                    workspace_id: WS,
                },
            ),
        ] {
            assert_eq!(can(&a, action, &resource), Decision::Hidden, "{action:?}");
        }
    }

    #[test]
    fn workspace_matrix() {
        let res = Resource::Workspace {
            org_id: ORG,
            workspace_id: WS,
        };
        // No workspace role: owner/admin see and manage everything, member is blind.
        check(Action::WorkspaceRead, res, [H, H, Y, Y]);
        check(Action::WorkspaceManage, res, [H, H, Y, Y]);
        let member_actor = |ws: WorkspaceRole| with_ws(actor(Some(OrgRole::Member), false), ws);
        for ws in [
            WorkspaceRole::Admin,
            WorkspaceRole::Editor,
            WorkspaceRole::Viewer,
        ] {
            assert!(
                can(&member_actor(ws), Action::WorkspaceRead, &res).is_allow(),
                "{ws:?}"
            );
        }
        assert!(
            can(
                &member_actor(WorkspaceRole::Admin),
                Action::WorkspaceManage,
                &res
            )
            .is_allow()
        );
        assert_eq!(
            can(
                &member_actor(WorkspaceRole::Editor),
                Action::WorkspaceManage,
                &res
            ),
            Decision::Forbidden
        );
        assert_eq!(
            can(
                &member_actor(WorkspaceRole::Viewer),
                Action::WorkspaceManage,
                &res
            ),
            Decision::Forbidden
        );
        // A role in a different workspace does not count.
        let mut elsewhere = actor(Some(OrgRole::Member), false);
        elsewhere.workspace = Some(WorkspaceMembership {
            workspace_id: Uuid::from_u128(11),
            role: WorkspaceRole::Admin,
        });
        assert_eq!(
            can(&elsewhere, Action::WorkspaceManage, &res),
            Decision::Hidden
        );
        // A workspace role without organization membership grants nothing.
        let ghost = with_ws(actor(None, false), WorkspaceRole::Admin);
        assert_eq!(can(&ghost, Action::WorkspaceRead, &res), Decision::Hidden);
    }

    #[test]
    fn wrong_resource_kind_is_denied() {
        let a = actor(Some(OrgRole::Owner), true);
        assert_eq!(
            can(&a, Action::OrganizationRead, &Resource::Instance),
            Decision::Forbidden
        );
        assert_eq!(can(&a, Action::UserDisable, &org()), Decision::Forbidden);
        assert_eq!(can(&a, Action::MemberAdd, &org()), Decision::Forbidden);
    }

    #[test]
    fn permits_carry_the_organization_only_for_scoped_actions() {
        let owner = actor(Some(OrgRole::Owner), true);
        let p = can(&owner, Action::OrganizationRead, &org())
            .permit()
            .unwrap();
        assert_eq!(p.org_id(), Some(ORG));
        assert_eq!(p.action(), Action::OrganizationRead);
        let p = can(&owner, Action::OrganizationCreate, &Resource::Instance)
            .permit()
            .unwrap();
        assert_eq!(p.org_id(), None);
        assert_eq!(
            can(&actor(None, false), Action::OrganizationRead, &org()).permit(),
            None
        );
    }

    /// Every action must be exercised by the tests above: adding an `Action`
    /// variant without extending `Action::ALL` fails to compile through the
    /// exhaustive match below, and a variant missing from this list fails here.
    #[test]
    fn every_action_is_covered_by_the_tables() {
        let covered: HashSet<Action> = [
            Action::OrganizationListOwn,
            Action::OrganizationRead,
            Action::OrganizationUpdate,
            Action::OrganizationCreate,
            Action::OrganizationListAll,
            Action::MemberList,
            Action::MemberAdd,
            Action::MemberChangeRole,
            Action::MemberRemove,
            Action::UserDisable,
            Action::UserEnable,
            Action::PasswordResetIssue,
            Action::PasswordResetVoid,
            Action::WorkspaceRead,
            Action::WorkspaceManage,
        ]
        .into_iter()
        .collect();
        for action in Action::ALL {
            // Exhaustive match: a new variant breaks the build here.
            match action {
                Action::OrganizationListOwn
                | Action::OrganizationRead
                | Action::OrganizationUpdate
                | Action::OrganizationCreate
                | Action::OrganizationListAll
                | Action::MemberList
                | Action::MemberAdd
                | Action::MemberChangeRole
                | Action::MemberRemove
                | Action::UserDisable
                | Action::UserEnable
                | Action::PasswordResetIssue
                | Action::PasswordResetVoid
                | Action::WorkspaceRead
                | Action::WorkspaceManage => {}
            }
            assert!(
                covered.contains(&action),
                "{action:?} has no policy table row"
            );
        }
        assert_eq!(covered.len(), Action::ALL.len());
    }

    #[test]
    fn write_actions_are_classified() {
        let reads: Vec<_> = Action::ALL.into_iter().filter(|a| !a.is_write()).collect();
        assert_eq!(
            reads,
            [
                Action::OrganizationListOwn,
                Action::OrganizationRead,
                Action::OrganizationListAll,
                Action::MemberList,
                Action::WorkspaceRead
            ]
        );
    }

    #[test]
    fn role_strings_round_trip() {
        for role in OrgRole::ALL {
            assert_eq!(OrgRole::parse(role.as_str()), Some(role));
        }
        for role in [
            WorkspaceRole::Admin,
            WorkspaceRole::Editor,
            WorkspaceRole::Viewer,
        ] {
            assert_eq!(WorkspaceRole::parse(role.as_str()), Some(role));
        }
        assert_eq!(OrgRole::parse("god"), None);
        assert_eq!(OrgRole::parse("Owner"), None);
        assert_eq!(WorkspaceRole::parse(""), None);
    }
}
