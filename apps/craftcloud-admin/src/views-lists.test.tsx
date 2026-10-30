import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { ORG_A, ORG_B, WS_1, mockFetch, renderApp } from "./test/helpers.tsx";
import { BOB, CARA, ME, ORG_MEMBERS, PROJECT_1, member, page, viewRoutes } from "./test/fixtures.ts";

type User = ReturnType<typeof userEvent.setup>;

async function openSection(user: User, name: string) {
  await user.click(await screen.findByRole("button", { name }));
  return screen.findByRole("heading", { level: 1, name });
}

describe("lists across pages", () => {
  it("opens the files of a project that lies beyond the first page of the selector", async () => {
    const third = { ...PROJECT_1, id: "00000000-0000-4000-8000-0000000000c3", name: "Third project" };
    const { fetch, calls } = mockFetch(
      viewRoutes({
        [`GET /workspaces/${WS_1.id}/projects`]: (call) =>
          call.query.get("cursor") === "c2" ? page([third]) : page([PROJECT_1], "c2"),
        [`GET /projects/${third.id}`]: { body: third },
        [`GET /projects/${third.id}/files`]: page([]),
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Projects");
    await user.click(await screen.findByRole("button", { name: "Load more" }));
    await user.click(await screen.findByRole("button", { name: "Open files of Third project" }));
    expect(await screen.findByText("This project has no files yet.")).toBeTruthy();
    expect(calls.some((c) => c.path === `/projects/${third.id}` && c.method === "GET")).toBe(true);
    expect([...document.querySelectorAll(".select__value")].at(-1)?.textContent).toBe("Third project");
  });

  it("offers workspace candidates from every page of the organization list", async () => {
    const { fetch } = mockFetch(
      viewRoutes({
        [`GET /organizations/${ORG_A.id}/members`]: (call) =>
          call.query.get("cursor") === null
            ? page([ORG_MEMBERS[0], ORG_MEMBERS[1]], "p2")
            : page([ORG_MEMBERS[2], ORG_MEMBERS[3]]),
        // Bob is in the workspace but only appears on the second page of that list.
        [`GET /workspaces/${WS_1.id}/members`]: (call) =>
          call.query.get("cursor") === null
            ? page([member(ME, "Ada", "admin")], "w2")
            : page([member(BOB, "Bob", "editor")]),
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Members");
    await user.click(await screen.findByRole("button", { name: "Add to workspace" }));
    const dialog = await screen.findByRole("dialog");
    const person = await within(dialog).findByRole("combobox", { name: "Person" });
    const names = within(person)
      .getAllByRole("option")
      .map((o) => o.textContent);
    expect(names.some((n) => n?.includes("Cara"))).toBe(true); // from page two
    expect(names.some((n) => n?.includes("Owen"))).toBe(true);
    expect(names.some((n) => n?.includes("Bob"))).toBe(false); // already in, listed on page two
    expect(names.some((n) => n?.includes("Ada"))).toBe(false);
    expect(CARA).toBeTruthy();
  });

  it("refreshes the workspaces after the viewer left a workspace", async () => {
    const { fetch, calls } = mockFetch(
      viewRoutes({
        "GET /organizations": { body: { items: [{ ...ORG_A, role: "member" }], next_cursor: null } },
        [`GET /organizations/${ORG_A.id}/workspaces`]: { body: { items: [{ ...WS_1, workspace_role: "editor" }], next_cursor: null } },
        [`GET /workspaces/${WS_1.id}/members`]: page([member(ME, "Ada", "editor"), member(BOB, "Bob", "admin")]),
        [`DELETE /workspaces/${WS_1.id}/members/${ME}`]: { status: 204 },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Members");
    const before = calls.filter((c) => c.path === `/organizations/${ORG_A.id}/workspaces`).length;
    await user.click(await screen.findByRole("button", { name: "Leave workspace" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Leave" }));
    await waitFor(() =>
      expect(calls.filter((c) => c.path === `/organizations/${ORG_A.id}/workspaces`).length).toBeGreaterThan(before),
    );
  });

  it("refreshes organizations and workspaces after the own organization role changed", async () => {
    const { fetch, calls } = mockFetch(
      viewRoutes({
        [`PATCH /organizations/${ORG_A.id}/members/${ME}`]: { body: member(ME, "Ada", "admin") },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Members");
    const count = (path: string) => calls.filter((c) => c.path === path && c.method === "GET").length;
    const orgsBefore = count("/organizations");
    const wsBefore = count(`/organizations/${ORG_A.id}/workspaces`);
    await user.click(await screen.findByRole("button", { name: "Change role: Ada" }));
    const dialog = await screen.findByRole("dialog");
    await user.selectOptions(within(dialog).getByRole("combobox", { name: "Role" }), "admin");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(count("/organizations")).toBeGreaterThan(orgsBefore));
    await waitFor(() => expect(count(`/organizations/${ORG_A.id}/workspaces`)).toBeGreaterThan(wsBefore));
  });

  it("drops the one-time invitation link when another organization is selected", async () => {
    const { fetch } = mockFetch(
      viewRoutes({
        [`POST /organizations/${ORG_A.id}/invitations`]: {
          status: 201,
          body: { invitation_id: "i", invite_url: "https://cloud.test/invite#token=SECRET", expires_at: "2026-11-01T10:00:00Z" },
        },
        [`GET /organizations/${ORG_B.id}/invitations`]: page([]),
        "GET /organizations": { body: { items: [ORG_A, { ...ORG_B, role: "admin" }], next_cursor: null } },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Invitations");
    await user.type(await screen.findByLabelText("E-mail address"), "x@example.test");
    await user.click(screen.getByRole("button", { name: "Create invitation" }));
    await screen.findByLabelText("Link");

    const orgButton = screen.getByRole("button", { name: `${ORG_A.name} Organization` });
    orgButton.focus();
    await user.keyboard("{Enter}");
    await user.click(await screen.findByRole("option", { name: ORG_B.name }));
    await waitFor(() => expect(screen.queryByLabelText("Link")).toBeNull());
    expect(screen.queryByDisplayValue(/SECRET/)).toBeNull();
  });
});
