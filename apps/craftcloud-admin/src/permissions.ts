// What the UI offers the viewer. This only decides which controls to show or
// enable (docs/05-security/permission-matrix.md); the server decides again on
// every request and a 403/404 is always handled, for example after a role
// changed in the meantime.

import type {
  OrgMember,
  OrgRole,
  Organization,
  Workspace,
  WorkspaceMember,
  WorkspaceRole,
} from "./api/types.ts";

export interface Scope {
  userId: string;
  organization: Organization | null;
  workspace: Workspace | null;
}

export function isOrgManager(organization: Organization | null): boolean {
  return organization?.role === "owner" || organization?.role === "admin";
}

/** Organization owners and admins act as workspace admins in every workspace. */
export function effectiveWorkspaceRole(scope: Scope): WorkspaceRole | null {
  if (scope.workspace === null) {
    return null;
  }
  if (isOrgManager(scope.organization)) {
    return "admin";
  }
  return scope.workspace.workspace_role;
}

/** Create or rename projects, create files, upload, restore trashed files. */
export function canWriteContent(scope: Scope): boolean {
  const role = effectiveWorkspaceRole(scope);
  return role === "admin" || role === "editor";
}

/** Move projects to the trash and restore them. */
export function canTrashProjects(scope: Scope): boolean {
  return effectiveWorkspaceRole(scope) === "admin";
}

/** Roles the viewer may give `target` in the organization (empty: no control). */
export function assignableOrgRoles(actor: OrgRole | undefined, target: OrgMember): OrgRole[] {
  if (actor === "owner") {
    return ["owner", "admin", "member"];
  }
  if (actor === "admin" && target.role !== "owner") {
    return ["admin", "member"];
  }
  return [];
}

export function canRemoveOrgMember(
  actor: OrgRole | undefined,
  actorUserId: string,
  target: OrgMember,
): boolean {
  if (actor === "owner") {
    return true;
  }
  if (actor === "admin") {
    return target.role !== "owner";
  }
  return actor === "member" && target.user_id === actorUserId;
}

export function canManageWorkspaceMembers(scope: Scope): boolean {
  return effectiveWorkspaceRole(scope) === "admin";
}

/** Workspace roles the viewer may hand out: managers all, workspace admins editor and viewer. */
export function grantableWorkspaceRoles(scope: Scope): WorkspaceRole[] {
  if (isOrgManager(scope.organization)) {
    return ["admin", "editor", "viewer"];
  }
  return effectiveWorkspaceRole(scope) === "admin" ? ["editor", "viewer"] : [];
}

export function assignableWorkspaceRoles(scope: Scope, target: WorkspaceMember): WorkspaceRole[] {
  if (isOrgManager(scope.organization)) {
    return ["admin", "editor", "viewer"];
  }
  if (effectiveWorkspaceRole(scope) === "admin" && target.role !== "admin") {
    return ["editor", "viewer"];
  }
  return [];
}

export function canRemoveWorkspaceMember(scope: Scope, target: WorkspaceMember): boolean {
  if (target.user_id === scope.userId) {
    return true;
  }
  if (isOrgManager(scope.organization)) {
    return true;
  }
  return effectiveWorkspaceRole(scope) === "admin" && target.role !== "admin";
}

/** The audit log of an organization: owners and admins (the server decides again). */
export function canReadAudit(organization: Organization | null): boolean {
  return isOrgManager(organization);
}

export function canListInvitations(organization: Organization | null): boolean {
  return isOrgManager(organization);
}

/** Organization managers always; workspace admins only when the organization allows it. */
export function canCreateInvitations(scope: Scope): boolean {
  if (isOrgManager(scope.organization)) {
    return true;
  }
  return (
    scope.organization?.workspace_admin_invites_enabled === true &&
    scope.workspace?.workspace_role === "admin"
  );
}
