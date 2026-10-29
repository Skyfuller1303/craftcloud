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

export interface OrgMember {
  user_id: Uuid;
  display_name: string;
  email: string;
  role: OrgRole;
  joined_at: string;
}

export interface WorkspaceMember {
  user_id: Uuid;
  display_name: string;
  email: string;
  role: WorkspaceRole;
  joined_at: string;
}

export interface Invitation {
  id: Uuid;
  email: string;
  org_role: "admin" | "member";
  workspace_id: Uuid | null;
  workspace_role: WorkspaceRole | null;
  status: "pending" | "accepted" | "revoked" | "expired" | "invalid";
  created_by: Uuid;
  created_at: string;
  expires_at: string;
}

export interface CreateInvitationRequest {
  email: string;
  org_role: "admin" | "member";
  workspace_id?: Uuid;
  workspace_role?: WorkspaceRole;
}

/** Returned once; `invite_url` is a secret and must never be stored. */
export interface CreatedInvitation {
  invitation_id: Uuid;
  invite_url: string;
  expires_at: string;
}

export interface Project {
  id: Uuid;
  organization_id: Uuid;
  workspace_id: Uuid;
  name: string;
  version: number;
  created_by: Uuid;
  created_at: string;
  updated_by: Uuid;
  updated_at: string;
  deleted_at: string | null;
}

/** File metadata (named `FileRecord` so it does not shadow the DOM `File`). */
export interface FileRecord {
  id: Uuid;
  organization_id: Uuid;
  workspace_id: Uuid;
  project_id: Uuid;
  name: string;
  app: string;
  format: string;
  size_bytes: number;
  sha256: string | null;
  head_version_id: Uuid | null;
  version: number;
  created_by: Uuid;
  created_at: string;
  updated_by: Uuid;
  updated_at: string;
  deleted_at: string | null;
}

export interface CreateFileRequest {
  name: string;
  app: string;
  format: string;
}

export type UploadState =
  | "created"
  | "receiving"
  | "stored"
  | "committed"
  | "aborted"
  | "expired"
  | "failed";

export interface Upload {
  upload_id: Uuid;
  file_id: Uuid;
  project_id: Uuid;
  workspace_id: Uuid;
  state: UploadState;
  size_bytes: number;
  sha256: string;
  max_bytes: number;
  created_by: Uuid;
  created_at: string;
  expires_at: string;
}

export interface FileVersion {
  id: Uuid;
  file_id: Uuid;
  number: number;
  size_bytes: number;
  sha256: string;
  base_version_id: Uuid | null;
  restored_from_version_id: Uuid | null;
  upload_id: Uuid | null;
  created_by: Uuid;
  created_at: string;
}

export interface ResetPasswordRequest {
  token: string;
  new_password: string;
}
