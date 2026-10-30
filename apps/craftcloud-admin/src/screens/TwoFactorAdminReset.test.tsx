import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { apiError, expectNoAxeViolations, loggedInRoutes, mockFetch, renderApp, ORG_A, WS_1 } from "../test/helpers.tsx";

const MEMBER = {
  user_id: "00000000-0000-4000-8000-0000000000c1",
  display_name: "Grace Hopper",
  email: "grace@example.test",
  role: "member",
  joined_at: "2026-10-01T10:00:00Z",
};
const RESET = {
  reset_url: "https://cloud.example.test/admin/reset-password#token=one-time-secret",
  expires_at: "2026-10-30T15:00:00Z",
};

describe.each([["en", "Members", "Reset account: issue a password reset link for Grace Hopper", "Also reset two-factor authentication"], ["de", "Mitglieder", "Konto zurücksetzen: Passwort-Zurücksetzlink für Grace Hopper ausstellen", "Auch die Zwei-Faktor-Anmeldung zurücksetzen"]] as const)(
  "operator two-factor reset (%s)",
  (language, navigation, resetButton, checkboxLabel) => {
    it("issues a one-time password link with the optional two-factor reset", async () => {
      const { fetch, calls } = mockFetch(loggedInRoutes({
        "GET /organizations": { body: { items: [ORG_A], next_cursor: null } },
        ["GET /organizations/" + ORG_A.id + "/members"]: { body: { items: [MEMBER], next_cursor: null } },
        ["GET /workspaces/" + WS_1.id + "/members"]: { body: { items: [], next_cursor: null } },
        ["POST /admin/users/" + MEMBER.user_id + "/password-reset"]: { status: 201, body: RESET },
      }));
      const user = userEvent.setup();
      renderApp(fetch, { language });
      const nav = await screen.findByRole("navigation");
      await user.click(await within(nav).findByRole("button", { name: navigation }));
      await screen.findByText("Grace Hopper");
      await user.click(await screen.findByRole("button", { name: resetButton }));
      await expectNoAxeViolations();
      await user.click(screen.getByRole("checkbox", { name: checkboxLabel }));
      await user.click(screen.getByRole("button", { name: language === "en" ? "Issue reset link" : "Zurücksetzlink ausstellen" }));
      const link = await screen.findByLabelText(language === "en" ? "One-time reset link" : "Einmaliger Zurücksetzlink");
      expect((link as HTMLInputElement).value).toBe(RESET.reset_url);
      expect(calls.find((call) => call.path === "/admin/users/" + MEMBER.user_id + "/password-reset")?.body)
        .toEqual({ reset_two_factor: true });
      await expectNoAxeViolations();
      await user.click(screen.getByRole("button", { name: language === "en" ? "Close" : "Schließen" }));
      expect(screen.queryByDisplayValue(RESET.reset_url)).toBeNull();
    });
  },
);

describe.each([["en", "Members", "Reset account: issue a password reset link for Grace Hopper", "Try again"], ["de", "Mitglieder", "Konto zurücksetzen: Passwort-Zurücksetzlink für Grace Hopper ausstellen", "Erneut versuchen"]] as const)(
  "operator reset error recovery (%s)",
  (language, navigation, resetButton, retryLabel) => {
    it("shows the first request error and retries the same reset", async () => {
      let attempts = 0;
      const { fetch, calls } = mockFetch(loggedInRoutes({
        "GET /organizations": { body: { items: [ORG_A], next_cursor: null } },
        ["GET /organizations/" + ORG_A.id + "/members"]: { body: { items: [MEMBER], next_cursor: null } },
        ["GET /workspaces/" + WS_1.id + "/members"]: { body: { items: [], next_cursor: null } },
        ["POST /admin/users/" + MEMBER.user_id + "/password-reset"]: () => {
          attempts += 1;
          return attempts === 1 ? apiError(503, "SERVER_ERROR") : { status: 201, body: RESET };
        },
      }));
      const user = userEvent.setup();
      renderApp(fetch, { language });
      const nav = await screen.findByRole("navigation");
      await user.click(await within(nav).findByRole("button", { name: navigation }));
      await screen.findByText("Grace Hopper");
      await user.click(await screen.findByRole("button", { name: resetButton }));
      await user.click(screen.getByRole("checkbox", { name: language === "en" ? "Also reset two-factor authentication" : "Auch die Zwei-Faktor-Anmeldung zurücksetzen" }));
      await user.click(screen.getByRole("button", { name: language === "en" ? "Issue reset link" : "Zurücksetzlink ausstellen" }));
      expect(await screen.findByRole("alert")).toBeTruthy();
      await user.click(screen.getByRole("button", { name: retryLabel }));
      expect(await screen.findByLabelText(language === "en" ? "One-time reset link" : "Einmaliger Zurücksetzlink")).toBeTruthy();
      expect(calls.filter((call) => call.path === "/admin/users/" + MEMBER.user_id + "/password-reset").map((call) => call.body))
        .toEqual([{ reset_two_factor: true }, { reset_two_factor: true }]);
    });
  },
);


