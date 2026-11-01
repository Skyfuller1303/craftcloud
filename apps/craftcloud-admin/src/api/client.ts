import type {
  AcceptedInvitation,
  AcceptInvitationRequest,
  AdminHealth,
  AdminOrganization,
  AdminUser,
  AdminDevicePage,
  AuditEvent,
  AuditFilters,
  AuditSource,
  CreateFileRequest,
  CreateInvitationRequest,
  CreatedInvitation,
  DeviceSession,
  ErrorBody,
  EmailDeliveryStatus,
  FileRecord,
  FileLock,
  FileVersion,
  FigcraftLiveStatus,
  FigcraftLiveLink,
  Invitation,
  IssuePasswordResetRequest,
  IssuedPasswordReset,
  LoginRequest,
  LoginResponse,
  SecondFactorLoginRequest,
  OrgMember,
  OrganizationGrant,
  OrganizationLock,
  OrganizationOverview,
  OrganizationApps,
  OrganizationUsage,
  OrgRole,
  Organization,
  Page,
  Project,
  ResetPasswordRequest,
  SessionInfo,
  ServerInfo,
  ShareGrant,
  SharedItem,
  SetupCompleteRequest,
  SetupCompleteResponse,
  SetupStatus,
  RecoveryCodesResponse,
  TwoFactorSetup,
  TwoFactorStatus,
  Upload,
  Workspace,
  WorkspaceMember,
  WorkspaceRole,
} from "./types.ts";

export type HttpMethod = "GET" | "POST" | "PUT" | "PATCH" | "DELETE";

export interface Endpoint {
  method: HttpMethod;
  /** Path template relative to /api/v1, exactly as written in the OpenAPI document. */
  path: string;
}

/** Every operation the UI calls. `endpoints.test.ts` checks these against contracts/openapi.yaml. */
export const ENDPOINTS = {
  requestEmailVerification: { method: "POST", path: "/auth/email-verification" },
  confirmEmailVerification: { method: "POST", path: "/auth/email-verification/confirm" },
  requestEmailChange: { method: "POST", path: "/auth/email-change" },
  confirmEmailChange: { method: "POST", path: "/auth/email-change/confirm" },
  requestAllEmailVerifications: { method: "POST", path: "/admin/users/email-verification" },
  emailDeliveryStatus: { method: "GET", path: "/admin/email-delivery" },
  sendTestEmail: { method: "POST", path: "/admin/email-delivery/test" },
  resendInvitation: { method: "POST", path: "/organizations/{org_id}/invitations/{invitation_id}/resend" },
  setupStatus: { method: "GET", path: "/setup/status" },
  serverInfo: { method: "GET", path: "/info" },
  setupComplete: { method: "POST", path: "/setup/complete" },
  login: { method: "POST", path: "/auth/login" },
  oidcDiscover: { method: "POST", path: "/auth/oidc/discover" },
  oidcStart: { method: "POST", path: "/auth/oidc/start" },
  oidcFinish: { method: "POST", path: "/auth/oidc/finish" },
  organizationSso: { method: "GET", path: "/organizations/{org_id}/sso" },
  saveOrganizationSso: { method: "PUT", path: "/organizations/{org_id}/sso" },
  deleteOrganizationSso: { method: "DELETE", path: "/organizations/{org_id}/sso" },
  testOrganizationSso: { method: "POST", path: "/organizations/{org_id}/sso/test" },
  adminSso: { method: "GET", path: "/admin/sso" },
  updateAdminSso: { method: "PUT", path: "/admin/sso" },
  scimStatus: { method: "GET", path: "/organizations/{org_id}/scim" },
  scimToken: { method: "POST", path: "/organizations/{org_id}/scim/token" },
  deleteScimToken: { method: "DELETE", path: "/organizations/{org_id}/scim/token" },
  scimMappings: { method: "GET", path: "/organizations/{org_id}/scim/mappings" },
  updateScimMappings: { method: "PUT", path: "/organizations/{org_id}/scim/mappings" },
  scimEvents: { method: "GET", path: "/organizations/{org_id}/scim/events" },
  loginSecondFactor: { method: "POST", path: "/auth/login/second-factor" },
  twoFactorStatus: { method: "GET", path: "/auth/2fa" },
  twoFactorSetup: { method: "POST", path: "/auth/2fa/setup" },
  twoFactorConfirm: { method: "POST", path: "/auth/2fa/confirm" },
  twoFactorDisable: { method: "POST", path: "/auth/2fa/disable" },
  regenerateRecoveryCodes: { method: "POST", path: "/auth/2fa/recovery-codes" },
  issuePasswordReset: { method: "POST", path: "/admin/users/{user_id}/password-reset" },
  session: { method: "GET", path: "/auth/session" },
  updateProfile: { method: "PATCH", path: "/auth/profile" },
  changePassword: { method: "POST", path: "/auth/password/change" },
  orgOverview: { method: "GET", path: "/organizations/{org_id}/overview" },
  orgApps: { method: "GET", path: "/organizations/{org_id}/apps" },
  orgAppAccess: { method: "PATCH", path: "/organizations/{org_id}/app-access" },
  memberApps: { method: "PUT", path: "/organizations/{org_id}/members/{user_id}/apps" },
  changeMemberApps: { method: "PATCH", path: "/organizations/{org_id}/members/{user_id}/apps" },
  updateAdminOrganizationSeats: { method: "PATCH", path: "/admin/organizations/{org_id}/seats" },
  listDevices: { method: "GET", path: "/auth/devices" },
  revokeDevice: { method: "DELETE", path: "/auth/devices/{id}" },
  logout: { method: "POST", path: "/auth/logout" },
  listOrganizations: { method: "GET", path: "/organizations" },
  updateOrganization: { method: "PATCH", path: "/organizations/{org_id}" },
  listWorkspaces: { method: "GET", path: "/organizations/{org_id}/workspaces" },
  createWorkspace: { method: "POST", path: "/organizations/{org_id}/workspaces" },
  renameWorkspace: { method: "PATCH", path: "/workspaces/{workspace_id}" },
  acceptInvitation: { method: "POST", path: "/invitations/accept" },
  resetPassword: { method: "POST", path: "/auth/password-reset" },
  listOrgMembers: { method: "GET", path: "/organizations/{org_id}/members" },
  listOrgGrants: { method: "GET", path: "/organizations/{org_id}/grants" },
  listOrgLocks: { method: "GET", path: "/organizations/{org_id}/locks" },
  orgUsage: { method: "GET", path: "/organizations/{org_id}/usage" },
  updateWorkspaceQuota: { method: "PUT", path: "/workspaces/{workspace_id}/quota" },
  listAdminOrganizations: { method: "GET", path: "/admin/organizations" },
  updateAdminOrganizationQuota: { method: "PUT", path: "/admin/organizations/{org_id}/quota" },
  listAdminUsers: { method: "GET", path: "/admin/users" },
  disableAdminUser: { method: "POST", path: "/admin/users/{user_id}/disable" },
  enableAdminUser: { method: "POST", path: "/admin/users/{user_id}/enable" },
  listAdminUserDevices: { method: "GET", path: "/admin/users/{user_id}/devices" },
  revokeAdminUserDevice: { method: "DELETE", path: "/admin/users/{user_id}/devices/{device_ref}" },
  listSharedWithMe: { method: "GET", path: "/organizations/{org_id}/shared-with-me" },
  changeOrgMemberRole: { method: "PATCH", path: "/organizations/{org_id}/members/{user_id}" },
  removeOrgMember: { method: "DELETE", path: "/organizations/{org_id}/members/{user_id}" },
  listWorkspaceMembers: { method: "GET", path: "/workspaces/{workspace_id}/members" },
  addWorkspaceMember: { method: "POST", path: "/workspaces/{workspace_id}/members" },
  changeWorkspaceMemberRole: {
    method: "PATCH",
    path: "/workspaces/{workspace_id}/members/{user_id}",
  },
  removeWorkspaceMember: { method: "DELETE", path: "/workspaces/{workspace_id}/members/{user_id}" },
  listOrgAudit: { method: "GET", path: "/organizations/{org_id}/audit" },
  exportOrgAudit: { method: "GET", path: "/organizations/{org_id}/audit/export" },
  listInstanceAudit: { method: "GET", path: "/admin/audit" },
  adminHealth: { method: "GET", path: "/admin/health" },
  exportInstanceAudit: { method: "GET", path: "/admin/audit/export" },
  listInvitations: { method: "GET", path: "/organizations/{org_id}/invitations" },
  createInvitation: { method: "POST", path: "/organizations/{org_id}/invitations" },
  revokeInvitation: {
    method: "DELETE",
    path: "/organizations/{org_id}/invitations/{invitation_id}",
  },
  listProjects: { method: "GET", path: "/workspaces/{workspace_id}/projects" },
  createProject: { method: "POST", path: "/workspaces/{workspace_id}/projects" },
  getProject: { method: "GET", path: "/projects/{project_id}" },
  getFile: { method: "GET", path: "/files/{file_id}" },
  renameProject: { method: "PATCH", path: "/projects/{project_id}" },
  trashProject: { method: "DELETE", path: "/projects/{project_id}" },
  restoreProject: { method: "POST", path: "/projects/{project_id}/restore" },
  listProjectGrants: { method: "GET", path: "/projects/{project_id}/grants" },
  createProjectGrant: { method: "POST", path: "/projects/{project_id}/grants" },
  listFiles: { method: "GET", path: "/projects/{project_id}/files" },
  createFile: { method: "POST", path: "/projects/{project_id}/files" },
  trashFile: { method: "DELETE", path: "/files/{file_id}" },
  restoreFile: { method: "POST", path: "/files/{file_id}/restore" },
  listFileGrants: { method: "GET", path: "/files/{file_id}/grants" },
  createFileGrant: { method: "POST", path: "/files/{file_id}/grants" },
  changeGrant: { method: "PATCH", path: "/grants/{grant_id}" },
  revokeGrant: { method: "DELETE", path: "/grants/{grant_id}" },
  createUpload: { method: "POST", path: "/files/{file_id}/uploads" },
  uploadContent: { method: "PUT", path: "/uploads/{upload_id}/content" },
  abortUpload: { method: "DELETE", path: "/uploads/{upload_id}" },
  listVersions: { method: "GET", path: "/files/{file_id}/versions" },
  commitVersion: { method: "POST", path: "/files/{file_id}/versions" },
  restoreVersion: { method: "POST", path: "/files/{file_id}/versions/{version_id}/restore" },
  downloadHead: { method: "GET", path: "/files/{file_id}/content" },
  downloadVersion: { method: "GET", path: "/files/{file_id}/versions/{version_id}/content" },
  acquireFileLock: { method: "POST", path: "/files/{file_id}/lock" },
  releaseFileLock: { method: "DELETE", path: "/files/{file_id}/lock" },
  figcraftLiveStatus: { method: "GET", path: "/files/{file_id}/live" },
  figcraftLiveLink: { method: "POST", path: "/files/{file_id}/live" },
  figcraftLiveSnapshot: { method: "POST", path: "/files/{file_id}/live/snapshot" },
  figcraftLiveStop: { method: "DELETE", path: "/files/{file_id}/live" },
} as const satisfies Record<string, Endpoint>;

export const API_BASE = "/api/v1";
export const CSRF_HEADER = "X-CSRF-Token";

/** Status 0 stands for "no HTTP response" (network failure). */
export const NETWORK_ERROR_CODE = "NETWORK_ERROR";

/** Error returned by the server (common error shape) or produced locally. */
export class ApiError extends Error {
  readonly status: number;
  readonly code: string;
  readonly requestId: string | null;
  readonly details: Record<string, unknown> | undefined;
  /** Seconds from the Retry-After header, when present and numeric. */
  readonly retryAfterSeconds: number | undefined;

  constructor(init: {
    status: number;
    code: string;
    message: string;
    requestId?: string | null;
    details?: Record<string, unknown> | undefined;
    retryAfterSeconds?: number | undefined;
  }) {
    super(init.message);
    this.name = "ApiError";
    this.status = init.status;
    this.code = init.code;
    this.requestId = init.requestId ?? null;
    this.details = init.details;
    this.retryAfterSeconds = init.retryAfterSeconds;
  }

  /** Names of invalid fields from a 422 VALIDATION_ERROR (`details.fields`). */
  get fields(): string[] {
    const fields = this.details?.["fields"];
    return Array.isArray(fields) ? fields.filter((f): f is string => typeof f === "string") : [];
  }
}

export function isApiError(value: unknown): value is ApiError {
  return value instanceof ApiError;
}

function expandPath(template: string, params: Record<string, string>): string {
  return template.replace(/\{([a-z_]+)\}/g, (_, name: string) => {
    const value = params[name];
    if (value === undefined) {
      throw new Error(`missing path parameter ${name}`);
    }
    return encodeURIComponent(value);
  });
}

/** Longest wait the UI honours (one day); a larger value is treated as that. */
const MAX_RETRY_AFTER_SECONDS = 86_400;

/**
 * `Retry-After` is either delay-seconds or an HTTP-date (RFC 9110 10.2.3).
 * A date in the past means "now"; garbage means no usable hint.
 */
export function parseRetryAfter(value: string | null, now: number = Date.now()): number | undefined {
  if (value === null) {
    return undefined;
  }
  const text = value.trim();
  if (/^\d{1,9}$/.test(text)) {
    return Math.min(Number(text), MAX_RETRY_AFTER_SECONDS);
  }
  if (!/^[A-Za-z]{3},? /.test(text)) {
    return undefined;
  }
  const at = Date.parse(text);
  if (Number.isNaN(at)) {
    return undefined;
  }
  return Math.min(Math.max(0, Math.ceil((at - now) / 1000)), MAX_RETRY_AFTER_SECONDS);
}

function isErrorBody(value: unknown): value is ErrorBody {
  if (typeof value !== "object" || value === null || !("error" in value)) {
    return false;
  }
  const error = (value as { error: unknown }).error;
  return (
    typeof error === "object" &&
    error !== null &&
    typeof (error as { code?: unknown }).code === "string" &&
    typeof (error as { message?: unknown }).message === "string"
  );
}

function buildError(
  status: number,
  statusText: string,
  parsed: unknown,
  retryAfter: string | null,
): ApiError {
  const retryAfterSeconds = parseRetryAfter(retryAfter);
  return isErrorBody(parsed)
    ? new ApiError({
        status,
        code: parsed.error.code,
        message: parsed.error.message,
        requestId: parsed.error.request_id ?? null,
        details: parsed.error.details,
        retryAfterSeconds,
      })
    : new ApiError({
        status,
        code: `HTTP_${status}`,
        message: statusText || "Unexpected server answer.",
        retryAfterSeconds,
      });
}

/** Local error code for a request the user cancelled. */
export const ABORTED_ERROR_CODE = "ABORTED";

/** Progress of a streamed upload, in bytes. */
export interface UploadProgress {
  sent: number;
  total: number;
}

export interface UploadContentOptions {
  onProgress?: (progress: UploadProgress) => void;
  signal?: AbortSignal;
}

export interface ApiClientOptions {
  /** Creates the XMLHttpRequest used for upload bodies (fetch cannot report upload progress). */
  xhrFactory?: () => XMLHttpRequest;
  fetchImpl?: typeof fetch;
  /**
   * Called when a request that needs a session answers 401 UNAUTHENTICATED
   * (session ended or expired). The client has already dropped its CSRF token.
   */
  onUnauthenticated?: () => void;
}

export interface ApiClient {
  requestEmailVerification(): Promise<unknown>;
  confirmEmailVerification(token: string): Promise<void>;
  requestEmailChange(body: { email: string; password: string; code?: string; recovery_code?: string }): Promise<unknown>;
  confirmEmailChange(token: string): Promise<void>;
  requestAllEmailVerifications(after?: string): Promise<{ queued: number; next_cursor: string | null; limited: boolean; retry_after_seconds: number | null }>;
  emailDeliveryStatus(): Promise<EmailDeliveryStatus>;
  sendTestEmail(): Promise<unknown>;
  resendInvitation(orgId: string, invitationId: string): Promise<unknown>;
  /** The CSRF token lives in memory only (never in storage or URLs). */
  setCsrfToken(token: string | null): void;
  hasCsrfToken(): boolean;
  setupStatus(): Promise<SetupStatus>;
  serverInfo(): Promise<ServerInfo>;
  setupComplete(body: SetupCompleteRequest): Promise<SetupCompleteResponse>;
  login(body: LoginRequest): Promise<LoginResponse>;
  discoverOidc(email: string): Promise<{ organization_id: string | null }>;
  startOidc(organizationId: string): Promise<{ authorization_url: string }>;
  finishOidc(body: { pending_id: string; password?: string; code?: string; recovery_code?: string }): Promise<{ completed: true } | { redirect_url: string }>;
  getOrganizationSso(orgId: string): Promise<Record<string, unknown>>;
  saveOrganizationSso(orgId: string, body: Record<string, unknown>): Promise<Record<string, unknown>>;
  deleteOrganizationSso(orgId: string): Promise<void>;
  testOrganizationSso(orgId: string): Promise<Record<string, unknown>>;
  getAdminSso(): Promise<{ enabled: boolean }>;
  updateAdminSso(enabled: boolean): Promise<{ enabled: boolean }>;
  getScimStatus(orgId: string): Promise<Record<string, unknown>>;
  createScimToken(orgId: string): Promise<{ token: string }>;
  deleteScimToken(orgId: string): Promise<void>;
  getScimMappings(orgId: string): Promise<Record<string, unknown>>;
  updateScimMappings(orgId: string, body: Record<string, unknown>): Promise<Record<string, unknown>>;
  getScimEvents(orgId: string): Promise<{ items: Record<string, unknown>[] }>;
  loginSecondFactor(body: SecondFactorLoginRequest): Promise<LoginResponse>;
  getTwoFactorStatus(): Promise<TwoFactorStatus>;
  setupTwoFactor(password: string): Promise<TwoFactorSetup>;
  confirmTwoFactor(code: string): Promise<RecoveryCodesResponse>;
  disableTwoFactor(body: { password: string; code?: string; recovery_code?: string }): Promise<void>;
  regenerateRecoveryCodes(body: { code?: string; recovery_code?: string }): Promise<RecoveryCodesResponse>;
  issuePasswordReset(userId: string, body: IssuePasswordResetRequest): Promise<IssuedPasswordReset>;
  session(): Promise<SessionInfo>;
  /** Changes the own display name (AC-08b); answers like `session()`. */
  updateProfile(displayName: string): Promise<SessionInfo>;
  /** Changes the own password; the server ends all other sessions and devices. */
  changePassword(body: { current_password: string; new_password: string }): Promise<void>;
  /** Aggregate counts of an organization (owner and admin only). */
  getOrganizationOverview(orgId: string): Promise<OrganizationOverview>;
  getOrganizationApps(orgId: string): Promise<OrganizationApps>;
  updateOrganizationAppMode(orgId: string, mode: "all" | "assigned"): Promise<{ mode: "all" | "assigned" }>;
  updateMemberApps(orgId: string, userId: string, apps: string[]): Promise<{ user_id: string; apps: string[] }>;
  changeMemberApps(orgId: string, userId: string, change: { add?: string[]; remove?: string[] }): Promise<{ user_id: string; apps: string[] }>;
  updateAdminOrganizationSeats(orgId: string, seatLimit: number | null): Promise<{ seat_limit: number | null }>;
  /**
   * Ends the session. The CSRF token is dropped only when the server confirmed
   * it (success or 401); on any other failure the promise rejects and the token
   * is kept so that the call can be repeated.
   */
  logout(): Promise<void>;
  listDevices(): Promise<DeviceSession[]>;
  revokeDevice(id: string): Promise<void>;
  listOrganizations(cursor?: string): Promise<Page<Organization>>;
  updateOrganizationTwoFactor(orgId: string, organizationVersion: number, required: boolean): Promise<Organization>;
  listWorkspaces(orgId: string, cursor?: string): Promise<Page<Workspace>>;
  createWorkspace(orgId: string, name: string, slug: string): Promise<Workspace>;
  renameWorkspace(workspaceId: string, version: number, name: string): Promise<Workspace>;
  acceptInvitation(body: AcceptInvitationRequest): Promise<AcceptedInvitation>;
  resetPassword(body: ResetPasswordRequest): Promise<void>;

  listOrgMembers(orgId: string, cursor?: string, query?: string, filters?: { role?: string; status?: string; two_factor?: string; sort?: string }): Promise<Page<OrgMember>>;
  listOrgGrants(orgId: string, cursor?: string, filters?: { workspace_id?: string; user_id?: string; target_kind?: string }): Promise<Page<OrganizationGrant>>;
  listOrgLocks(orgId: string, cursor?: string, filters?: { workspace_id?: string; user_id?: string }): Promise<Page<OrganizationLock>>;
  getOrganizationUsage(orgId: string, cursor?: string): Promise<OrganizationUsage>;
  updateOrganizationSettings(orgId: string, organizationVersion: number, body: { name?: string; workspace_admin_invites_enabled?: boolean; require_two_factor?: boolean }): Promise<Organization>;
  updateWorkspaceQuota(workspaceId: string, quotaBytes: number | null): Promise<{ quota_bytes: number | null }>;
  listAdminOrganizations(cursor?: string): Promise<Page<AdminOrganization>>;
  updateAdminOrganizationQuota(orgId: string, quotaBytes: number | null): Promise<{ quota_bytes: number | null }>;
  listAdminUsers(cursor?: string, query?: string): Promise<Page<AdminUser>>;
  disableAdminUser(userId: string): Promise<{ orphaned_organization_ids: string[] }>;
  enableAdminUser(userId: string): Promise<void>;
  listAdminUserDevices(userId: string, cursor?: string): Promise<AdminDevicePage>;
  revokeAdminUserDevice(userId: string, deviceRef: string): Promise<void>;
  listSharedWithMe(orgId: string, cursor?: string): Promise<Page<SharedItem>>;
  changeOrgMemberRole(orgId: string, userId: string, role: OrgRole): Promise<OrgMember>;
  removeOrgMember(orgId: string, userId: string): Promise<void>;
  listWorkspaceMembers(workspaceId: string, cursor?: string): Promise<Page<WorkspaceMember>>;
  addWorkspaceMember(
    workspaceId: string,
    userId: string,
    role: WorkspaceRole,
  ): Promise<WorkspaceMember>;
  changeWorkspaceMemberRole(
    workspaceId: string,
    userId: string,
    role: WorkspaceRole,
  ): Promise<WorkspaceMember>;
  removeWorkspaceMember(workspaceId: string, userId: string): Promise<void>;

  /** Operator only: component status, pool, disk, uploads, maintenance, audit drops, version (CC-F15). */
  getAdminHealth(): Promise<AdminHealth>;
  listAudit(source: AuditSource, filters: AuditFilters, cursor?: string): Promise<Page<AuditEvent>>;
  /** URL of the NDJSON export for a plain link; the browser streams the body, the page never holds it. */
  auditExportUrl(source: AuditSource, filters: AuditFilters): string;

  listInvitations(orgId: string, cursor?: string): Promise<Page<Invitation>>;
  createInvitation(orgId: string, body: CreateInvitationRequest): Promise<CreatedInvitation>;
  revokeInvitation(orgId: string, invitationId: string): Promise<void>;

  listProjects(workspaceId: string, deleted: boolean, cursor?: string): Promise<Page<Project>>;
  getProject(projectId: string): Promise<Project>;
  getFile(fileId: string): Promise<FileRecord>;
  getFigcraftLiveStatus(fileId: string): Promise<FigcraftLiveStatus>;
  getFigcraftLiveLink(fileId: string, renew?: boolean): Promise<FigcraftLiveLink>;
  snapshotFigcraftLive(fileId: string): Promise<FileVersion>;
  stopFigcraftLive(fileId: string): Promise<void>;
  createProject(workspaceId: string, name: string): Promise<Project>;
  renameProject(project: Pick<Project, "id" | "version">, name: string): Promise<Project>;
  trashProject(projectId: string): Promise<void>;
  restoreProject(projectId: string): Promise<Project>;
  listProjectGrants(projectId: string, cursor?: string): Promise<Page<ShareGrant>>;
  createProjectGrant(projectId: string, userId: string, role: "viewer" | "editor"): Promise<ShareGrant>;

  listFiles(projectId: string, deleted: boolean, cursor?: string): Promise<Page<FileRecord>>;
  createFile(projectId: string, body: CreateFileRequest): Promise<FileRecord>;
  trashFile(fileId: string): Promise<void>;
  restoreFile(fileId: string): Promise<FileRecord>;
  listFileGrants(fileId: string, cursor?: string): Promise<Page<ShareGrant>>;
  createFileGrant(fileId: string, userId: string, role: "viewer" | "editor"): Promise<ShareGrant>;
  changeGrant(grantId: string, version: number, role: "viewer" | "editor"): Promise<ShareGrant>;
  revokeGrant(grantId: string): Promise<void>;

  /** `idempotencyKey` makes a repeated request return the same session. */
  createUpload(
    fileId: string,
    body: { size_bytes: number; sha256: string },
    idempotencyKey: string,
  ): Promise<Upload>;
  /** Streams the body from the browser (no copy in memory) and reports progress. */
  uploadContent(uploadId: string, content: Blob, options?: UploadContentOptions): Promise<Upload>;
  abortUpload(uploadId: string): Promise<void>;
  listVersions(fileId: string, cursor?: string): Promise<Page<FileVersion>>;
  /**
   * Makes a stored upload the next version. `expectedHeadVersionId` is the
   * current head (compare-and-set); `null` means "the file has no version yet".
   */
  commitVersion(
    fileId: string,
    uploadId: string,
    expectedHeadVersionId: string | null,
    idempotencyKey?: string,
  ): Promise<FileVersion>;
  restoreVersion(
    fileId: string,
    versionId: string,
    expectedHeadVersionId: string,
  ): Promise<FileVersion>;
  /** URL for a plain link or `<a download>`; the browser streams the body, the page never holds it. */
  downloadUrl(fileId: string, versionId?: string): string;
  acquireFileLock(fileId: string, clientId: string, takeover?: boolean): Promise<FileLock>;
  releaseFileLock(fileId: string, force?: boolean): Promise<void>;
}

interface CallConfig {
  params?: Record<string, string>;
  query?: Record<string, string | undefined>;
  body?: unknown;
  /** A 401 from this call is an expected answer, not "the session expired". */
  anonymous?: boolean;
  /** Extra request headers (preconditions, idempotency key). */
  headers?: Record<string, string>;
}

/** Strong entity tags the server derives from the `version` field of a row or a version id. */
export const etag = {
  organization: (version: number) => `"org-v${version}"`,
  workspace: (version: number) => `"ws-v${version}"`,
  project: (version: number) => `"prj-v${version}"`,
  grant: (version: number) => `"grant-v${version}"`,
  content: (versionId: string) => `"version:${versionId}"`,
};

/** The non-empty filters as query parameters. */
function auditQuery(filters: AuditFilters): Record<string, string | undefined> {
  const out: Record<string, string | undefined> = {};
  for (const [key, value] of Object.entries(filters)) {
    if (typeof value === "string" && value !== "") {
      out[key] = value;
    }
  }
  return out;
}

export function createApiClient(options: ApiClientOptions = {}): ApiClient {
  let csrfToken: string | null = null;

  async function call<T>(endpoint: Endpoint, config: CallConfig = {}): Promise<T> {
    let url = API_BASE + expandPath(endpoint.path, config.params ?? {});
    const search = new URLSearchParams();
    for (const [key, value] of Object.entries(config.query ?? {})) {
      if (value !== undefined) {
        search.set(key, value);
      }
    }
    const query = search.toString();
    if (query !== "") {
      url += `?${query}`;
    }

    const headers: Record<string, string> = {
      Accept: "application/json",
      ...(config.headers ?? {}),
    };
    if (config.body !== undefined) {
      headers["Content-Type"] = "application/json";
    }
    if (endpoint.method !== "GET" && csrfToken !== null) {
      headers[CSRF_HEADER] = csrfToken;
    }

    let response: Response;
    try {
      response = await (options.fetchImpl ?? fetch)(url, {
        method: endpoint.method,
        headers,
        credentials: "same-origin",
        cache: "no-store",
        redirect: "error",
        ...(config.body !== undefined ? { body: JSON.stringify(config.body) } : {}),
      });
    } catch {
      throw new ApiError({
        status: 0,
        code: NETWORK_ERROR_CODE,
        message: "The server could not be reached.",
      });
    }

    if (response.ok) {
      if (response.status === 204) {
        return undefined as T;
      }
      try {
        return (await response.json()) as T;
      } catch {
        throw new ApiError({
          status: response.status,
          code: "INVALID_RESPONSE",
          message: "The server answer could not be read.",
        });
      }
    }

    let parsed: unknown;
    try {
      parsed = await response.json();
    } catch {
      parsed = undefined;
    }
    const error = buildError(
      response.status,
      response.statusText,
      parsed,
      response.headers.get("Retry-After"),
    );

    if (response.status === 401 && config.anonymous !== true && error.code === "UNAUTHENTICATED") {
      csrfToken = null;
      options.onUnauthenticated?.();
    }
    throw error;
  }

  return {
    setCsrfToken(token) {
      csrfToken = token;
    },
    hasCsrfToken: () => csrfToken !== null,
    setupStatus: () => call(ENDPOINTS.setupStatus, { anonymous: true }),
    serverInfo: () => call(ENDPOINTS.serverInfo, { anonymous: true }),
    requestEmailVerification: () => call(ENDPOINTS.requestEmailVerification, { body: {} }),
    confirmEmailVerification: (token) => call(ENDPOINTS.confirmEmailVerification, { body: { token }, anonymous: true }),
    requestEmailChange: (body) => call(ENDPOINTS.requestEmailChange, { body }),
    confirmEmailChange: (token) => call(ENDPOINTS.confirmEmailChange, { body: { token }, anonymous: true }),
    requestAllEmailVerifications: (after) => call(ENDPOINTS.requestAllEmailVerifications, { query: { after }, body: {} }),
    emailDeliveryStatus: () => call(ENDPOINTS.emailDeliveryStatus),
    sendTestEmail: () => call(ENDPOINTS.sendTestEmail, { body: {} }),
    resendInvitation: (orgId, invitationId) => call(ENDPOINTS.resendInvitation, { params: { org_id: orgId, invitation_id: invitationId }, body: {} }),
    setupComplete: (body) => call(ENDPOINTS.setupComplete, { body, anonymous: true }),
    async login(body) {
      const result = await call<LoginResponse>(ENDPOINTS.login, { body, anonymous: true });
      csrfToken = result.csrf_token;
      return result;
    },
    discoverOidc: (email) => call(ENDPOINTS.oidcDiscover, { body: { email }, anonymous: true }),
    startOidc: (organizationId) => call(ENDPOINTS.oidcStart, { body: { organization_id: organizationId }, anonymous: true }),
    finishOidc: (body) => call(ENDPOINTS.oidcFinish, { body, anonymous: true }),
    getOrganizationSso: (orgId) => call(ENDPOINTS.organizationSso, { params: { org_id: orgId } }),
    saveOrganizationSso: (orgId, body) => call(ENDPOINTS.saveOrganizationSso, { params: { org_id: orgId }, body }),
    deleteOrganizationSso: (orgId) => call(ENDPOINTS.deleteOrganizationSso, { params: { org_id: orgId } }),
    testOrganizationSso: (orgId) => call(ENDPOINTS.testOrganizationSso, { params: { org_id: orgId }, body: {} }),
    getAdminSso: () => call(ENDPOINTS.adminSso),
    updateAdminSso: (enabled) => call(ENDPOINTS.updateAdminSso, { body: { enabled } }),
    getScimStatus: (orgId) => call(ENDPOINTS.scimStatus, { params: { org_id: orgId } }),
    createScimToken: (orgId) => call(ENDPOINTS.scimToken, { params: { org_id: orgId }, body: {} }),
    deleteScimToken: (orgId) => call(ENDPOINTS.deleteScimToken, { params: { org_id: orgId } }),
    getScimMappings: (orgId) => call(ENDPOINTS.scimMappings, { params: { org_id: orgId } }),
    updateScimMappings: (orgId, body) => call(ENDPOINTS.updateScimMappings, { params: { org_id: orgId }, body }),
    getScimEvents: (orgId) => call(ENDPOINTS.scimEvents, { params: { org_id: orgId } }),
    async loginSecondFactor(body) {
      const result = await call<LoginResponse>(ENDPOINTS.loginSecondFactor, { body, anonymous: true });
      csrfToken = result.csrf_token;
      return result;
    },
    getTwoFactorStatus: () => call(ENDPOINTS.twoFactorStatus),
    getFigcraftLiveStatus: (fileId) => call(ENDPOINTS.figcraftLiveStatus, { params: { file_id: fileId } }),
    getFigcraftLiveLink: (fileId, renew = false) => call(ENDPOINTS.figcraftLiveLink, { params: { file_id: fileId }, body: { renew } }),
    snapshotFigcraftLive: (fileId) => call(ENDPOINTS.figcraftLiveSnapshot, { params: { file_id: fileId } }),
    stopFigcraftLive: (fileId) => call(ENDPOINTS.figcraftLiveStop, { params: { file_id: fileId } }),
    setupTwoFactor: (password) => call(ENDPOINTS.twoFactorSetup, { body: { password } }),
    confirmTwoFactor: (code) => call(ENDPOINTS.twoFactorConfirm, { body: { code } }),
    disableTwoFactor: (body) => call(ENDPOINTS.twoFactorDisable, { body }),
    regenerateRecoveryCodes: (body) => call(ENDPOINTS.regenerateRecoveryCodes, { body }),
    issuePasswordReset: (userId, body) =>
      call(ENDPOINTS.issuePasswordReset, { params: { user_id: userId }, body }),
    async session() {
      const result = await call<SessionInfo>(ENDPOINTS.session, { anonymous: true });
      csrfToken = result.csrf_token;
      return result;
    },
    updateProfile: (displayName) => call(ENDPOINTS.updateProfile, { body: { display_name: displayName } }),
    changePassword: (body) => call(ENDPOINTS.changePassword, { body }),
    getOrganizationOverview: (orgId) => call(ENDPOINTS.orgOverview, { params: { org_id: orgId } }),
    getOrganizationApps: (orgId) => call(ENDPOINTS.orgApps, { params: { org_id: orgId } }),
    updateOrganizationAppMode: (orgId, mode) => call(ENDPOINTS.orgAppAccess, { params: { org_id: orgId }, body: { mode } }),
    updateMemberApps: (orgId, userId, apps) => call(ENDPOINTS.memberApps, { params: { org_id: orgId, user_id: userId }, body: { apps } }),
    changeMemberApps: (orgId, userId, change) => call(ENDPOINTS.changeMemberApps, { params: { org_id: orgId, user_id: userId }, body: change }),
    updateAdminOrganizationSeats: (orgId, seatLimit) => call(ENDPOINTS.updateAdminOrganizationSeats, { params: { org_id: orgId }, body: { seat_limit: seatLimit } }),
    async logout() {
      try {
        await call<void>(ENDPOINTS.logout, { anonymous: true });
      } catch (error) {
        // 401 means the session is already gone: that is what logging out wants.
        // Any other failure leaves the session alive on the server, so the token
        // stays and the caller can try again.
        if (!isApiError(error) || error.status !== 401) {
          throw error;
        }
      }
      csrfToken = null;
    },
    async listDevices() {
      const result = await call<{ devices: DeviceSession[] }>(ENDPOINTS.listDevices);
      return result.devices;
    },
    revokeDevice: (id) => call(ENDPOINTS.revokeDevice, { params: { id } }),
    listOrganizations: (cursor) =>
      call(ENDPOINTS.listOrganizations, { query: { limit: "100", cursor } }),
    updateOrganizationTwoFactor: (orgId, version, required) =>
      call(ENDPOINTS.updateOrganization, {
        params: { org_id: orgId },
        body: { require_two_factor: required },
        headers: { "If-Match": etag.organization(version) },
      }),
    listWorkspaces: (orgId, cursor) =>
      call(ENDPOINTS.listWorkspaces, {
        params: { org_id: orgId },
        query: { limit: "100", cursor },
      }),
    createWorkspace: (orgId, name, slug) =>
      call(ENDPOINTS.createWorkspace, { params: { org_id: orgId }, body: { name, slug } }),
    renameWorkspace: (workspaceId, version, name) =>
      call(ENDPOINTS.renameWorkspace, { params: { workspace_id: workspaceId }, body: { name }, headers: { "If-Match": etag.workspace(version) } }),
    async acceptInvitation(body) {
      const result = await call<AcceptedInvitation>(ENDPOINTS.acceptInvitation, {
        body,
        anonymous: true,
      });
      if (result.csrf_token !== undefined) {
        csrfToken = result.csrf_token;
      }
      return result;
    },
    resetPassword: (body) => call(ENDPOINTS.resetPassword, { body, anonymous: true }),

    listOrgMembers: (orgId, cursor, query, filters = {}) =>
      call(ENDPOINTS.listOrgMembers, { params: { org_id: orgId }, query: { limit: "50", cursor, query, ...filters } }),
    listOrgGrants: (orgId, cursor, filters = {}) =>
      call(ENDPOINTS.listOrgGrants, { params: { org_id: orgId }, query: { limit: "50", cursor, ...filters } }),
    listOrgLocks: (orgId, cursor, filters = {}) =>
      call(ENDPOINTS.listOrgLocks, { params: { org_id: orgId }, query: { limit: "50", cursor, ...filters } }),
    getOrganizationUsage: (orgId, cursor) =>
      call(ENDPOINTS.orgUsage, { params: { org_id: orgId }, query: { limit: "100", cursor } }),
    updateOrganizationSettings: (orgId, version, body) =>
      call(ENDPOINTS.updateOrganization, { params: { org_id: orgId }, body, headers: { "If-Match": etag.organization(version) } }),
    updateWorkspaceQuota: (workspaceId, quotaBytes) =>
      call(ENDPOINTS.updateWorkspaceQuota, { params: { workspace_id: workspaceId }, body: { quota_bytes: quotaBytes } }),
    listAdminOrganizations: (cursor) => call(ENDPOINTS.listAdminOrganizations, { query: { limit: "100", cursor } }),
    updateAdminOrganizationQuota: (orgId, quotaBytes) =>
      call(ENDPOINTS.updateAdminOrganizationQuota, { params: { org_id: orgId }, body: { quota_bytes: quotaBytes } }),
    listAdminUsers: (cursor, query) => call(ENDPOINTS.listAdminUsers, { query: { limit: "100", cursor, query } }),
    disableAdminUser: (userId) => call(ENDPOINTS.disableAdminUser, { params: { user_id: userId } }),
    enableAdminUser: (userId) => call(ENDPOINTS.enableAdminUser, { params: { user_id: userId } }),
    listAdminUserDevices: (userId, cursor) => call<AdminDevicePage>(ENDPOINTS.listAdminUserDevices, {
      params: { user_id: userId },
      query: { limit: "50", cursor },
    }),
    revokeAdminUserDevice: (userId, deviceRef) => call(ENDPOINTS.revokeAdminUserDevice, { params: { user_id: userId, device_ref: deviceRef } }),
    listSharedWithMe: (orgId, cursor) =>
      call(ENDPOINTS.listSharedWithMe, { params: { org_id: orgId }, query: { limit: "50", cursor } }),
    changeOrgMemberRole: (orgId, userId, role) =>
      call(ENDPOINTS.changeOrgMemberRole, {
        params: { org_id: orgId, user_id: userId },
        body: { role },
      }),
    removeOrgMember: (orgId, userId) =>
      call(ENDPOINTS.removeOrgMember, { params: { org_id: orgId, user_id: userId } }),
    listWorkspaceMembers: (workspaceId, cursor) =>
      call(ENDPOINTS.listWorkspaceMembers, {
        params: { workspace_id: workspaceId },
        query: { limit: "50", cursor },
      }),
    addWorkspaceMember: (workspaceId, userId, role) =>
      call(ENDPOINTS.addWorkspaceMember, {
        params: { workspace_id: workspaceId },
        body: { user_id: userId, role },
      }),
    changeWorkspaceMemberRole: (workspaceId, userId, role) =>
      call(ENDPOINTS.changeWorkspaceMemberRole, {
        params: { workspace_id: workspaceId, user_id: userId },
        body: { role },
      }),
    removeWorkspaceMember: (workspaceId, userId) =>
      call(ENDPOINTS.removeWorkspaceMember, {
        params: { workspace_id: workspaceId, user_id: userId },
      }),

    getAdminHealth: () => call(ENDPOINTS.adminHealth),
    listAudit: (source, filters, cursor) =>
      call(source.kind === "organization" ? ENDPOINTS.listOrgAudit : ENDPOINTS.listInstanceAudit, {
        params: source.kind === "organization" ? { org_id: source.orgId } : {},
        query: { limit: "50", cursor, ...auditQuery(filters) },
      }),
    auditExportUrl(source, filters) {
      const endpoint =
        source.kind === "organization" ? ENDPOINTS.exportOrgAudit : ENDPOINTS.exportInstanceAudit;
      const params = source.kind === "organization" ? { org_id: source.orgId } : {};
      const search = new URLSearchParams();
      for (const [key, value] of Object.entries(auditQuery(filters))) {
        if (value !== undefined) {
          search.set(key, value);
        }
      }
      const query = search.toString();
      return API_BASE + expandPath(endpoint.path, params) + (query === "" ? "" : `?${query}`);
    },

    listInvitations: (orgId, cursor) =>
      call(ENDPOINTS.listInvitations, {
        params: { org_id: orgId },
        query: { limit: "50", cursor },
      }),
    createInvitation: (orgId, body) =>
      call(ENDPOINTS.createInvitation, { params: { org_id: orgId }, body }),
    revokeInvitation: (orgId, invitationId) =>
      call(ENDPOINTS.revokeInvitation, { params: { org_id: orgId, invitation_id: invitationId } }),

    listProjects: (workspaceId, deleted, cursor) =>
      call(ENDPOINTS.listProjects, {
        params: { workspace_id: workspaceId },
        query: { limit: "50", deleted: deleted ? "true" : undefined, cursor },
      }),
    getProject: (projectId) => call(ENDPOINTS.getProject, { params: { project_id: projectId } }),
    getFile: (fileId) => call(ENDPOINTS.getFile, { params: { file_id: fileId } }),
    createProject: (workspaceId, name) =>
      call(ENDPOINTS.createProject, { params: { workspace_id: workspaceId }, body: { name } }),
    renameProject: (project, name) =>
      call(ENDPOINTS.renameProject, {
        params: { project_id: project.id },
        body: { name },
        headers: { "If-Match": etag.project(project.version) },
      }),
    trashProject: (projectId) =>
      call(ENDPOINTS.trashProject, { params: { project_id: projectId } }),
    restoreProject: (projectId) =>
      call(ENDPOINTS.restoreProject, { params: { project_id: projectId } }),
    listProjectGrants: (projectId, cursor) =>
      call(ENDPOINTS.listProjectGrants, { params: { project_id: projectId }, query: { limit: "50", cursor } }),
    createProjectGrant: (projectId, userId, role) =>
      call(ENDPOINTS.createProjectGrant, { params: { project_id: projectId }, body: { user_id: userId, role } }),

    listFiles: (projectId, deleted, cursor) =>
      call(ENDPOINTS.listFiles, {
        params: { project_id: projectId },
        query: { limit: "50", deleted: deleted ? "true" : undefined, cursor },
      }),
    createFile: (projectId, body) =>
      call(ENDPOINTS.createFile, { params: { project_id: projectId }, body }),
    trashFile: (fileId) => call(ENDPOINTS.trashFile, { params: { file_id: fileId } }),
    restoreFile: (fileId) => call(ENDPOINTS.restoreFile, { params: { file_id: fileId } }),
    listFileGrants: (fileId, cursor) =>
      call(ENDPOINTS.listFileGrants, { params: { file_id: fileId }, query: { limit: "50", cursor } }),
    createFileGrant: (fileId, userId, role) =>
      call(ENDPOINTS.createFileGrant, { params: { file_id: fileId }, body: { user_id: userId, role } }),
    changeGrant: (grantId, version, role) =>
      call(ENDPOINTS.changeGrant, { params: { grant_id: grantId }, body: { role }, headers: { "If-Match": etag.grant(version) } }),
    revokeGrant: (grantId) => call(ENDPOINTS.revokeGrant, { params: { grant_id: grantId } }),

    createUpload: (fileId, body, idempotencyKey) =>
      call(ENDPOINTS.createUpload, {
        params: { file_id: fileId },
        body,
        headers: { "Idempotency-Key": idempotencyKey },
      }),
    uploadContent: (uploadId, content, uploadOptions = {}) =>
      new Promise<Upload>((resolve, reject) => {
        const url = API_BASE + expandPath(ENDPOINTS.uploadContent.path, { upload_id: uploadId });
        const xhr = (options.xhrFactory ?? (() => new XMLHttpRequest()))();
        const { onProgress, signal } = uploadOptions;
        const cancelled = () =>
          new ApiError({ status: 0, code: ABORTED_ERROR_CODE, message: "Upload cancelled." });
        if (signal?.aborted === true) {
          reject(cancelled());
          return;
        }
        xhr.open("PUT", url);
        xhr.responseType = "text";
        xhr.withCredentials = true;
        xhr.setRequestHeader("Content-Type", "application/octet-stream");
        xhr.setRequestHeader("Accept", "application/json");
        if (csrfToken !== null) {
          xhr.setRequestHeader(CSRF_HEADER, csrfToken);
        }
        xhr.upload.onprogress = (event) => {
          onProgress?.({ sent: event.loaded, total: content.size });
        };
        const onAbort = () => xhr.abort();
        signal?.addEventListener("abort", onAbort, { once: true });
        const done = () => signal?.removeEventListener("abort", onAbort);
        xhr.onerror = () => {
          done();
          reject(
            new ApiError({
              status: 0,
              code: NETWORK_ERROR_CODE,
              message: "The server could not be reached.",
            }),
          );
        };
        xhr.ontimeout = xhr.onerror;
        xhr.onabort = () => {
          done();
          reject(cancelled());
        };
        xhr.onload = () => {
          done();
          let parsed: unknown;
          try {
            parsed = JSON.parse(xhr.responseText);
          } catch {
            parsed = undefined;
          }
          if (xhr.status >= 200 && xhr.status < 300) {
            if (parsed === undefined) {
              reject(
                new ApiError({
                  status: xhr.status,
                  code: "INVALID_RESPONSE",
                  message: "The server answer could not be read.",
                }),
              );
              return;
            }
            onProgress?.({ sent: content.size, total: content.size });
            resolve(parsed as Upload);
            return;
          }
          const error = buildError(
            xhr.status,
            xhr.statusText,
            parsed,
            xhr.getResponseHeader("Retry-After"),
          );
          if (xhr.status === 401 && error.code === "UNAUTHENTICATED") {
            csrfToken = null;
            options.onUnauthenticated?.();
          }
          reject(error);
        };
        xhr.send(content);
      }),
    abortUpload: (uploadId) => call(ENDPOINTS.abortUpload, { params: { upload_id: uploadId } }),
    listVersions: (fileId, cursor) =>
      call(ENDPOINTS.listVersions, { params: { file_id: fileId }, query: { limit: "50", cursor } }),
    commitVersion: (fileId, uploadId, expectedHeadVersionId, idempotencyKey) =>
      call(ENDPOINTS.commitVersion, {
        params: { file_id: fileId },
        body: { upload_id: uploadId },
        headers: {
          ...(expectedHeadVersionId === null
            ? { "If-None-Match": "*" }
            : { "If-Match": etag.content(expectedHeadVersionId) }),
          ...(idempotencyKey === undefined ? {} : { "Idempotency-Key": idempotencyKey }),
        },
      }),
    restoreVersion: (fileId, versionId, expectedHeadVersionId) =>
      call(ENDPOINTS.restoreVersion, {
        params: { file_id: fileId, version_id: versionId },
        headers: { "If-Match": etag.content(expectedHeadVersionId) },
      }),
    downloadUrl: (fileId, versionId) =>
      versionId === undefined
        ? API_BASE + expandPath(ENDPOINTS.downloadHead.path, { file_id: fileId })
        : API_BASE +
          expandPath(ENDPOINTS.downloadVersion.path, { file_id: fileId, version_id: versionId }),
    acquireFileLock: (fileId, clientId, takeover) =>
      call(ENDPOINTS.acquireFileLock, {
        params: { file_id: fileId },
        body: { client_id: clientId, ...(takeover === true ? { takeover: true } : {}) },
      }),
    releaseFileLock: (fileId, force) =>
      call(ENDPOINTS.releaseFileLock, {
        params: { file_id: fileId },
        ...(force === true ? { query: { force: "true" } } : {}),
      }),
  };
}
