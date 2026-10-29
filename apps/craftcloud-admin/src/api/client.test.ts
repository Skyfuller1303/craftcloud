import { describe, expect, it, vi } from "vitest";
import { apiError, mockFetch } from "../test/helpers.tsx";
import { fakeXhr } from "../test/fakeXhr.ts";
import { ABORTED_ERROR_CODE, ApiError, ENDPOINTS, createApiClient, parseRetryAfter } from "./client.ts";

describe("api client", () => {
  it("sends same-origin cookies, never a CSRF header on GET and the token on unsafe methods", async () => {
    const { fetch, calls, impl } = mockFetch({
      "POST /auth/login": { body: { user_id: "u", csrf_token: "tok-1", expires_at: "x" } },
      "GET /organizations": { body: { items: [], next_cursor: null } },
      "POST /auth/logout": { status: 204 },
    });
    const client = createApiClient({ fetchImpl: fetch });

    await client.login({ email: "a@example.test", password: "pw" });
    await client.listOrganizations();
    await client.logout();

    const [login, list, logout] = calls;
    expect(login?.headers["x-csrf-token"]).toBeUndefined(); // no token before login
    expect(login?.headers["content-type"]).toBe("application/json");
    expect(list?.headers["x-csrf-token"]).toBeUndefined();
    expect(logout?.headers["x-csrf-token"]).toBe("tok-1");
    for (const [, init] of impl.mock.calls) {
      expect(init?.credentials).toBe("same-origin");
      expect(init?.redirect).toBe("error");
    }
    expect(client.hasCsrfToken()).toBe(false); // dropped after logout
  });

  it("keeps the CSRF token when logout fails, so that it can be repeated", async () => {
    let attempts = 0;
    const { fetch, calls } = mockFetch({
      "GET /auth/session": { body: { csrf_token: "tok" } },
      "POST /auth/logout": () => (++attempts === 1 ? apiError(503, "AUTH_UNAVAILABLE") : { status: 204 }),
    });
    const client = createApiClient({ fetchImpl: fetch });
    await client.session();
    expect(client.hasCsrfToken()).toBe(true);
    await expect(client.logout()).rejects.toBeInstanceOf(ApiError);
    expect(client.hasCsrfToken()).toBe(true);
    await client.logout();
    expect(client.hasCsrfToken()).toBe(false);
    const sent = calls.filter((c) => c.path === "/auth/logout").map((c) => c.headers["x-csrf-token"]);
    expect(sent).toEqual(["tok", "tok"]);
  });

  it("keeps the CSRF token when logout cannot reach the server", async () => {
    const calls: string[] = [];
    const fetchImpl = (async (input: RequestInfo | URL) => {
      calls.push(String(input));
      if (String(input).endsWith("/auth/session")) {
        return new Response(JSON.stringify({ csrf_token: "tok" }), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      }
      throw new TypeError("Failed to fetch");
    }) as typeof fetch;
    const client = createApiClient({ fetchImpl });
    await client.session();
    await expect(client.logout()).rejects.toBeDefined();
    expect(client.hasCsrfToken()).toBe(true);
  });

  it("treats 401 on logout as done: the session is already gone", async () => {
    const { fetch } = mockFetch({
      "GET /auth/session": { body: { csrf_token: "tok" } },
      "POST /auth/logout": apiError(401, "UNAUTHENTICATED"),
    });
    const client = createApiClient({ fetchImpl: fetch });
    await client.session();
    await expect(client.logout()).resolves.toBeUndefined();
    expect(client.hasCsrfToken()).toBe(false);
  });

  it("builds paged URLs with an encoded path parameter and cursor", async () => {
    const { fetch, impl } = mockFetch({
      "GET /organizations/a%2Fb/workspaces": { body: { items: [], next_cursor: null } },
    });
    const client = createApiClient({ fetchImpl: fetch });
    await client.listWorkspaces("a/b", "c+d");
    expect(String(impl.mock.calls[0]?.[0])).toBe("/api/v1/organizations/a%2Fb/workspaces?limit=100&cursor=c%2Bd");
  });

  it("maps the common error shape, request id, fields and Retry-After", async () => {
    const { fetch } = mockFetch({
      "POST /setup/complete": apiError(422, "VALIDATION_ERROR", {
        details: { fields: ["email", 7, "organization.slug"] },
      }),
      "POST /auth/login": apiError(429, "RATE_LIMITED", { headers: { "Retry-After": "42" } }),
    });
    const client = createApiClient({ fetchImpl: fetch });

    const validation = await client
      .setupComplete({
        bootstrap_token: "t",
        email: "e",
        display_name: "n",
        password: "p",
        organization: { name: "o", slug: "s" },
      })
      .catch((e: unknown) => e);
    expect(validation).toBeInstanceOf(ApiError);
    expect(validation).toMatchObject({ status: 422, code: "VALIDATION_ERROR", requestId: "req-1" });
    expect((validation as ApiError).fields).toEqual(["email", "organization.slug"]);

    const limited = (await client.login({ email: "a", password: "b" }).catch((e: unknown) => e)) as ApiError;
    expect(limited).toMatchObject({ status: 429, code: "RATE_LIMITED", retryAfterSeconds: 42 });
  });

  it("maps non-JSON error answers and network failures", async () => {
    const plain = createApiClient({
      fetchImpl: (async () => new Response("<html>bad gateway</html>", { status: 502, statusText: "Bad Gateway" })) as typeof fetch,
    });
    await expect(plain.session()).rejects.toMatchObject({ status: 502, code: "HTTP_502" });

    const offline = createApiClient({
      fetchImpl: (async () => {
        throw new TypeError("Failed to fetch");
      }) as typeof fetch,
    });
    await expect(offline.setupStatus()).rejects.toMatchObject({ status: 0, code: "NETWORK_ERROR" });
  });

  it("reports an unreadable success answer", async () => {
    const client = createApiClient({
      fetchImpl: (async () => new Response("not json", { status: 200 })) as typeof fetch,
    });
    await expect(client.setupStatus()).rejects.toMatchObject({ code: "INVALID_RESPONSE" });
  });

  it("signals an ended session only for authenticated calls", async () => {
    const onUnauthenticated = vi.fn();
    const { fetch } = mockFetch({
      "GET /organizations": apiError(401, "UNAUTHENTICATED"),
      "POST /auth/login": apiError(401, "INVALID_CREDENTIALS"),
      "GET /auth/session": apiError(401, "UNAUTHENTICATED"),
    });
    const client = createApiClient({ fetchImpl: fetch, onUnauthenticated });
    await client.login({ email: "a", password: "b" }).catch(() => undefined);
    await client.session().catch(() => undefined);
    expect(onUnauthenticated).not.toHaveBeenCalled();
    await client.listOrganizations().catch(() => undefined);
    expect(onUnauthenticated).toHaveBeenCalledTimes(1);
  });

  it("keeps the token in memory only", async () => {
    const { fetch } = mockFetch({
      "GET /auth/session": { body: { csrf_token: "secret-token-value" } },
    });
    const client = createApiClient({ fetchImpl: fetch });
    await client.session();
    expect(JSON.stringify({ ...window.localStorage })).not.toContain("secret-token-value");
    expect(JSON.stringify({ ...window.sessionStorage })).not.toContain("secret-token-value");
    expect(window.location.href).not.toContain("secret-token-value");
  });

  it("only uses documented endpoint templates", () => {
    for (const endpoint of Object.values(ENDPOINTS)) {
      expect(endpoint.path.startsWith("/")).toBe(true);
      expect(endpoint.path.startsWith("/api")).toBe(false);
    }
  });

  it("sends preconditions and the idempotency key as headers and builds download URLs", async () => {
    const { fetch, calls } = mockFetch({
      "POST /files/f1/uploads": { status: 201, body: {} },
      "POST /files/f1/versions": { status: 201, body: {} },
      "POST /files/f1/versions/v1/restore": { status: 201, body: {} },
      "PATCH /projects/p1": { body: {} },
    });
    const client = createApiClient({ fetchImpl: fetch });
    await client.createUpload("f1", { size_bytes: 3, sha256: "ab" }, "key-1");
    await client.commitVersion("f1", "u1", null);
    await client.commitVersion("f1", "u1", "head-1", "key-1");
    await client.restoreVersion("f1", "v1", "head-1");
    await client.renameProject({ id: "p1", version: 7 }, "New");
    expect(calls[0]?.headers["idempotency-key"]).toBe("key-1");
    expect(calls[1]?.headers["if-none-match"]).toBe("*");
    expect(calls[1]?.headers["if-match"]).toBeUndefined();
    expect(calls[2]?.headers["if-match"]).toBe('"version:head-1"');
    expect(calls[2]?.headers["idempotency-key"]).toBe("key-1");
    expect(calls[3]?.headers["if-match"]).toBe('"version:head-1"');
    expect(calls[4]?.headers["if-match"]).toBe('"prj-v7"');
    expect(client.downloadUrl("f 1")).toBe("/api/v1/files/f%201/content");
    expect(client.downloadUrl("f1", "v1")).toBe("/api/v1/files/f1/versions/v1/content");
  });

  it("streams an upload body through XHR with the CSRF token and reports errors like fetch does", async () => {
    const { factory, requests } = fakeXhr((request) =>
      request.url.includes("bad")
        ? { status: 429, body: { error: { code: "RATE_LIMITED", message: "m", request_id: "r" } }, headers: { "Retry-After": "7" } }
        : { body: { upload_id: "u", state: "stored" } },
    );
    const { fetch } = mockFetch({ "GET /auth/session": { body: { csrf_token: "tok" } } });
    const client = createApiClient({ fetchImpl: fetch, xhrFactory: factory });
    await client.session();
    const blob = new Blob(["abcd"]);
    const progress: number[] = [];
    await client.uploadContent("good", blob, { onProgress: (p) => progress.push(p.sent) });
    expect(requests[0]?.headers["x-csrf-token"]).toBe("tok");
    expect(requests[0]?.body).toBe(blob);
    expect(progress.at(-1)).toBe(4);
    const failure = await client.uploadContent("bad", blob).catch((e: unknown) => e);
    expect(failure).toMatchObject({ status: 429, code: "RATE_LIMITED", retryAfterSeconds: 7 });
  });

  it("maps an XHR network failure, an abort and a 401 on upload", async () => {
    const outcomes: Array<"network-error" | "hang" | { status: number; body: unknown }> = [
      "network-error",
      "hang",
      { status: 401, body: { error: { code: "UNAUTHENTICATED", message: "m", request_id: null } } },
    ];
    const { factory } = fakeXhr(() => outcomes.shift() ?? "network-error");
    let ended = 0;
    const client = createApiClient({ xhrFactory: factory, onUnauthenticated: () => (ended += 1) });
    const blob = new Blob(["abcd"]);
    await expect(client.uploadContent("u1", blob)).rejects.toMatchObject({ code: "NETWORK_ERROR", status: 0 });
    const controller = new AbortController();
    const pending = client.uploadContent("u2", blob, { signal: controller.signal });
    await new Promise((resolve) => setTimeout(resolve, 5));
    controller.abort();
    await expect(pending).rejects.toMatchObject({ code: ABORTED_ERROR_CODE });
    await expect(client.uploadContent("u3", blob)).rejects.toMatchObject({ status: 401 });
    expect(ended).toBe(1);
    const already = new AbortController();
    already.abort();
    await expect(client.uploadContent("u4", blob, { signal: already.signal })).rejects.toMatchObject({ code: ABORTED_ERROR_CODE });
  });

  it("reads Retry-After as seconds or as an HTTP date", () => {
    const now = Date.parse("2026-10-29T12:00:00Z");
    expect(parseRetryAfter("30", now)).toBe(30);
    expect(parseRetryAfter(" 7 ", now)).toBe(7);
    expect(parseRetryAfter("Thu, 29 Oct 2026 12:01:30 GMT", now)).toBe(90);
    expect(parseRetryAfter("Thu, 29 Oct 2026 11:00:00 GMT", now)).toBe(0); // already past
    expect(parseRetryAfter("Fri, 30 Oct 2099 12:00:00 GMT", now)).toBe(86_400); // capped
    expect(parseRetryAfter("999999999", now)).toBe(86_400);
    expect(parseRetryAfter("soon", now)).toBeUndefined();
    expect(parseRetryAfter("-5", now)).toBeUndefined();
    expect(parseRetryAfter(null, now)).toBeUndefined();
  });
});
