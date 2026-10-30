import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { apiError, expectNoAxeViolations, loggedInRoutes, mockFetch, renderApp, SESSION, ORG_A } from "../test/helpers.tsx";
import type { Handler } from "../test/helpers.tsx";

const ENROLLMENT = {
  secret: "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP",
  otpauth_url: "otpauth://totp/CraftCloud:ada%40example.test?secret=JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP&issuer=CraftCloud&algorithm=SHA1&digits=6&period=30",
};
const RECOVERY = { recovery_codes: ["ABCD234567", "BCDE345678", "CDEF456789", "DEFG56789A", "EFGH6789AB", "FGHJ789ABC", "GHJK89ABCD", "HJKM9ABCDE", "JKMNABCDE2", "KMNPBCDE23"] };

describe.each([["en", "Settings", "Two-factor authentication"], ["de", "Einstellungen", "Zwei-Faktor-Anmeldung"]] as const)(
  "two-factor settings (%s)",
  (language, navigation, heading) => {
    it("sets up TOTP, saves recovery codes once, regenerates them and changes the organization policy", async () => {
      const routes: Record<string, Handler> = loggedInRoutes({
        "GET /organizations": { body: { items: [{ ...ORG_A, require_two_factor: false }], next_cursor: null } },
        "GET /auth/2fa": { body: { enabled: false, available: true, recovery_codes_remaining: 0 } },
        "POST /auth/2fa/setup": (call) => {
          expect(call.body).toEqual({ password: "correct horse battery staple" });
          return { body: ENROLLMENT };
        },
        "POST /auth/2fa/confirm": { body: RECOVERY },
        "POST /auth/2fa/recovery-codes": { body: RECOVERY },
        "POST /auth/2fa/disable": { status: 204 },
        ["PATCH /organizations/" + ORG_A.id]: (call) => {
          expect(call.body).toEqual({ require_two_factor: true });
          return { body: { ...ORG_A, version: 2, require_two_factor: true } };
        },
      });
      const { fetch, calls } = mockFetch(routes);
      const user = userEvent.setup();
      renderApp(fetch, { language });
      const nav = await screen.findByRole("navigation");
      await user.click(await within(nav).findByRole("button", { name: navigation }));
      await screen.findByRole("heading", { name: heading, level: 2 });
      await expectNoAxeViolations();

      const password = screen.getByLabelText(language === "en" ? "Current password" : "Aktuelles Passwort");
      await user.type(password, "correct horse battery staple");
      await user.click(screen.getByRole("button", { name: language === "en" ? "Set up two-factor authentication" : "Zwei-Faktor-Anmeldung einrichten" }));
      const qr = await screen.findByRole("img", { name: language === "en" ? "Authenticator setup QR code" : "QR-Code zur Authenticator-Einrichtung" });
      expect(qr.querySelector("path")).not.toBeNull();
      expect(qr.querySelector("script, style, [style], [onload], [onclick]")).toBeNull();
      expect(screen.getByText(ENROLLMENT.secret)).toBeTruthy();
      await expectNoAxeViolations();

      await user.type(screen.getByLabelText(language === "en" ? "Authentication code" : "Authentifizierungscode"), "123456");
      await user.click(screen.getByRole("button", { name: language === "en" ? "Confirm and show recovery codes" : "Bestätigen und Wiederherstellungscodes anzeigen" }));
      const codeArea = await screen.findByRole("textbox", { name: language === "en" ? "Recovery codes" : "Wiederherstellungscodes" });
      expect((codeArea as HTMLTextAreaElement).value).toBe(RECOVERY.recovery_codes.join("\n"));
      await expectNoAxeViolations();

      const createObjectUrl = vi.fn(() => "blob:recovery-codes");
      Object.defineProperty(URL, "createObjectURL", { configurable: true, value: createObjectUrl });
      const revokeObjectUrl = vi.fn();
      Object.defineProperty(URL, "revokeObjectURL", { configurable: true, value: revokeObjectUrl });
      await user.click(screen.getByRole("button", { name: language === "en" ? "Download recovery codes" : "Wiederherstellungscodes herunterladen" }));
      expect(createObjectUrl).toHaveBeenCalledOnce();
      await user.click(screen.getByRole("button", { name: language === "en" ? "I have saved the codes" : "Ich habe die Codes gespeichert" }));

      const proof = screen.getByLabelText(language === "en" ? "Authentication code" : "Authentifizierungscode");
      await user.type(proof, "654321");
      await user.click(screen.getByRole("button", { name: language === "en" ? "Generate new recovery codes" : "Neue Wiederherstellungscodes erstellen" }));
      await screen.findByRole("textbox", { name: language === "en" ? "Recovery codes" : "Wiederherstellungscodes" });
      expect(calls.find((call) => call.path === "/auth/2fa/recovery-codes")?.body).toEqual({ code: "654321" });
      await expectNoAxeViolations();

      await user.click(screen.getByRole("button", { name: language === "en" ? "I have saved the codes" : "Ich habe die Codes gespeichert" }));
      await user.type(screen.getByLabelText(language === "en" ? "Authentication code" : "Authentifizierungscode"), "111222");
      await user.type(screen.getByLabelText(language === "en" ? "Current password" : "Aktuelles Passwort"), "correct horse battery staple");
      await user.click(screen.getByRole("button", { name: language === "en" ? "Disable two-factor authentication" : "Zwei-Faktor-Anmeldung deaktivieren" }));
      await waitFor(() => expect(calls.some((call) => call.path === "/auth/2fa/disable")).toBe(true));
      expect(calls.find((call) => call.path === "/auth/2fa/disable")?.body).toEqual({
        password: "correct horse battery staple",
        code: "111222",
      });
      const checkbox = screen.getByRole("checkbox", { name: language === "en" ? "Make two-factor authentication mandatory" : "Zwei-Faktor-Anmeldung verpflichtend machen" });
      await user.click(checkbox);
      await waitFor(() => expect(calls.some((call) => call.path === "/organizations/" + ORG_A.id && call.headers["if-match"] === '"org-v1"')).toBe(true));
      await expectNoAxeViolations();
    });

    it("renders server key unavailability and keeps setup disabled", async () => {
      const { fetch } = mockFetch(loggedInRoutes({
        "GET /auth/2fa": { body: { enabled: false, available: false, recovery_codes_remaining: 0 } },
      }));
      const user = userEvent.setup();
      renderApp(fetch, { language });
      const nav = await screen.findByRole("navigation");
      await user.click(await within(nav).findByRole("button", { name: navigation }));
      const message = language === "en"
        ? "Two-factor authentication is unavailable because the server secret key is not configured."
        : "Die Zwei-Faktor-Anmeldung ist nicht verfügbar, weil der Serverschlüssel nicht eingerichtet ist.";
      await screen.findByText(message);
      await expectNoAxeViolations();
      expect(screen.queryByRole("button", { name: /set up two-factor|zwei-faktor-anmeldung einrichten/i })).toBeNull();
    });
  },
);

describe.each([["en"], ["de"]] as const)("second-factor login (%s)", (language) => {
  it("finishes a password challenge with a recovery code using keyboard interaction", async () => {
    let authenticated = false;
    const routes: Record<string, Handler> = {
      "GET /setup/status": { body: { setup_required: false } },
      "GET /auth/session": () => authenticated ? { body: SESSION } : apiError(401, "UNAUTHENTICATED"),
      "POST /auth/login": apiError(401, "SECOND_FACTOR_REQUIRED", { details: { challenge_id: "00000000-0000-4000-8000-0000000000f2" } }),
      "POST /auth/login/second-factor": (call) => {
        authenticated = true;
        expect(call.body).toEqual({
          challenge_id: "00000000-0000-4000-8000-0000000000f2",
          recovery_code: "ABCD234567",
        });
        return { body: { user_id: SESSION.user_id, csrf_token: "after-2fa", expires_at: SESSION.expires_at } };
      },
      "GET /organizations": { body: { items: [], next_cursor: null } },
    };
    const { fetch, calls } = mockFetch(routes);
    const user = userEvent.setup();
    renderApp(fetch, { language });
    await screen.findByRole("heading", { name: language === "en" ? "Log in" : "Anmelden" });
    await user.type(screen.getByLabelText(language === "en" ? "E-mail address" : "E-Mail-Adresse"), "ada@example.test");
    await user.type(screen.getByLabelText(language === "en" ? "Password" : "Passwort"), "password");
    await user.click(screen.getByRole("button", { name: language === "en" ? "Log in" : "Anmelden" }));
    await screen.findByLabelText(language === "en" ? "Authentication code" : "Authentifizierungscode");
    await expectNoAxeViolations();
    await user.click(screen.getByRole("button", { name: language === "en" ? "Use a recovery code" : "Wiederherstellungscode verwenden" }));
    await user.type(screen.getByLabelText(language === "en" ? "Recovery code" : "Wiederherstellungscode"), "ABCD234567");
    await user.keyboard("{Enter}");
    await waitFor(() => expect(calls.some((call) => call.path === "/auth/login/second-factor")).toBe(true));
    await expectNoAxeViolations();
  });

  it("keeps the challenge alive and explains an incorrect second factor", async () => {
    let authenticated = false;
    let attempts = 0;
    const routes: Record<string, Handler> = {
      "GET /setup/status": { body: { setup_required: false } },
      "GET /auth/session": () => authenticated ? { body: SESSION } : apiError(401, "UNAUTHENTICATED"),
      "POST /auth/login": apiError(401, "SECOND_FACTOR_REQUIRED", { details: { challenge_id: "00000000-0000-4000-8000-0000000000f2" } }),
      "POST /auth/login/second-factor": () => {
        attempts += 1;
        if (attempts === 1) return apiError(401, "INVALID_CREDENTIALS");
        authenticated = true;
        return { body: { user_id: SESSION.user_id, csrf_token: "after-2fa", expires_at: SESSION.expires_at } };
      },
      "GET /organizations": { body: { items: [], next_cursor: null } },
    };
    const { fetch, calls } = mockFetch(routes);
    const user = userEvent.setup();
    renderApp(fetch, { language });
    await screen.findByRole("heading", { name: language === "en" ? "Log in" : "Anmelden" });
    await user.type(screen.getByLabelText(language === "en" ? "E-mail address" : "E-Mail-Adresse"), "ada@example.test");
    await user.type(screen.getByLabelText(language === "en" ? "Password" : "Passwort"), "password");
    await user.click(screen.getByRole("button", { name: language === "en" ? "Log in" : "Anmelden" }));
    const code = await screen.findByLabelText(language === "en" ? "Authentication code" : "Authentifizierungscode");
    await user.type(code, "111111");
    await user.click(screen.getByRole("button", { name: language === "en" ? "Verify and log in" : "Prüfen und anmelden" }));
    const message = language === "en"
      ? "The authentication code or recovery code is incorrect."
      : "Der Authentifizierungscode oder Wiederherstellungscode ist falsch.";
    await screen.findByText(message);
    expect(screen.getByLabelText(language === "en" ? "Authentication code" : "Authentifizierungscode")).toBeTruthy();
    await user.type(screen.getByLabelText(language === "en" ? "Authentication code" : "Authentifizierungscode"), "222222");
    await user.click(screen.getByRole("button", { name: language === "en" ? "Verify and log in" : "Prüfen und anmelden" }));
    await waitFor(() => expect(calls.filter((call) => call.path === "/auth/login/second-factor")).toHaveLength(2));
  });
});



it("releases and reapplies an organization setup gate after factor changes", async () => {
  let enabled = false;
  const requiredOrg = { ...ORG_A, require_two_factor: true };
  const { fetch } = mockFetch(loggedInRoutes({
    "GET /organizations": { body: { items: [requiredOrg], next_cursor: null } },
    ["GET /organizations/" + ORG_A.id + "/workspaces"]: apiError(403, "TWO_FACTOR_SETUP_REQUIRED"),
    "GET /auth/2fa": () => ({ body: { enabled, available: true, recovery_codes_remaining: enabled ? 10 : 0 } }),
    "POST /auth/2fa/setup": { body: ENROLLMENT },
    "POST /auth/2fa/confirm": () => { enabled = true; return { body: RECOVERY }; },
    "POST /auth/2fa/disable": () => { enabled = false; return { status: 204 }; },
  }));
  const user = userEvent.setup();
  renderApp(fetch);
  const nav = await screen.findByRole("navigation");
  expect(await within(nav).findByRole("button", { name: "Settings" })).toBeTruthy();
  await screen.findByText("This organization requires two-factor authentication. Set it up to continue here; other organizations remain available.");
  await user.click(within(nav).getByRole("button", { name: "Settings" }));
  await user.type(screen.getByLabelText("Current password"), "fixture-password");
  await user.click(screen.getByRole("button", { name: "Set up two-factor authentication" }));
  await screen.findByRole("img", { name: "Authenticator setup QR code" });
  await user.type(screen.getByLabelText("Authentication code"), "123456");
  await user.click(screen.getByRole("button", { name: "Confirm and show recovery codes" }));
  await waitFor(() => expect(screen.queryByText("This organization requires two-factor authentication. Set it up to continue here; other organizations remain available.")).toBeNull());
  expect(within(nav).getByRole("button", { name: "Projects" })).toBeTruthy();
  await user.click(screen.getByRole("button", { name: "I have saved the codes" }));
  await user.type(screen.getByLabelText("Authentication code"), "654321");
  await user.type(screen.getByLabelText("Current password"), "fixture-password");
  await user.click(screen.getByRole("button", { name: "Disable two-factor authentication" }));
  await waitFor(() => expect(screen.getByText("This organization requires two-factor authentication. Set it up to continue here; other organizations remain available.")).toBeTruthy());
  expect(within(nav).queryByRole("button", { name: "Projects" })).toBeNull();
});

it("clears the setup password when the server rejects enrollment", async () => {
  const { fetch } = mockFetch(loggedInRoutes({
    "GET /auth/2fa": { body: { enabled: false, available: true, recovery_codes_remaining: 0 } },
    "POST /auth/2fa/setup": apiError(503, "TWO_FACTOR_UNAVAILABLE"),
  }));
  const user = userEvent.setup();
  renderApp(fetch);
  await user.click(await screen.findByRole("button", { name: "Settings" }));
  const password = screen.getByLabelText("Current password") as HTMLInputElement;
  await user.type(password, "fixture-password");
  await user.click(screen.getByRole("button", { name: "Set up two-factor authentication" }));
  await screen.findByText("The service is temporarily unavailable. Try again later. Reference: req-1");
  expect(password.value).toBe("");
});

it("clears the disable password and proof when the server rejects deactivation", async () => {
  const { fetch } = mockFetch(loggedInRoutes({
    "GET /auth/2fa": { body: { enabled: true, available: true, recovery_codes_remaining: 5 } },
    "POST /auth/2fa/disable": apiError(503, "SERVER_ERROR"),
  }));
  const user = userEvent.setup();
  renderApp(fetch);
  await user.click(await screen.findByRole("button", { name: "Settings" }));
  const proof = screen.getByLabelText("Authentication code") as HTMLInputElement;
  const password = screen.getByLabelText("Current password") as HTMLInputElement;
  await user.type(proof, "123456");
  await user.type(password, "fixture-password");
  await user.click(screen.getByRole("button", { name: "Disable two-factor authentication" }));
  await screen.findByText("The service is temporarily unavailable. Try again later. Reference: req-1");
  expect(password.value).toBe("");
  expect(proof.value).toBe("");
});

it("retries a failed organization policy enable with the original true target", async () => {
  let attempts = 0;
  let policyRequired = false;
  const { fetch, calls } = mockFetch(loggedInRoutes({
    "GET /organizations": () => ({ body: { items: [{ ...ORG_A, require_two_factor: policyRequired }], next_cursor: null } }),
    "GET /auth/2fa": { body: { enabled: true, available: true, recovery_codes_remaining: 8 } },
    ["PATCH /organizations/" + ORG_A.id]: (call) => {
      attempts += 1;
      expect(call.body).toEqual({ require_two_factor: true });
      if (attempts === 1) return apiError(503, "SERVER_ERROR");
      policyRequired = true;
      return { body: { ...ORG_A, version: 2, require_two_factor: true } };
    },
  }));
  const user = userEvent.setup();
  renderApp(fetch);
  await user.click(await screen.findByRole("button", { name: "Settings" }));
  const policy = await screen.findByRole("checkbox", { name: "Make two-factor authentication mandatory" });
  await user.click(policy);
  await waitFor(() => expect(attempts).toBe(1));
  expect((policy as HTMLInputElement).checked).toBe(false);
  const policySection = screen.getByRole("region", { name: "Organization policy" });
  await user.click(within(policySection).getByRole("button", { name: "Try again" }));
  await waitFor(() => expect(attempts).toBe(2));
  await waitFor(() => expect((screen.getByRole("checkbox", { name: "Make two-factor authentication mandatory" }) as HTMLInputElement).checked).toBe(true));
  expect(calls.filter((call) => call.path === "/organizations/" + ORG_A.id).map((call) => call.body))
    .toEqual([{ require_two_factor: true }, { require_two_factor: true }]);
});
