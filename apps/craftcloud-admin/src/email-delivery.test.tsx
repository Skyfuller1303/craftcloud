import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { ReactNode } from "react";
import { takeEmailToken } from "./App.tsx";
import { AuthProvider } from "./auth.tsx";
import { createApiClient } from "./api/client.ts";
import type { Organization, SessionInfo } from "./api/types.ts";
import { AnnouncerProvider } from "./components/Announcer.tsx";
import { I18nProvider } from "./i18n/index.tsx";
import { ProfileSection } from "./screens/ProfileSection.tsx";
import { InvitationsScreen } from "./screens/InvitationsScreen.tsx";
import { EmailConfirmationScreen, VerificationNotice } from "./screens/EmailAccount.tsx";
import { EmailSystem } from "./screens/EmailSystem.tsx";
import { expectNoAxeViolations, loggedInRoutes, mockFetch, ORG_A, SESSION } from "./test/helpers.tsx";
import type { Handler } from "./test/helpers.tsx";

function mount(children: ReactNode, language: string, enabled: boolean, overrides: Record<string, Handler> = {}) {
  const network = mockFetch(loggedInRoutes({
    "GET /info": { body: { features: [], feature_flags: { email_delivery: enabled } } },
    [`GET /organizations/${ORG_A.id}/invitations`]: { body: { items: [], next_cursor: null } },
    ...overrides,
  }));
  render(<I18nProvider initial={language}><AnnouncerProvider><AuthProvider createClient={(onUnauthenticated) => createApiClient({ fetchImpl: network.fetch, onUnauthenticated })}>
    <main><h1>CraftCloud</h1>{children}</main>
  </AuthProvider></AnnouncerProvider></I18nProvider>);
  return network;
}

describe.each(["en", "de"])("email delivery (%s)", (language) => {
  const de = language === "de";
  it("offers password-and-factor email change only with delivery and clears secrets", async () => {
    const network = mount(<ProfileSection session={SESSION as SessionInfo} />, language, true, {
      "POST /auth/email-change": { status: 202, body: { message: "accepted" } },
    });
    const user = userEvent.setup();
    await user.type(await screen.findByLabelText(de ? "Neue E-Mail-Adresse" : "New e-mail address"), "new@example.test");
    const password = screen.getByLabelText(de ? "Aktuelles Passwort" : "Current password");
    await user.type(password, "Synthetic-password-123");
    await user.type(screen.getByLabelText(de ? "Authenticator-Code" : "Authenticator code"), "123456");
    await expectNoAxeViolations();
    await user.click(screen.getByRole("button", { name: de ? "Bestätigungslink senden" : "Send confirmation link" }));
    await waitFor(() => expect(network.calls.find((call) => call.path === "/auth/email-change")?.body).toEqual({ email: "new@example.test", password: "Synthetic-password-123", code: "123456" }));
    await waitFor(() => expect((password as HTMLInputElement).value).toBe(""));
    expect(localStorage.getItem("password")).toBeNull();
  });

  it("keeps the address read-only when delivery is disabled", async () => {
    const network = mount(<ProfileSection session={SESSION as SessionInfo} />, language, false);
    await waitFor(() => expect(network.calls.some((call) => call.path === "/info")).toBe(true));
    expect(screen.queryByLabelText(de ? "Neue E-Mail-Adresse" : "New e-mail address")).toBeNull();
    await expectNoAxeViolations();
  });

  it.each([false, true])("invitation creation respects delivery=%s and has no secret storage", async (enabled) => {
    const network = mount(<InvitationsScreen scope={{ userId: SESSION.user_id, organization: ORG_A as Organization, workspace: null }} workspaces={[]} />, language, enabled, {
      [`POST /organizations/${ORG_A.id}/invitations`]: { status: 201, body: { invitation_id: "invite-one", expires_at: "2026-11-03T12:00:00Z", ...(enabled ? { sent_to: "guest@example.test" } : { invite_url: "https://cloud.example.test/invite#token=synthetic-invitation" }) } },
    });
    const user = userEvent.setup();
    await user.type(screen.getByLabelText(de ? "E-Mail-Adresse" : "E-mail address"), "guest@example.test");
    await user.click(screen.getByRole("button", { name: de ? "Einladung erstellen" : "Create invitation" }));
    if (enabled) {
      await screen.findByText(de ? "Einladung an guest@example.test gesendet." : "Invitation sent to guest@example.test.");
      expect(screen.queryByLabelText("Link")).toBeNull();
    } else {
      await screen.findByDisplayValue("https://cloud.example.test/invite#token=synthetic-invitation");
    }
    expect(network.calls.find((call) => call.method === "POST")?.body).toMatchObject({ language });
    expect(JSON.stringify(localStorage)).not.toContain("synthetic-invitation");
    expect(JSON.stringify(sessionStorage)).not.toContain("synthetic-invitation");
    await expectNoAxeViolations();
  });

  it("requests self verification with a resend cooldown", async () => {
    const network = mount(<VerificationNotice />, language, true, { "POST /auth/email-verification": { status: 202, body: {} } });
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: de ? "Bestätigungslink senden" : "Send confirmation link" }));
    const button = await screen.findByRole("button", { name: de ? "Erneut senden in 60 s" : "Send again in 60 s" });
    expect(button.hasAttribute("disabled")).toBe(true);
    expect(network.calls.filter((call) => call.path === "/auth/email-verification")).toHaveLength(1);
    await expectNoAxeViolations();
  });

  it("shows mail status and sends a test without a recipient parameter", async () => {
    const network = mount(<EmailSystem revision={0} />, language, true, {
      "GET /admin/email-delivery": { body: { configured: true, last_success_at: null, failed_count: 2, suppressed_count: 3 } },
      "POST /admin/email-delivery/test": { status: 202, body: {} },
    });
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: de ? "Testmail senden" : "Send test email" }));
    await waitFor(() => expect(network.calls.find((call) => call.path === "/admin/email-delivery/test")?.body).toEqual({}));
    await expectNoAxeViolations();
  });
});

it.each(["verify", "change"] as const)("removes %s email capability before use and waits for explicit confirmation", async (kind) => {
  const replaceState = vi.fn();
  expect(takeEmailToken({ pathname: `/admin/${kind}-email`, hash: "#token=synthetic-capability" }, { replaceState }, "/admin/")).toEqual({ token: "synthetic-capability", kind });
  expect(replaceState).toHaveBeenCalledWith(null, "", "/admin/");
  const onFinished = vi.fn();
  const path = kind === "verify" ? "/auth/email-verification/confirm" : "/auth/email-change/confirm";
  const network = mount(<EmailConfirmationScreen kind={kind} token="synthetic-capability" onFinished={onFinished} />, "en", true, { [`POST ${path}`]: { status: 204 } });
  expect(network.calls.some((call) => call.method === "POST")).toBe(false);
  await userEvent.setup().click(screen.getByRole("button", { name: "Confirm e-mail address" }));
  await waitFor(() => expect(onFinished).toHaveBeenCalledOnce());
  expect(network.calls.find((call) => call.path === path)?.body).toEqual({ token: "synthetic-capability" });
});

it("continues operator verification from the server cursor and respects its cooldown", async () => {
  const network = mount(<EmailSystem revision={0} />, "en", true, {
    "GET /admin/email-delivery": { body: { configured: true, last_success_at: null, failed_count: 0, suppressed_count: 0 } },
    "POST /admin/users/email-verification": (call) => ({ status: 202, body: call.query.has("after")
      ? { queued: 0, next_cursor: "last-user", limited: true, retry_after_seconds: 3600 }
      : { queued: 30, next_cursor: "last-user", limited: false, retry_after_seconds: null } }),
  });
  const user = userEvent.setup();
  await user.click(await screen.findByRole("button", { name: "Verify all unverified accounts…" }));
  await user.click(screen.getByRole("button", { name: "Send confirmation links" }));
  await screen.findByText("Confirmation emails queued: 30.");
  await user.click(screen.getByRole("button", { name: "Continue sending confirmation links…" }));
  await user.click(screen.getByRole("button", { name: "Send confirmation links" }));
  await screen.findByText("Send again in 3600 s");
  expect(screen.getByRole("button", { name: "Continue sending confirmation links…" }).hasAttribute("disabled")).toBe(true);
  expect(network.calls.filter((call) => call.method === "POST").map((call) => call.query.get("after"))).toEqual([null, "last-user"]);
});
