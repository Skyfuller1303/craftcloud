import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { apiError, expectNoAxeViolations, mockFetch, renderApp } from "./test/helpers.tsx";
import { FILE_1, viewRoutes } from "./test/fixtures.ts";
import type { Handler } from "./test/helpers.tsx";

const path = `/files/${FILE_1.id}/live`;
const status = { state: "active", access: "view", can_snapshot: false, can_stop: false, last_snapshot_at: null, participants: null };
const link = "figcraft://join?server=wss%3A%2F%2Fcloud.example%2Ffigcraft%2F&file=fixture&token=synthetic-first";

function routes(extra: Record<string, Handler> = {}) {
  return viewRoutes({
    "GET /info": { body: { features: [], feature_flags: { figcraft_live: true } } },
    [`GET ${path}`]: { body: status },
    [`POST ${path}`]: { body: { access: "view", join_url: link } },
    ...extra,
  });
}

async function files(user: ReturnType<typeof userEvent.setup>) {
  await user.click(await screen.findByRole("button", { name: /Projects|Projekte/ }));
  await user.click(await screen.findByRole("button", { name: /Open files of Brand kit|Dateien von Brand kit öffnen/ }));
}

describe("FigCraft live", () => {
  it("does not offer or publish live content when discovery disables the companion", async () => {
    const mock = mockFetch(routes({ "GET /info": { body: { features: [], feature_flags: { figcraft_live: false } } } }));
    const user = userEvent.setup();
    renderApp(mock.fetch);
    await files(user);
    await screen.findByRole("row", { name: /logo\.fig/ });
    expect(screen.queryByRole("button", { name: "Open logo.fig live" })).toBeNull();
    expect(mock.calls.some((call) => call.path.endsWith("/live"))).toBe(false);
  });

  it.each(["en", "de"])("keeps viewer links in memory and rotates only on request (%s)", async (language) => {
    let generation = 0;
    const mock = mockFetch(routes({ [`POST ${path}`]: (call) => {
      expect(call.body).toEqual({ renew: generation > 0 });
      generation++;
      return { body: { access: "view", join_url: generation === 1 ? link : link.replace("first", "second") } };
    } }));
    const user = userEvent.setup();
    renderApp(mock.fetch, { language });
    await files(user);
    const opener = await screen.findByRole("button", { name: /Open logo.fig live|logo.fig live öffnen/ });
    await user.click(opener);
    const dialog = await screen.findByRole("dialog");
    await within(dialog).findByText(/Your access: view|Dein Zugriff: Lesen/);
    expect(mock.calls.filter((call) => call.method === "POST" && call.path === path)).toHaveLength(0);
    expect(within(dialog).queryByRole("button", { name: /Save cloud snapshot|Cloud-Sicherung erstellen/ })).toBeNull();
    expect(within(dialog).queryByRole("button", { name: /End live collaboration|Live-Zusammenarbeit beenden/ })).toBeNull();
    await user.click(within(dialog).getByRole("button", { name: /Show personal link|Persönlichen Link anzeigen/ }));
    const input = await within(dialog).findByRole("textbox", { name: /Personal FigCraft link|Persönlicher FigCraft-Link/ });
    expect((input as HTMLInputElement).value).toBe(link);
    await expectNoAxeViolations();
    await user.click(within(dialog).getByRole("button", { name: /Renew link|Link erneuern/ }));
    await within(dialog).findByDisplayValue(link.replace("first", "second"));
    expect(JSON.stringify(localStorage)).not.toContain("synthetic-");
    expect(JSON.stringify(sessionStorage)).not.toContain("synthetic-");
    expect(location.href).not.toContain("synthetic-");
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    await waitFor(() => expect(document.activeElement).toBe(opener));
    expect(screen.queryByDisplayValue(link.replace("first", "second"))).toBeNull();
  });

  it("clears a displayed token after revocation and never creates an unsafe launch link", async () => {
    let count = 0;
    const mock = mockFetch(routes({ [`POST ${path}`]: () => {
      count++;
      return count === 1 ? { body: { access: "view", join_url: link } } : apiError(403, "FORBIDDEN");
    } }));
    const user = userEvent.setup();
    renderApp(mock.fetch);
    await files(user);
    await user.click(await screen.findByRole("button", { name: "Open logo.fig live" }));
    await user.click(await screen.findByRole("button", { name: "Show personal link" }));
    await screen.findByDisplayValue(link);
    await user.click(screen.getByRole("button", { name: "Renew link" }));
    await screen.findByRole("alert");
    expect(screen.queryByDisplayValue(link)).toBeNull();
    expect(screen.queryByRole("link", { name: "Open in FigCraft" })).toBeNull();
  });

  it("rejects a non-FigCraft scheme from the link response", async () => {
    const mock = mockFetch(routes({ [`POST ${path}`]: { body: { access: "view", join_url: "javascript:alert(1)" } } }));
    const user = userEvent.setup();
    renderApp(mock.fetch);
    await files(user);
    await user.click(await screen.findByRole("button", { name: "Open logo.fig live" }));
    await user.click(await screen.findByRole("button", { name: "Show personal link" }));
    await screen.findByRole("alert");
    expect(screen.queryByRole("link", { name: "Open in FigCraft" })).toBeNull();
  });

  it("saves only on request and confirms ending live collaboration before disconnecting", async () => {
    let active = true;
    const mock = mockFetch(routes({
      [`GET ${path}`]: () => ({ body: { ...status, state: active ? "active" : "inactive", access: "edit", can_snapshot: true, can_stop: true } }),
      [`POST ${path}/snapshot`]: { body: { id: "synthetic-version" } },
      [`DELETE ${path}`]: () => { active = false; return { status: 204 }; },
    }));
    const user = userEvent.setup();
    renderApp(mock.fetch);
    await files(user);
    await user.click(await screen.findByRole("button", { name: "Open logo.fig live" }));
    await user.click(await screen.findByRole("button", { name: "Save cloud snapshot" }));
    await screen.findByText("Cloud snapshot saved.");
    expect(mock.calls.filter((call) => call.path === `${path}/snapshot`)).toHaveLength(1);
    await user.click(screen.getByRole("button", { name: "End live collaboration" }));
    const confirmation = screen.getAllByRole("dialog").at(-1);
    expect(confirmation).toBeDefined();
    expect(mock.calls.some((call) => call.method === "DELETE")).toBe(false);
    await user.click(within(confirmation!).getByRole("button", { name: "End live collaboration" }));
    await screen.findByText("Creating a link starts live collaboration from the current version.");
    expect(mock.calls.filter((call) => call.method === "DELETE" && call.path === path)).toHaveLength(1);
  });
});
