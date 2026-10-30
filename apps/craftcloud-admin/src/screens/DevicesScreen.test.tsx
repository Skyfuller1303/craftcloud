import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { apiError, expectNoAxeViolations, loggedInRoutes, mockFetch, renderApp } from "../test/helpers.tsx";

const DEVICE = {
  id: "00000000-0000-4000-8000-0000000000d1",
  name: "PhotoCraft on LAPTOP-7",
  client_kind: "app",
  client_id: "photocraft",
  created_at: "2026-10-28T10:00:00Z",
  last_used_at: "2026-10-29T10:00:00Z",
};

describe.each([["en", "Settings", "Devices"], ["de", "Einstellungen", "Geräte"]] as const)("device settings (%s)", (language, navLabel, title) => {
  it("shows device details, revokes one device, and passes axe", async () => {
    const { fetch, calls } = mockFetch(loggedInRoutes({
      "GET /auth/devices": { body: { devices: [DEVICE] } },
      [`DELETE /auth/devices/${DEVICE.id}`]: { status: 204 },
    }));
    const user = userEvent.setup();
    renderApp(fetch, { language });
    const nav = await screen.findByRole("navigation");
    await user.click(within(nav).getByRole("button", { name: navLabel }));
    const table = await screen.findByRole("table", { name: title });
    expect(within(table).getByText("PhotoCraft on LAPTOP-7")).toBeTruthy();
    expect(within(table).getByText("photocraft")).toBeTruthy();
    await expectNoAxeViolations();
    await user.click(within(table).getByRole("button", { name: language === "en" ? "Sign out" : "Abmelden" }));
    await waitFor(() => expect(screen.queryByRole("table")).toBeNull());
    expect(calls.some((call) => call.method === "DELETE" && call.path === `/auth/devices/${DEVICE.id}`)).toBe(true);
    await expectNoAxeViolations();
  });

  it("shows empty and error states accessibly", async () => {
    const { fetch } = mockFetch(loggedInRoutes({
      "GET /auth/devices": { body: { devices: [] } },
    }));
    const user = userEvent.setup();
    renderApp(fetch, { language });
    const nav = await screen.findByRole("navigation");
    await user.click(within(nav).getByRole("button", { name: navLabel }));
    await screen.findByText(language === "en" ? "There are no registered devices." : "Es sind keine Geräte registriert.");
    await expectNoAxeViolations();
  });
});

it("shows a recoverable device-list error", async () => {
  let fail = true;
  const { fetch } = mockFetch(loggedInRoutes({
    "GET /auth/devices": () => fail ? apiError(503, "UNAVAILABLE") : { body: { devices: [] } },
  }));
  const user = userEvent.setup();
  renderApp(fetch);
  const nav = await screen.findByRole("navigation");
  await user.click(within(nav).getByRole("button", { name: "Settings" }));
  const alert = await screen.findByRole("alert");
  await expectNoAxeViolations();
  fail = false;
  await user.click(within(alert.parentElement!).getByRole("button", { name: "Try again" }));
  await screen.findByText("There are no registered devices.");
});
