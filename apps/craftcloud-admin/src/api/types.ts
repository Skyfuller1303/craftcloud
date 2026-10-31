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

export interface SecondFactorLoginRequest {
  challenge_id: Uuid;
  code?: string;
  recovery_code?: string;
}

export interface TwoFactorStatus {
  enabled: boolean;
  available: boolean;
  recovery_codes_remaining: number;
}

export interface TwoFactorSetup {
  secret: string;
  otpauth_url: string;
}

export interface RecoveryCodesResponse {
  recovery_codes: string[];
}

export interface IssuedPasswordReset {
  reset_url?: string;
  sent_to?: string;
  expires_at: string;
}

export interface SessionInfo {
  email_verified?: boolean;
  user_id: Uuid;
  email: string;
  display_name: string;
  instance_role: "operator" | null;
  csrf_token: string;
  created_at: string;
  expires_at: string;
}

export interface DeviceSession {
  id: Uuid;
  name: string;
  client_kind: "app" | "launcher" | "cli";
  client_id: string;
  created_at: string;
  last_used_at: string | null;
}

export type OrgRole = "owner" | "admin" | "member" | "guest";

export interface Organization {
  id: Uuid;
  slug: string;
  name: string;
  role: OrgRole;
  version: number;
  workspace_admin_invites_enabled: boolean;
  require_two_factor?: boolean;
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
  status?: "active" | "invited" | "disabled";
  account_disabled?: boolean;
  two_factor_enabled?: boolean;
}

export interface OrganizationGrant extends ShareGrant { target_name: string; }
export interface OrganizationLock {
  file_id: Uuid; file_name: string; workspace_id: Uuid; holder_user_id: Uuid;
  holder_display_name: string; since: string; expires_at: string; session_kind: "browser" | "device";
}
export interface WorkspaceUsage {
  workspace_id: Uuid; effective_limit_bytes: number | null; quota_bytes: number | null;
  reserved_bytes: number; committed_bytes: number;
}
export interface UsageEntry { quota_bytes: number | null; reserved_bytes: number; committed_bytes: number; }
export interface OrganizationOverview {
  members: {
    total: number;
    by_status: { active: number; invited: number; disabled: number };
    by_role: { owner: number; admin: number; member: number; guest: number };
  };
  two_factor: { active_members: number; enrolled: number };
  counts: {
    workspaces: number; projects: number; files: number;
    active_locks: number; active_grants: number; pending_invitations: number;
  };
  storage: { used_bytes: number; committed_bytes: number; reserved_bytes: number; quota_bytes: number | null };
  /** One entry per UTC day, oldest first (30 days). */
  activity_daily: { date: string; count: number }[];
}
export interface OrganizationUsage { organization: UsageEntry; workspaces: WorkspaceUsage[]; next_cursor: string | null; }
export interface AdminOrganization {
  id: Uuid; slug: string; name: string; member_count: number; created_at: string;
  committed_bytes: number; reserved_bytes: number; quota_bytes: number | null;
}
export interface AdminUser {
  id: Uuid; email: string; display_name: string; created_at: string; disabled: boolean;
  email_verified: boolean; origin: { organization_id: Uuid | null; invited_by_user_id: Uuid | null; invitation_id: Uuid } | null;
}
export interface AdminDevice {
  device_ref: string; name: string; client_kind: string; client_id: string; created_at: string; last_used_at: string;
}
export interface AdminDevicePage {
  devices: AdminDevice[];
  next_cursor: string | null;
}

export interface WorkspaceMember {
  user_id: Uuid;
  display_name: string;
  email: string;
  role: WorkspaceRole;
  joined_at: string;
}

export type AuditResult = "success" | "denied" | "failed";

/** One audit event. Ids and codes only; the server never stores names or e-mail addresses in events. */
export interface AuditEvent {
  id: Uuid;
  created_at: string;
  last_at: string;
  /** Occurrences folded into the row (repeated denials and login failures). */
  count: number;
  org_id: Uuid | null;
  actor_id: Uuid | null;
  action: string;
  target: { type: string | null; id: Uuid } | null;
  workspace_id: Uuid | null;
  result: AuditResult;
  request_id: string | null;
  details: Record<string, string | number | boolean>;
}

/** Filters of the audit listing and export; empty or missing values do not filter. */
export interface AuditFilters {
  action?: string;
  actor?: string;
  target?: string;
  workspace?: string;
  result?: AuditResult;
  /** RFC 3339, inclusive. */
  from?: string;
  /** RFC 3339, exclusive. */
  to?: string;
}

/** Which audit log to read: one organization's, or the instance-level one (operator). */
export type AuditSource = { kind: "organization"; orgId: string } | { kind: "instance" };

export interface Invitation {
  id: Uuid;
  email: string;
  org_role: "admin" | "member" | "guest";
  workspace_id: Uuid | null;
  workspace_role: WorkspaceRole | null;
  status: "pending" | "accepted" | "revoked" | "expired" | "invalid";
  created_by: Uuid;
  created_at: string;
  expires_at: string;
}

export interface CreateInvitationRequest {
  language?: string;
  email: string;
  org_role: "admin" | "member" | "guest";
  workspace_id?: Uuid;
  workspace_role?: WorkspaceRole;
}

/** Returned once; `invite_url` is a secret and must never be stored. */
export interface CreatedInvitation {
  invitation_id: Uuid;
  invite_url?: string;
  sent_to?: string;
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
  can_share?: boolean;
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
  can_share?: boolean;
  can_force_lock?: boolean;
  lock?: FileLock | null;
}

export interface FileLock {
  holder_display_name: string;
  since: string;
  expires_at: string;
  mine: boolean;
  held_by_current_session: boolean;
}

export interface ShareGrant {
  id: Uuid;
  target_kind: "project" | "file";
  target_id: Uuid;
  grantee: { user_id: Uuid; display_name: string; email: string };
  role: "viewer" | "editor";
  created_by: Uuid;
  created_at: string;
  version: number;
}

export interface SharedItem {
  target_kind: "project" | "file";
  target_id: Uuid;
  role: "viewer" | "editor";
  /** The name is null for organization guests, who only learn what was shared with them. */
  workspace: { id: Uuid; name: string | null };
  project: { id: Uuid; name: string } | null;
  file: { id: Uuid; name: string } | null;
}

export interface ServerInfo {
  features: string[];
  feature_flags?: { two_factor?: boolean; grants?: boolean; locks?: boolean; figcraft_live?: boolean; email_delivery?: boolean };
}

export interface EmailDeliveryStatus {
  configured: boolean;
  last_success_at: string | null;
  failed_count: number;
  suppressed_count: number;
  transport?: "smtp" | "graph" | null;
  sender?: string | null;
  certificate_expires_at?: string | null;
  last_error_code?: string | null;
}

export interface FigcraftLiveStatus {
  state: "inactive" | "publishing" | "active" | "stopping" | "republish";
  access: "edit" | "view";
  can_snapshot: boolean;
  can_stop: boolean;
  last_snapshot_at: string | null;
  participants: number | null;
}

export interface FigcraftLiveLink {
  join_url: string;
  web_url?: string;
  access: "edit" | "view";
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
  actor_kind?: "user" | "figcraft_live";
  created_at: string;
}

export interface ResetPasswordRequest {
  token: string;
  new_password: string;
}

export interface IssuePasswordResetRequest {
  reset_two_factor?: boolean;
}

/** Condition of one component in the operator health view. */
export type HealthLevel = "ok" | "degraded" | "down" | "not_configured";

export type StorageBinding =
  | "matches"
  | "unbound"
  | "epoch_conflict"
  | "mismatch"
  | "error"
  | "unknown"
  | "not_configured";

/** The newest maintenance run as the server reports it (counts and codes only). */
export interface MaintenanceRunSummary {
  trigger: "scheduled" | "cli";
  started_at: string;
  finished_at: string;
  duration_ms: number;
  error_codes: string[];
}

/** `GET /admin/health` (operator only): numbers, flags and codes, no tenant data. */
export interface AdminHealth {
  status: HealthLevel;
  checked_at: string;
  components: {
    database: {
      status: HealthLevel;
      pool: { size: number; idle: number; in_use: number; max: number };
    };
    migrations: { status: HealthLevel };
    storage: { status: HealthLevel; binding: StorageBinding; free_bytes: number | null };
  };
  uploads: { open_sessions: number | null; overdue_sessions: number | null };
  maintenance: {
    last_run: MaintenanceRunSummary | null;
    last_run_ok: boolean | null;
    epoch_conflict: boolean;
  };
  audit: {
    dropped_events_total: number;
    dropped_events_lossy: number;
    dropped_events_important: number;
  };
  server: { version: string; uptime_seconds: number };
  backup: { last_success_age_seconds: number | null };
}
