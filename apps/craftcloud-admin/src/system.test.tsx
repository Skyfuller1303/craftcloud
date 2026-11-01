import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import {
  ORG_A,
  SESSION,
  apiError,
  expectNoAxeViolations,
  mockFetch,
  renderApp,
} from "./test/helpers.tsx";
import { viewRoutes } from "./test/fixtures.ts";
import type { AdminHealth } from "./api/types.ts";

type User = ReturnType<typeof userEvent.setup>;

const PLAIN = { ...SESSION, instance_role: null };

const HEALTH: AdminHealth = {
  status: "ok",
  checked_at: "2026-10-29T10:00:00Z",
  components: {
    database: { status: "ok", pool: { size: 4, idle: 3, in_use: 1, max: 10 } },
    migrations: { status: "ok" },
    storage: { status: "ok", binding: "matches", free_bytes: 5 * 1024 * 1024 * 1024 },
  },
  uploads: { open_sessions: 3, overdue_sessions: 1 },
  maintenance: {
    last_run: {
      trigger: "scheduled",
      started_at: "2026-10-29T09:00:00Z",
      finished_at: "2026-10-29T09:01:00Z",
      duration_ms: 60000,
      error_codes: [],
    },
    last_run_ok: true,
    epoch_conflict: false,
  },
  audit: { dropped_events_total: 5, dropped_events_lossy: 4, dropped_events_important: 1 },
  server: { version: "1.2.3", uptime_seconds: 2 * 86400 + 3 * 3600 + 120 },
  backup: { last_success_age_seconds: null },
  archive: {
    configured: true,
    provider_type: "s3",
    endpoint: "https://objects.example.test",
    local_bytes: 1024,
    archive_bytes: 2048,
    pending_archives: 2,
    failed_deletions: 1,
    last_verified_at: "2026-10-29T09:30:00Z",
    last_error_code: "ARCHIVE_DELETE_FAILED",
  },
};

async function openSystem(user: User) {
  await user.click(await screen.findByRole("button", { name: /^(System)$/ }));
  return screen.findByRole("heading", { level: 1 });
}

describe("system view", () => {
  it("lists pending domain claims and approves one, which is then removed from the list", async () => {
    const claim = { organization_id: ORG_A.id, organization_name: "Acme Studio", domain: "acme.test", status: "pending", requested_at: "2026-10-30T10:00:00Z" };
    const { fetch, calls } = mockFetch(viewRoutes({
      "GET /admin/health": { body: HEALTH },
      "GET /admin/sso": { body: { enabled: true } },
      "GET /admin/sso/domain-claims": { body: { items: [claim] } },
      [`PUT /admin/sso/domain-claims/${ORG_A.id}/acme.test`]: { body: { organization_id: ORG_A.id, domain: "acme.test", status: "approved" } },
    }));
    const user = userEvent.setup();
    renderApp(fetch);
    await openSystem(user);
    expect(await screen.findByText("acme.test (Acme Studio)")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Approve acme.test for Acme Studio" }));
    await waitFor(() => expect(screen.queryByText("acme.test (Acme Studio)")).toBeNull());
    expect(calls.find((call) => call.method === "PUT" && call.path.endsWith("/acme.test"))?.body).toEqual({ decision: "approved" });
    expect(screen.getByText("No pending domain claims.")).toBeTruthy();
  });

  it("shows the components, uploads, maintenance, audit counters, version and uptime", async () => {
    const { fetch, calls } = mockFetch(viewRoutes({ "GET /admin/health": { body: HEALTH } }));
    const user = userEvent.setup();
    renderApp(fetch);
    await openSystem(user);
    await screen.findByText("1.2.3");
    expect(screen.getByRole("heading", { level: 1, name: "System" })).toBeTruthy();
    const facts = within(screen.getByRole("main"));
    expect(facts.getByText("2 d 3 h")).toBeTruthy();
    expect(facts.getByText("1 of 10 in use, 3 idle")).toBeTruthy();
    expect(facts.getByText("Paired with this database")).toBeTruthy();
    expect(facts.getByText("5 GiB")).toBeTruthy();
    expect(facts.getByText("objects.example.test", { exact: false })).toBeTruthy();
    expect(facts.getByText("2 KiB")).toBeTruthy();
    expect(facts.getByText("ARCHIVE_DELETE_FAILED")).toBeTruthy();
    expect(facts.getByText("3")).toBeTruthy();
    expect(facts.getByText("Succeeded")).toBeTruthy();
    expect(
      facts.getByText("5 in total (4 denials and failed sign-ins, 1 downloads and exports)"),
    ).toBeTruthy();
    // No backup recorded yet (the age comes from the audit event backup.created).
    expect(facts.getByText(/No successful backup recorded yet/)).toBeTruthy();
    expect(calls.filter((call) => call.path === "/admin/health").length).toBe(1);
    // No alert while everything is fine.
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("explains an epoch conflict and a component that is down", async () => {
    const degraded: AdminHealth = {
      ...HEALTH,
      status: "degraded",
      components: {
        ...HEALTH.components,
        storage: { status: "degraded", binding: "epoch_conflict", free_bytes: null },
      },
      maintenance: {
        ...HEALTH.maintenance,
        last_run_ok: false,
        last_run: { ...HEALTH.maintenance.last_run!, error_codes: ["STORAGE_ERROR"] },
        epoch_conflict: true,
      },
    };
    const { fetch } = mockFetch(viewRoutes({ "GET /admin/health": { body: degraded } }));
    const user = userEvent.setup();
    renderApp(fetch);
    await openSystem(user);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toMatch(/storage-bind/);
    expect(screen.getByText("Epoch conflict: a restore is suspected")).toBeTruthy();
    expect(screen.getByText("Failed: STORAGE_ERROR")).toBeTruthy();
    expect(screen.getAllByText("Degraded").length).toBe(2);
    // Free space that cannot be measured is said so.
    expect(screen.getAllByText("Not available").length).toBeGreaterThan(0);
  });

  it("shows a database that is down and unknown counters without inventing numbers", async () => {
    const down: AdminHealth = {
      ...HEALTH,
      status: "down",
      components: {
        database: { status: "down", pool: { size: 0, idle: 0, in_use: 0, max: 10 } },
        migrations: { status: "down" },
        storage: { status: "ok", binding: "unknown", free_bytes: null },
      },
      uploads: { open_sessions: null, overdue_sessions: null },
      maintenance: { last_run: null, last_run_ok: null, epoch_conflict: false },
    };
    const { fetch } = mockFetch(viewRoutes({ "GET /admin/health": { body: down } }));
    const user = userEvent.setup();
    renderApp(fetch);
    await openSystem(user);
    await screen.findByText(/At least one component is down/);
    expect(screen.getByText("No run recorded yet.")).toBeTruthy();
    expect(screen.getByText("Not checked (database unreachable)")).toBeTruthy();
    expect(screen.getAllByText("Down").length).toBe(3);
  });

  it("refreshes on request and recovers from an error", async () => {
    let answers = 0;
    const { fetch, calls } = mockFetch(
      viewRoutes({
        "GET /admin/health": () => {
          answers += 1;
          return answers === 1 ? apiError(503, "UNAVAILABLE") : { body: HEALTH };
        },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSystem(user);
    await screen.findByRole("alert");
    await user.click(screen.getByRole("button", { name: "Try again" }));
    await screen.findByText("1.2.3");
    expect(screen.queryByRole("alert")).toBeNull();
    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() => expect(calls.filter((c) => c.path === "/admin/health").length).toBe(3));
  });

  it("a 403 from the server is shown as an error, never as data", async () => {
    const { fetch } = mockFetch(viewRoutes({ "GET /admin/health": apiError(403, "FORBIDDEN") }));
    const user = userEvent.setup();
    renderApp(fetch);
    await openSystem(user);
    await screen.findByRole("alert");
    expect(screen.queryByText("1.2.3")).toBeNull();
  });

  it("is not offered to people who are not the operator and never requests the data", async () => {
    const { fetch, calls } = mockFetch(
      viewRoutes({ "GET /auth/session": { body: PLAIN }, "GET /admin/health": { body: HEALTH } }),
    );
    renderApp(fetch);
    const nav = await screen.findByRole("navigation");
    await waitFor(() => expect(within(nav).getAllByRole("button").length).toBeGreaterThan(5));
    expect(within(nav).queryByRole("button", { name: "System" })).toBeNull();
    expect(calls.some((call) => call.path === "/admin/health")).toBe(false);
  });

  it("keeps system access for an operator without an organization", async () => {
    const { fetch, calls } = mockFetch(viewRoutes({
      "GET /organizations": { body: { items: [], next_cursor: null } },
      "GET /admin/health": { body: HEALTH },
    }));
    const user = userEvent.setup();
    renderApp(fetch);
    await user.click(await screen.findByRole("button", { name: "System" }));
    await screen.findByText("1.2.3");
    expect(calls.some((call) => call.path === "/admin/health")).toBe(true);
  });

  it("keeps operator system access while a guest organization is selected", async () => {
    const { fetch, calls } = mockFetch(viewRoutes({
      "GET /organizations": { body: { items: [{ ...ORG_A, role: "guest" }], next_cursor: null } },
      [`GET /organizations/${ORG_A.id}/workspaces`]: { body: { items: [], next_cursor: null } },
      "GET /admin/health": { body: HEALTH },
    }));
    const user = userEvent.setup();
    renderApp(fetch);
    await user.click(await screen.findByRole("button", { name: "System" }));
    await screen.findByText("1.2.3");
    expect(calls.some((call) => call.path === "/admin/health")).toBe(true);
  });

  it("is shown in German", async () => {
    const { fetch } = mockFetch(viewRoutes({ "GET /admin/health": { body: HEALTH } }));
    const user = userEvent.setup();
    renderApp(fetch, { language: "de" });
    await user.click(await screen.findByRole("button", { name: "System" }));
    await screen.findByText("1.2.3");
    expect(screen.getByText("Mit dieser Datenbank gekoppelt")).toBeTruthy();
    expect(screen.getByText("1 von 10 belegt, 3 frei")).toBeTruthy();
    expect(screen.getByText("2 T 3 Std.")).toBeTruthy();
    expect(screen.getByText("Erfolgreich")).toBeTruthy();
  });
});

const languages = [
  ["en", "English"],
  ["de", "German"],
] as const;

describe.each(languages)("axe: system view (%s)", (language) => {
  it("has no violations with data, a conflict, an error and the loading state", async () => {
    let step = 0;
    const conflict: AdminHealth = {
      ...HEALTH,
      status: "degraded",
      maintenance: { ...HEALTH.maintenance, epoch_conflict: true },
    };
    const { fetch } = mockFetch(
      viewRoutes({
        "GET /admin/health": () => {
          step += 1;
          if (step === 1) return { body: HEALTH };
          if (step === 2) return { body: conflict };
          return apiError(503, "UNAVAILABLE");
        },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch, { language });
    const nav = await screen.findByRole("navigation");
    await user.click(within(nav).getAllByRole("button").at(-1)!);
    await waitFor(() => expect(document.querySelectorAll("dl.facts").length).toBeGreaterThan(3));
    await expectNoAxeViolations();

    const refresh = () => document.querySelector<HTMLButtonElement>("main button.button")!;
    // Epoch conflict: an alert next to the data.
    await user.click(refresh());
    await screen.findByRole("alert");
    await waitFor(() => expect(document.querySelectorAll("dl.facts").length).toBeGreaterThan(3));
    await expectNoAxeViolations();

    // Request error: the notice replaces the data.
    await user.click(refresh());
    // Mail status comes from its own successful request and remains visible.
    await waitFor(() => expect(Array.from(document.querySelectorAll("dl.facts"))
      .filter((facts) => !facts.closest('[aria-labelledby="email-delivery-title"]'))).toHaveLength(0));
    await screen.findByRole("alert");
    await expectNoAxeViolations();
  });

  it("is operable by keyboard", async () => {
    const { fetch } = mockFetch(viewRoutes({ "GET /admin/health": { body: HEALTH } }));
    const user = userEvent.setup();
    renderApp(fetch, { language });
    const nav = await screen.findByRole("navigation");
    const system = within(nav).getAllByRole("button").at(-1)!;
    system.focus();
    await user.keyboard("{Enter}");
    await waitFor(() => expect(document.querySelectorAll("dl.facts").length).toBeGreaterThan(3));
    const refresh = document.querySelector<HTMLButtonElement>("main button.button")!;
    refresh.focus();
    expect(document.activeElement).toBe(refresh);
    await user.keyboard("{Enter}");
    await waitFor(() => expect(document.querySelectorAll("dl.facts").length).toBeGreaterThan(3));
  });
});
