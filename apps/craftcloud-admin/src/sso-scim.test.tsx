import { act, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { takeOidcPending } from "./App.tsx";
import { ORG_A, ORG_B, WS_1, apiError, expectNoAxeViolations, loggedInRoutes, mockFetch, renderApp } from "./test/helpers.tsx";
import type { Handler } from "./test/helpers.tsx";

const routes = (role: string = "owner", extra: Record<string, Handler> = {}) => loggedInRoutes({
  "GET /organizations": { body: { items: [{ ...ORG_A, role }, ORG_B], next_cursor: null } },
  "GET /info": { body: { features: [], feature_flags: { two_factor: true } } },
  "GET /auth/2fa": { body: { enabled: false, available: true, recovery_codes_remaining: 0 } },
  [`GET /organizations/${ORG_A.id}/sso`]: { body: { discovery_url: "https://idp.example.test/.well-known/openid-configuration", client_id: "craft", allowed_domains: ["example.test"], enabled: true, jit_enabled: true, sso_required: false, trust_mfa: true, version: 7 } },
  [`GET /organizations/${ORG_A.id}/scim`]: { body: { token_configured: true, groups: [{ id: "engineering", display_name: "Engineering" }] } },
  [`GET /organizations/${ORG_A.id}/scim/mappings`]: { body: { items: [{ group_id: "engineering", workspace_id: WS_1.id, role: "viewer" }] } },
  [`GET /organizations/${ORG_A.id}/scim/events`]: { body: { items: [{ id: "event-1", action: "user.created", created_at: "2026-10-30T10:00:00Z" }] } },
  [`POST /organizations/${ORG_A.id}/scim/token`]: { body: { token: "once-only-secret" } },
  [`PUT /organizations/${ORG_A.id}/sso`]: { body: { version: 8 } },
  [`PUT /organizations/${ORG_A.id}/scim/mappings`]: { body: {} },
  ...extra,
});

function openSettings() {
  window.history.replaceState(null, "", `/admin/organizations/${ORG_A.id}/org-settings`);
}

describe("organization SSO and SCIM settings", () => {
  it.each([["en"], ["de"]] as const)("loads owner controls accessibly in %s", async (language) => {
    openSettings();
    const { fetch } = mockFetch(routes());
    renderApp(fetch, { language });
    await screen.findByLabelText(language === "en" ? "OIDC discovery URL" : "OIDC-Discovery-URL");
    expect((screen.getByLabelText(language === "en" ? "Trust provider multi-factor authentication" : "Mehr-Faktor-Authentifizierung des Anbieters vertrauen") as HTMLInputElement).checked).toBe(true);
    expect(document.body.textContent).not.toContain("client_secret");
    expect(document.body.textContent).not.toContain("server-secret");
    await expectNoAxeViolations();
  });

  it("saves domains, version, and optional secret; clears secret field", async () => {
    openSettings();
    const { fetch, calls } = mockFetch(routes("owner", { [`PUT /organizations/${ORG_A.id}/sso`]: (call) => ({ body: { ...(call.body as object), version: 8 } }) }));
    const user = userEvent.setup();
    renderApp(fetch);
    await screen.findByLabelText("OIDC discovery URL");
    const secret = screen.getByLabelText("Client secret (leave blank to keep current secret)");
    await user.type(secret, "new-secret");
    await user.clear(screen.getByLabelText("Allowed e-mail domains (comma separated)"));
    await user.type(screen.getByLabelText("Allowed e-mail domains (comma separated)"), "example.test, acme.test");
    await user.click(screen.getByRole("button", { name: "Save single sign-on" }));
    await waitFor(() => expect(calls.some((call) => call.method === "PUT" && call.path.endsWith("/sso"))).toBe(true));
    const save = calls.find((call) => call.method === "PUT" && call.path.endsWith("/sso"));
    expect(save?.body).toMatchObject({ version: 7, allowed_domains: ["example.test", "acme.test"], client_secret: "new-secret" });
    await waitFor(() => expect((secret as HTMLInputElement).value).toBe(""));
  });

  it("shows unapproved domains as pending operator approval", async () => {
    openSettings();
    const { fetch } = mockFetch(routes("owner", {
      [`GET /organizations/${ORG_A.id}/sso`]: { body: { discovery_url: "https://idp.example.test/.well-known/openid-configuration", client_id: "craft", allowed_domains: ["example.test", "acme.test"], domain_claims: [{ domain: "example.test", status: "approved" }, { domain: "acme.test", status: "pending" }], enabled: true, jit_enabled: true, sso_required: false, trust_mfa: true, version: 7 } },
    }));
    renderApp(fetch);
    await screen.findByLabelText("OIDC discovery URL");
    const list = await screen.findByRole("list", { name: "Domain approval status" });
    expect(list.textContent).toContain("example.test: Approved");
    expect(list.textContent).toContain("acme.test: Pending operator approval");
  });

  it("restores provider MFA trust when removing SSO configuration", async () => {
    openSettings();
    const { fetch } = mockFetch(routes());
    const user = userEvent.setup();
    renderApp(fetch);
    await screen.findByLabelText("OIDC discovery URL");
    await user.click(screen.getByRole("button", { name: "Remove configuration" }));
    await waitFor(() => expect((screen.getByLabelText("Trust provider multi-factor authentication") as HTMLInputElement).checked).toBe(true));
  });

  it("does not fetch or expose controls for non-owners", async () => {
    openSettings();
    const { fetch, calls } = mockFetch(routes("member"));
    renderApp(fetch);
    expect(await screen.findByText("This view is not available yet.")).toBeTruthy();
    expect(calls.some((call) => call.path.includes("/sso") || call.path.includes("/scim/"))).toBe(false);
    expect(screen.queryByRole("button", { name: "Generate SCIM token" })).toBeNull();
  });

  it("shows token once, then clears it on dismiss and organization change", async () => {
    openSettings();
    const { fetch } = mockFetch(routes());
    const user = userEvent.setup();
    renderApp(fetch);
    await screen.findByLabelText("OIDC discovery URL");
    await user.click(screen.getByRole("button", { name: "Generate SCIM token" }));
    expect(await screen.findByDisplayValue("once-only-secret")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Dismiss token" }));
    expect(screen.queryByDisplayValue("once-only-secret")).toBeNull();
  });

  it("clears an already visible token when switching organizations", async () => {
    openSettings();
    const { fetch } = mockFetch(routes());
    const user = userEvent.setup();
    renderApp(fetch);
    await screen.findByLabelText("OIDC discovery URL");
    await user.click(screen.getByRole("button", { name: "Generate SCIM token" }));
    expect(await screen.findByDisplayValue("once-only-secret")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: /Acme Studio.*Organization/ }));
    await user.click(await screen.findByRole("option", { name: "Beta Lab" }));
    await waitFor(() => expect(screen.queryByLabelText("OIDC discovery URL")).toBeNull());
    expect(screen.queryByDisplayValue("once-only-secret")).toBeNull();
  });

  it("ignores a late token response after organization change", async () => {
    openSettings();
    let release: ((reply: { body: unknown }) => void) | undefined;
    const pending = new Promise<{ body: unknown }>((resolve) => { release = resolve; });
    const { fetch } = mockFetch(routes("owner", { [`POST /organizations/${ORG_A.id}/scim/token`]: () => pending }));
    const user = userEvent.setup();
    renderApp(fetch);
    await screen.findByLabelText("OIDC discovery URL");
    await user.click(screen.getByRole("button", { name: "Generate SCIM token" }));
    await user.click(screen.getByRole("button", { name: /Acme Studio.*Organization/ }));
    await user.click(await screen.findByRole("option", { name: ORG_B.name }));
    await waitFor(() => expect(screen.queryByLabelText("OIDC discovery URL")).toBeNull());
    await act(async () => { release?.({ body: { token: "late-secret" } }); await pending; });
    await waitFor(() => expect(screen.queryByDisplayValue("late-secret")).toBeNull());
  });

  it("saves only viewer/editor mappings and renders SCIM events", async () => {
    openSettings();
    const { fetch, calls } = mockFetch(routes());
    const user = userEvent.setup();
    renderApp(fetch);
    await screen.findByLabelText("OIDC discovery URL");
    expect(await screen.findByText(/SCIM token configured/)).toBeTruthy();
    expect(await screen.findByText("user created")).toBeTruthy();
    expect(screen.getByRole("columnheader", { name: "Date" })).toBeTruthy();
    const role = screen.getByLabelText("Workspace role");
    await user.selectOptions(role, "editor");
    await user.click(screen.getByRole("button", { name: "Save mappings" }));
    await waitFor(() => expect(calls.some((call) => call.method === "PUT" && call.path.endsWith("/scim/mappings"))).toBe(true));
    const mappingSave = calls.find((call) => call.method === "PUT" && call.path.endsWith("/scim/mappings"));
    expect(mappingSave?.body).toEqual({ items: [{ group_id: "engineering", workspace_id: WS_1.id, role: "editor" }] });
  });
});

describe("SSO login and OIDC callback", () => {
  it("keeps local password recovery available after company-domain discovery", async () => {
    const { fetch, calls } = mockFetch({
      "GET /setup/status": { body: { setup_required: false } },
      "GET /auth/session": apiError(401, "UNAUTHENTICATED"),
      "POST /auth/oidc/discover": { body: { organization_id: ORG_A.id } },
    });
    const user = userEvent.setup();
    renderApp(fetch);
    await user.type(await screen.findByLabelText("E-mail address"), "ada@example.test");
    expect(await screen.findByLabelText("Password")).toBeTruthy();
    expect(await screen.findByRole("button", { name: "Sign in with company account" })).toBeTruthy();
    expect(calls.some((call) => call.path === "/auth/login")).toBe(false);
  });

  it("keeps password sign-in usable when company discovery fails", async () => {
    const { fetch, calls } = mockFetch({
      "GET /setup/status": { body: { setup_required: false } },
      "GET /auth/session": apiError(401, "UNAUTHENTICATED"),
      "POST /auth/oidc/discover": apiError(503, "SSO_UNAVAILABLE"),
      "POST /auth/login": apiError(401, "INVALID_CREDENTIALS"),
    });
    const user = userEvent.setup(); renderApp(fetch);
    await user.type(await screen.findByLabelText("E-mail address"), "operator@example.test");
    await waitFor(() => expect(calls.some((call) => call.path === "/auth/oidc/discover")).toBe(true));
    await user.type(screen.getByLabelText("Password"), "synthetic-password");
    await user.click(screen.getByRole("button", {name:"Log in"}));
    await waitFor(() => expect(calls.some((call) => call.path === "/auth/login")).toBe(true));
  });

  it("consumes OIDC fragment before rendering; asks local password, then factor; rejects hostile native callback", async () => {
    const replaceState = vi.fn();
    expect(takeOidcPending({ pathname: "/admin", hash: "#oidc=pending-1" }, { replaceState } as unknown as History, "/admin")).toBe("pending-1");
    expect(replaceState).toHaveBeenCalledWith(null, "", "/admin");
    const { fetch, calls } = mockFetch({
      "GET /setup/status": { body: { setup_required: false } },
      "GET /auth/session": apiError(401, "UNAUTHENTICATED"),
      "POST /auth/oidc/pending": { body: { desktop: null } },
      "POST /auth/oidc/finish": (call) => {
        const body = call.body as Record<string, unknown>;
        if (body.password === undefined) return apiError(400, "SSO_LINK_REQUIRED");
        if (body.code === undefined) return apiError(400, "SSO_SECOND_FACTOR_REQUIRED");
        return { body: { redirect_url: "https://evil.example/steal?code=x&state=y" } };
      },
    });
    const user = userEvent.setup();
    renderApp(fetch, { oidcPending: "pending-1" });
    expect(await screen.findByLabelText("Password")).toBeTruthy();
    expect(screen.queryByLabelText("Authentication code")).toBeNull();
    expect(calls.some((call) => call.path === "/auth/oidc/finish" && (call.body as Record<string, unknown>).password === undefined)).toBe(true);
    await user.type(screen.getByLabelText("Password"), "local-password");
    await user.click(screen.getByRole("button", { name: "Continue" }));
    await user.type(await screen.findByLabelText("Authentication code"), "123456");
    await user.click(screen.getByRole("button", { name: "Continue" }));
    expect((await screen.findByRole("alert")).textContent).toContain("Invalid native sign-in destination.");
    expect(calls.some((call) => call.path === "/auth/oidc/finish" && (call.body as Record<string, unknown>).code === "123456")).toBe(true);
  });
});

describe("native OIDC confirmation", () => {
  it("asks which device and app signs in before completing a native flow", async () => {
    const { fetch, calls } = mockFetch({
      "GET /setup/status": { body: { setup_required: false } },
      "GET /auth/session": apiError(401, "UNAUTHENTICATED"),
      "POST /auth/oidc/pending": { body: { desktop: { client_kind: "launcher", device_name: "Studio PC" } } },
      "POST /auth/oidc/finish": { body: { redirect_url: "http://127.0.0.1:43121/oidc/callback?code=c&state=s" } },
    });
    const user = userEvent.setup();
    renderApp(fetch, { oidcPending: "pending-native" });
    expect(await screen.findByText("Sign in CraftCloud on Studio PC?")).toBeTruthy();
    expect(screen.getByText("Application type: launcher")).toBeTruthy();
    expect(calls.some((call) => call.path === "/auth/oidc/finish")).toBe(false);
    await user.click(screen.getByRole("button", { name: "Sign in on this device" }));
    await waitFor(() => expect(calls.some((call) => call.path === "/auth/oidc/finish")).toBe(true));
    expect(calls.find((call) => call.path === "/auth/oidc/finish")?.body).toMatchObject({ pending_id: "pending-native", confirm_desktop: true });
  });

  it("cancelling a native confirmation never calls finish", async () => {
    const { fetch, calls } = mockFetch({
      "GET /setup/status": { body: { setup_required: false } },
      "GET /auth/session": apiError(401, "UNAUTHENTICATED"),
      "POST /auth/oidc/pending": { body: { desktop: { client_kind: "app", device_name: "Laptop" } } },
    });
    const user = userEvent.setup();
    renderApp(fetch, { oidcPending: "pending-native" });
    await screen.findByText("Sign in CraftCloud on Laptop?");
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    expect(calls.some((call) => call.path === "/auth/oidc/finish")).toBe(false);
  });
});
