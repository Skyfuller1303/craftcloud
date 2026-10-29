import { createHash } from "node:crypto";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { WS_1, apiError, mockFetch, renderApp } from "./test/helpers.tsx";
import { fakeXhr } from "./test/fakeXhr.ts";
import type { UploadHandler } from "./test/fakeXhr.ts";
import { FILE_1, FILE_EMPTY, HEAD, ME, PROJECT_1, version, viewRoutes } from "./test/fixtures.ts";

type User = ReturnType<typeof userEvent.setup>;

const BYTES = new Uint8Array(200_000).map((_, i) => (i * 7) % 251);
const SHA = createHash("sha256").update(BYTES).digest("hex");
const UPLOAD = {
  upload_id: "00000000-0000-4000-8000-0000000000a9",
  file_id: FILE_1.id,
  project_id: PROJECT_1.id,
  workspace_id: WS_1.id,
  state: "created",
  size_bytes: BYTES.length,
  sha256: SHA,
  max_bytes: BYTES.length,
  created_by: ME,
  created_at: "x",
  expires_at: "y",
};
const STORED = { ...UPLOAD, state: "stored" };
const picked = () => new File([BYTES], "logo.fig", { lastModified: 1_700_000_000_000 });

async function openVersionDialog(user: User) {
  await user.click(await screen.findByRole("button", { name: "Projects" }));
  await user.click(await screen.findByRole("button", { name: "Open files of Brand kit" }));
  await user.click(await screen.findByRole("button", { name: "Upload a new version of logo.fig" }));
  const dialog = await screen.findByRole("dialog");
  await user.upload(within(dialog).getByLabelText("File"), picked());
  return dialog;
}

/** Holds back requests whose URL matches until `release()`; everything else passes through. */
function gate(fetch: typeof globalThis.fetch, match: (url: string, method: string) => boolean) {
  let release!: () => void;
  const open = new Promise<void>((resolve) => (release = resolve));
  const gated = (async (input: RequestInfo | URL, init?: RequestInit) => {
    if (match(String(input), init?.method ?? "GET")) await open;
    return fetch(input, init);
  }) as typeof globalThis.fetch;
  return { gated, release };
}

const baseRoutes = (extra = {}) =>
  viewRoutes({
    [`POST /files/${FILE_1.id}/uploads`]: { status: 201, body: UPLOAD },
    [`DELETE /uploads/${UPLOAD.upload_id}`]: { status: 204 },
    [`POST /files/${FILE_1.id}/versions`]: { status: 201, body: version(3, "v3", { file_id: FILE_1.id }) },
    ...extra,
  });

describe("upload sessions are never leaked", () => {
  it("releases a reservation that arrives after the user cancelled", async () => {
    const { fetch, calls } = mockFetch(baseRoutes());
    const { gated, release } = gate(fetch, (url, method) => method === "POST" && url.endsWith("/uploads"));
    const { factory, requests } = fakeXhr(() => ({ body: STORED }));
    const user = userEvent.setup();
    renderApp(gated, { xhrFactory: factory });
    const dialog = await openVersionDialog(user);
    await user.click(within(dialog).getByRole("button", { name: "Upload" }));
    await within(dialog).findByText("Reserving space…");
    await user.click(within(dialog).getByRole("button", { name: "Cancel upload" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(calls.some((c) => c.method === "DELETE")).toBe(false); // nothing known yet

    release(); // the reservation answer arrives now
    await waitFor(() =>
      expect(calls.some((c) => c.method === "DELETE" && c.path === `/uploads/${UPLOAD.upload_id}`)).toBe(true),
    );
    expect(requests).toHaveLength(0); // no byte was sent
    expect(calls.some((c) => c.path.endsWith("/versions") && c.method === "POST")).toBe(false);
  });

  it("releases an open session when the dialog is unmounted without Cancel", async () => {
    const { fetch, calls } = mockFetch(baseRoutes());
    const { factory } = fakeXhr(() => "hang");
    const user = userEvent.setup();
    const view = renderApp(fetch, { xhrFactory: factory });
    const dialog = await openVersionDialog(user);
    await user.click(within(dialog).getByRole("button", { name: "Upload" }));
    await within(dialog).findByRole("progressbar");
    view.unmount();
    await waitFor(() =>
      expect(calls.some((c) => c.method === "DELETE" && c.path === `/uploads/${UPLOAD.upload_id}`)).toBe(true),
    );
  });

  it("gives the reservation back when the body fails on the client side", async () => {
    const { fetch, calls } = mockFetch(baseRoutes());
    const { factory } = fakeXhr(() => "network-error");
    const user = userEvent.setup();
    renderApp(fetch, { xhrFactory: factory });
    const dialog = await openVersionDialog(user);
    await user.click(within(dialog).getByRole("button", { name: "Upload" }));
    await within(dialog).findByRole("alert");
    expect(calls.filter((c) => c.method === "DELETE" && c.path === `/uploads/${UPLOAD.upload_id}`)).toHaveLength(1);
  });

  it("cannot be cancelled while the version is being recorded", async () => {
    const { fetch, calls } = mockFetch(baseRoutes());
    const { gated, release } = gate(fetch, (url, method) => method === "POST" && url.endsWith("/versions"));
    const { factory } = fakeXhr(() => ({ body: STORED }));
    const user = userEvent.setup();
    renderApp(gated, { xhrFactory: factory });
    const dialog = await openVersionDialog(user);
    await user.click(within(dialog).getByRole("button", { name: "Upload" }));
    await within(dialog).findByText("Saving the version…");
    expect((within(dialog).getByRole("button", { name: "Cancel upload" }) as HTMLButtonElement).disabled).toBe(true);
    release();
    expect(await within(dialog).findByText("logo.fig was uploaded.")).toBeTruthy();
    expect(calls.some((c) => c.method === "DELETE")).toBe(false);
  });
});

describe("restartable body failures", () => {
  it.each(["CHECKSUM_MISMATCH", "LENGTH_MISMATCH", "UPLOAD_INTERRUPTED"])(
    "%s offers Try again, which hashes and uploads again with a new session",
    async (code) => {
      let puts = 0;
      const handler: UploadHandler = () => {
        puts += 1;
        return puts === 1 ? apiError(code === "UPLOAD_INTERRUPTED" ? 400 : 422, code) : { body: STORED };
      };
      const { fetch, calls } = mockFetch(baseRoutes());
      const { factory, requests } = fakeXhr(handler);
      const user = userEvent.setup();
      renderApp(fetch, { xhrFactory: factory });
      const dialog = await openVersionDialog(user);
      await user.click(within(dialog).getByRole("button", { name: "Upload" }));
      await within(dialog).findByRole("alert");
      await user.click(within(dialog).getByRole("button", { name: "Try again" }));
      expect(await within(dialog).findByText("logo.fig was uploaded.")).toBeTruthy();
      expect(requests).toHaveLength(2);
      expect(calls.filter((c) => c.path.endsWith("/uploads") && c.method === "POST")).toHaveLength(2);
    },
  );
});

describe("keep both after a failure", () => {
  it("retries the commit on the copy that already exists instead of creating another", async () => {
    const CURRENT = "00000000-0000-4000-8000-0000000000ee";
    const copy = { ...FILE_EMPTY, id: "00000000-0000-4000-8000-0000000000da", name: "logo (conflict copy).fig" };
    let commitsOnCopy = 0;
    const { fetch, calls } = mockFetch(
      baseRoutes({
        [`POST /files/${FILE_1.id}/versions`]: apiError(412, "VERSION_CONFLICT", { details: { current_version_id: CURRENT } }),
        [`POST /projects/${PROJECT_1.id}/files`]: { status: 201, body: copy },
        [`POST /files/${copy.id}/versions`]: () => {
          commitsOnCopy += 1;
          return commitsOnCopy === 1 ? apiError(503, "UNAVAILABLE") : { status: 201, body: version(1, "vc", { file_id: copy.id }) };
        },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch, { xhrFactory: fakeXhr(() => ({ body: STORED })).factory });
    const dialog = await openVersionDialog(user);
    await user.click(within(dialog).getByRole("button", { name: "Upload" }));
    await user.click(await within(dialog).findByRole("button", { name: "Keep both" }));
    await within(dialog).findByRole("alert");
    await user.click(within(dialog).getByRole("button", { name: "Try again" }));
    expect(await within(dialog).findByText("Saved as a separate file: logo (conflict copy).fig.")).toBeTruthy();
    expect(calls.filter((c) => c.path === `/projects/${PROJECT_1.id}/files` && c.method === "POST")).toHaveLength(1);
    expect(commitsOnCopy).toBe(2);
    expect(HEAD).toBeTruthy();
  });
});
