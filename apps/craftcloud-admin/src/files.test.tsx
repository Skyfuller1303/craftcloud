import { createHash } from "node:crypto";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { ORG_A, WS_1, apiError, mockFetch, renderApp } from "./test/helpers.tsx";
import type { Reply } from "./test/helpers.tsx";
import { fakeXhr } from "./test/fakeXhr.ts";
import type { UploadHandler } from "./test/fakeXhr.ts";
import { FILE_1, FILE_EMPTY, HEAD, ME, PROJECT_1, version, viewRoutes } from "./test/fixtures.ts";

type User = ReturnType<typeof userEvent.setup>;

const CONTENT = new Uint8Array(5 * 1024 * 1024 + 17).map((_, i) => (i * 31) % 251);
const CONTENT_SHA = createHash("sha256").update(CONTENT).digest("hex");
const NEW_FILE = { ...FILE_EMPTY, id: "00000000-0000-4000-8000-0000000000d9", name: "poster.png", format: "png", app: "generic" };
const UPLOAD = {
  upload_id: "00000000-0000-4000-8000-0000000000a9",
  file_id: NEW_FILE.id,
  project_id: PROJECT_1.id,
  workspace_id: WS_1.id,
  state: "created",
  size_bytes: CONTENT.length,
  sha256: CONTENT_SHA,
  max_bytes: CONTENT.length,
  created_by: ME,
  created_at: "2026-10-29T10:00:00Z",
  expires_at: "2026-10-30T10:00:00Z",
};
const STORED = { ...UPLOAD, state: "stored" };
const NEW_VERSION = version(1, "00000000-0000-4000-8000-0000000000e9", { file_id: NEW_FILE.id });

function picked(name = "poster.png") {
  return new File([CONTENT], name, { type: "image/png", lastModified: 1_700_000_000_000 });
}

async function openFiles(user: User) {
  await user.click(await screen.findByRole("button", { name: "Projects" }));
  await user.click(await screen.findByRole("button", { name: "Open files of Brand kit" }));
  await screen.findByRole("row", { name: /logo\.fig/ });
}

const okPut: UploadHandler = () => ({ body: STORED });

function uploadRoutes(extra: Record<string, ReturnType<typeof viewRoutes>[string]> = {}) {
  return viewRoutes({
    [`POST /projects/${PROJECT_1.id}/files`]: { status: 201, body: NEW_FILE },
    [`POST /files/${NEW_FILE.id}/uploads`]: { status: 201, body: UPLOAD },
    [`POST /files/${NEW_FILE.id}/versions`]: { status: 201, body: NEW_VERSION },
    ...extra,
  });
}

async function chooseAndSubmit(user: User, file = picked()) {
  await user.click(await screen.findByRole("button", { name: "Upload file" }));
  const dialog = await screen.findByRole("dialog", { name: "Upload a file" });
  await user.upload(within(dialog).getByLabelText("File"), file);
  return dialog;
}

describe("file list and downloads", () => {
  it("rechecks an expired lock with the server and re-enables new versions when released", async () => {
    const lock = {
      holder_display_name: "Ada",
      since: new Date().toISOString(),
      expires_at: new Date(Date.now() + 1800).toISOString(),
      mine: true,
      held_by_current_session: false,
    };
    const lockedFile = { ...FILE_1, app: "photocraft", lock };
    const { fetch, calls } = mockFetch(viewRoutes({
      [`GET /projects/${PROJECT_1.id}/files`]: { body: { items: [{ ...lockedFile, lock: { ...lock, expires_at: new Date(Date.now() + 1800).toISOString() } }], next_cursor: null } },
      [`GET /files/${lockedFile.id}`]: { body: { ...lockedFile, lock: null } },
    }));
    const user = userEvent.setup();
    renderApp(fetch);
    await openFiles(user);
    const row = screen.getByRole("row", { name: /logo\.fig/ });
    expect(within(row).getByRole("button", { name: /Upload a new version of logo\.fig/ }).hasAttribute("disabled")).toBe(true);
    await waitFor(() => expect(within(screen.getByRole("row", { name: /logo\.fig/ })).getByRole("button", { name: /Upload a new version of logo\.fig/ }).hasAttribute("disabled")).toBe(false), { timeout: 8000 });
    expect(calls.some((call) => call.path === `/files/${lockedFile.id}` && call.method === "GET")).toBe(true);
  }, 20_000);

  it("rechecks expired locks on loaded later pages by file id", async () => {
    const lock = {
      holder_display_name: "Ada",
      since: new Date().toISOString(),
      expires_at: new Date(Date.now() + 1800).toISOString(),
      mine: false,
      held_by_current_session: false,
    };
    const laterFile = {
      ...FILE_1,
      id: "00000000-0000-4000-8000-0000000000d7",
      name: "later.png",
      app: "photocraft",
      lock,
    };
    const { fetch, calls } = mockFetch(viewRoutes({
      [`GET /projects/${PROJECT_1.id}/files`]: (call) => call.query.has("cursor")
        ? { body: { items: [{ ...laterFile, lock: { ...lock, expires_at: new Date(Date.now() + 1800).toISOString() } }], next_cursor: null } }
        : { body: { items: [FILE_1], next_cursor: "page-2" } },
      [`GET /files/${laterFile.id}`]: { body: { ...laterFile, lock: null } },
    }));
    const user = userEvent.setup();
    renderApp(fetch);
    await openFiles(user);
    await user.click(screen.getByRole("button", { name: "Load more" }));
    const row = await screen.findByRole("row", { name: /later\.png/ });
    expect(within(row).getByRole("button", { name: /Upload a new version of later\.png/ }).hasAttribute("disabled")).toBe(true);
    await waitFor(() => {
      const refreshed = screen.getByRole("row", { name: /later\.png/ });
      expect(within(refreshed).getByRole("button", { name: /Upload a new version of later\.png/ }).hasAttribute("disabled")).toBe(false);
    }, { timeout: 8000 });
    expect(calls.some((call) => call.path === `/files/${laterFile.id}` && call.method === "GET")).toBe(true);
  }, 20_000);

  it("expired lock override yields to a later list refresh with a new foreign lock", async () => {
    const expiredLock = {
      holder_display_name: "Ada",
      since: new Date().toISOString(),
      expires_at: new Date(Date.now() + 1800).toISOString(),
      mine: false,
      held_by_current_session: false,
    };
    const renewedLock = { ...expiredLock, holder_display_name: "Bob", expires_at: new Date(Date.now() + 60_000).toISOString() };
    const lockedFile = { ...FILE_1, app: "photocraft", lock: expiredLock };
    let lists = 0;
    const { fetch } = mockFetch(viewRoutes({
      [`GET /projects/${PROJECT_1.id}/files`]: () => {
        lists += 1;
        return { body: { items: lists === 1
          ? [{ ...lockedFile, lock: { ...expiredLock, expires_at: new Date(Date.now() + 1800).toISOString() } }, FILE_EMPTY]
          : [{ ...lockedFile, lock: { ...renewedLock, expires_at: new Date(Date.now() + 60_000).toISOString() } }], next_cursor: null } };
      },
      [`GET /files/${lockedFile.id}`]: { body: { ...lockedFile, lock: null } },
      [`DELETE /files/${FILE_EMPTY.id}`]: { status: 204 },
    }));
    const user = userEvent.setup();
    renderApp(fetch);
    await openFiles(user);
    const versionButton = () => within(screen.getByRole("row", { name: /logo\.fig/ })).getByRole("button", { name: /Upload a new version of logo\.fig/ });
    expect(versionButton().hasAttribute("disabled")).toBe(true);
    await waitFor(() => expect(versionButton().hasAttribute("disabled")).toBe(false), { timeout: 8000 });
    await user.click(within(screen.getByRole("row", { name: /draft\.fig/ })).getByRole("button", { name: /Move to trash: draft\.fig/ }));
    const confirmation = await screen.findByRole("dialog", { name: /Move file to the trash/ });
    await user.click(within(confirmation).getByRole("button", { name: "Move to trash" }));
    await waitFor(() => expect(lists).toBeGreaterThan(1));
    await waitFor(() => expect(versionButton().hasAttribute("disabled")).toBe(true));
    expect(screen.getByText(/In progress by Bob/)).toBeTruthy();
  }, 20_000);

  it("links downloads to the API (no blob in memory) and marks files without content", async () => {
    const { fetch, calls } = mockFetch(viewRoutes());
    const user = userEvent.setup();
    renderApp(fetch);
    await openFiles(user);
    const link = screen.getByRole("link", { name: "Download logo.fig" });
    expect(link.getAttribute("href")).toBe(`/api/v1/files/${FILE_1.id}/content`);
    expect(link.hasAttribute("download")).toBe(true);
    const empty = screen.getByRole("row", { name: /draft\.fig/ });
    expect(within(empty).getByText("No content yet")).toBeTruthy();
    expect(within(empty).queryByRole("link")).toBeNull();
    expect(within(screen.getByRole("row", { name: /logo\.fig/ })).getByText("5 MiB")).toBeTruthy();
    // Listing never fetches file contents.
    expect(calls.some((c) => c.path.endsWith("/content"))).toBe(false);
  });

  it("shows a trash view with restore and explains a name clash", async () => {
    const { fetch, calls } = mockFetch(
      viewRoutes({
        [`GET /projects/${PROJECT_1.id}/files`]: (call) =>
          call.query.get("deleted") === "true"
            ? { body: { items: [{ ...FILE_1, deleted_at: "2026-10-28T10:00:00Z" }], next_cursor: null } }
            : { body: { items: [FILE_1], next_cursor: null } },
        [`POST /files/${FILE_1.id}/restore`]: apiError(409, "FILE_NAME_TAKEN"),
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openFiles(user);
    await user.click(screen.getByRole("checkbox", { name: "Show trash" }));
    await user.click(await screen.findByRole("button", { name: "Restore logo.fig" }));
    expect((await screen.findByRole("alert")).textContent).toContain("A file with this name already exists");
    expect(calls.filter((c) => c.path.endsWith("/restore")).length).toBe(1);
  });

  it("moves a file to the trash after confirmation", async () => {
    let trashed = false;
    const { fetch, calls } = mockFetch(
      viewRoutes({
        [`GET /projects/${PROJECT_1.id}/files`]: () => ({
          body: { items: trashed ? [FILE_EMPTY] : [FILE_1, FILE_EMPTY], next_cursor: null },
        }),
        [`DELETE /files/${FILE_1.id}`]: () => {
          trashed = true;
          return { status: 204 };
        },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openFiles(user);
    await user.click(screen.getByRole("button", { name: "Move to trash: logo.fig" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Move to trash" }));
    await waitFor(() => expect(screen.queryByRole("row", { name: /logo\.fig/ })).toBeNull());
    expect(calls.some((c) => c.method === "DELETE")).toBe(true);
  });

  it("offers a viewer downloads and history but no change", async () => {
    const { fetch } = mockFetch(
      viewRoutes({
        "GET /organizations": { body: { items: [{ ...ORG_A, role: "member" }], next_cursor: null } },
        [`GET /organizations/${ORG_A.id}/workspaces`]: {
          body: { items: [{ ...WS_1, workspace_role: "viewer" }], next_cursor: null },
        },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openFiles(user);
    const fileRow = screen.getByRole("row", { name: /logo\.fig/ });
    expect(within(fileRow).getByRole("link", { name: /Download/ })).toBeTruthy();
    expect(within(fileRow).getByRole("button", { name: /Versions of/ })).toBeTruthy();
    expect(within(fileRow).queryByRole("button", { name: /new version|trash/i })).toBeNull();
    expect(screen.queryByRole("button", { name: "Upload file" })).toBeNull();
  });
});

describe("upload", () => {
  it("uploads a new file: record, hash, reserved session, streamed PUT, first version", async () => {
    const { factory, requests } = fakeXhr(okPut);
    const { fetch, calls } = mockFetch(uploadRoutes());
    const user = userEvent.setup();
    renderApp(fetch, { xhrFactory: factory });
    await openFiles(user);
    const file = picked();
    const dialog = await chooseAndSubmit(user, file);
    expect((within(dialog).getByLabelText("File name") as HTMLInputElement).value).toBe("poster.png");
    expect(within(dialog).getByText(/Size: 5 MiB/)).toBeTruthy();
    // Keyboard only from here: file input -> name -> app -> Cancel -> Upload.
    await user.tab();
    await user.tab();
    await user.tab();
    await user.tab();
    expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Upload" }));
    await user.keyboard("{Enter}");

    expect(await within(dialog).findByText("poster.png was uploaded.")).toBeTruthy();
    const order = calls.filter((c) => c.method !== "GET").map((c) => `${c.method} ${c.path}`);
    expect(order).toEqual([
      `POST /projects/${PROJECT_1.id}/files`,
      `POST /files/${NEW_FILE.id}/uploads`,
      `POST /files/${NEW_FILE.id}/versions`,
    ]);
    expect(calls.find((c) => c.path.endsWith("/files") && c.method === "POST")?.body).toEqual({
      name: "poster.png",
      app: "generic",
      format: "png",
    });
    const reserve = calls.find((c) => c.path.endsWith("/uploads"));
    expect(reserve?.body).toEqual({ size_bytes: CONTENT.length, sha256: CONTENT_SHA });
    expect(reserve?.headers["idempotency-key"]).toMatch(/^[0-9a-f-]{36}$/);
    const commit = calls.find((c) => c.path.endsWith("/versions"));
    expect(commit?.headers["if-none-match"]).toBe("*");
    expect(commit?.headers["if-match"]).toBeUndefined();
    expect(commit?.body).toEqual({ upload_id: UPLOAD.upload_id });

    expect(requests).toHaveLength(1);
    expect(requests[0]?.method).toBe("PUT");
    expect(requests[0]?.url).toBe(`/api/v1/uploads/${UPLOAD.upload_id}/content`);
    expect(requests[0]?.headers["x-csrf-token"]).toBe("csrf-from-session");
    expect(requests[0]?.body).toBe(file); // the browser streams the File itself

    await user.click(within(dialog).getByRole("button", { name: "Close" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    // The list was reloaded after the upload.
    expect(calls.filter((c) => c.method === "GET" && c.path === `/projects/${PROJECT_1.id}/files`).length).toBeGreaterThan(1);
  });

  it("uploads a new version on top of the head with If-Match", async () => {
    const upload = { ...UPLOAD, file_id: FILE_1.id };
    const { factory } = fakeXhr(() => ({ body: { ...upload, state: "stored" } }));
    const { fetch, calls } = mockFetch(
      viewRoutes({
        [`POST /files/${FILE_1.id}/uploads`]: { status: 201, body: upload },
        [`POST /files/${FILE_1.id}/versions`]: { status: 201, body: version(3, "v3", { file_id: FILE_1.id }) },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch, { xhrFactory: factory });
    await openFiles(user);
    await user.click(screen.getByRole("button", { name: "Upload a new version of logo.fig" }));
    const dialog = await screen.findByRole("dialog", { name: "Upload a new version of logo.fig" });
    expect(within(dialog).queryByLabelText("File name")).toBeNull();
    await user.upload(within(dialog).getByLabelText("File"), picked("logo.fig"));
    await user.click(within(dialog).getByRole("button", { name: "Upload" }));
    expect(await within(dialog).findByText("logo.fig was uploaded.")).toBeTruthy();
    const commit = calls.find((c) => c.path === `/files/${FILE_1.id}/versions` && c.method === "POST");
    expect(commit?.headers["if-match"]).toBe(`"version:${HEAD}"`);
    expect(commit?.headers["if-none-match"]).toBeUndefined();
    expect(calls.some((c) => c.method === "POST" && c.path.endsWith("/projects/" + PROJECT_1.id + "/files"))).toBe(false);
  });

  it("validates the form locally and moves focus to the first problem", async () => {
    const { fetch, calls } = mockFetch(uploadRoutes());
    const user = userEvent.setup();
    renderApp(fetch, { xhrFactory: fakeXhr(okPut).factory });
    await openFiles(user);
    await user.click(await screen.findByRole("button", { name: "Upload file" }));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Upload" }));
    expect(await within(dialog).findByText("Choose a file.")).toBeTruthy();
    await waitFor(() => expect(document.activeElement).toBe(within(dialog).getByLabelText("File")));
    expect(calls.some((c) => c.method === "POST")).toBe(false);
  });

  it("returns to the form when the name is taken (409)", async () => {
    const { fetch } = mockFetch(uploadRoutes({ [`POST /projects/${PROJECT_1.id}/files`]: apiError(409, "FILE_NAME_TAKEN") }));
    const user = userEvent.setup();
    renderApp(fetch, { xhrFactory: fakeXhr(okPut).factory });
    await openFiles(user);
    const dialog = await chooseAndSubmit(user);
    await user.click(within(dialog).getByRole("button", { name: "Upload" }));
    expect(await within(dialog).findByText("A file with this name already exists in this project.")).toBeTruthy();
    await waitFor(() => expect(document.activeElement).toBe(within(dialog).getByLabelText("File name")));
  });

  it("explains an exhausted quota (413 QUOTA_EXCEEDED) before any byte is sent", async () => {
    const { factory, requests } = fakeXhr(okPut);
    const { fetch } = mockFetch(uploadRoutes({ [`POST /files/${NEW_FILE.id}/uploads`]: apiError(413, "QUOTA_EXCEEDED") }));
    const user = userEvent.setup();
    renderApp(fetch, { xhrFactory: factory });
    await openFiles(user);
    const dialog = await chooseAndSubmit(user);
    await user.click(within(dialog).getByRole("button", { name: "Upload" }));
    expect((await within(dialog).findByRole("alert")).textContent).toContain("storage quota is used up");
    expect(requests).toHaveLength(0);
    // 413 is not retryable: there is nothing to repeat.
    expect(within(dialog).queryByRole("button", { name: "Try again" })).toBeNull();
  });

  it("explains a file above the server maximum and a full disk (507)", async () => {
    const replies: UploadHandler[] = [
      () => apiError(413, "PAYLOAD_TOO_LARGE"),
      () => apiError(507, "INSUFFICIENT_STORAGE"),
    ];
    const { factory } = fakeXhr((request) => replies.shift()?.(request) ?? { body: STORED });
    const { fetch } = mockFetch(uploadRoutes());
    const user = userEvent.setup();
    renderApp(fetch, { xhrFactory: factory });
    await openFiles(user);
    const dialog = await chooseAndSubmit(user);
    await user.click(within(dialog).getByRole("button", { name: "Upload" }));
    expect((await within(dialog).findByRole("alert")).textContent).toContain("larger than the server accepts");
    await user.click(within(dialog).getByRole("button", { name: "Close" }));
    const second = await chooseAndSubmit(user);
    await user.click(within(second).getByRole("button", { name: "Upload" }));
    await waitFor(() => expect(within(second).getByRole("alert").textContent).toContain("no space left"));
  });

  it("retries after a network error with a fresh session and without hashing or creating again", async () => {
    let puts = 0;
    const { factory, requests } = fakeXhr(() => {
      puts += 1;
      return puts === 1 ? "network-error" : { body: STORED };
    });
    const keys: string[] = [];
    const { fetch, calls } = mockFetch(
      uploadRoutes({
        [`POST /files/${NEW_FILE.id}/uploads`]: (call) => {
          keys.push(call.headers["idempotency-key"] ?? "");
          return { status: 201, body: UPLOAD };
        },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch, { xhrFactory: factory });
    await openFiles(user);
    const dialog = await chooseAndSubmit(user);
    await user.click(within(dialog).getByRole("button", { name: "Upload" }));
    expect((await within(dialog).findByRole("alert")).textContent).toContain("cannot be reached");
    await user.click(within(dialog).getByRole("button", { name: "Try again" }));
    expect(await within(dialog).findByText("poster.png was uploaded.")).toBeTruthy();
    expect(requests).toHaveLength(2);
    expect(calls.filter((c) => c.path === `/projects/${PROJECT_1.id}/files` && c.method === "POST")).toHaveLength(1);
    // A session whose body failed cannot be resumed, so the retry reserves a new one.
    expect(keys).toHaveLength(2);
    expect(keys[0]).not.toBe(keys[1]);
  });

  it("reuses the idempotency key when only the reservation failed", async () => {
    let attempt = 0;
    const keys: string[] = [];
    const { fetch } = mockFetch(
      uploadRoutes({
        [`POST /files/${NEW_FILE.id}/uploads`]: (call) => {
          keys.push(call.headers["idempotency-key"] ?? "");
          attempt += 1;
          return attempt === 1 ? apiError(503, "STORAGE_UNAVAILABLE") : { status: 201, body: UPLOAD };
        },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch, { xhrFactory: fakeXhr(okPut).factory });
    await openFiles(user);
    const dialog = await chooseAndSubmit(user);
    await user.click(within(dialog).getByRole("button", { name: "Upload" }));
    expect((await within(dialog).findByRole("alert")).textContent).toContain("File storage is not available");
    await user.click(within(dialog).getByRole("button", { name: "Try again" }));
    expect(await within(dialog).findByText("poster.png was uploaded.")).toBeTruthy();
    expect(keys[0]).toBe(keys[1]);
  });

  it("cancels a running upload: aborts the request and releases the reservation", async () => {
    const { factory, requests } = fakeXhr(() => "hang");
    const { fetch, calls } = mockFetch(
      uploadRoutes({ [`DELETE /uploads/${UPLOAD.upload_id}`]: { status: 204 } }),
    );
    const user = userEvent.setup();
    renderApp(fetch, { xhrFactory: factory });
    await openFiles(user);
    const dialog = await chooseAndSubmit(user);
    await user.click(within(dialog).getByRole("button", { name: "Upload" }));
    const bar = await within(dialog).findByRole("progressbar", { name: "Uploading…" });
    await waitFor(() => expect(Number(bar.getAttribute("value"))).toBeGreaterThan(0));
    await user.click(within(dialog).getByRole("button", { name: "Cancel upload" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(requests[0]?.aborted).toBe(true);
    expect(calls.some((c) => c.method === "DELETE" && c.path === `/uploads/${UPLOAD.upload_id}`)).toBe(true);
    expect(calls.some((c) => c.path.endsWith("/versions"))).toBe(false);
  });

  it("does not close a busy dialog with Escape", async () => {
    const { factory } = fakeXhr(() => "hang");
    const { fetch } = mockFetch(uploadRoutes({ [`DELETE /uploads/${UPLOAD.upload_id}`]: { status: 204 } }));
    const user = userEvent.setup();
    renderApp(fetch, { xhrFactory: factory });
    await openFiles(user);
    const dialog = await chooseAndSubmit(user);
    await user.click(within(dialog).getByRole("button", { name: "Upload" }));
    await within(dialog).findByRole("progressbar");
    await user.keyboard("{Escape}");
    expect(screen.getByRole("dialog")).toBeTruthy();
    await user.click(within(dialog).getByRole("button", { name: "Cancel upload" }));
  });
});

describe("conflict on a new version (412 VERSION_CONFLICT)", () => {
  const CURRENT = "00000000-0000-4000-8000-0000000000ee";
  const upload = { ...UPLOAD, file_id: FILE_1.id };

  function conflictRoutes(extra: Record<string, ReturnType<typeof viewRoutes>[string]> = {}) {
    let commits = 0;
    return viewRoutes({
      [`POST /files/${FILE_1.id}/uploads`]: { status: 201, body: upload },
      [`POST /files/${FILE_1.id}/versions`]: (call) => {
        commits += 1;
        return commits === 1 && call.headers["if-match"] === `"version:${HEAD}"`
          ? ({ ...apiError(412, "VERSION_CONFLICT", { details: { current_version_id: CURRENT } }) } as Reply)
          : { status: 201, body: version(4, "v4", { file_id: FILE_1.id }) };
      },
      ...extra,
    });
  }

  async function reachConflict(user: User) {
    await openFiles(user);
    await user.click(screen.getByRole("button", { name: "Upload a new version of logo.fig" }));
    const dialog = await screen.findByRole("dialog");
    await user.upload(within(dialog).getByLabelText("File"), picked("logo.fig"));
    await user.click(within(dialog).getByRole("button", { name: "Upload" }));
    await screen.findByRole("heading", { name: "The file was changed meanwhile" });
    return dialog;
  }

  it("offers keep both, overwrite and cancel, with keep both focused first", async () => {
    const { fetch } = mockFetch(conflictRoutes({ [`DELETE /uploads/${UPLOAD.upload_id}`]: { status: 204 } }));
    const user = userEvent.setup();
    renderApp(fetch, { xhrFactory: fakeXhr(() => ({ body: { ...upload, state: "stored" } })).factory });
    const dialog = await reachConflict(user);
    const buttons = within(dialog).getAllByRole("button").map((b) => b.textContent);
    expect(buttons).toEqual(["Cancel", "Overwrite", "Keep both"]);
    await waitFor(() => expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Keep both" })));
    await waitFor(() =>
      expect(screen.getByTestId("announcer").textContent).toContain("Somebody else saved a new version meanwhile"),
    );
  });

  it("overwrite commits on top of the current head from details.current_version_id", async () => {
    const { fetch, calls } = mockFetch(conflictRoutes());
    const user = userEvent.setup();
    renderApp(fetch, { xhrFactory: fakeXhr(() => ({ body: { ...upload, state: "stored" } })).factory });
    const dialog = await reachConflict(user);
    await user.click(within(dialog).getByRole("button", { name: "Overwrite" }));
    expect(await within(dialog).findByText("logo.fig was uploaded.")).toBeTruthy();
    const commits = calls.filter((c) => c.path === `/files/${FILE_1.id}/versions` && c.method === "POST");
    expect(commits.map((c) => c.headers["if-match"])).toEqual([`"version:${HEAD}"`, `"version:${CURRENT}"`]);
    // The same stored upload is committed again; nothing is uploaded twice.
    expect(commits.map((c) => (c.body as { upload_id: string }).upload_id)).toEqual([upload.upload_id, upload.upload_id]);
  });

  it("keep both saves the stored upload as the first version of a separate copy", async () => {
    const copy = { ...FILE_EMPTY, id: "00000000-0000-4000-8000-0000000000da", name: "logo (conflict copy).fig" };
    const { fetch, calls } = mockFetch(
      conflictRoutes({
        [`POST /projects/${PROJECT_1.id}/files`]: { status: 201, body: copy },
        [`POST /files/${copy.id}/versions`]: { status: 201, body: version(1, "vc", { file_id: copy.id }) },
      }),
    );
    const user = userEvent.setup();
    const { factory, requests } = fakeXhr(() => ({ body: { ...upload, state: "stored" } }));
    renderApp(fetch, { xhrFactory: factory });
    const dialog = await reachConflict(user);
    await user.click(within(dialog).getByRole("button", { name: "Keep both" }));
    expect(await within(dialog).findByText("Saved as a separate file: logo (conflict copy).fig.")).toBeTruthy();
    expect(calls.find((c) => c.path === `/projects/${PROJECT_1.id}/files` && c.method === "POST")?.body).toEqual({
      name: "logo (conflict copy).fig",
      app: "figcraft",
      format: "fig",
    });
    const commit = calls.find((c) => c.path === `/files/${copy.id}/versions`);
    expect(commit?.headers["if-none-match"]).toBe("*");
    expect((commit?.body as { upload_id: string }).upload_id).toBe(upload.upload_id);
    expect(requests).toHaveLength(1); // no second upload
  });

  it("keep both tries the next copy name when the first is taken", async () => {
    const copy = { ...FILE_EMPTY, id: "00000000-0000-4000-8000-0000000000db", name: "logo (conflict copy 2).fig" };
    let creates = 0;
    const { fetch, calls } = mockFetch(
      conflictRoutes({
        [`POST /projects/${PROJECT_1.id}/files`]: () => {
          creates += 1;
          return creates === 1 ? apiError(409, "FILE_NAME_TAKEN") : { status: 201, body: copy };
        },
        [`POST /files/${copy.id}/versions`]: { status: 201, body: version(1, "vc", { file_id: copy.id }) },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch, { xhrFactory: fakeXhr(() => ({ body: { ...upload, state: "stored" } })).factory });
    const dialog = await reachConflict(user);
    await user.click(within(dialog).getByRole("button", { name: "Keep both" }));
    expect(await within(dialog).findByText(/conflict copy 2/)).toBeTruthy();
    const names = calls
      .filter((c) => c.path === `/projects/${PROJECT_1.id}/files` && c.method === "POST")
      .map((c) => (c.body as { name: string }).name);
    expect(names).toEqual(["logo (conflict copy).fig", "logo (conflict copy 2).fig"]);
  });

  it("cancel discards the stored upload and closes the dialog, focus back on the row action", async () => {
    const { fetch, calls } = mockFetch(conflictRoutes({ [`DELETE /uploads/${UPLOAD.upload_id}`]: { status: 204 } }));
    const user = userEvent.setup();
    renderApp(fetch, { xhrFactory: fakeXhr(() => ({ body: { ...upload, state: "stored" } })).factory });
    const dialog = await reachConflict(user);
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(calls.some((c) => c.method === "DELETE" && c.path === `/uploads/${UPLOAD.upload_id}`)).toBe(true);
    await waitFor(() =>
      expect(document.activeElement).toBe(screen.getByRole("button", { name: "Upload a new version of logo.fig" })),
    );
  });

  it("is operable with the keyboard alone", async () => {
    const { fetch, calls } = mockFetch(conflictRoutes());
    const user = userEvent.setup();
    renderApp(fetch, { xhrFactory: fakeXhr(() => ({ body: { ...upload, state: "stored" } })).factory });
    const dialog = await reachConflict(user);
    await waitFor(() => expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Keep both" })));
    await user.tab(); // Cancel, first action in DOM order
    expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Cancel" }));
    await user.tab(); // Overwrite
    expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Overwrite" }));
    await user.keyboard("{Enter}");
    expect(await within(dialog).findByText("logo.fig was uploaded.")).toBeTruthy();
    expect(calls.filter((c) => c.path.endsWith("/versions") && c.method === "POST")).toHaveLength(2);
  });
});

describe("version history", () => {
  const OLD = "00000000-0000-4000-8000-0000000000e0";

  it("lists versions, links downloads and restores an old one as a new version", async () => {
    let restored = false;
    const { fetch, calls } = mockFetch(
      viewRoutes({
        [`GET /files/${FILE_1.id}/versions`]: () => ({
          body: {
            items: restored
              ? [version(3, "v3", { restored_from_version_id: OLD }), version(2, HEAD), version(1, OLD)]
              : [version(2, HEAD), version(1, OLD)],
            next_cursor: null,
          },
        }),
        [`POST /files/${FILE_1.id}/versions/${OLD}/restore`]: () => {
          restored = true;
          return { status: 201, body: version(3, "v3", { restored_from_version_id: OLD }) };
        },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openFiles(user);
    await user.click(screen.getByRole("button", { name: "Versions of logo.fig" }));
    const dialog = await screen.findByRole("dialog", { name: "Versions of logo.fig" });
    expect(await within(dialog).findByRole("row", { name: /2 · Current/ })).toBeTruthy();
    expect(within(dialog).getByRole("link", { name: "Download version 1" }).getAttribute("href")).toBe(
      `/api/v1/files/${FILE_1.id}/versions/${OLD}/content`,
    );
    // The current version cannot be restored onto itself.
    expect(within(within(dialog).getByRole("row", { name: /2 · Current/ })).queryByRole("button")).toBeNull();

    await user.click(within(dialog).getByRole("button", { name: "Restore version 1 as a new version" }));
    expect(await within(dialog).findByRole("row", { name: /3 · Current · Restored/ })).toBeTruthy();
    const restore = calls.find((c) => c.path.endsWith("/restore"));
    expect(restore?.headers["if-match"]).toBe(`"version:${HEAD}"`);
    await waitFor(() =>
      expect(screen.getByTestId("announcer").textContent).toBe("Version 1 restored as version 3."),
    );

    const filesBefore = calls.filter((c) => c.path === `/projects/${PROJECT_1.id}/files`).length;
    await user.click(within(dialog).getByRole("button", { name: "Close" }));
    await waitFor(() =>
      expect(calls.filter((c) => c.path === `/projects/${PROJECT_1.id}/files`).length).toBe(filesBefore + 1),
    );
  });

  it("explains 412 on restore and reloads the history", async () => {
    const { fetch, calls } = mockFetch(
      viewRoutes({ [`POST /files/${FILE_1.id}/versions/00000000-0000-4000-8000-0000000000e0/restore`]: apiError(412, "VERSION_CONFLICT") }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openFiles(user);
    await user.click(screen.getByRole("button", { name: "Versions of logo.fig" }));
    const dialog = await screen.findByRole("dialog");
    await user.click(await within(dialog).findByRole("button", { name: "Restore version 1 as a new version" }));
    expect((await within(dialog).findByRole("alert")).textContent).toContain("Somebody else changed this item");
    const before = calls.filter((c) => c.path === `/files/${FILE_1.id}/versions` && c.method === "GET").length;
    await user.click(within(dialog).getByRole("button", { name: "Reload" }));
    await waitFor(() =>
      expect(calls.filter((c) => c.path === `/files/${FILE_1.id}/versions` && c.method === "GET").length).toBe(before + 1),
    );
  });
});
