import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { ORG_A, WS_1, apiError, mockFetch, renderApp } from "./test/helpers.tsx";
import {
  BOB,
  CARA,
  ME,
  ORG_MEMBERS,
  OWEN,
  PROJECT_1,
  PROJECT_2,
  member,
  page,
  viewRoutes,
} from "./test/fixtures.ts";

type User = ReturnType<typeof userEvent.setup>;

async function openSection(user: User, name: string) {
  await user.click(await screen.findByRole("button", { name }));
  return screen.findByRole("heading", { level: 1, name });
}

const asOrgRole = (role: string) => ({
  "GET /organizations": { body: { items: [{ ...ORG_A, role }], next_cursor: null } },
});
const asWorkspaceRole = (role: string | null) => ({
  [`GET /organizations/${ORG_A.id}/workspaces`]: {
    body: { items: [{ ...WS_1, workspace_role: role }], next_cursor: null },
  },
});

function row(name: string | RegExp, table = "Organization members") {
  return within(screen.getByRole("table", { name: table })).getByRole("row", { name });
}

describe("members", () => {
  it("lists members and changes a role with the keyboard only, then returns focus", async () => {
    let caraRole = "member"; // the fake server keeps state, so the background re-read agrees
    const { fetch, calls } = mockFetch(
      viewRoutes({
        [`GET /organizations/${ORG_A.id}/members`]: () =>
          page(ORG_MEMBERS.map((m) => (m.user_id === CARA ? { ...m, role: caraRole } : m))),
        [`PATCH /organizations/${ORG_A.id}/members/${CARA}`]: (call) => {
          caraRole = (call.body as { role: string }).role;
          return { body: { ...member(CARA, "Cara", caraRole) } };
        },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Members");
    expect(await screen.findAllByText("bob@example.test")).toBeTruthy();

    const change = within(row(/Cara/)).getByRole("button", { name: "Change role: Cara" });
    change.focus();
    await user.keyboard("{Enter}");
    const dialog = await screen.findByRole("dialog", { name: "Change role" });
    const select = within(dialog).getByRole("combobox", { name: "Role" });
    await user.selectOptions(select, "admin");
    await user.tab();
    expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Cancel" }));
    await user.tab();
    expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Save" }));
    await user.keyboard("{Enter}");

    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(calls.find((c) => c.method === "PATCH")?.body).toEqual({ role: "admin" });
    expect(calls.find((c) => c.method === "PATCH")?.headers["x-csrf-token"]).toBe("csrf-from-session");
    expect(within(row(/Cara/)).getByText("Admin")).toBeTruthy();
    await waitFor(() => expect(document.activeElement).toBe(change));
    await waitFor(() => expect(screen.getByTestId("announcer").textContent).toBe("Cara is now Admin."));
  });

  it("offers an admin no control on owners and no owner role", async () => {
    const { fetch } = mockFetch(viewRoutes(asOrgRole("admin")));
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Members");
    await screen.findAllByText("bob@example.test");
    expect(within(row(/Ada/)).getByRole("button", { name: "Reset account: issue a password reset link for Ada" })).toBeTruthy();
    expect(within(row(/Ada/)).queryByRole("button", { name: "Change role: Ada" })).toBeNull(); // owner
    expect(within(row(/Owen/)).getByRole("button", { name: "Reset account: issue a password reset link for Owen" })).toBeTruthy();
    expect(within(row(/Owen/)).queryByRole("button", { name: "Change role: Owen" })).toBeNull(); // owner
    const bob = within(row(/Bob/));
    expect(bob.getByRole("button", { name: "Change role: Bob" })).toBeTruthy();
    expect(bob.getByRole("button", { name: "Remove Bob" })).toBeTruthy();
    await user.click(bob.getByRole("button", { name: "Change role: Bob" }));
    const options = within(await screen.findByRole("combobox", { name: "Role" })).getAllByRole("option");
    expect(options.map((o) => o.textContent)).toEqual(["Admin", "Member", "Guest"]);
  });

  it("lets a plain member only leave and shows no workspace management", async () => {
    const { fetch } = mockFetch(
      viewRoutes({
        ...asOrgRole("member"),
        ...asWorkspaceRole("editor"),
        [`GET /organizations/${ORG_A.id}/members`]: page([member(ME, "Ada", "member"), member(BOB, "Bob", "admin")]),
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Members");
    await screen.findAllByText("bob@example.test");
    expect(within(row(/Bob/)).queryByRole("button", { name: "Change role: Bob" })).toBeNull();
    expect(within(row(/Bob/)).queryByRole("button", { name: "Remove Bob" })).toBeNull();
    expect(within(row(/Ada/)).getByRole("button", { name: "Leave organization" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Add to workspace" })).toBeNull();
    expect(screen.queryByRole("button", { name: /Change role in workspace/ })).toBeNull();
  });

  it("shows the server's 403 inside the dialog and reloads on request", async () => {
    const { fetch, calls } = mockFetch(
      viewRoutes({ [`PATCH /organizations/${ORG_A.id}/members/${CARA}`]: apiError(403, "FORBIDDEN") }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Members");
    await user.click(await screen.findByRole("button", { name: "Change role: Cara" }));
    const dialog = await screen.findByRole("dialog");
    await user.selectOptions(within(dialog).getByRole("combobox", { name: "Role" }), "admin");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    expect((await within(dialog).findByRole("alert")).textContent).toContain("You do not have permission");
    const before = calls.filter((c) => c.path === `/organizations/${ORG_A.id}/members`).length;
    await user.click(within(dialog).getByRole("button", { name: "Reload" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    await waitFor(() =>
      expect(calls.filter((c) => c.path === `/organizations/${ORG_A.id}/members`).length).toBe(before + 1),
    );
  });

  it("removes a member after confirmation and explains LAST_OWNER", async () => {
    let attempt = 0;
    let removed = false;
    const { fetch, calls } = mockFetch(
      viewRoutes({
        [`GET /organizations/${ORG_A.id}/members`]: () =>
          page(ORG_MEMBERS.filter((m) => !(removed && m.user_id === OWEN))),
        [`DELETE /organizations/${ORG_A.id}/members/${OWEN}`]: () => {
          attempt += 1;
          if (attempt === 1) return apiError(409, "LAST_OWNER");
          removed = true;
          return { status: 204 };
        },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Members");
    await user.click(await screen.findByRole("button", { name: "Remove Owen" }));
    let dialog = await screen.findByRole("dialog", { name: "Remove member" });
    await user.click(within(dialog).getByRole("button", { name: "Remove" }));
    expect((await within(dialog).findByRole("alert")).textContent).toContain("at least one active owner");
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

    await user.click(screen.getByRole("button", { name: "Remove Owen" }));
    dialog = await screen.findByRole("dialog", { name: "Remove member" });
    await user.click(within(dialog).getByRole("button", { name: "Remove" }));
    await waitFor(() => expect(screen.queryByRole("row", { name: /Owen/ })).toBeNull());
    expect(calls.filter((c) => c.method === "DELETE").length).toBe(2);
    // The row that had the focus is gone: focus moves to the main region, not the body.
    await waitFor(() => expect(document.activeElement?.id).toBe("main"));
  });

  it("adds an organization member to the workspace with a grantable role", async () => {
    const { fetch, calls } = mockFetch(
      viewRoutes({
        [`POST /workspaces/${WS_1.id}/members`]: (call) => ({
          status: 201,
          body: { ...member(CARA, "Cara", (call.body as { role: string }).role) },
        }),
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Members");
    await user.click(await screen.findByRole("button", { name: "Add to workspace" }));
    const dialog = await screen.findByRole("dialog", { name: "Add to workspace" });
    const person = within(dialog).getByRole("combobox", { name: "Person" });
    // Ada and Bob are already in the workspace.
    expect(within(person).queryByRole("option", { name: /Bob/ })).toBeNull();
    await user.selectOptions(person, CARA);
    await user.selectOptions(within(dialog).getByRole("combobox", { name: "Role" }), "editor");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(calls.find((c) => c.method === "POST")?.body).toEqual({ user_id: CARA, role: "editor" });
  });

  it("gives a workspace admin only editor and viewer", async () => {
    const { fetch } = mockFetch(viewRoutes({ ...asOrgRole("member"), ...asWorkspaceRole("admin") }));
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Members");
    await user.click(await screen.findByRole("button", { name: "Add to workspace" }));
    const dialog = await screen.findByRole("dialog");
    const roles = within(within(dialog).getByRole("combobox", { name: "Role" })).getAllByRole("option");
    expect(roles.map((o) => o.textContent)).toEqual(["Editor", "Viewer"]);
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    // The workspace admin Ada has no change/remove controls on herself except leaving.
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    const ada = within(row(/Ada/, "Workspace members"));
    expect(ada.queryByRole("button", { name: /Change role in workspace/ })).toBeNull();
    expect(ada.getByRole("button", { name: "Leave workspace" })).toBeTruthy();
    expect(within(row(/Bob/, "Workspace members")).getByRole("button", { name: /Change role in workspace: Bob/ })).toBeTruthy();
  });

  it("pages through members", async () => {
    const { fetch, calls } = mockFetch(
      viewRoutes({
        [`GET /organizations/${ORG_A.id}/members`]: (call) =>
          call.query.get("cursor") === null
            ? page([ORG_MEMBERS[0]], "next")
            : page([ORG_MEMBERS[1]]),
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Members");
    await user.click(await screen.findByRole("button", { name: "Load more" }));
    expect(await screen.findAllByText("bob@example.test")).toBeTruthy();
    expect(calls.filter((c) => c.path === `/organizations/${ORG_A.id}/members`)[1]?.query.get("cursor")).toBe("next");
  });
});

describe("invitations", () => {
  const CREATED = {
    invitation_id: "00000000-0000-4000-8000-0000000000f1",
    invite_url: "https://cloud.test/invite#token=SECRET-ONE-TIME",
    expires_at: "2026-11-01T10:00:00Z",
  };

  it("creates an invitation, shows the one-time link with a copy button and never stores it", async () => {
    const { fetch, calls } = mockFetch(
      viewRoutes({ [`POST /organizations/${ORG_A.id}/invitations`]: { status: 201, body: CREATED } }),
    );
    const user = userEvent.setup(); // installs the clipboard stub
    renderApp(fetch);
    await openSection(user, "Invitations");
    await user.type(await screen.findByLabelText("E-mail address"), "new@example.test");
    await user.selectOptions(screen.getByRole("combobox", { name: "Workspace" }), WS_1.id);
    await user.selectOptions(screen.getByRole("combobox", { name: "Workspace role" }), "editor");
    await user.click(screen.getByRole("button", { name: "Create invitation" }));

    const link = await screen.findByLabelText("Link");
    expect((link as HTMLInputElement).value).toBe(CREATED.invite_url);
    expect(calls.find((c) => c.method === "POST")?.body).toEqual({
      email: "new@example.test",
      org_role: "member",
      workspace_id: WS_1.id,
      workspace_role: "editor",
    });
    await user.click(screen.getByRole("button", { name: "Copy link" }));
    expect(await navigator.clipboard.readText()).toBe(CREATED.invite_url);
    await waitFor(() => expect(screen.getByTestId("announcer").textContent).toBe("Link copied."));
    expect(JSON.stringify({ ...window.localStorage, ...window.sessionStorage })).not.toContain("SECRET");
    expect(window.location.href).not.toContain("SECRET");

    await user.click(screen.getByRole("button", { name: "Hide link" }));
    expect(screen.queryByLabelText("Link")).toBeNull();
    // The list never returns tokens or links; leaving and re-entering the view shows none.
    await openSection(user, "Overview");
    await openSection(user, "Invitations");
    expect(screen.queryByDisplayValue(/SECRET/)).toBeNull();
  });

  it("falls back to manual copying when the clipboard is refused", async () => {
    const { fetch } = mockFetch(
      viewRoutes({ [`POST /organizations/${ORG_A.id}/invitations`]: { status: 201, body: CREATED } }),
    );
    const user = userEvent.setup();
    vi.spyOn(navigator.clipboard, "writeText").mockRejectedValue(new Error("denied"));
    renderApp(fetch);
    await openSection(user, "Invitations");
    await user.type(await screen.findByLabelText("E-mail address"), "new@example.test");
    await user.click(screen.getByRole("button", { name: "Create invitation" }));
    await user.click(await screen.findByRole("button", { name: "Copy link" }));
    expect((await screen.findByRole("alert")).textContent).toContain("copy it by hand");
  });

  it("limits a workspace admin to a member invitation into the own workspace, editor at most", async () => {
    const { fetch, calls } = mockFetch(
      viewRoutes({
        "GET /organizations": {
          body: { items: [{ ...ORG_A, role: "member", workspace_admin_invites_enabled: true }], next_cursor: null },
        },
        ...asWorkspaceRole("admin"),
        [`POST /organizations/${ORG_A.id}/invitations`]: { status: 201, body: CREATED },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Invitations");
    await user.type(await screen.findByLabelText("E-mail address"), "new@example.test");
    expect(screen.getByRole("combobox", { name: "Organization role" })).toBeTruthy();
    expect(within(screen.getByRole("combobox", { name: "Organization role" })).getAllByRole("option").map((option) => option.textContent)).toEqual(["Member", "Guest"]);
    const roles = within(screen.getByRole("combobox", { name: "Workspace role" })).getAllByRole("option");
    expect(roles.map((o) => o.textContent)).toEqual(["Editor", "Viewer"]);
    await user.click(screen.getByRole("button", { name: "Create invitation" }));
    await screen.findByLabelText("Link");
    expect(calls.find((c) => c.method === "POST")?.body).toEqual({
      email: "new@example.test",
      org_role: "member",
      workspace_id: WS_1.id,
      workspace_role: "viewer", // least privilege is the default; editor is the most they may pick
    });
    // Workspace admins cannot list invitations; the UI does not even ask.
    expect(calls.some((c) => c.method === "GET" && c.path.endsWith("/invitations"))).toBe(false);
  });

  it("lets an eligible workspace admin issue a guest invitation scoped to their workspace", async () => {
    const { fetch, calls } = mockFetch(viewRoutes({
      "GET /organizations": {
        body: { items: [{ ...ORG_A, role: "member", workspace_admin_invites_enabled: true }], next_cursor: null },
      },
      ...asWorkspaceRole("admin"),
      [`POST /organizations/${ORG_A.id}/invitations`]: { status: 201, body: CREATED },
    }));
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Invitations");
    await user.type(await screen.findByLabelText("E-mail address"), "guest@example.test");
    await user.selectOptions(screen.getByRole("combobox", { name: "Organization role" }), "guest");
    await user.click(screen.getByRole("button", { name: "Create invitation" }));
    await screen.findByLabelText("Link");
    expect(calls.find((call) => call.method === "POST")?.body).toEqual({
      email: "guest@example.test",
      org_role: "guest",
      workspace_id: WS_1.id,
    });
  });

  it("hides the form when the organization setting forbids workspace admin invitations", async () => {
    const { fetch } = mockFetch(
      viewRoutes({ "GET /organizations": { body: { items: [{ ...ORG_A, role: "member" }], next_cursor: null } } }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Invitations");
    expect(await screen.findByText(/Only organization owners and admins/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Create invitation" })).toBeNull();
  });

  it("explains a duplicate invitation and a missing public URL", async () => {
    let status = 409;
    const { fetch } = mockFetch(
      viewRoutes({
        [`POST /organizations/${ORG_A.id}/invitations`]: () =>
          status === 409 ? apiError(409, "INVITATION_PENDING") : apiError(503, "AUTH_UNAVAILABLE"),
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Invitations");
    await user.type(await screen.findByLabelText("E-mail address"), "new@example.test");
    await user.click(screen.getByRole("button", { name: "Create invitation" }));
    expect((await screen.findByRole("alert")).textContent).toContain("already has an open invitation");
    status = 503;
    await user.click(screen.getByRole("button", { name: "Create invitation" }));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("temporarily unavailable"));
  });

  it("validates the address locally", async () => {
    const { fetch, calls } = mockFetch(viewRoutes());
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Invitations");
    await user.type(await screen.findByLabelText("E-mail address"), "not-an-address");
    await user.click(screen.getByRole("button", { name: "Create invitation" }));
    expect(await screen.findByText("Enter a valid e-mail address.")).toBeTruthy();
    expect(calls.some((c) => c.method === "POST")).toBe(false);
  });

  it("lists invitations with names instead of ids and revokes a pending one", async () => {
    const invitation = (id: string, status: string) => ({
      id,
      email: `${status}@example.test`,
      org_role: "member",
      workspace_id: WS_1.id,
      workspace_role: "editor",
      status,
      created_by: ME,
      created_at: "2026-10-28T10:00:00Z",
      expires_at: "2026-10-31T10:00:00Z",
    });
    const { fetch, calls } = mockFetch(
      viewRoutes({
        [`GET /organizations/${ORG_A.id}/invitations`]: page([invitation("i1", "pending"), invitation("i2", "revoked")]),
        [`DELETE /organizations/${ORG_A.id}/invitations/i1`]: { status: 204 },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Invitations");
    const pending = await screen.findByRole("row", { name: /pending@example.test/ });
    expect(within(pending).getByText(/Main workspace · Editor/)).toBeTruthy();
    expect(within(screen.getByRole("row", { name: /revoked@example.test/})).queryByRole("button")).toBeNull();
    await user.click(within(pending).getByRole("button", { name: "Revoke invitation for pending@example.test" }));
    const dialog = await screen.findByRole("dialog", { name: "Revoke invitation" });
    await user.click(within(dialog).getByRole("button", { name: "Revoke" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(calls.some((c) => c.method === "DELETE" && c.path.endsWith("/invitations/i1"))).toBe(true);
  });
});

describe("projects", () => {
  it("lists, creates, renames with If-Match, trashes and restores", async () => {
    // A small fake server: the lists always reflect what the earlier calls did.
    type Row = { id: string } & Record<string, unknown>;
    const state = { live: [PROJECT_1, PROJECT_2] as Row[], trash: [] as Row[] };
    const { fetch, calls } = mockFetch(
      viewRoutes({
        [`GET /workspaces/${WS_1.id}/projects`]: (call) =>
          page(call.query.get("deleted") === "true" ? state.trash : state.live),
        [`POST /workspaces/${WS_1.id}/projects`]: () => {
          const flyer = { ...PROJECT_2, id: "p3", name: "Flyer" };
          state.live = [...state.live, flyer];
          return { status: 201, body: flyer };
        },
        [`PATCH /projects/${PROJECT_1.id}`]: (call) => {
          const renamed = { ...PROJECT_1, name: (call.body as { name: string }).name, version: 4 };
          state.live = state.live.map((p) => (p.id === renamed.id ? renamed : p));
          return { body: renamed };
        },
        [`DELETE /projects/${PROJECT_2.id}`]: () => {
          state.live = state.live.filter((p) => p.id !== PROJECT_2.id);
          state.trash = [{ ...PROJECT_2, deleted_at: "2026-10-28T10:00:00Z" }];
          return { status: 204 };
        },
        [`POST /projects/${PROJECT_2.id}/restore`]: () => {
          state.trash = [];
          state.live = [...state.live, PROJECT_2];
          return { body: PROJECT_2 };
        },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Projects");
    expect(await screen.findByRole("row", { name: /Brand kit/ })).toBeTruthy();

    await user.type(screen.getByLabelText("New project name"), "Flyer");
    await user.click(screen.getByRole("button", { name: "Create project" }));
    expect(await screen.findByRole("row", { name: /Flyer/ })).toBeTruthy();
    expect(calls.find((c) => c.method === "POST")?.body).toEqual({ name: "Flyer" });

    await user.click(screen.getByRole("button", { name: "Rename Brand kit" }));
    const dialog = await screen.findByRole("dialog", { name: "Rename project" });
    const input = within(dialog).getByLabelText("Project name");
    await user.clear(input);
    await user.type(input, "Brand kit 2");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    const patch = calls.find((c) => c.method === "PATCH");
    expect(patch?.headers["if-match"]).toBe('"prj-v3"');
    expect(patch?.body).toEqual({ name: "Brand kit 2" });
    expect(await screen.findByRole("row", { name: /Brand kit 2/ })).toBeTruthy();

    await user.click(screen.getByRole("button", { name: "Move to trash: Poster" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Move to trash" }));
    await waitFor(() => expect(screen.queryByRole("row", { name: /Poster/ })).toBeNull());

    await user.click(screen.getByRole("checkbox", { name: "Show trash" }));
    await user.click(await screen.findByRole("button", { name: "Restore Poster" }));
    await waitFor(() => expect(screen.queryByRole("row", { name: /Poster/ })).toBeNull());
    expect(calls.some((c) => c.path === `/projects/${PROJECT_2.id}/restore`)).toBe(true);
    await waitFor(() => expect(screen.getByTestId("announcer").textContent).toBe("Project Poster restored."));
  });

  it("marks a taken name on the field (409) and validates locally", async () => {
    const { fetch } = mockFetch(
      viewRoutes({ [`POST /workspaces/${WS_1.id}/projects`]: apiError(409, "PROJECT_NAME_TAKEN") }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Projects");
    await user.click(await screen.findByRole("button", { name: "Create project" }));
    expect(await screen.findByText("Enter a name of 1 to 100 characters.")).toBeTruthy();
    await user.type(screen.getByLabelText("New project name"), "Brand kit");
    await user.click(screen.getByRole("button", { name: "Create project" }));
    expect(await screen.findByText("A project with this name already exists.")).toBeTruthy();
    await waitFor(() => expect(document.activeElement).toBe(screen.getByLabelText("New project name")));
  });

  it("shows only what a viewer may do", async () => {
    const { fetch } = mockFetch(viewRoutes({ ...asOrgRole("member"), ...asWorkspaceRole("viewer") }));
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Projects");
    const projectRow = await screen.findByRole("row", { name: /Brand kit/ });
    expect(within(projectRow).getByRole("button", { name: /Open files/ })).toBeTruthy();
    expect(within(projectRow).queryByRole("button", { name: /Rename/ })).toBeNull();
    expect(within(projectRow).queryByRole("button", { name: /trash/ })).toBeNull();
    expect(screen.queryByRole("button", { name: "Create project" })).toBeNull();
    expect(screen.getByText(/read projects here but not change them/)).toBeTruthy();
  });

  it("lets an editor create and rename but not trash", async () => {
    const { fetch } = mockFetch(viewRoutes({ ...asOrgRole("member"), ...asWorkspaceRole("editor") }));
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Projects");
    const projectRow = await screen.findByRole("row", { name: /Brand kit/ });
    expect(within(projectRow).getByRole("button", { name: /Rename/ })).toBeTruthy();
    expect(within(projectRow).queryByRole("button", { name: /trash/ })).toBeNull();
    expect(screen.getByRole("button", { name: "Create project" })).toBeTruthy();
  });

  it("explains an expired restore window (410)", async () => {
    const { fetch } = mockFetch(
      viewRoutes({
        [`GET /workspaces/${WS_1.id}/projects`]: (call) =>
          call.query.get("deleted") === "true"
            ? page([{ ...PROJECT_2, deleted_at: "2026-09-01T10:00:00Z" }])
            : page([PROJECT_1]),
        [`POST /projects/${PROJECT_2.id}/restore`]: apiError(410, "RETENTION_EXPIRED"),
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Projects");
    await user.click(await screen.findByRole("checkbox", { name: "Show trash" }));
    await user.click(await screen.findByRole("button", { name: "Restore Poster" }));
    expect((await screen.findByRole("alert")).textContent).toContain("retention period has passed");
  });

  it("opens the files of a project from the list", async () => {
    const { fetch } = mockFetch(viewRoutes());
    const user = userEvent.setup();
    renderApp(fetch);
    await openSection(user, "Projects");
    await user.click(await screen.findByRole("button", { name: "Open files of Brand kit" }));
    expect(await screen.findByRole("heading", { level: 1, name: "Files" })).toBeTruthy();
    expect(await screen.findByRole("row", { name: /logo\.fig/ })).toBeTruthy();
  });
});
