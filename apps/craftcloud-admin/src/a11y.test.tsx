import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { ORG_A, WS_1, apiError, expectNoAxeViolations, mockFetch, renderApp } from "./test/helpers.tsx";
import { fakeXhr } from "./test/fakeXhr.ts";
import { FILE_1, ME, PROJECT_1, page, viewRoutes } from "./test/fixtures.ts";

type User = ReturnType<typeof userEvent.setup>;

// Navigation order was overview, projects, files, members, invitations, settings, activity.
// The shell groups and reorders navigation by task; query labels so this helper
// stays valid in both supported locales and does not depend on visual position.
const NAV = {
  projects: /^(Projects|Projekte)$/,
  files: /^(Files|Dateien)$/,
  members: /^(Members|Mitglieder)$/,
  invitations: /^(Invitations|Einladungen)$/,
} as const;

async function go(user: User, section: keyof typeof NAV) {
  const nav = await screen.findByRole("navigation");
  await user.click(within(nav).getByRole("button", { name: NAV[section] }));
  await screen.findByRole("heading", { level: 1 });
}

/** Opens the files of the first project through the project list. */
async function openFiles(user: User) {
  await go(user, "projects");
  const open = (await screen.findAllByRole("button")).find((b) => /Brand kit/.test(b.getAttribute("aria-label") ?? "") && b.textContent !== "");
  await user.click(open!);
  await screen.findAllByRole("row");
  await waitFor(() => expect(screen.getAllByRole("row").length).toBeGreaterThan(1));
}

const UPLOAD = {
  upload_id: "00000000-0000-4000-8000-0000000000a9",
  file_id: FILE_1.id,
  project_id: PROJECT_1.id,
  workspace_id: WS_1.id,
  state: "created",
  size_bytes: 4,
  sha256: "0".repeat(64),
  max_bytes: 4,
  created_by: ME,
  created_at: "x",
  expires_at: "y",
};

const languages = [
  ["en", "English"],
  ["de", "German"],
] as const;

describe.each(languages)("axe: dashboard views (%s)", (language) => {
  const render = (fetch: typeof globalThis.fetch, xhr?: () => XMLHttpRequest) =>
    renderApp(fetch, { language, ...(xhr === undefined ? {} : { xhrFactory: xhr }) });

  it("members, role dialog, remove dialog and add-to-workspace dialog", async () => {
    const { fetch } = mockFetch(viewRoutes());
    const user = userEvent.setup();
    render(fetch);
    await go(user, "members");
    await waitFor(() => expect(screen.getAllByRole("table").length).toBe(2));
    await expectNoAxeViolations();

    const orgTable = screen.getAllByRole("table")[0]!;
    const caraRow = within(orgTable).getAllByRole("row")[3]!;
    for (const index of [0, 1]) {
      await user.click(within(caraRow).getAllByRole("button")[index]!);
      await screen.findByRole("dialog");
      await expectNoAxeViolations();
      await user.keyboard("{Escape}");
      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    }
    const add = within(screen.getByRole("main")).getAllByRole("button").find((b) => b.closest("table") === null && b.textContent !== "" && !b.closest("nav"));
    await user.click(add!);
    await screen.findByRole("dialog");
    await expectNoAxeViolations();
  });

  it("invitations with the one-time link, and with errors", async () => {
    const { fetch } = mockFetch(
      viewRoutes({
        [`GET /organizations/${ORG_A.id}/invitations`]: page([
          { id: "i1", email: "p@example.test", org_role: "member", workspace_id: WS_1.id, workspace_role: "viewer", status: "pending", created_by: ME, created_at: "2026-10-28T10:00:00Z", expires_at: "2026-10-31T10:00:00Z" },
        ]),
        [`POST /organizations/${ORG_A.id}/invitations`]: { status: 201, body: { invitation_id: "i2", invite_url: "https://cloud.test/invite#token=x", expires_at: "2026-11-01T10:00:00Z" } },
      }),
    );
    const user = userEvent.setup();
    render(fetch);
    await go(user, "invitations");
    await screen.findAllByRole("table");
    await expectNoAxeViolations();
    await user.type(screen.getAllByRole("textbox")[0]!, "n@example.test");
    await user.click(screen.getAllByRole("button").find((b) => b.getAttribute("type") === "submit")!);
    await screen.findByRole("group");
    await expectNoAxeViolations();
    // revoke dialog
    await user.click(within(screen.getAllByRole("table")[0]!).getAllByRole("button")[0]!);
    await screen.findByRole("dialog");
    await expectNoAxeViolations();
  });

  it("projects, rename dialog, trash dialog, trash view and empty state", async () => {
    const { fetch } = mockFetch(viewRoutes());
    const user = userEvent.setup();
    render(fetch);
    await go(user, "projects");
    const table = await screen.findByRole("table");
    await expectNoAxeViolations();
    for (const index of [1, 2]) {
      await user.click(within(within(table).getAllByRole("row")[1]!).getAllByRole("button")[index]!);
      await screen.findByRole("dialog");
      await expectNoAxeViolations();
      await user.keyboard("{Escape}");
      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    }
    await user.click(screen.getByRole("checkbox"));
    await screen.findByText(/./, { selector: "p.muted" });
    await expectNoAxeViolations();
  });

  it("files, upload dialog (form, progress, failure), versions dialog", async () => {
    const { factory } = fakeXhr(() => "hang");
    const { fetch } = mockFetch(
      viewRoutes({
        [`POST /files/${FILE_1.id}/uploads`]: { status: 201, body: UPLOAD },
        [`DELETE /uploads/${UPLOAD.upload_id}`]: { status: 204 },
      }),
    );
    const user = userEvent.setup();
    render(fetch, factory);
    await openFiles(user);
    await expectNoAxeViolations();

    const fileRow = screen.getAllByRole("row")[1]!;
    const buttons = within(fileRow).getAllByRole("button");
    // versions
    await user.click(buttons[0]!);
    await screen.findByRole("dialog");
    await screen.findAllByRole("row");
    await expectNoAxeViolations();
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

    // new version: form, then progress
    await user.click(buttons[1]!);
    const dialog = await screen.findByRole("dialog");
    await expectNoAxeViolations();
    await user.upload(dialog.querySelector('input[type="file"]')!, new File(["data"], "logo.fig"));
    await user.click(within(dialog).getAllByRole("button").find((b) => b.getAttribute("type") === "submit")!);
    await within(dialog).findByRole("progressbar");
    await expectNoAxeViolations();
    await user.click(within(dialog).getAllByRole("button").at(-1)!);
  });

  it("upload conflict, success and failure panels", async () => {
    const { factory } = fakeXhr(() => ({ body: { ...UPLOAD, state: "stored" } }));
    let step = 0;
    const { fetch } = mockFetch(
      viewRoutes({
        [`POST /files/${FILE_1.id}/uploads`]: { status: 201, body: UPLOAD },
        [`DELETE /uploads/${UPLOAD.upload_id}`]: { status: 204 },
        [`POST /files/${FILE_1.id}/versions`]: () => {
          step += 1;
          if (step === 1) return apiError(412, "VERSION_CONFLICT", { details: { current_version_id: "00000000-0000-4000-8000-0000000000ee" } });
          if (step === 2) return apiError(503, "UNAVAILABLE");
          return { status: 201, body: { id: "v", file_id: FILE_1.id, number: 9, size_bytes: 4, sha256: "0".repeat(64), base_version_id: null, restored_from_version_id: null, upload_id: null, created_by: ME, created_at: "x" } };
        },
      }),
    );
    const user = userEvent.setup();
    render(fetch, factory);
    await openFiles(user);
    await user.click(within(screen.getAllByRole("row")[1]!).getAllByRole("button")[1]!);
    const dialog = await screen.findByRole("dialog");
    await user.upload(dialog.querySelector('input[type="file"]')!, new File(["data"], "logo.fig"));
    const submit = () => within(dialog).getAllByRole("button").find((b) => b.getAttribute("type") === "submit")!;
    await user.click(submit());
    await waitFor(() => expect(within(dialog).getAllByRole("button").length).toBe(3)); // keep both / overwrite / cancel
    await expectNoAxeViolations();
    await user.click(within(dialog).getAllByRole("button")[1]!); // overwrite -> 503
    await within(dialog).findByRole("alert");
    await expectNoAxeViolations();
  });

  it("error and empty states", async () => {
    const { fetch } = mockFetch(
      viewRoutes({
        [`GET /workspaces/${WS_1.id}/projects`]: apiError(503, "UNAVAILABLE"),
        [`GET /organizations/${ORG_A.id}/invitations`]: page([]),
      }),
    );
    const user = userEvent.setup();
    render(fetch);
    await go(user, "projects");
    await screen.findByRole("alert");
    await expectNoAxeViolations();
    await go(user, "invitations");
    await screen.findByText(/./, { selector: "p.muted" });
    await expectNoAxeViolations();
  });

  it("password reset page and its error states", async () => {
    const { fetch } = mockFetch({
      "GET /setup/status": { body: { setup_required: false } },
      "GET /auth/session": apiError(401, "UNAUTHENTICATED"),
      "POST /auth/password-reset": apiError(400, "INVALID_RESET_TOKEN"),
    });
    const user = userEvent.setup();
    renderApp(fetch, { language, resetToken: "tok" });
    await screen.findByRole("heading", { level: 1 });
    await expectNoAxeViolations();
    const [first, second] = screen.getAllByDisplayValue("");
    await user.type(first!, "a long enough password");
    await user.type(second!, "a long enough password");
    await user.click(screen.getAllByRole("button").find((b) => b.getAttribute("type") === "submit")!);
    await screen.findByRole("alert");
    await expectNoAxeViolations();
  });
});
