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
    /// List the accounts of the instance (operator only; account metadata, no organization data).
    UserListAll,
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
    /// Manage a workspace (rename).
    WorkspaceManage,
    /// Create a workspace in an organization.
    WorkspaceCreate,
    /// List the workspaces of an organization that the actor can see.
    WorkspaceList,
    /// List the members of a workspace.
    WorkspaceMemberList,
    /// Add an organization member to a workspace.
    WorkspaceMemberAdd,
    /// Change a workspace member's role.
    WorkspaceMemberChangeRole,
    /// Remove a workspace member (or leave).
    WorkspaceMemberRemove,
    /// List the invitations of an organization.
    InvitationList,
    /// Create an invitation.
    InvitationCreate,
    /// Revoke an invitation.
    InvitationRevoke,
    /// List the projects of a workspace.
    ProjectList,
    /// Read a project.
    ProjectRead,
    /// Create a project.
    ProjectCreate,
    /// Rename a project.
    ProjectRename,
    /// Move a project to the trash.
    ProjectDelete,
    /// Restore a project from the trash.
    ProjectRestore,
    /// List the files of a project (also the trash).
    FileList,
    /// Read file metadata.
    FileRead,
    /// Create a file (metadata).
    FileCreate,
    /// Rename a file.
    FileRename,
    /// Move a file to the trash.
    FileDelete,
    /// Restore a file from the trash.
    FileRestore,
    /// Create an upload session for a file.
    UploadCreate,
    /// Read an upload session.
    UploadRead,
    /// Send the content of an upload session.
    UploadWrite,
    /// Abort an upload session.
    UploadAbort,
    /// Set the storage quota of an organization (operator).
    OrganizationQuotaSet,
    /// Set the storage quota of a workspace.
    WorkspaceQuotaSet,
    /// Read the storage usage of an organization.
    UsageRead,
    /// List the versions of a file.
    VersionList,
    /// Read one version of a file.
    VersionRead,
    /// Download the content of a file version.
    FileDownload,
    /// Commit a stored upload as a new version of a file.
    VersionCommit,
    /// Create a new version from an older one.
    VersionRestore,
    /// Read the status of the maintenance job (operator).
    MaintenanceRead,
    /// Read and export the audit log of an organization (owner, admin).
    AuditRead,
    /// Read and export the instance-level audit log (operator).
    InstanceAuditRead,
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
            | Self::UserListAll
            | Self::MaintenanceRead
            | Self::AuditRead
            | Self::InstanceAuditRead
            | Self::MemberList
            | Self::WorkspaceRead
            | Self::WorkspaceList
            | Self::WorkspaceMemberList
            | Self::InvitationList
            | Self::ProjectList
            | Self::ProjectRead
            | Self::FileList
            | Self::FileRead
            | Self::UploadRead
            | Self::UsageRead
            | Self::VersionList
            | Self::VersionRead
            | Self::FileDownload => false,
            Self::OrganizationUpdate
            | Self::OrganizationCreate
            | Self::MemberAdd
            | Self::MemberChangeRole
            | Self::MemberRemove
            | Self::UserDisable
            | Self::UserEnable
            | Self::PasswordResetIssue
            | Self::PasswordResetVoid
            | Self::WorkspaceManage
            | Self::WorkspaceCreate
            | Self::WorkspaceMemberAdd
            | Self::WorkspaceMemberChangeRole
            | Self::WorkspaceMemberRemove
            | Self::InvitationCreate
            | Self::InvitationRevoke
            | Self::ProjectCreate
            | Self::ProjectRename
            | Self::ProjectDelete
            | Self::ProjectRestore
            | Self::FileCreate
            | Self::FileRename
            | Self::FileDelete
            | Self::FileRestore
            | Self::UploadCreate
            | Self::UploadWrite
            | Self::UploadAbort
            | Self::OrganizationQuotaSet
            | Self::WorkspaceQuotaSet
            | Self::VersionCommit
            | Self::VersionRestore => true,
        }
    }

    /// Every action. Tests assert that the policy table covers all of them.
    pub const ALL: [Action; 52] = [
        Action::OrganizationListOwn,
        Action::OrganizationRead,
        Action::OrganizationUpdate,
        Action::OrganizationCreate,
        Action::OrganizationListAll,
        Action::UserListAll,
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
        Action::WorkspaceCreate,
        Action::WorkspaceList,
        Action::WorkspaceMemberList,
        Action::WorkspaceMemberAdd,
        Action::WorkspaceMemberChangeRole,
        Action::WorkspaceMemberRemove,
        Action::InvitationList,
        Action::InvitationCreate,
        Action::InvitationRevoke,
        Action::ProjectList,
        Action::ProjectRead,
        Action::ProjectCreate,
        Action::ProjectRename,
        Action::ProjectDelete,
        Action::ProjectRestore,
        Action::FileList,
        Action::FileRead,
        Action::FileCreate,
        Action::FileRename,
        Action::FileDelete,
        Action::FileRestore,
        Action::UploadCreate,
        Action::UploadRead,
        Action::UploadWrite,
        Action::UploadAbort,
        Action::OrganizationQuotaSet,
        Action::WorkspaceQuotaSet,
        Action::UsageRead,
        Action::VersionList,
        Action::VersionRead,
        Action::FileDownload,
        Action::VersionCommit,
        Action::VersionRestore,
        Action::MaintenanceRead,
        Action::AuditRead,
        Action::InstanceAuditRead,
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

/// The workspace member an action targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceMemberTarget {
    /// The member's user id.
    pub user_id: Uuid,
    /// The member's current workspace role.
    pub role: WorkspaceRole,
}

/// What an invitation grants (and who created it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvitationSpec {
    /// Organization role granted (an invitation never grants `owner`).
    pub org_role: OrgRole,
    /// Workspace and workspace role granted, if any.
    pub workspace: Option<(Uuid, WorkspaceRole)>,
    /// The organization setting that lets workspace admins invite.
    pub workspace_admin_invites_enabled: bool,
    /// Who created the invitation (relevant for revoking).
    pub created_by: Option<Uuid>,
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
    /// Membership data of a workspace.
    WorkspaceMember {
        /// The organization.
        org_id: Uuid,
        /// The workspace.
        workspace_id: Uuid,
        /// The existing member the action targets; `None` for adding, or if
        /// the target is not a member.
        target: Option<WorkspaceMemberTarget>,
        /// The workspace role being assigned (add, change role).
        role: Option<WorkspaceRole>,
    },
    /// An upload session of a workspace, created by `creator`.
    Upload {
        /// The organization.
        org_id: Uuid,
        /// The workspace.
        workspace_id: Uuid,
        /// Who created the session.
        creator: Uuid,
    },
    /// Invitations of an organization. `spec` is `None` for listing.
    Invitation {
        /// The organization.
        org_id: Uuid,
        /// What the invitation grants.
        spec: Option<InvitationSpec>,
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

/// Role of the actor in the given workspace, only if the loaded workspace
/// membership is for that workspace.
fn workspace_role_in(actor: &Actor, workspace_id: Uuid) -> Option<WorkspaceRole> {
    actor
        .workspace
        .filter(|w| w.workspace_id == workspace_id)
        .map(|w| w.role)
}

/// Who may manage a workspace's members.
#[derive(Clone, Copy)]
enum Manager {
    /// Organization owner or admin.
    Org,
    /// Admin of this workspace.
    Workspace,
    /// Can see the workspace but not manage it.
    None,
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
            | A::UserListAll
            | A::MaintenanceRead
            | A::InstanceAuditRead
            | A::OrganizationQuotaSet
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

        // ---- workspace listing / creation ----
        (A::WorkspaceList, Resource::Organization { org_id }) => match role_in(actor, *org_id) {
            Some(_) => allow(action, Some(*org_id)),
            None => Decision::Hidden,
        },
        (A::WorkspaceCreate, Resource::Organization { org_id }) => match role_in(actor, *org_id) {
            Some(OrgRole::Owner | OrgRole::Admin) => allow(action, Some(*org_id)),
            Some(OrgRole::Member) => Decision::Forbidden,
            None => Decision::Hidden,
        },

        // ---- workspace members ----
        (
            A::WorkspaceMemberList
            | A::WorkspaceMemberAdd
            | A::WorkspaceMemberChangeRole
            | A::WorkspaceMemberRemove,
            Resource::WorkspaceMember {
                org_id,
                workspace_id,
                target,
                role,
            },
        ) => {
            let ws_role = workspace_role_in(actor, *workspace_id);
            let org_role = role_in(actor, *org_id);
            // Visibility first: whoever cannot see the workspace gets 404.
            let manager = match (org_role, ws_role) {
                (None, _) | (Some(OrgRole::Member), None) => return Decision::Hidden,
                (Some(OrgRole::Owner | OrgRole::Admin), _) => Manager::Org,
                (Some(OrgRole::Member), Some(WorkspaceRole::Admin)) => Manager::Workspace,
                (Some(OrgRole::Member), Some(_)) => Manager::None,
            };
            match action {
                A::WorkspaceMemberList => allow(action, Some(*org_id)),
                A::WorkspaceMemberAdd => match (manager, role) {
                    (Manager::None, _) | (_, None) => Decision::Forbidden,
                    (Manager::Org, Some(_)) => allow(action, Some(*org_id)),
                    // A workspace admin assigns at most editor.
                    (Manager::Workspace, Some(WorkspaceRole::Editor | WorkspaceRole::Viewer)) => {
                        allow(action, Some(*org_id))
                    }
                    (Manager::Workspace, Some(WorkspaceRole::Admin)) => Decision::Forbidden,
                },
                A::WorkspaceMemberChangeRole => match (manager, target, role) {
                    (Manager::None, _, _) => Decision::Forbidden,
                    // Whether the target is a member is only revealed to managers.
                    (_, None, _) => Decision::Hidden,
                    (_, Some(_), None) => Decision::Forbidden,
                    (Manager::Org, Some(_), Some(_)) => allow(action, Some(*org_id)),
                    // Never touch workspace admins (also not oneself), never promote to admin.
                    (Manager::Workspace, Some(t), Some(new_role)) => {
                        if t.role != WorkspaceRole::Admin && *new_role != WorkspaceRole::Admin {
                            allow(action, Some(*org_id))
                        } else {
                            Decision::Forbidden
                        }
                    }
                },
                A::WorkspaceMemberRemove => match (manager, target) {
                    (Manager::None, None) => Decision::Forbidden,
                    (Manager::None, Some(t)) => {
                        // Leaving is allowed for everybody.
                        if t.user_id == actor.user_id {
                            allow(action, Some(*org_id))
                        } else {
                            Decision::Forbidden
                        }
                    }
                    (_, None) => Decision::Hidden,
                    (Manager::Org, Some(_)) => allow(action, Some(*org_id)),
                    (Manager::Workspace, Some(t)) => {
                        if t.role != WorkspaceRole::Admin || t.user_id == actor.user_id {
                            allow(action, Some(*org_id))
                        } else {
                            Decision::Forbidden
                        }
                    }
                },
                _ => Decision::Forbidden,
            }
        }

        // ---- projects and files (decided by the roles in the workspace) ----
        (
            A::ProjectList
            | A::ProjectRead
            | A::FileList
            | A::FileRead
            | A::VersionList
            | A::VersionRead
            | A::FileDownload,
            Resource::Workspace {
                org_id,
                workspace_id,
            },
        ) => match (
            role_in(actor, *org_id),
            workspace_role_in(actor, *workspace_id),
        ) {
            (None, _) | (Some(OrgRole::Member), None) => Decision::Hidden,
            _ => allow(action, Some(*org_id)),
        },
        (
            A::ProjectCreate
            | A::ProjectRename
            | A::FileCreate
            | A::FileRename
            | A::FileDelete
            | A::FileRestore
            | A::VersionRestore,
            Resource::Workspace {
                org_id,
                workspace_id,
            },
        ) => match (
            role_in(actor, *org_id),
            workspace_role_in(actor, *workspace_id),
        ) {
            (None, _) | (Some(OrgRole::Member), None) => Decision::Hidden,
            (Some(OrgRole::Owner | OrgRole::Admin), _)
            | (Some(OrgRole::Member), Some(WorkspaceRole::Admin | WorkspaceRole::Editor)) => {
                allow(action, Some(*org_id))
            }
            (Some(OrgRole::Member), Some(WorkspaceRole::Viewer)) => Decision::Forbidden,
        },
        (
            A::ProjectDelete | A::ProjectRestore,
            Resource::Workspace {
                org_id,
                workspace_id,
            },
        ) => match (
            role_in(actor, *org_id),
            workspace_role_in(actor, *workspace_id),
        ) {
            (None, _) | (Some(OrgRole::Member), None) => Decision::Hidden,
            (Some(OrgRole::Owner | OrgRole::Admin), _)
            | (Some(OrgRole::Member), Some(WorkspaceRole::Admin)) => allow(action, Some(*org_id)),
            (Some(OrgRole::Member), Some(WorkspaceRole::Editor | WorkspaceRole::Viewer)) => {
                Decision::Forbidden
            }
        },

        // ---- uploads and quota ----
        (
            A::UploadCreate,
            Resource::Workspace {
                org_id,
                workspace_id,
            },
        ) => match (
            role_in(actor, *org_id),
            workspace_role_in(actor, *workspace_id),
        ) {
            (None, _) | (Some(OrgRole::Member), None) => Decision::Hidden,
            (Some(OrgRole::Owner | OrgRole::Admin), _)
            | (Some(OrgRole::Member), Some(WorkspaceRole::Admin | WorkspaceRole::Editor)) => {
                allow(action, Some(*org_id))
            }
            (Some(OrgRole::Member), Some(WorkspaceRole::Viewer)) => Decision::Forbidden,
        },
        (
            A::UploadRead | A::UploadAbort | A::UploadWrite | A::VersionCommit,
            Resource::Upload {
                org_id,
                workspace_id,
                creator,
            },
        ) => {
            let ws_role = workspace_role_in(actor, *workspace_id);
            let org_role = role_in(actor, *org_id);
            let manager = matches!(
                (org_role, ws_role),
                (Some(OrgRole::Owner | OrgRole::Admin), _)
                    | (Some(OrgRole::Member), Some(WorkspaceRole::Admin))
            );
            let can_write = manager
                || matches!(
                    (org_role, ws_role),
                    (Some(OrgRole::Member), Some(WorkspaceRole::Editor))
                );
            match (org_role, ws_role) {
                (None, _) | (Some(OrgRole::Member), None) => Decision::Hidden,
                _ => {
                    let own = *creator == actor.user_id;
                    let ok = match action {
                        // Reading and aborting: the creator (whatever their role
                        // is now) or a manager of the workspace.
                        A::UploadRead | A::UploadAbort => own || manager,
                        // Sending content: only the creator, and only while they
                        // still may write to the workspace.
                        _ => own && can_write,
                    };
                    if ok {
                        allow(action, Some(*org_id))
                    } else {
                        Decision::Forbidden
                    }
                }
            }
        }
        (
            A::WorkspaceQuotaSet,
            Resource::Workspace {
                org_id,
                workspace_id,
            },
        ) => match (
            role_in(actor, *org_id),
            workspace_role_in(actor, *workspace_id),
        ) {
            (None, _) | (Some(OrgRole::Member), None) => Decision::Hidden,
            (Some(OrgRole::Owner | OrgRole::Admin), _) => allow(action, Some(*org_id)),
            (Some(OrgRole::Member), Some(_)) => Decision::Forbidden,
        },
        (A::UsageRead, Resource::Organization { org_id }) => match role_in(actor, *org_id) {
            Some(OrgRole::Owner | OrgRole::Admin) => allow(action, Some(*org_id)),
            Some(OrgRole::Member) => Decision::Forbidden,
            None => Decision::Hidden,
        },

        // ---- audit log ----
        (A::AuditRead, Resource::Organization { org_id }) => match role_in(actor, *org_id) {
            Some(OrgRole::Owner | OrgRole::Admin) => allow(action, Some(*org_id)),
            Some(OrgRole::Member) => Decision::Forbidden,
            None => Decision::Hidden,
        },

        // ---- invitations ----
        (A::InvitationList, Resource::Invitation { org_id, .. }) => match role_in(actor, *org_id) {
            Some(OrgRole::Owner | OrgRole::Admin) => allow(action, Some(*org_id)),
            Some(OrgRole::Member) => Decision::Forbidden,
            None => Decision::Hidden,
        },
        (A::InvitationCreate, Resource::Invitation { org_id, spec }) => {
            let Some(spec) = spec else {
                return Decision::Forbidden;
            };
            let org_role = role_in(actor, *org_id);
            match org_role {
                None => Decision::Hidden,
                // Nobody invites an owner.
                Some(_) if spec.org_role == OrgRole::Owner => Decision::Forbidden,
                Some(OrgRole::Owner | OrgRole::Admin) => allow(action, Some(*org_id)),
                Some(OrgRole::Member) => {
                    // A workspace admin of exactly the invited workspace, if the
                    // organization allows it, inviting plain members up to editor.
                    let allowed = spec.workspace_admin_invites_enabled
                        && spec.org_role == OrgRole::Member
                        && spec.workspace.is_some_and(|(ws, ws_role)| {
                            workspace_role_in(actor, ws) == Some(WorkspaceRole::Admin)
                                && matches!(ws_role, WorkspaceRole::Editor | WorkspaceRole::Viewer)
                        });
                    if allowed {
                        allow(action, Some(*org_id))
                    } else {
                        Decision::Forbidden
                    }
                }
            }
        }
        (A::InvitationRevoke, Resource::Invitation { org_id, spec }) => {
            match (role_in(actor, *org_id), spec) {
                (None, _) => Decision::Hidden,
                (Some(OrgRole::Owner | OrgRole::Admin), Some(_)) => allow(action, Some(*org_id)),
                // Unknown invitation: managers learn that (404), others get 403.
                (Some(OrgRole::Owner | OrgRole::Admin), None) => Decision::Hidden,
                // A workspace admin revokes only invitations they created for their workspace.
                (Some(OrgRole::Member), Some(spec)) => {
                    let own = spec.created_by == Some(actor.user_id);
                    let ws_admin = spec.workspace.is_some_and(|(ws, _)| {
                        workspace_role_in(actor, ws) == Some(WorkspaceRole::Admin)
                    });
                    if own && ws_admin {
                        allow(action, Some(*org_id))
                    } else {
                        Decision::Forbidden
                    }
                }
                (Some(OrgRole::Member), None) => Decision::Forbidden,
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
            Action::UserListAll,
            Action::MaintenanceRead,
            Action::InstanceAuditRead,
            Action::OrganizationQuotaSet,
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
            Action::UserListAll,
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
            Action::WorkspaceCreate,
            Action::WorkspaceList,
            Action::WorkspaceMemberList,
            Action::WorkspaceMemberAdd,
            Action::WorkspaceMemberChangeRole,
            Action::WorkspaceMemberRemove,
            Action::InvitationList,
            Action::InvitationCreate,
            Action::InvitationRevoke,
            Action::ProjectList,
            Action::ProjectRead,
            Action::ProjectCreate,
            Action::ProjectRename,
            Action::ProjectDelete,
            Action::ProjectRestore,
            Action::FileList,
            Action::FileRead,
            Action::FileCreate,
            Action::FileRename,
            Action::FileDelete,
            Action::FileRestore,
            Action::UploadCreate,
            Action::UploadRead,
            Action::UploadWrite,
            Action::UploadAbort,
            Action::OrganizationQuotaSet,
            Action::WorkspaceQuotaSet,
            Action::UsageRead,
            Action::VersionList,
            Action::VersionRead,
            Action::FileDownload,
            Action::VersionCommit,
            Action::VersionRestore,
            Action::MaintenanceRead,
            Action::AuditRead,
            Action::InstanceAuditRead,
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
                | Action::UserListAll
                | Action::MemberList
                | Action::MemberAdd
                | Action::MemberChangeRole
                | Action::MemberRemove
                | Action::UserDisable
                | Action::UserEnable
                | Action::PasswordResetIssue
                | Action::PasswordResetVoid
                | Action::WorkspaceRead
                | Action::WorkspaceManage
                | Action::WorkspaceCreate
                | Action::WorkspaceList
                | Action::WorkspaceMemberList
                | Action::WorkspaceMemberAdd
                | Action::WorkspaceMemberChangeRole
                | Action::WorkspaceMemberRemove
                | Action::InvitationList
                | Action::InvitationCreate
                | Action::InvitationRevoke
                | Action::ProjectList
                | Action::ProjectRead
                | Action::ProjectCreate
                | Action::ProjectRename
                | Action::ProjectDelete
                | Action::ProjectRestore
                | Action::FileList
                | Action::FileRead
                | Action::FileCreate
                | Action::FileRename
                | Action::FileDelete
                | Action::FileRestore
                | Action::UploadCreate
                | Action::UploadRead
                | Action::UploadWrite
                | Action::UploadAbort
                | Action::OrganizationQuotaSet
                | Action::WorkspaceQuotaSet
                | Action::UsageRead
                | Action::VersionList
                | Action::VersionRead
                | Action::FileDownload
                | Action::VersionCommit
                | Action::VersionRestore
                | Action::MaintenanceRead
                | Action::AuditRead
                | Action::InstanceAuditRead => {}
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
                Action::UserListAll,
                Action::MemberList,
                Action::WorkspaceRead,
                Action::WorkspaceList,
                Action::WorkspaceMemberList,
                Action::InvitationList,
                Action::ProjectList,
                Action::ProjectRead,
                Action::FileList,
                Action::FileRead,
                Action::UploadRead,
                Action::UsageRead,
                Action::VersionList,
                Action::VersionRead,
                Action::FileDownload,
                Action::MaintenanceRead,
                Action::AuditRead,
                Action::InstanceAuditRead
            ]
        );
    }

    #[test]
    fn project_and_file_actions_follow_workspace_roles() {
        let ws = Resource::Workspace {
            org_id: ORG,
            workspace_id: WS,
        };
        // Columns: outsider, plain member (no workspace role), viewer, editor,
        // workspace admin, organization admin, organization owner.
        let cast: [Actor; 7] = [
            actor(None, false),
            actor(Some(OrgRole::Member), false),
            with_ws(actor(Some(OrgRole::Member), false), WorkspaceRole::Viewer),
            with_ws(actor(Some(OrgRole::Member), false), WorkspaceRole::Editor),
            with_ws(actor(Some(OrgRole::Member), false), WorkspaceRole::Admin),
            actor(Some(OrgRole::Admin), false),
            actor(Some(OrgRole::Owner), false),
        ];
        let reads = [H, H, Y, Y, Y, Y, Y];
        let version_reads = reads;
        let editor_writes = [H, H, F, Y, Y, Y, Y];
        let admin_writes = [H, H, F, F, Y, Y, Y];
        for (action, expected) in [
            (Action::ProjectList, reads),
            (Action::ProjectRead, reads),
            (Action::FileList, reads),
            (Action::FileRead, reads),
            (Action::VersionList, version_reads),
            (Action::VersionRead, version_reads),
            (Action::FileDownload, version_reads),
            (Action::VersionRestore, [H, H, F, Y, Y, Y, Y]),
            (Action::ProjectCreate, editor_writes),
            (Action::ProjectRename, editor_writes),
            (Action::FileCreate, editor_writes),
            (Action::FileRename, editor_writes),
            (Action::FileDelete, editor_writes),
            (Action::FileRestore, editor_writes),
            (Action::ProjectDelete, admin_writes),
            (Action::ProjectRestore, admin_writes),
        ] {
            let got: Vec<Want> = cast.iter().map(|a| outcome(can(a, action, &ws))).collect();
            assert_eq!(got, expected, "{action:?}");
        }
        // A workspace role in another workspace grants nothing here, and the
        // operator has no implicit access.
        let mut elsewhere = actor(Some(OrgRole::Member), false);
        elsewhere.workspace = Some(WorkspaceMembership {
            workspace_id: Uuid::from_u128(11),
            role: WorkspaceRole::Admin,
        });
        assert_eq!(can(&elsewhere, Action::FileCreate, &ws), Decision::Hidden);
        assert_eq!(
            can(&actor(None, true), Action::FileRead, &ws),
            Decision::Hidden
        );
        let foreign = Resource::Workspace {
            org_id: OTHER_ORG,
            workspace_id: WS,
        };
        assert_eq!(
            can(
                &actor(Some(OrgRole::Owner), false),
                Action::ProjectCreate,
                &foreign
            ),
            Decision::Hidden
        );
    }

    #[test]
    fn upload_and_quota_actions_follow_roles_and_creator() {
        let ws = Resource::Workspace {
            org_id: ORG,
            workspace_id: WS,
        };
        let cast: [Actor; 7] = [
            actor(None, false),
            actor(Some(OrgRole::Member), false),
            with_ws(actor(Some(OrgRole::Member), false), WorkspaceRole::Viewer),
            with_ws(actor(Some(OrgRole::Member), false), WorkspaceRole::Editor),
            with_ws(actor(Some(OrgRole::Member), false), WorkspaceRole::Admin),
            actor(Some(OrgRole::Admin), false),
            actor(Some(OrgRole::Owner), false),
        ];
        let got =
            |action| -> Vec<Want> { cast.iter().map(|a| outcome(can(a, action, &ws))).collect() };
        assert_eq!(got(Action::UploadCreate), [H, H, F, Y, Y, Y, Y]);
        assert_eq!(got(Action::WorkspaceQuotaSet), [H, H, F, F, F, Y, Y]);
        // Upload sessions: creator or manager read/abort; only the creator, still editor+, writes.
        let upload = |creator| Resource::Upload {
            org_id: ORG,
            workspace_id: WS,
            creator,
        };
        let own = upload(ME);
        let foreign = upload(OTHER);
        let outcomes = |action, res: &Resource| -> Vec<Want> {
            cast.iter().map(|a| outcome(can(a, action, res))).collect()
        };
        // Own session: everybody who sees the workspace reads and aborts it;
        // writing needs editor or better.
        assert_eq!(outcomes(Action::UploadRead, &own), [H, H, Y, Y, Y, Y, Y]);
        assert_eq!(outcomes(Action::UploadAbort, &own), [H, H, Y, Y, Y, Y, Y]);
        assert_eq!(outcomes(Action::UploadWrite, &own), [H, H, F, Y, Y, Y, Y]);
        // Somebody else's session: managers read and abort, nobody else writes.
        assert_eq!(
            outcomes(Action::UploadRead, &foreign),
            [H, H, F, F, Y, Y, Y]
        );
        assert_eq!(
            outcomes(Action::UploadAbort, &foreign),
            [H, H, F, F, Y, Y, Y]
        );
        assert_eq!(
            outcomes(Action::UploadWrite, &foreign),
            [H, H, F, F, F, F, F]
        );
        // Committing follows the same rule as sending content: creator, still editor or better.
        assert_eq!(outcomes(Action::VersionCommit, &own), [H, H, F, Y, Y, Y, Y]);
        assert_eq!(
            outcomes(Action::VersionCommit, &foreign),
            [H, H, F, F, F, F, F]
        );
        // Usage: organization owners and admins.
        let org_res = Resource::Organization { org_id: ORG };
        let usage: Vec<Want> = cast
            .iter()
            .map(|a| outcome(can(a, Action::UsageRead, &org_res)))
            .collect();
        assert_eq!(usage, [H, F, F, F, F, Y, Y]);
        // The audit log: organization owners and admins; the operator has no
        // implicit access to an organization's audit log.
        let audit: Vec<Want> = cast
            .iter()
            .map(|a| outcome(can(a, Action::AuditRead, &org_res)))
            .collect();
        assert_eq!(audit, [H, F, F, F, F, Y, Y]);
        assert_eq!(
            can(
                &actor(None, true),
                Action::AuditRead,
                &Resource::Organization { org_id: ORG }
            ),
            Decision::Hidden
        );
        // An organization action on the instance resource is denied.
        assert_eq!(
            can(
                &actor(Some(OrgRole::Owner), true),
                Action::AuditRead,
                &Resource::Instance
            ),
            Decision::Forbidden
        );
        // The organization quota is the operator's.
        assert_eq!(
            can(
                &actor(Some(OrgRole::Owner), false),
                Action::OrganizationQuotaSet,
                &Resource::Instance
            ),
            Decision::Forbidden
        );
        assert!(
            can(
                &actor(None, true),
                Action::OrganizationQuotaSet,
                &Resource::Instance
            )
            .is_allow()
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

    // ---- workspaces and invitations (PR-007) ----

    fn ws_actor(org: Option<OrgRole>, ws: Option<WorkspaceRole>) -> Actor {
        let mut a = actor(org, false);
        a.workspace = ws.map(|role| WorkspaceMembership {
            workspace_id: WS,
            role,
        });
        a
    }

    fn wm(target: Option<(Uuid, WorkspaceRole)>, role: Option<WorkspaceRole>) -> Resource {
        Resource::WorkspaceMember {
            org_id: ORG,
            workspace_id: WS,
            target: target.map(|(user_id, role)| WorkspaceMemberTarget { user_id, role }),
            role,
        }
    }

    #[test]
    fn workspace_listing_and_creation() {
        let org_res = Resource::Organization { org_id: ORG };
        check(Action::WorkspaceList, org_res, [H, Y, Y, Y]);
        check(Action::WorkspaceCreate, org_res, [H, F, Y, Y]);
    }

    #[test]
    fn workspace_member_visibility_and_listing() {
        use WorkspaceRole::{Admin, Editor, Viewer};
        let list = wm(None, None);
        // Org owner/admin see every workspace, plain members only their own, others 404.
        for org in [OrgRole::Owner, OrgRole::Admin] {
            assert!(
                can(
                    &ws_actor(Some(org), None),
                    Action::WorkspaceMemberList,
                    &list
                )
                .is_allow()
            );
        }
        assert_eq!(
            can(
                &ws_actor(Some(OrgRole::Member), None),
                Action::WorkspaceMemberList,
                &list
            ),
            Decision::Hidden
        );
        assert_eq!(
            can(&ws_actor(None, None), Action::WorkspaceMemberList, &list),
            Decision::Hidden
        );
        for ws in [Admin, Editor, Viewer] {
            assert!(
                can(
                    &ws_actor(Some(OrgRole::Member), Some(ws)),
                    Action::WorkspaceMemberList,
                    &list
                )
                .is_allow()
            );
        }
        // A workspace role without organization membership grants nothing.
        assert_eq!(
            can(
                &ws_actor(None, Some(Admin)),
                Action::WorkspaceMemberList,
                &list
            ),
            Decision::Hidden
        );
    }

    #[test]
    fn workspace_admins_assign_at_most_editor_and_never_touch_other_admins() {
        use WorkspaceRole::{Admin, Editor, Viewer};
        let org_manager = ws_actor(Some(OrgRole::Admin), None);
        let ws_admin = ws_actor(Some(OrgRole::Member), Some(Admin));
        let editor = ws_actor(Some(OrgRole::Member), Some(Editor));
        let viewer = ws_actor(Some(OrgRole::Member), Some(Viewer));

        for role in [Admin, Editor, Viewer] {
            let add = wm(None, Some(role));
            assert!(
                can(&org_manager, Action::WorkspaceMemberAdd, &add).is_allow(),
                "{role:?}"
            );
            assert_eq!(
                can(&ws_admin, Action::WorkspaceMemberAdd, &add).is_allow(),
                role != Admin,
                "{role:?}"
            );
            assert_eq!(
                can(&editor, Action::WorkspaceMemberAdd, &add),
                Decision::Forbidden
            );
            assert_eq!(
                can(&viewer, Action::WorkspaceMemberAdd, &add),
                Decision::Forbidden
            );
        }
        // Change role: org managers anything; ws admin only among editor/viewer targets.
        for target in [Admin, Editor, Viewer] {
            for new_role in [Admin, Editor, Viewer] {
                let res = wm(Some((OTHER, target)), Some(new_role));
                assert!(can(&org_manager, Action::WorkspaceMemberChangeRole, &res).is_allow());
                let allowed = target != Admin && new_role != Admin;
                assert_eq!(
                    can(&ws_admin, Action::WorkspaceMemberChangeRole, &res).is_allow(),
                    allowed,
                    "{target:?}->{new_role:?}"
                );
                assert_eq!(
                    can(&editor, Action::WorkspaceMemberChangeRole, &res),
                    Decision::Forbidden
                );
            }
        }
        // A workspace admin cannot change their own role either.
        let me = wm(Some((ME, Admin)), Some(Editor));
        assert_eq!(
            can(&ws_admin, Action::WorkspaceMemberChangeRole, &me),
            Decision::Forbidden
        );
        // Unknown target: managers 404, non-managers 403.
        let unknown = wm(None, Some(Editor));
        assert_eq!(
            can(&org_manager, Action::WorkspaceMemberChangeRole, &unknown),
            Decision::Hidden
        );
        assert_eq!(
            can(&ws_admin, Action::WorkspaceMemberChangeRole, &unknown),
            Decision::Hidden
        );
        assert_eq!(
            can(&editor, Action::WorkspaceMemberChangeRole, &unknown),
            Decision::Forbidden
        );
        // Remove.
        for target in [Editor, Viewer] {
            let res = wm(Some((OTHER, target)), None);
            assert!(can(&ws_admin, Action::WorkspaceMemberRemove, &res).is_allow());
            assert_eq!(
                can(&editor, Action::WorkspaceMemberRemove, &res),
                Decision::Forbidden
            );
        }
        let other_admin = wm(Some((OTHER, Admin)), None);
        assert_eq!(
            can(&ws_admin, Action::WorkspaceMemberRemove, &other_admin),
            Decision::Forbidden
        );
        assert!(can(&org_manager, Action::WorkspaceMemberRemove, &other_admin).is_allow());
        // Everybody may leave a workspace.
        for (a, role) in [(&ws_admin, Admin), (&editor, Editor), (&viewer, Viewer)] {
            assert!(
                can(
                    a,
                    Action::WorkspaceMemberRemove,
                    &wm(Some((ME, role)), None)
                )
                .is_allow()
            );
        }
        assert_eq!(
            can(&editor, Action::WorkspaceMemberRemove, &wm(None, None)),
            Decision::Forbidden
        );
        assert_eq!(
            can(&org_manager, Action::WorkspaceMemberRemove, &wm(None, None)),
            Decision::Hidden
        );
    }

    fn inv(org_role: OrgRole, workspace: Option<WorkspaceRole>, enabled: bool) -> Resource {
        Resource::Invitation {
            org_id: ORG,
            spec: Some(InvitationSpec {
                org_role,
                workspace: workspace.map(|r| (WS, r)),
                workspace_admin_invites_enabled: enabled,
                created_by: Some(ME),
            }),
        }
    }

    #[test]
    fn invitation_rights() {
        use WorkspaceRole::{Admin, Editor, Viewer};
        let owner = ws_actor(Some(OrgRole::Owner), None);
        let admin = ws_actor(Some(OrgRole::Admin), None);
        let ws_admin = ws_actor(Some(OrgRole::Member), Some(Admin));
        let editor = ws_actor(Some(OrgRole::Member), Some(Editor));
        let outsider = ws_actor(None, None);

        for org_role in [OrgRole::Admin, OrgRole::Member] {
            for ws in [None, Some(Admin), Some(Editor), Some(Viewer)] {
                for a in [&owner, &admin] {
                    assert!(can(a, Action::InvitationCreate, &inv(org_role, ws, true)).is_allow());
                    // The org setting only restricts workspace admins.
                    assert!(can(a, Action::InvitationCreate, &inv(org_role, ws, false)).is_allow());
                }
            }
        }
        // Nobody invites an owner.
        for a in [&owner, &admin, &ws_admin] {
            assert_eq!(
                can(
                    a,
                    Action::InvitationCreate,
                    &inv(OrgRole::Owner, None, true)
                ),
                Decision::Forbidden
            );
        }
        // Workspace admin: member + own workspace + at most editor + setting on.
        assert!(
            can(
                &ws_admin,
                Action::InvitationCreate,
                &inv(OrgRole::Member, Some(Editor), true)
            )
            .is_allow()
        );
        assert!(
            can(
                &ws_admin,
                Action::InvitationCreate,
                &inv(OrgRole::Member, Some(Viewer), true)
            )
            .is_allow()
        );
        for denied in [
            inv(OrgRole::Member, Some(Editor), false), // setting off
            inv(OrgRole::Member, Some(Admin), true),   // workspace admin role
            inv(OrgRole::Admin, Some(Editor), true),   // org admin role
            inv(OrgRole::Member, None, true),          // org-only invitation
        ] {
            assert_eq!(
                can(&ws_admin, Action::InvitationCreate, &denied),
                Decision::Forbidden,
                "{denied:?}"
            );
        }
        // Invitations for a different workspace than the actor administers.
        let elsewhere = Resource::Invitation {
            org_id: ORG,
            spec: Some(InvitationSpec {
                org_role: OrgRole::Member,
                workspace: Some((Uuid::from_u128(77), Editor)),
                workspace_admin_invites_enabled: true,
                created_by: Some(ME),
            }),
        };
        assert_eq!(
            can(&ws_admin, Action::InvitationCreate, &elsewhere),
            Decision::Forbidden
        );
        // Editors/plain members cannot invite; outsiders do not exist.
        assert_eq!(
            can(
                &editor,
                Action::InvitationCreate,
                &inv(OrgRole::Member, Some(Viewer), true)
            ),
            Decision::Forbidden
        );
        assert_eq!(
            can(
                &ws_actor(Some(OrgRole::Member), None),
                Action::InvitationCreate,
                &inv(OrgRole::Member, None, true)
            ),
            Decision::Forbidden
        );
        assert_eq!(
            can(
                &outsider,
                Action::InvitationCreate,
                &inv(OrgRole::Member, None, true)
            ),
            Decision::Hidden
        );
        // Listing: owner/admin only.
        let list = Resource::Invitation {
            org_id: ORG,
            spec: None,
        };
        check(Action::InvitationList, list, [H, F, Y, Y]);
        // Revoking: managers any; a workspace admin only their own invitation of their workspace.
        let own = inv(OrgRole::Member, Some(Editor), true);
        assert!(can(&owner, Action::InvitationRevoke, &own).is_allow());
        assert!(can(&ws_admin, Action::InvitationRevoke, &own).is_allow());
        let foreign = Resource::Invitation {
            org_id: ORG,
            spec: Some(InvitationSpec {
                created_by: Some(OTHER),
                ..match own {
                    Resource::Invitation { spec: Some(s), .. } => s,
                    _ => unreachable!(),
                }
            }),
        };
        assert_eq!(
            can(&ws_admin, Action::InvitationRevoke, &foreign),
            Decision::Forbidden
        );
        assert_eq!(
            can(&editor, Action::InvitationRevoke, &own),
            Decision::Forbidden
        );
        assert_eq!(
            can(
                &owner,
                Action::InvitationRevoke,
                &Resource::Invitation {
                    org_id: ORG,
                    spec: None
                }
            ),
            Decision::Hidden
        );
        assert_eq!(
            can(
                &ws_admin,
                Action::InvitationRevoke,
                &Resource::Invitation {
                    org_id: ORG,
                    spec: None
                }
            ),
            Decision::Forbidden
        );
        assert_eq!(
            can(&outsider, Action::InvitationRevoke, &own),
            Decision::Hidden
        );
    }

    #[test]
    fn workspace_and_invitation_actions_do_not_cross_organizations() {
        let mut a = ws_actor(Some(OrgRole::Owner), Some(WorkspaceRole::Admin));
        a.membership = Some(OrgMembership {
            org_id: OTHER_ORG,
            role: OrgRole::Owner,
        });
        for (action, resource) in [
            (
                Action::WorkspaceCreate,
                Resource::Organization { org_id: ORG },
            ),
            (
                Action::WorkspaceList,
                Resource::Organization { org_id: ORG },
            ),
            (Action::WorkspaceMemberList, wm(None, None)),
            (
                Action::WorkspaceMemberAdd,
                wm(None, Some(WorkspaceRole::Viewer)),
            ),
            (
                Action::InvitationList,
                Resource::Invitation {
                    org_id: ORG,
                    spec: None,
                },
            ),
            (Action::InvitationCreate, inv(OrgRole::Member, None, true)),
        ] {
            assert_eq!(can(&a, action, &resource), Decision::Hidden, "{action:?}");
        }
    }
}
