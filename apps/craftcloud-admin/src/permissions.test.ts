import { describe, expect, it } from "vitest";
import type {
  OrgMember,
  OrgRole,
  Organization,
  Workspace,
  WorkspaceMember,
  WorkspaceRole,
} from "./api/types.ts";
import {
  assignableOrgRoles,
  assignableWorkspaceRoles,
  canCreateInvitations,
  canListInvitations,
  canManageWorkspaceMembers,
  canRemoveOrgMember,
  canRemoveWorkspaceMember,
  canTrashProjects,
  canWriteContent,
  effectiveWorkspaceRole,
  grantableWorkspaceRoles,
} from "./permissions.ts";
import type { Scope } from "./permissions.ts";

const org = (role: OrgRole, invites = false): Organization => ({
  id: "o",
  slug: "o",
  name: "O",
  role,
  version: 1,
  workspace_admin_invites_enabled: invites,
  created_at: "x",
});
const ws = (role: WorkspaceRole | null): Workspace => ({
  id: "w",
  organization_id: "o",
  slug: "w",
  name: "W",
  workspace_role: role,
  version: 1,
  created_at: "x",
});
const scope = (orgRole: OrgRole, wsRole: WorkspaceRole | null, invites = false): Scope => ({
  userId: "me",
  organization: org(orgRole, invites),
  workspace: ws(wsRole),
});
const member = (role: OrgRole, id = "other"): OrgMember => ({
  user_id: id,
  display_name: "n",
  email: "e",
  role,
  joined_at: "x",
});
const wsMember = (role: WorkspaceRole, id = "other"): WorkspaceMember => ({
  user_id: id,
  display_name: "n",
  email: "e",
  role,
  joined_at: "x",
});

describe("content permissions (matrix PR-008)", () => {
  it.each([
    ["owner", null, true, true],
    ["admin", null, true, true],
    ["member", "admin", true, true],
    ["member", "editor", true, false],
    ["member", "viewer", false, false],
    ["member", null, false, false],
  ] as const)("org %s / workspace %s: write=%s trash=%s", (o, w, write, trash) => {
    expect(canWriteContent(scope(o, w))).toBe(write);
    expect(canTrashProjects(scope(o, w))).toBe(trash);
  });

  it("is never permissive without a workspace", () => {
    const none: Scope = { userId: "me", organization: org("owner"), workspace: null };
    expect(effectiveWorkspaceRole(none)).toBeNull();
    expect(canWriteContent(none)).toBe(false);
  });
});

describe("organization member actions (matrix PR-006)", () => {
  it("owners assign everything and remove anybody", () => {
    expect(assignableOrgRoles("owner", member("owner"))).toEqual(["owner", "admin", "member"]);
    expect(canRemoveOrgMember("owner", "me", member("owner"))).toBe(true);
  });
  it("admins never touch owners and never promote to owner", () => {
    expect(assignableOrgRoles("admin", member("owner"))).toEqual([]);
    expect(assignableOrgRoles("admin", member("member"))).toEqual(["admin", "member"]);
    expect(canRemoveOrgMember("admin", "me", member("owner"))).toBe(false);
    expect(canRemoveOrgMember("admin", "me", member("admin"))).toBe(true);
  });
  it("plain members only leave", () => {
    expect(assignableOrgRoles("member", member("member"))).toEqual([]);
    expect(canRemoveOrgMember("member", "me", member("member", "me"))).toBe(true);
    expect(canRemoveOrgMember("member", "me", member("member"))).toBe(false);
    expect(canRemoveOrgMember(undefined, "me", member("member", "me"))).toBe(false);
  });
});

describe("workspace member actions (matrix PR-007)", () => {
  it("managers grant any role; workspace admins only editor and viewer", () => {
    expect(grantableWorkspaceRoles(scope("owner", null))).toEqual(["admin", "editor", "viewer"]);
    expect(grantableWorkspaceRoles(scope("member", "admin"))).toEqual(["editor", "viewer"]);
    expect(grantableWorkspaceRoles(scope("member", "editor"))).toEqual([]);
    expect(canManageWorkspaceMembers(scope("member", "admin"))).toBe(true);
    expect(canManageWorkspaceMembers(scope("member", "editor"))).toBe(false);
  });
  it("workspace admins never touch workspace admins", () => {
    const s = scope("member", "admin");
    expect(assignableWorkspaceRoles(s, wsMember("admin"))).toEqual([]);
    expect(assignableWorkspaceRoles(s, wsMember("viewer"))).toEqual(["editor", "viewer"]);
    expect(canRemoveWorkspaceMember(s, wsMember("admin"))).toBe(false);
    expect(canRemoveWorkspaceMember(s, wsMember("admin", "me"))).toBe(true);
    expect(canRemoveWorkspaceMember(s, wsMember("editor"))).toBe(true);
  });
  it("editors and viewers can only leave", () => {
    const s = scope("member", "editor");
    expect(assignableWorkspaceRoles(s, wsMember("viewer"))).toEqual([]);
    expect(canRemoveWorkspaceMember(s, wsMember("viewer"))).toBe(false);
    expect(canRemoveWorkspaceMember(s, wsMember("editor", "me"))).toBe(true);
  });
});

describe("invitations", () => {
  it("lists only for organization managers", () => {
    expect(canListInvitations(org("owner"))).toBe(true);
    expect(canListInvitations(org("admin"))).toBe(true);
    expect(canListInvitations(org("member"))).toBe(false);
    expect(canListInvitations(null)).toBe(false);
  });
  it("lets workspace admins create only when the organization allows it", () => {
    expect(canCreateInvitations(scope("member", "admin", false))).toBe(false);
    expect(canCreateInvitations(scope("member", "admin", true))).toBe(true);
    expect(canCreateInvitations(scope("member", "editor", true))).toBe(false);
    expect(canCreateInvitations(scope("admin", null, false))).toBe(true);
  });
});
