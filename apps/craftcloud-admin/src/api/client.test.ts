import { describe, expect, it, vi } from "vitest";
import { apiError, mockFetch } from "../test/helpers.tsx";
import { ApiError, ENDPOINTS, createApiClient } from "./client.ts";

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

  it("drops the CSRF token even when logout fails", async () => {
    const { fetch } = mockFetch({
      "GET /auth/session": { body: { csrf_token: "tok" } },
      "POST /auth/logout": apiError(503, "AUTH_UNAVAILABLE"),
    });
    const client = createApiClient({ fetchImpl: fetch });
    await client.session();
    expect(client.hasCsrfToken()).toBe(true);
    await expect(client.logout()).rejects.toBeInstanceOf(ApiError);
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
});
