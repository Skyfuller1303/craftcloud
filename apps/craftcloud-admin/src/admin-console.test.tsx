import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Handler } from "./test/helpers.tsx";
import { ORG_A, ORG_B, WS_1 } from "./test/helpers.tsx";
import { apiError, expectNoAxeViolations, loggedInRoutes, mockFetch, renderApp } from "./test/helpers.tsx";
import { BOB, CARA, member, page } from "./test/fixtures.ts";
import { formatBytes } from "./format.ts";

const MEMBERS = [
  { ...member(BOB, "Bob Example", "admin"), status: "active", account_disabled: false, two_factor_enabled: true },
  { ...member(CARA, "Cara Example", "member"), status: "disabled", account_disabled: true, two_factor_enabled: false },
];

function adminRoutes(overrides: Record<string, Handler> = {}): Record<string, Handler> {
  return loggedInRoutes({
    [`GET /organizations/${ORG_A.id}/members`]: page(MEMBERS),
    [`GET /organizations/${ORG_A.id}/invitations`]: page([]),
    [`GET /organizations/${ORG_A.id}/overview`]: { body: { members: { total: 0, by_status: { active: 0, invited: 0, disabled: 0 }, by_role: { owner: 0, admin: 0, member: 0, guest: 0 } }, two_factor: { active_members: 0, enrolled: 0 }, counts: { workspaces: 0, projects: 0, files: 0, active_locks: 0, active_grants: 0, pending_invitations: 0 }, storage: { used_bytes: 0, committed_bytes: 0, reserved_bytes: 0, quota_bytes: null }, activity_daily: Array.from({ length: 30 }, (_, i) => ({ date: `2026-10-${String(i + 1).padStart(2, "0")}`, count: 0 })) } },
    [`GET /organizations/${ORG_A.id}/usage`]: { body: { organization: { quota_bytes: null, committed_bytes: 0, reserved_bytes: 0 }, workspaces: [], next_cursor: null } },
    [`GET /organizations/${ORG_A.id}/grants`]: page([]),
    [`GET /organizations/${ORG_A.id}/locks`]: page([]),
    [`GET /organizations/${ORG_A.id}/audit`]: page([]),
    [`GET /organizations/${ORG_A.id}/apps`]: { body: { mode: "assigned", apps: [{ id: "photocraft", name: "PhotoCraft", assigned_members: 1, active_devices: 2, storage_bytes: 4096 }], seat_usage: { used: 2, limit: 5 } } },
    "GET /info": { body: { features: ["sharing.grants"], feature_flags: { two_factor: true } } },
    ...overrides,
  });
}

describe.each([["en"], ["de"]] as const)("admin console (%s)", (language) => {
  it("assigns app access with per-person results and shows apps usage accessibly", async () => {
    window.history.replaceState(null, "", `/admin/organizations/${ORG_A.id}/people`);
    const { fetch, calls } = mockFetch(adminRoutes({
      [`GET /organizations/${ORG_A.id}/members`]: page([
        ...MEMBERS.map((person) => ({ ...person, role: "member", status: "active", apps: person.user_id === BOB ? ["photocraft"] : [] })),
        { ...member("00000000-0000-4000-8000-0000000000aa", "Dana Admin", "admin"), status: "active", apps: [] },
      ]),
      [`PATCH /organizations/${ORG_A.id}/members/${BOB}/apps`]: { body: { user_id: BOB, apps: ["photocraft"] } },
      [`PATCH /organizations/${ORG_A.id}/members/${CARA}/apps`]: apiError(403, "FORBIDDEN"),
    }));
    const user = userEvent.setup();
    renderApp(fetch, { language });
    await screen.findAllByText("PhotoCraft");
    await user.click(screen.getByLabelText(language === "en" ? "Select Bob Example" : "Bob Example auswählen"));
    await user.click(screen.getByLabelText(language === "en" ? "Select Cara Example" : "Cara Example auswählen"));
    await user.click(screen.getByLabelText(language === "en" ? "Select Dana Admin" : "Dana Admin auswählen"));
    expect(within(screen.getByRole("row", { name: /Dana Admin/ })).getByText(language === "en" ? "Always all apps" : "Immer alle Apps")).toBeTruthy();
    await user.selectOptions(screen.getByLabelText(language === "en" ? "App" : "App"), "photocraft");
    await user.click(screen.getByRole("button", { name: language === "en" ? "Assign selected app" : "Ausgewählte App zuweisen" }));
    await screen.findByText(language === "en" ? /Failed/ : /Fehlgeschlagen/);
    expect(calls.filter((call) => call.method === "PATCH" && call.path.endsWith("/apps"))).toHaveLength(2);
    expect(calls.some((call) => call.path.includes("0000000000aa"))).toBe(false);
    expect(calls.filter((call) => call.method === "PUT" && call.path.endsWith("/apps"))).toHaveLength(0);
    await user.click(screen.getByRole("button", { name: language === "en" ? "Apps" : "Apps" }));
    await screen.findByRole("heading", { level: 2, name: language === "en" ? "Apps usage" : "App-Nutzung" });
    await expectNoAxeViolations();
    expect(screen.getByText("2")).toBeTruthy();
    expect(screen.getByText("PhotoCraft")).toBeTruthy();
  });

  it("preserves page-two assignments across remove then assign and stops after organization change", async () => {
    window.history.replaceState(null, "", `/admin/organizations/${ORG_A.id}/people`);
    const firstPage = Array.from({ length: 50 }, (_, index) => ({
      ...member(`00000000-0000-4000-8000-${String(index + 100).padStart(12, "0")}`, `Person ${index}`, "member"),
      status: "active",
      apps: [],
    }));
    const laterPerson = { ...member(BOB, "Bob Example", "member"), status: "active", apps: ["photocraft"] };
    const laterCara = { ...member(CARA, "Cara Example", "member"), status: "active", apps: [] };
    let bobApps = ["photocraft"];
    const releaseSave: { current?: (reply: { body: unknown }) => void } = {};
    const pending = new Promise<{ body: unknown }>((resolve) => { releaseSave.current = resolve; });
    const { fetch, calls } = mockFetch(adminRoutes({
      [`GET /organizations/${ORG_A.id}/members`]: (call) => call.query.get("cursor") === "next"
        ? page([laterPerson, laterCara])
        : { body: { items: firstPage, next_cursor: "next" } },
      [`GET /organizations/${ORG_A.id}/apps`]: { body: { mode: "assigned", apps: [
        { id: "photocraft", name: "PhotoCraft", assigned_members: 1, active_devices: 1, storage_bytes: 1024 },
        { id: "figcraft", name: "FigCraft", assigned_members: 1, active_devices: 1, storage_bytes: 2048 },
      ], seat_usage: { used: 51, limit: null } } },
      [`PATCH /organizations/${ORG_A.id}/members/${BOB}/apps`]: (call) => {
        const change = call.body as { add?: string[]; remove?: string[] };
        bobApps = [...bobApps.filter((app) => !(change.remove ?? []).includes(app)), ...(change.add ?? [])];
        return { body: { user_id: BOB, apps: bobApps } };
      },
      [`PATCH /organizations/${ORG_A.id}/members/${CARA}/apps`]: () => pending,
    }));
    const user = userEvent.setup();
    renderApp(fetch, { language });
    await screen.findByText("Person 0");
    await user.click(screen.getByRole("button", { name: language === "en" ? "Load more" : "Mehr laden" }));
    await screen.findByText("Bob Example");
    await user.click(screen.getByLabelText(language === "en" ? "Select Bob Example" : "Bob Example auswählen"));
    const appSelect = screen.getByLabelText("App");
    await user.selectOptions(appSelect, "photocraft");
    await user.click(screen.getByRole("button", { name: language === "en" ? "Remove selected app" : "Ausgewählte App entziehen" }));
    await waitFor(() => expect(calls.filter((call) => call.method === "PATCH")).toHaveLength(1));
    await waitFor(() => expect(within(screen.getByRole("row", { name: /Bob Example/ })).getByText(language === "en" ? "None assigned" : "Keine zugewiesen")).toBeTruthy());
    await user.selectOptions(appSelect, "figcraft");
    await user.click(screen.getByRole("button", { name: language === "en" ? "Assign selected app" : "Ausgewählte App zuweisen" }));
    await waitFor(() => expect(calls.filter((call) => call.method === "PATCH")).toHaveLength(2));
    const writes = calls.filter((call) => call.method === "PATCH");
    expect(writes.map((call) => call.body)).toEqual([{ remove: ["photocraft"] }, { add: ["figcraft"] }]);
    await waitFor(() => expect(within(screen.getByRole("row", { name: /Bob Example/ })).getByText("FigCraft")).toBeTruthy());

    await user.click(screen.getByLabelText(language === "en" ? "Select Bob Example" : "Bob Example auswählen"));
    await user.click(screen.getByLabelText(language === "en" ? "Select Cara Example" : "Cara Example auswählen"));
    await user.selectOptions(appSelect, "photocraft");
    await user.click(screen.getByRole("button", { name: language === "en" ? "Assign selected app" : "Ausgewählte App zuweisen" }));
    await waitFor(() => expect(calls.filter((call) => call.method === "PATCH")).toHaveLength(3));
    await user.click(screen.getByRole("button", { name: `${ORG_A.name} ${language === "en" ? "Organization" : "Organisation"}` }));
    await user.click(await screen.findByRole("option", { name: "Beta Lab" }));
    releaseSave.current?.({ body: { user_id: CARA, apps: ["photocraft"] } });
    await waitFor(() => expect(calls.filter((call) => call.method === "PATCH")).toHaveLength(3));
    expect(calls.filter((call) => call.method === "PATCH").map((call) => call.path)).toEqual([
      `/organizations/${ORG_A.id}/members/${BOB}/apps`,
      `/organizations/${ORG_A.id}/members/${BOB}/apps`,
      `/organizations/${ORG_A.id}/members/${CARA}/apps`,
    ]);
  });

  it("loads People on deep link, filters, shows invitation row errors and partial bulk result", async () => {
    window.history.replaceState(null, "", `/admin/organizations/${ORG_A.id}/people`);
    let bobRemoved = false;
    const { fetch, calls } = mockFetch(adminRoutes({
      [`GET /organizations/${ORG_A.id}/members`]: () => page(MEMBERS.filter((member) => !(bobRemoved && member.user_id === BOB))),
      [`DELETE /organizations/${ORG_A.id}/members/${BOB}`]: () => { bobRemoved = true; return { status: 204 }; },
      [`DELETE /organizations/${ORG_A.id}/members/${CARA}`]: apiError(403, "FORBIDDEN"),
      [`POST /organizations/${ORG_A.id}/invitations`]: { status: 201, body: { invitation_id: "invite-1", invite_url: "https://cloud.test/invite#token=secret", expires_at: "2026-11-01T10:00:00Z" } },
    }));
    const user = userEvent.setup();
    renderApp(fetch, { language });
    await screen.findByRole("heading", { level: 1, name: /People|Personen/ });
    await screen.findByText("Bob Example");
    expect(calls.some((call) => call.path === `/organizations/${ORG_A.id}/members` && call.query.get("status") === "all")).toBe(true);
    await screen.findByText(/There are no invitations yet\.|Es gibt noch keine Einladungen/);
    expect(calls.filter((call) => call.method === "GET" && call.path === `/organizations/${ORG_A.id}/invitations`)).toHaveLength(1);
    await expectNoAxeViolations();

    const detailButton = screen.getAllByRole("button", { name: "Details" })[0]!;
    await user.click(detailButton);
    await screen.findByRole("dialog", { name: "Bob Example" });
    await expectNoAxeViolations();
    await user.click(within(screen.getByRole("dialog", { name: "Bob Example" })).getByRole("button", { name: language === "en" ? "Close" : "Schließen" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    await waitFor(() => expect(document.activeElement).toBe(detailButton));

    const emails = screen.getByLabelText(/Email addresses|E-Mail-Adressen/);
    await user.type(emails, "invalid-address\nb@example.test");
    await user.click(screen.getByRole("button", { name: /Send invitations|Einladungen senden/ }));
    await screen.findByText(/Invalid email address|Ungültige E-Mail-Adresse/);
    await screen.findByText("https://cloud.test/invite#token=secret");
    expect(localStorage.length).toBe(0);

    await user.click(screen.getByRole("checkbox", { name: /Select Bob Example|Bob Example auswählen/ }));
    await user.click(screen.getByRole("checkbox", { name: /Select Cara Example|Cara Example auswählen/ }));
    await user.click(screen.getByRole("button", { name: language === "en" ? "Remove selected" : "Auswahl entfernen" }));
    const dialog = await screen.findByRole("dialog");
    await expectNoAxeViolations();
    await user.click(within(dialog).getByRole("button", { name: /Remove selected|Auswahl entfernen/ }));
    await screen.findByText(/Done|Erledigt/);
    await screen.findByText(/Failed|Fehlgeschlagen/);
    expect(within(screen.getByRole("table")).queryByText("Bob Example")).toBeNull();
    const results = screen.getByRole("region", { name: /Batch results|Ergebnisse der Sammelaktion/ });
    expect(within(results).getByText("Bob Example")).toBeTruthy();
    expect(within(results).getByText((_, element) => element?.tagName === "LI" && element.textContent?.includes(language === "en" ? "Done" : "Erledigt") === true)).toBeTruthy();
    await user.selectOptions(screen.getByLabelText(language === "en" ? "Role" : "Rolle"), "member");
    expect(within(results).getByText("Bob Example")).toBeTruthy();
  });

  it("hides admin security fields and direct People route from ordinary member", async () => {
    window.history.replaceState(null, "", `/admin/organizations/${ORG_A.id}/people`);
    const { fetch, calls } = mockFetch(adminRoutes({
      "GET /organizations": { body: { items: [{ ...ORG_A, role: "member" }], next_cursor: null } },
      [`GET /organizations/${ORG_A.id}/members`]: page(MEMBERS.map(({ status: _status, account_disabled: _disabled, two_factor_enabled: _factor, ...ordinary }) => ordinary)),
    }));
    renderApp(fetch, { language });
    await waitFor(() => expect(screen.queryByRole("heading", { level: 1, name: /People|Personen/ })).toBeNull());
    expect(screen.queryByText(/Two-factor authentication|Zwei-Faktor-Anmeldung/)).toBeNull();
    expect(calls.some((call) => call.path === `/organizations/${ORG_A.id}/members` && call.query.get("status") === "all")).toBe(false);
    await expectNoAxeViolations();
  });

  it("confirms operator password reset and shows the one-time link in a themed dialog", async () => {
    window.history.replaceState(null, "", "/admin/accounts");
    const resetUrl = "https://cloud.test/reset#token=one-time-secret";
    const { fetch } = mockFetch(adminRoutes({
      "GET /admin/users": { body: { items: [{ id: BOB, email: "bob@example.test", display_name: "Bob Example", created_at: "2026-10-01T10:00:00Z", disabled: false, email_verified: true, origin: null }], next_cursor: null } },
      [`POST /admin/users/${BOB}/password-reset`]: { body: { reset_url: resetUrl, expires_at: "2026-11-01T10:00:00Z" } },
    }));
    const user = userEvent.setup();
    const prompt = vi.spyOn(window, "prompt").mockImplementation(() => null);
    renderApp(fetch, { language });
    await user.click(await screen.findByRole("button", { name: language === "en" ? "Accounts" : "Konten" }));
    await user.click(await screen.findByRole("button", { name: language === "en" ? "Reset password" : "Passwort zurücksetzen" }));
    const confirm = await screen.findByRole("dialog", { name: language === "en" ? "Issue account reset link" : "Link zum Kontozurücksetzen ausstellen" });
    await expectNoAxeViolations();
    await user.click(within(confirm).getByRole("button", { name: language === "en" ? "Issue reset link" : "Zurücksetzlink ausstellen" }));
    const linkDialog = await screen.findByRole("dialog", { name: language === "en" ? "Password reset link" : "Passwort-Zurücksetzlink" });
    expect((within(linkDialog).getByLabelText(language === "en" ? "One-time reset link" : "Einmaliger Zurücksetzlink") as HTMLInputElement).value).toBe(resetUrl);
    await expectNoAxeViolations();
    expect(JSON.stringify({ ...window.localStorage, ...window.sessionStorage })).not.toContain("one-time-secret");
    expect(prompt).not.toHaveBeenCalled();
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  });

  it("prevents duplicate invitation batches while the first request is pending", async () => {
    window.history.replaceState(null, "", `/admin/organizations/${ORG_A.id}/people`);
    let complete = (_reply: { status: number; body: unknown }) => {};
    const pending = new Promise<{ status: number; body: unknown }>((resolve) => { complete = resolve; });
    const { fetch, calls } = mockFetch(adminRoutes({
      [`POST /organizations/${ORG_A.id}/invitations`]: () => pending,
    }));
    const user = userEvent.setup();
    renderApp(fetch, { language });
    await screen.findByRole("heading", { level: 1, name: /People|Personen/ });
    await user.type(await screen.findByLabelText(/Email addresses|E-Mail-Adressen/), "new@example.test");
    const send = screen.getByRole("button", { name: language === "en" ? "Send invitations" : "Einladungen senden" });
    await user.click(send);
    await waitFor(() => expect((send as HTMLButtonElement).disabled).toBe(true));
    await user.click(send);
    expect(calls.filter((call) => call.method === "POST" && call.path.endsWith("/invitations"))).toHaveLength(1);
    complete({ status: 201, body: { invitation_id: "i2", invite_url: "https://cloud.test/invite#token=one-time", expires_at: "2026-11-01T10:00:00Z" } });
    await waitFor(() => expect((send as HTMLButtonElement).disabled).toBe(false));
  });

  it("syncs organization and workspace scope from browser history without rewriting the target URL", async () => {
    window.history.replaceState(null, "", `/admin/organizations/${ORG_A.id}/people`);
    const { fetch, calls } = mockFetch(adminRoutes({
      "GET /organizations": { body: { items: [ORG_A, { ...ORG_B, role: "owner" }], next_cursor: null } },
      [`GET /organizations/${ORG_B.id}/members`]: page(MEMBERS),
      [`GET /organizations/${ORG_B.id}/invitations`]: page([]),
      [`GET /organizations/${ORG_B.id}/usage`]: { body: { organization: { quota_bytes: null, committed_bytes: 0, reserved_bytes: 0 }, workspaces: [], next_cursor: null } },
      [`GET /organizations/${ORG_B.id}/grants`]: page([]),
    }));
    renderApp(fetch, { language });
    await screen.findByRole("heading", { level: 1, name: /People|Personen/ });
    window.history.pushState(null, "", `/admin/organizations/${ORG_B.id}/people`);
    window.dispatchEvent(new PopStateEvent("popstate"));
    await waitFor(() => expect(calls.some((call) => call.path === `/organizations/${ORG_B.id}/members`)).toBe(true));
    await waitFor(() => expect(window.location.pathname).toBe(`/admin/organizations/${ORG_B.id}/people`));
    window.history.pushState(null, "", `/admin/organizations/${ORG_A.id}/people`);
    window.dispatchEvent(new PopStateEvent("popstate"));
    await waitFor(() => expect(window.location.pathname).toBe(`/admin/organizations/${ORG_A.id}/people`));
    window.history.pushState(null, "", `/admin/workspaces/${WS_1.id}/grants`);
    window.dispatchEvent(new PopStateEvent("popstate"));
    await screen.findByRole("heading", { level: 1, name: language === "en" ? "Grants" : "Freigaben" });
    await waitFor(() => expect(window.location.pathname).toBe(`/admin/workspaces/${WS_1.id}/grants`));
    expect(calls.some((call) => call.path === `/organizations/${ORG_A.id}/grants`)).toBe(true);
  });

  it("keeps workspace quota saves disabled until usage loads, and after a usage error", async () => {
    window.history.replaceState(null, "", `/admin/organizations/${ORG_A.id}/workspaces`);
    let complete = (_reply: { status: number; body: unknown }) => {};
    const pending = new Promise<{ status: number; body: unknown }>((resolve) => { complete = resolve; });
    const { fetch } = mockFetch(adminRoutes({
      [`GET /organizations/${ORG_A.id}/usage`]: () => pending,
    }));
    const user = userEvent.setup();
    const app = renderApp(fetch, { language });
    const quotaInput = await screen.findByLabelText(language === "en" ? `Quota for ${WS_1.name}` : `Kontingent für ${WS_1.name}`) as HTMLInputElement;
    const saveButton = screen.getAllByRole("button", { name: language === "en" ? "Save quota" : "Kontingent speichern" })[0] as HTMLButtonElement;
    expect(saveButton.disabled).toBe(true);
    complete({ status: 200, body: { organization: { quota_bytes: null, committed_bytes: 0, reserved_bytes: 0 }, workspaces: [{ workspace_id: WS_1.id, quota_bytes: 2048 }, { workspace_id: "00000000-0000-4000-8000-0000000000b2", quota_bytes: null }], next_cursor: null } });
    await waitFor(() => expect(quotaInput.value).toBe("2048"));
    expect(saveButton.disabled).toBe(false);
    await user.clear(quotaInput);
    await user.click(saveButton);
    await waitFor(() => expect(saveButton.disabled).toBe(false));
    await expectNoAxeViolations();
    app.unmount();

    window.history.replaceState(null, "", `/admin/organizations/${ORG_A.id}/workspaces`);
    const failed = mockFetch(adminRoutes({ [`GET /organizations/${ORG_A.id}/usage`]: apiError(503, "UNAVAILABLE") }));
    const rerendered = renderApp(failed.fetch, { language });
    await screen.findByRole("alert");
    expect((screen.getAllByRole("button", { name: language === "en" ? "Save quota" : "Kontingent speichern" })[0] as HTMLButtonElement).disabled).toBe(true);
    rerendered.unmount();
  });
});

it("discards a delayed device page after closing and switching accounts", async () => {
  window.history.replaceState(null, "", "/admin/accounts");
  let finishOldPage = (_reply: { status: number; body: unknown }) => {};
  const oldPage = new Promise<{ status: number; body: unknown }>((resolve) => { finishOldPage = resolve; });
  const users = [BOB, CARA].map((id, index) => ({ id, email: `${index === 0 ? "bob" : "cara"}@example.test`, display_name: index === 0 ? "Bob Example" : "Cara Example", created_at: "2026-10-01T10:00:00Z", disabled: false, email_verified: true, origin: null }));
  const { fetch } = mockFetch(adminRoutes({
    "GET /admin/users": { body: { items: users, next_cursor: null } },
    [`GET /admin/users/${BOB}/devices`]: (call) => call.query.get("cursor") === "old-next" ? oldPage : { body: { devices: [{ device_ref: "ds_old", name: "Old browser", client_kind: "browser", client_id: "old", created_at: "2026-10-01T10:00:00Z", last_used_at: null }], next_cursor: "old-next" } },
    [`GET /admin/users/${CARA}/devices`]: { body: { devices: [{ device_ref: "ds_current", name: "Current browser", client_kind: "browser", client_id: "current", created_at: "2026-10-02T10:00:00Z", last_used_at: null }], next_cursor: null } },
  }));
  const user = userEvent.setup();
  renderApp(fetch);
  await user.click(await screen.findByRole("button", { name: "Accounts" }));
  const bobRow = screen.getByRole("row", { name: /Bob Example/ });
  await user.click(within(bobRow).getByRole("button", { name: "Devices" }));
  await screen.findByText(/Old browser/);
  await user.click(screen.getByRole("button", { name: "Load more" }));
  await user.click(screen.getByRole("button", { name: "Close" }));
  const caraRow = screen.getByRole("row", { name: /Cara Example/ });
  await user.click(within(caraRow).getByRole("button", { name: "Devices" }));
  await screen.findByText(/Current browser/);
  finishOldPage({ status: 200, body: { devices: [{ device_ref: "ds_stale", name: "Stale browser", client_kind: "browser", client_id: "stale", created_at: "2026-10-03T10:00:00Z", last_used_at: null }], next_cursor: null } });
  await waitFor(() => expect(screen.queryByText(/Stale browser/)).toBeNull());
  expect(screen.getByText(/Current browser/)).toBeTruthy();
  expect(screen.queryByRole("button", { name: "Load more" })).toBeNull();
});

describe.each([["en", "Overview"], ["de", "Übersicht"]] as const)("admin screen coverage (%s)", (language, overviewTitle) => {
  const org = `/admin/organizations/${ORG_A.id}`;
  it("shows physical workspace storage separately and handles older usage responses", async () => {
    window.history.replaceState(null, "", `${org}/overview`);
    const { fetch } = mockFetch(adminRoutes({
      [`GET /organizations/${ORG_A.id}/usage`]: { body: {
        organization: { quota_bytes: null, committed_bytes: 1536, reserved_bytes: 0, physical_bytes: 900 },
        workspaces: [
          { workspace_id: WS_1.id, effective_limit_bytes: 4096, quota_bytes: 4096, committed_bytes: 1024, reserved_bytes: 512, physical_bytes: 333 },
          { workspace_id: "00000000-0000-4000-8000-000000000042", effective_limit_bytes: null, quota_bytes: null, committed_bytes: 2048, reserved_bytes: 0 },
        ],
        next_cursor: null,
      } },
    }));
    renderApp(fetch, { language });
    await screen.findByRole("heading", { level: 1, name: overviewTitle });
    await screen.findByRole("columnheader", { name: language === "en" ? "Physical storage" : "Physischer Speicher" });
    const workspaceRow = screen.getByRole("row", { name: /333 B/ });
    expect(within(workspaceRow).getByText(formatBytes(language, 1536))).toBeTruthy();
    expect(within(workspaceRow).getByText(formatBytes(language, 333))).toBeTruthy();
    const oldResponseRow = screen.getByRole("row", { name: /Unavailable|Nicht verfügbar/ });
    expect(within(oldResponseRow).getByText(language === "en" ? "Unavailable" : "Nicht verfügbar")).toBeTruthy();
  });

  const cases = [
    [`${org}/overview`, overviewTitle, {}],
    [`${org}/workspaces`, language === "en" ? "Workspaces" : "Workspaces", {}],
    [`${org}/grants`, language === "en" ? "Grants" : "Freigaben", {}],
    [`${org}/locks`, language === "en" ? "Locks" : "Sperren", {}],
    [`${org}/activity`, language === "en" ? "Activity" : "Aktivität", {}],
    [`${org}/org-settings`, language === "en" ? "Organization settings" : "Organisationseinstellungen", {}],
    ["/admin/organizations", language === "en" ? "Organizations" : "Organisationen", {
      "GET /admin/organizations": { body: { items: [], next_cursor: null } },
    }],
    ["/admin/accounts", language === "en" ? "Accounts" : "Konten", {
      "GET /admin/users": { body: { items: [], next_cursor: null } },
    }],
    ["/admin/system", language === "en" ? "System" : "System", {
      "GET /admin/health": { body: {
        status: "ok", checked_at: "2026-10-29T10:00:00Z",
        components: {
          database: { status: "ok", pool: { size: 4, idle: 3, in_use: 1, max: 10 } },
          migrations: { status: "ok" },
          storage: { status: "ok", binding: "matches", free_bytes: 5 * 1024 * 1024 * 1024 },
        },
        uploads: { open_sessions: 3, overdue_sessions: 1 },
        maintenance: { last_run: null, last_run_ok: true, epoch_conflict: false },
        audit: { dropped_events_total: 0, dropped_events_lossy: 0, dropped_events_important: 0 },
        server: { version: "1.2.3", uptime_seconds: 60 },
        backup: { last_success_age_seconds: null },
        archive: {
          configured: false, provider_type: null, endpoint: null, local_bytes: null,
          archive_bytes: null, pending_archives: null, failed_deletions: null,
          last_verified_at: null, last_error_code: null,
        },
      } },
    }],
  ] as const;

  it.each(cases)("renders and passes axe on %s", async (path, heading, extraRoutes) => {
    window.history.replaceState(null, "", path);
    const { fetch } = mockFetch(adminRoutes(extraRoutes));
    renderApp(fetch, { language });
    await screen.findByRole("heading", { level: 1, name: heading });
    await expectNoAxeViolations();
  });
});
