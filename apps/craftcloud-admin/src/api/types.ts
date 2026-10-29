// Hand-written types for the subset of contracts/openapi.yaml that the admin
// UI uses. The test `endpoints.test.ts` fails when a used path or method is
// missing from the contract, so the two cannot drift apart silently.

export type Uuid = string;

export interface ErrorBody {
  error: {
    code: string;
    message: string;
    request_id: string | null;
    details?: Record<string, unknown>;
  };
}

export interface SetupStatus {
  setup_required: boolean;
}

export interface SetupCompleteRequest {
  bootstrap_token: string;
  email: string;
  display_name: string;
  password: string;
  organization: { name: string; slug: string };
}

export interface SetupCompleteResponse {
  user_id: Uuid;
  organization_id: Uuid;
}

export interface LoginRequest {
  email: string;
  password: string;
}

export interface LoginResponse {
  user_id: Uuid;
  csrf_token: string;
  expires_at: string;
}

export interface SessionInfo {
  user_id: Uuid;
  email: string;
  display_name: string;
  instance_role: "operator" | null;
  csrf_token: string;
  created_at: string;
  expires_at: string;
}

export type OrgRole = "owner" | "admin" | "member";

export interface Organization {
  id: Uuid;
  slug: string;
  name: string;
  role: OrgRole;
  version: number;
  workspace_admin_invites_enabled: boolean;
  created_at: string;
}

export type WorkspaceRole = "admin" | "editor" | "viewer";

export interface Workspace {
  id: Uuid;
  organization_id: Uuid;
  slug: string;
  name: string;
  workspace_role: WorkspaceRole | null;
  version: number;
  created_at: string;
}

export interface Page<T> {
  items: T[];
  next_cursor: string | null;
}

export interface AcceptInvitationRequest {
  token: string;
  display_name?: string;
  password?: string;
}

export interface AcceptedInvitation {
  user_id: Uuid;
  organization_id: Uuid;
  workspace_id: Uuid | null;
  account_created: boolean;
  workspace_role_applied: boolean;
  csrf_token?: string;
  expires_at?: string;
}
