import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { apiError, expectNoAxeViolations, mockFetch, ORG_A, SESSION } from "./test/helpers.tsx";
import { FILE_1, ME, PROJECT_1, page, viewRoutes, BOB, member } from "./test/fixtures.ts";
import { renderApp } from "./test/helpers.tsx";
import type { ShareGrant } from "./api/types.ts";

const grant: ShareGrant = {
  id: "00000000-0000-4000-8000-0000000000f1",
  target_kind: "project" as const,
  target_id: PROJECT_1.id,
  grantee: { user_id: BOB, display_name: "Bob", email: "bob@example.test" },
  role: "viewer" as const,
  created_by: ME,
  created_at: "2026-10-04T10:00:00Z",
  version: 2,
};

const locales = ["en", "de"] as const;

describe.each(locales)("sharing dialog (%s)", (language) => {
  const shareName = language === "en" ? "Share Brand kit" : "Brand kit teilen";
  it("searches, adds, changes with If-Match, revokes, and supports keyboard focus", async () => {
    let current: typeof grant | null = null;
    const routes = viewRoutes({
      "GET /info": { body: { api_version: "1.0", features: ["sharing.grants", "sharing.edit-locks"], server_mode: "development-prototype" } },
      [`GET /workspaces/${PROJECT_1.workspace_id}/projects`]: page([{ ...PROJECT_1, can_share: true }]),
      [`GET /projects/${PROJECT_1.id}/grants`]: () => page(current === null ? [] : [current]),
      [`GET /organizations/${ORG_A.id}/members`]: page([member(BOB, "Bob", "member")]),
      [`POST /projects/${PROJECT_1.id}/grants`]: (call) => {
        expect(call.body).toEqual({ user_id: BOB, role: "editor" });
        current = { ...grant, role: "editor" };
        return { status: 201, body: current };
      },
      [`PATCH /grants/${grant.id}`]: (call) => {
        expect(call.headers["if-match"]).toBe('"grant-v2"');
        expect(call.body).toEqual({ role: "viewer" });
        current = { ...grant, version: 3 };
        return { body: current };
      },
      [`DELETE /grants/${grant.id}`]: () => {
        current = null;
        return { status: 204 };
      },
    });
    const { fetch } = mockFetch(routes);
    const user = userEvent.setup();
    renderApp(fetch, { language });
    await user.click(await screen.findByRole("button", { name: /Projects|Projekte/ }));
    const share = await screen.findByRole("button", { name: shareName });
    await user.click(share);
    const dialog = await screen.findByRole("dialog");
    await expectNoAxeViolations();
    await user.keyboard("{Tab}");
    expect(dialog.contains(document.activeElement)).toBe(true);
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    await waitFor(() => expect(document.activeElement).toBe(share));
    await user.click(share);
    const activeDialog = await screen.findByRole("dialog");
    const search = within(activeDialog).getByRole("textbox", { name: /Search organization members|Organisationsmitglieder suchen/ });
    await user.type(search, "bo");
    const person = await within(activeDialog).findByRole("combobox", { name: /Person/ });
    await user.selectOptions(person, BOB);
    const role = within(activeDialog).getByRole("combobox", { name: /^Role$|^Rolle$/ });
    await user.selectOptions(role, "editor");
    await user.keyboard("{Tab}");
    await expectNoAxeViolations();
    await user.click(within(activeDialog).getByRole("button", { name: /Share|Teilen/ }));
    await screen.findByText("Bob");
    const roleForBob = within(activeDialog).getByRole("combobox", { name: /Role for Bob|Rolle für Bob/ });
    await user.selectOptions(roleForBob, "viewer");
    await waitFor(() => expect(current?.version).toBe(3));
    await user.click(within(activeDialog).getByRole("button", { name: /Remove|Entfernen/ }));
    const confirmation = await screen.findByRole("dialog", { name: /Remove access|Zugriff entfernen/ });
    await expectNoAxeViolations();
    await user.click(within(confirmation).getByRole("button", { name: /Remove|Entfernen/ }));
    await waitFor(() => expect(current).toBeNull());
  });

  it("shows stale grant errors and offers reload", async () => {
    const routes = viewRoutes({
      "GET /info": { body: { api_version: "1.0", features: ["sharing.grants"], server_mode: "development-prototype" } },
      [`GET /workspaces/${PROJECT_1.workspace_id}/projects`]: page([{ ...PROJECT_1, can_share: true }]),
      [`GET /projects/${PROJECT_1.id}/grants`]: page([grant]),
      [`PATCH /grants/${grant.id}`]: apiError(412, "VERSION_CONFLICT"),
    });
    const { fetch } = mockFetch(routes);
    const user = userEvent.setup();
    renderApp(fetch, { language });
    await user.click(await screen.findByRole("button", { name: /Projects|Projekte/ }));
    await user.click(await screen.findByRole("button", { name: shareName }));
    const dialog = await screen.findByRole("dialog");
    const role = await within(dialog).findByRole("combobox", { name: /Role for Bob|Rolle für Bob/ });
    await user.selectOptions(role, "editor");
    await within(dialog).findByRole("alert");
    await expectNoAxeViolations();
    await user.click(within(dialog).getByRole("button", { name: /Reload|Neu laden/ }));
    await waitFor(() => expect(within(dialog).queryByRole("alert")).toBeNull());
  });

  it("keeps the failed grant and role for retry while a change is pending", async () => {
    let finishFirst: (reply: ReturnType<typeof apiError>) => void = () => undefined;
    let changes = 0;
    const bodies: unknown[] = [];
    const routes = viewRoutes({
      "GET /info": { body: { api_version: "1.0", features: ["sharing.grants"], server_mode: "development-prototype" } },
      [`GET /workspaces/${PROJECT_1.workspace_id}/projects`]: page([{ ...PROJECT_1, can_share: true }]),
      [`GET /projects/${PROJECT_1.id}/grants`]: page([grant]),
      [`PATCH /grants/${grant.id}`]: (call) => {
        bodies.push(call.body);
        changes += 1;
        if (changes === 1) return new Promise((resolve) => { finishFirst = resolve; });
        return { body: { ...grant, role: "editor", version: 3 } };
      },
    });
    const { fetch, calls } = mockFetch(routes);
    const user = userEvent.setup();
    renderApp(fetch, { language });
    await user.click(await screen.findByRole("button", { name: /Projects|Projekte/ }));
    await user.click(await screen.findByRole("button", { name: shareName }));
    const dialog = await screen.findByRole("dialog");
    const role = await within(dialog).findByRole("combobox", { name: /Role for Bob|Rolle f.?.r Bob/ });
    await user.selectOptions(role, "editor");
    await waitFor(() => expect(calls.filter((call) => call.method === "PATCH").length).toBe(1));
    expect((role as HTMLSelectElement).disabled).toBe(true);
    await user.selectOptions(role, "viewer");
    expect(calls.filter((call) => call.method === "PATCH").length).toBe(1);
    finishFirst(apiError(500, "UNAVAILABLE"));
    await within(dialog).findByRole("alert");
    await user.click(within(dialog).getByRole("button", { name: /Try again|Erneut versuchen/ }));
    await waitFor(() => expect(calls.filter((call) => call.method === "PATCH").length).toBe(2));
    expect(bodies).toEqual([{ role: "editor" }, { role: "editor" }]);
  });

  it("keeps revoke failure inside confirmation and reloads after 403", async () => {
    const routes = viewRoutes({
      "GET /info": { body: { api_version: "1.0", features: ["sharing.grants"], server_mode: "development-prototype" } },
      [`GET /workspaces/${PROJECT_1.workspace_id}/projects`]: page([{ ...PROJECT_1, can_share: true }]),
      [`GET /projects/${PROJECT_1.id}/grants`]: page([grant]),
      [`DELETE /grants/${grant.id}`]: apiError(403, "FORBIDDEN"),
    });
    const { fetch } = mockFetch(routes);
    const user = userEvent.setup();
    renderApp(fetch, { language });
    await user.click(await screen.findByRole("button", { name: /Projects|Projekte/ }));
    await user.click(await screen.findByRole("button", { name: shareName }));
    const shareDialog = await screen.findByRole("dialog", { name: /Share|teilen/i });
    await user.click(within(shareDialog).getByRole("button", { name: /Remove|Entfernen/ }));
    const confirmation = await screen.findByRole("dialog", { name: /Remove access|Zugriff entfernen/ });
    await user.click(within(confirmation).getByRole("button", { name: /Remove|Entfernen/ }));
    await within(confirmation).findByRole("alert");
    await expectNoAxeViolations();
    await user.click(within(confirmation).getByRole("button", { name: /Reload|Neu laden/ }));
    await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
  });
});

describe.each([["en", /Shared with me/], ["de", /Für mich freigegeben/]] as const)("guest shared view (%s)", (language, sharedName) => {
it("shows only shared navigation and same-user other-session lock", async () => {
  const guestOrg = { ...ORG_A, role: "guest" };
  const lockedFile = {
    ...FILE_1,
    app: "photocraft",
    lock: { holder_display_name: "Ada", since: "2026-10-30T10:00:00Z", expires_at: "2026-10-30T10:05:00Z", mine: true, held_by_current_session: false },
  };
  const routes = {
    "GET /setup/status": { body: { setup_required: false } },
    "GET /auth/session": { body: SESSION },
    "GET /organizations": page([guestOrg]),
    [`GET /organizations/${ORG_A.id}/workspaces`]: page([]),
    "GET /info": { body: { api_version: "1.0", features: ["sharing.grants", "sharing.edit-locks"], server_mode: "development-prototype" } },
    [`GET /organizations/${ORG_A.id}/shared-with-me`]: page([{ target_kind: "file", target_id: lockedFile.id, role: "editor", workspace: { id: "w", name: "Guests" }, project: { id: PROJECT_1.id, name: PROJECT_1.name }, file: { id: lockedFile.id, name: lockedFile.name } }]),
    [`GET /files/${lockedFile.id}`]: { body: lockedFile },
    [`GET /files/${lockedFile.id}/versions`]: page([]),
  };
  const { fetch } = mockFetch(routes);
  const user = userEvent.setup();
  renderApp(fetch, { language });
  await user.click(await screen.findByRole("button", { name: sharedName }));
  expect(screen.queryByRole("button", { name: /Members|Invitations|Projects/ })).toBeNull();
  expect(await screen.findByText(language === "en" ? /In progress by Ada/ : /In Bearbeitung von Ada/)).toBeTruthy();
  await expectNoAxeViolations();
  await user.click(screen.getByRole("button", { name: language === "en" ? /Versions/ : /Versionen/ }));
  await screen.findByRole("dialog");
  const versionsDialog = screen.getByRole("dialog");
  expect(within(versionsDialog).getByText(language === "en" ? /A new version is unavailable/ : /Eine neue Version ist erst verfügbar/)).toBeTruthy();
  await expectNoAxeViolations();
});
});

it("blocks web upload for an own lock held by another session and confirms admin force", async () => {
  let locked = true;
  const file = {
    ...FILE_1,
    app: "photocraft",
    can_force_lock: true,
    lock: { holder_display_name: "Ada", since: "2026-10-30T10:00:00Z", expires_at: "2026-10-30T10:05:00Z", mine: true, held_by_current_session: false },
  };
  const routes = viewRoutes({
    "GET /info": { body: { api_version: "1.0", features: ["sharing.grants", "sharing.edit-locks"], server_mode: "development-prototype" } },
    [`GET /projects/${PROJECT_1.id}/files`]: () => page([{ ...file, lock: locked ? file.lock : null }]),
    [`DELETE /files/${file.id}/lock`]: (call) => {
      expect(call.query.get("force")).toBe("true");
      locked = false;
      return { status: 204 };
    },
  });
  const { fetch } = mockFetch(routes);
  const user = userEvent.setup();
  renderApp(fetch, { language: "en" });
  await user.click(await screen.findByRole("button", { name: /Projects|Projekte/ }));
  await user.click(await screen.findByRole("button", { name: /Open files of Brand kit/ }));
  await screen.findByRole("table");
  const fileRow = screen.getAllByRole("row").find((row) => row.textContent?.includes(file.name));
  expect(within(fileRow!).getByRole("button", { name: `Upload a new version of ${file.name}` }).hasAttribute("disabled")).toBe(true);
  expect(screen.getByText(/A new version is unavailable/)).toBeTruthy();
  await user.click(within(fileRow!).getByRole("button", { name: `Release lock for ${file.name}` }));
  const confirmation = await screen.findByRole("dialog", { name: /Release edit lock/ });
  await expectNoAxeViolations();
  await user.click(within(confirmation).getByRole("button", { name: /Release lock/ }));
  await waitFor(() => expect(locked).toBe(false));
});
