import type {
  AcceptedInvitation,
  AcceptInvitationRequest,
  AdminHealth,
  AuditEvent,
  AuditFilters,
  AuditSource,
  CreateFileRequest,
  CreateInvitationRequest,
  CreatedInvitation,
  ErrorBody,
  FileRecord,
  FileVersion,
  Invitation,
  LoginRequest,
  LoginResponse,
  OrgMember,
  OrgRole,
  Organization,
  Page,
  Project,
  ResetPasswordRequest,
  SessionInfo,
  SetupCompleteRequest,
  SetupCompleteResponse,
  SetupStatus,
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
  setupStatus: { method: "GET", path: "/setup/status" },
  setupComplete: { method: "POST", path: "/setup/complete" },
  login: { method: "POST", path: "/auth/login" },
  session: { method: "GET", path: "/auth/session" },
  logout: { method: "POST", path: "/auth/logout" },
  listOrganizations: { method: "GET", path: "/organizations" },
  listWorkspaces: { method: "GET", path: "/organizations/{org_id}/workspaces" },
  acceptInvitation: { method: "POST", path: "/invitations/accept" },
  resetPassword: { method: "POST", path: "/auth/password-reset" },
  listOrgMembers: { method: "GET", path: "/organizations/{org_id}/members" },
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
  renameProject: { method: "PATCH", path: "/projects/{project_id}" },
  trashProject: { method: "DELETE", path: "/projects/{project_id}" },
  restoreProject: { method: "POST", path: "/projects/{project_id}/restore" },
  listFiles: { method: "GET", path: "/projects/{project_id}/files" },
  createFile: { method: "POST", path: "/projects/{project_id}/files" },
  trashFile: { method: "DELETE", path: "/files/{file_id}" },
  restoreFile: { method: "POST", path: "/files/{file_id}/restore" },
  createUpload: { method: "POST", path: "/files/{file_id}/uploads" },
  uploadContent: { method: "PUT", path: "/uploads/{upload_id}/content" },
  abortUpload: { method: "DELETE", path: "/uploads/{upload_id}" },
  listVersions: { method: "GET", path: "/files/{file_id}/versions" },
  commitVersion: { method: "POST", path: "/files/{file_id}/versions" },
  restoreVersion: { method: "POST", path: "/files/{file_id}/versions/{version_id}/restore" },
  downloadHead: { method: "GET", path: "/files/{file_id}/content" },
  downloadVersion: { method: "GET", path: "/files/{file_id}/versions/{version_id}/content" },
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
  /** The CSRF token lives in memory only (never in storage or URLs). */
  setCsrfToken(token: string | null): void;
  hasCsrfToken(): boolean;
  setupStatus(): Promise<SetupStatus>;
  setupComplete(body: SetupCompleteRequest): Promise<SetupCompleteResponse>;
  login(body: LoginRequest): Promise<LoginResponse>;
  session(): Promise<SessionInfo>;
  logout(): Promise<void>;
  listOrganizations(cursor?: string): Promise<Page<Organization>>;
  listWorkspaces(orgId: string, cursor?: string): Promise<Page<Workspace>>;
  acceptInvitation(body: AcceptInvitationRequest): Promise<AcceptedInvitation>;
  resetPassword(body: ResetPasswordRequest): Promise<void>;

  listOrgMembers(orgId: string, cursor?: string): Promise<Page<OrgMember>>;
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
  createProject(workspaceId: string, name: string): Promise<Project>;
  renameProject(project: Pick<Project, "id" | "version">, name: string): Promise<Project>;
  trashProject(projectId: string): Promise<void>;
  restoreProject(projectId: string): Promise<Project>;

  listFiles(projectId: string, deleted: boolean, cursor?: string): Promise<Page<FileRecord>>;
  createFile(projectId: string, body: CreateFileRequest): Promise<FileRecord>;
  trashFile(fileId: string): Promise<void>;
  restoreFile(fileId: string): Promise<FileRecord>;

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
  project: (version: number) => `"prj-v${version}"`,
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
    setupComplete: (body) => call(ENDPOINTS.setupComplete, { body, anonymous: true }),
    async login(body) {
      const result = await call<LoginResponse>(ENDPOINTS.login, { body, anonymous: true });
      csrfToken = result.csrf_token;
      return result;
    },
    async session() {
      const result = await call<SessionInfo>(ENDPOINTS.session, { anonymous: true });
      csrfToken = result.csrf_token;
      return result;
    },
    async logout() {
      try {
        await call<void>(ENDPOINTS.logout, { anonymous: true });
      } finally {
        // Whatever the answer was, this page must not keep acting as the session.
        csrfToken = null;
      }
    },
    listOrganizations: (cursor) =>
      call(ENDPOINTS.listOrganizations, { query: { limit: "100", cursor } }),
    listWorkspaces: (orgId, cursor) =>
      call(ENDPOINTS.listWorkspaces, {
        params: { org_id: orgId },
        query: { limit: "100", cursor },
      }),
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

    listOrgMembers: (orgId, cursor) =>
      call(ENDPOINTS.listOrgMembers, { params: { org_id: orgId }, query: { limit: "50", cursor } }),
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

    listFiles: (projectId, deleted, cursor) =>
      call(ENDPOINTS.listFiles, {
        params: { project_id: projectId },
        query: { limit: "50", deleted: deleted ? "true" : undefined, cursor },
      }),
    createFile: (projectId, body) =>
      call(ENDPOINTS.createFile, { params: { project_id: projectId }, body }),
    trashFile: (fileId) => call(ENDPOINTS.trashFile, { params: { file_id: fileId } }),
    restoreFile: (fileId) => call(ENDPOINTS.restoreFile, { params: { file_id: fileId } }),

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
  };
}
