import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { Handler } from "./test/helpers.tsx";
import {
  ORG_A,
  SESSION,
  WS_1,
  apiError,
  expectNoAxeViolations,
  loggedInRoutes,
  mockFetch,
  renderApp,
} from "./test/helpers.tsx";
import { FILE_1, HEAD, PROJECT_1, page, version } from "./test/fixtures.ts";

const ORG = `/organizations/${ORG_A.id}`;

const days = (counts: Record<number, number> = {}) =>
  Array.from({ length: 30 }, (_, index) => ({
    date: `2026-10-${String(index + 1).padStart(2, "0")}`,
    count: counts[index] ?? 0,
  }));

const OVERVIEW = {
  members: {
    total: 6,
    by_status: { active: 4, invited: 1, disabled: 1 },
    by_role: { owner: 1, admin: 1, member: 1, guest: 1 },
  },
  two_factor: { active_members: 4, enrolled: 1 },
  counts: { workspaces: 3, projects: 5, files: 12, active_locks: 2, active_grants: 7, pending_invitations: 2 },
  storage: { used_bytes: 5 * 1024 * 1024, committed_bytes: 4 * 1024 * 1024, reserved_bytes: 1024 * 1024, quota_bytes: 10 * 1024 * 1024 },
  activity_daily: days({ 27: 3, 28: 9, 29: 1 }),
};

function adminRoutes(overrides: Record<string, Handler> = {}): Record<string, Handler> {
  return loggedInRoutes({
    [`GET ${ORG}/overview`]: { body: OVERVIEW },
    [`GET ${ORG}/usage`]: {
      body: {
        organization: { quota_bytes: null, committed_bytes: 0, reserved_bytes: 0 },
        workspaces: [],
        next_cursor: null,
      },
    },
    [`GET ${ORG}/audit`]: page([]),
    "GET /info": { body: { features: ["sharing.grants"], feature_flags: { two_factor: true } } },
    "GET /auth/devices": { body: { devices: [] } },
    ...overrides,
  });
}

describe.each([
  ["en", "Overview", "Activity in the last 30 days", "Active people", "Show data as table"],
  ["de", "Übersicht", "Aktivität in den letzten 30 Tagen", "Aktive Personen", "Daten als Tabelle anzeigen"],
] as const)("organization overview (%s)", (language, heading, chartName, peopleTile, tableToggle) => {
  it("shows counts from the single overview call and an accessible activity chart", async () => {
    window.history.replaceState(null, "", `/admin/organizations/${ORG_A.id}/overview`);
    const { fetch, calls } = mockFetch(adminRoutes());
    renderApp(fetch, { language });
    await screen.findByRole("heading", { level: 1, name: heading });
    const tile = await screen.findByText(peopleTile);
    expect(tile.nextElementSibling?.textContent).toBe("4");
    const metrics = tile.closest("dl")!;
    expect(within(metrics).getByText("25%")).toBeTruthy();
    expect(metrics.textContent).toContain("12");
    expect(metrics.textContent).toContain("5 MiB");
    expect(metrics.textContent).toContain("10 MiB");

    // The chart has a text alternative and the same numbers as a table.
    const chart = screen.getByRole("img", { name: new RegExp(chartName) });
    expect(chart.getAttribute("aria-labelledby")).toBeTruthy();
    expect(chart.querySelector("desc")?.textContent).toMatch(/13/);
    expect(chart.querySelectorAll("rect")).toHaveLength(3);
    const user = userEvent.setup();
    await user.click(screen.getByText(tableToggle));
    const table = within(screen.getByRole("group", { hidden: false }) ?? document.body).getByRole("table", { name: chartName });
    expect(within(table).getAllByRole("row")).toHaveLength(31);
    await expectNoAxeViolations();

    // No crawling of member, invitation, lock or grant lists any more.
    const paths = calls.map((call) => call.path);
    expect(paths.filter((path) => path === `${ORG}/overview`)).toHaveLength(1);
    for (const crawled of ["members", "invitations", "locks", "grants"]) {
      expect(paths).not.toContain(`${ORG}/${crawled}`);
    }
    expect(screen.queryByText(/listing limit/)).toBeNull();
  });

  it("explains an empty activity series and hides two-factor coverage without the feature", async () => {
    window.history.replaceState(null, "", `/admin/organizations/${ORG_A.id}/overview`);
    const { fetch } = mockFetch(adminRoutes({
      [`GET ${ORG}/overview`]: { body: { ...OVERVIEW, activity_daily: days() } },
      "GET /info": { body: { features: [], feature_flags: { two_factor: false } } },
    }));
    renderApp(fetch, { language });
    await screen.findByText(peopleTile);
    const chart = screen.getByRole("img", { name: new RegExp(chartName) });
    expect(chart.querySelector("desc")?.textContent).toMatch(language === "en" ? /No activity/ : /Keine Aktivität/);
    expect(chart.querySelectorAll("rect")).toHaveLength(0);
    expect(screen.queryByText(language === "en" ? "Two-factor coverage" : "Abdeckung der Zwei-Faktor-Anmeldung")).toBeNull();
    await expectNoAxeViolations();
  });
});

describe("organization overview errors", () => {
  it("shows the error and loads again on retry", async () => {
    window.history.replaceState(null, "", `/admin/organizations/${ORG_A.id}/overview`);
    let attempts = 0;
    const { fetch } = mockFetch(adminRoutes({
      [`GET ${ORG}/overview`]: () => (++attempts === 1 ? apiError(503, "UNAVAILABLE") : { body: OVERVIEW }),
    }));
    const user = userEvent.setup();
    renderApp(fetch);
    await screen.findByRole("alert");
    await user.click(screen.getByRole("button", { name: "Try again" }));
    await screen.findByText("Active people");
    expect(attempts).toBe(2);
  });
});

describe.each([
  ["en", "Profile", "Display name", "Save display name"],
  ["de", "Profil", "Anzeigename", "Anzeigenamen speichern"],
] as const)("profile (%s)", (language, title, nameLabel, saveLabel) => {
  const open = async (user: ReturnType<typeof userEvent.setup>) => {
    await user.click(await screen.findByRole("button", { name: /^(Settings|Einstellungen)$/ }));
    return screen.findByRole("heading", { level: 2, name: title });
  };

  it("saves the trimmed display name, updates the header and keeps the e-mail read-only", async () => {
    let body: unknown = null;
    const { fetch } = mockFetch(adminRoutes({
      "PATCH /auth/profile": (call) => {
        body = call.body;
        return { body: { ...SESSION, display_name: "Ada Lovelace" } };
      },
    }));
    const user = userEvent.setup();
    renderApp(fetch, { language });
    await open(user);
    const input = screen.getByLabelText(nameLabel);
    expect((input as HTMLInputElement).value).toBe("Ada Example");
    // The e-mail address is text, not an input.
    expect(screen.queryByDisplayValue("ada@example.test")).toBeNull();
    expect(screen.getAllByText("ada@example.test").length).toBeGreaterThan(0);
    expect(screen.getByRole("button", { name: saveLabel }).hasAttribute("disabled")).toBe(true);
    await user.clear(input);
    await user.type(input, "  Ada Lovelace  ");
    await expectNoAxeViolations();
    await user.click(screen.getByRole("button", { name: saveLabel }));
    await waitFor(() => expect(body).toEqual({ display_name: "Ada Lovelace" }));
    await waitFor(() => expect(document.querySelector(".shell__user-name")?.textContent).toBe("Ada Lovelace"));
    expect((screen.getByLabelText(nameLabel) as HTMLInputElement).value).toBe("Ada Lovelace");
    expect(screen.getByRole("link", { name: /Two-factor authentication|Zwei-Faktor-Anmeldung/ }).getAttribute("href")).toBe("#two-factor-title");
    expect(screen.getByRole("link", { name: /Devices|Geräte/ }).getAttribute("href")).toBe("#devices-title");
  });

  it("validates before sending and shows server field errors", async () => {
    let requests = 0;
    const { fetch } = mockFetch(adminRoutes({
      "PATCH /auth/profile": () => {
        requests += 1;
        return apiError(422, "VALIDATION_ERROR", { details: { fields: ["display_name"] } });
      },
    }));
    const user = userEvent.setup();
    renderApp(fetch, { language });
    await open(user);
    const input = screen.getByLabelText(nameLabel);
    await user.clear(input);
    await user.type(input, "   ");
    await user.click(screen.getByRole("button", { name: saveLabel }));
    expect(requests).toBe(0);
    expect(await screen.findByText(/1 (to|bis) 100/)).toBeTruthy();
    expect(document.activeElement).toBe(input);
    await user.clear(input);
    await user.type(input, "Valid name");
    await user.click(screen.getByRole("button", { name: saveLabel }));
    await waitFor(() => expect(requests).toBe(1));
    expect(await screen.findByText(/1 (to|bis) 100/)).toBeTruthy();
    await expectNoAxeViolations();
  });
});

describe("guest view", () => {
  const guestOrg = { ...ORG_A, role: "guest" };
  const sibling = { ...FILE_1, id: "00000000-0000-4000-8000-0000000000d9", name: "secret-sibling.fig" };
  const shared = (role: "viewer" | "editor") => ({
    target_kind: "file",
    target_id: FILE_1.id,
    role,
    workspace: { id: WS_1.id, name: "Studio" },
    project: { id: PROJECT_1.id, name: PROJECT_1.name },
    file: { id: FILE_1.id, name: FILE_1.name },
  });
  const guestRoutes = (role: "viewer" | "editor"): Record<string, Handler> => ({
    "GET /setup/status": { body: { setup_required: false } },
    "GET /auth/session": { body: { ...SESSION, instance_role: null } },
    "GET /organizations": page([guestOrg]),
    [`GET ${ORG}/workspaces`]: page([]),
    "GET /info": { body: { api_version: "1.0", features: ["sharing.grants"], server_mode: "development-prototype" } },
    [`GET ${ORG}/shared-with-me`]: page([shared(role)]),
    [`GET /files/${FILE_1.id}`]: { body: { ...FILE_1, lock: null } },
    [`GET /files/${FILE_1.id}/versions`]: page([version(2, HEAD), version(1, "00000000-0000-4000-8000-0000000000e0")]),
    "POST /auth/logout": { status: 204 },
  });

  describe.each([["en", /Shared with me/, /Versions/], ["de", /Für mich freigegeben/, /Versionen/]] as const)("(%s)", (language, sharedName, versionsName) => {
    it.each(["viewer", "editor"] as const)("shows only the shared %s item with details, download and versions", async (role) => {
      const { fetch, calls } = mockFetch(guestRoutes(role));
      const user = userEvent.setup();
      renderApp(fetch, { language });
      // The guest never sees navigation into workspaces, projects or administration.
      const nav = await screen.findByRole("navigation");
      await waitFor(() => expect(within(nav).getByRole("button", { name: sharedName })).toBeTruthy());
      expect(within(nav).queryByRole("button", { name: /Projects|Projekte|Files|Dateien|Members|Mitglieder|People|Personen/ })).toBeNull();
      expect(within(nav).queryByText(/Administration|Verwaltung|Instance|Instanz/)).toBeNull();
      // "Shared with me" is the guest's home page; no click needed.
      expect(await screen.findByRole("heading", { level: 1, name: sharedName })).toBeTruthy();
      expect(within(nav).queryByRole("button", { name: /^(Overview|Übersicht)$/ })).toBeNull();
      await screen.findByText(FILE_1.name);
      expect(screen.queryByText("secret-sibling.fig")).toBeNull();
      expect(screen.queryByText(PROJECT_1.name)).toBeNull();
      expect(screen.queryByText("Studio")).toBeNull();
      // Details of the item: format, size and the time of the last change.
      expect(await screen.findByText(/fig · .*5 MiB.* · /)).toBeTruthy();
      const download = screen.getByRole("link", { name: new RegExp(FILE_1.name) });
      expect(download.getAttribute("href")).toBe(`/api/v1/files/${FILE_1.id}/content`);
      expect(screen.getByText(role === "editor" ? (language === "en" ? "Editor" : "Bearbeiter") : (language === "en" ? "Viewer" : "Betrachter"))).toBeTruthy();
      await user.click(screen.getByRole("button", { name: versionsName }));
      const dialog = await screen.findByRole("dialog");
      await within(dialog).findAllByText(/2/);
      const restore = within(dialog).queryAllByRole("button", { name: /restore|wiederherstellen/i });
      expect(restore.length > 0).toBe(role === "editor");
      await expectNoAxeViolations();
      // Nothing outside the grant was requested.
      const paths = calls.map((call) => call.path);
      expect(paths.some((path) => path.startsWith("/projects/") || path.startsWith("/workspaces/"))).toBe(false);
      expect(paths).not.toContain(`${ORG}/members`);
      expect(paths).not.toContain(`${ORG}/overview`);
      expect(sibling.name).toBe("secret-sibling.fig");
    });
  });
});
