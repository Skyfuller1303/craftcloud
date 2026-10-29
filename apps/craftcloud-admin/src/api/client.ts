import type {
  AcceptedInvitation,
  AcceptInvitationRequest,
  ErrorBody,
  LoginRequest,
  LoginResponse,
  Organization,
  Page,
  SessionInfo,
  SetupCompleteRequest,
  SetupCompleteResponse,
  SetupStatus,
  Workspace,
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

function parseRetryAfter(value: string | null): number | undefined {
  if (value === null || !/^\d{1,9}$/.test(value.trim())) {
    return undefined;
  }
  return Number(value.trim());
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

export interface ApiClientOptions {
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
}

interface CallConfig {
  params?: Record<string, string>;
  query?: Record<string, string | undefined>;
  body?: unknown;
  /** A 401 from this call is an expected answer, not "the session expired". */
  anonymous?: boolean;
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

    const headers: Record<string, string> = { Accept: "application/json" };
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
    const retryAfterSeconds = parseRetryAfter(response.headers.get("Retry-After"));
    const error = isErrorBody(parsed)
      ? new ApiError({
          status: response.status,
          code: parsed.error.code,
          message: parsed.error.message,
          requestId: parsed.error.request_id ?? null,
          details: parsed.error.details,
          retryAfterSeconds,
        })
      : new ApiError({
          status: response.status,
          code: `HTTP_${response.status}`,
          message: response.statusText || "Unexpected server answer.",
          retryAfterSeconds,
        });

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
  };
}
