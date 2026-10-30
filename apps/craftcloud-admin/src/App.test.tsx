import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { takeInviteToken } from "./App.tsx";
import {
  ORG_A,
  ORG_B,
  SESSION,
  WS_1,
  WS_2,
  apiError,
  expectNoAxeViolations,
  loggedInRoutes,
  mockFetch,
  renderApp,
} from "./test/helpers.tsx";

const LOGGED_OUT = {
  "GET /setup/status": { body: { setup_required: false } },
  "GET /auth/session": apiError(401, "UNAUTHENTICATED"),
};

async function fillLogin(user: ReturnType<typeof userEvent.setup>, email = "ada@example.test", password = "correct horse battery") {
  await user.type(await screen.findByLabelText("E-mail address"), email);
  await user.type(screen.getByLabelText("Password"), password);
  await user.click(screen.getByRole("button", { name: "Log in" }));
}

describe("startup", () => {
  it("shows a loading state and then the login form", async () => {
    const { fetch } = mockFetch(LOGGED_OUT);
    renderApp(fetch);
    expect(screen.getByRole("status").textContent).toBe("Loading…");
    expect(await screen.findByRole("heading", { level: 1, name: "Log in" })).toBeTruthy();
  });

  it("shows the setup form while setup is required", async () => {
    const { fetch } = mockFetch({ "GET /setup/status": { body: { setup_required: true } } });
    renderApp(fetch);
    expect(await screen.findByRole("heading", { level: 1, name: "Set up CraftCloud" })).toBeTruthy();
  });

  it("offers a retry when the server is unavailable", async () => {
    let attempts = 0;
    const { fetch } = mockFetch({
      "GET /setup/status": () => {
        attempts += 1;
        return attempts === 1 ? apiError(503, "SETUP_UNAVAILABLE") : { body: { setup_required: true } };
      },
    });
    const user = userEvent.setup();
    renderApp(fetch);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("temporarily unavailable");
    expect(alert.textContent).toContain("Reference: req-1");
    expect(alert.textContent).not.toContain("server text for");
    await user.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("heading", { name: "Set up CraftCloud" })).toBeTruthy();
  });

  it("reports an unreachable server", async () => {
    const offline = (async () => {
      throw new TypeError("Failed to fetch");
    }) as typeof fetch;
    renderApp(offline);
    expect((await screen.findByRole("alert")).textContent).toContain("cannot be reached");
  });
});

describe("login", () => {
  it("logs in and stores no secret in browser storage", async () => {
    const { fetch, calls } = mockFetch({
      ...loggedInRoutes(),
      "GET /auth/session": (() => {
        let first = true;
        return () => {
          if (first) {
            first = false;
            return apiError(401, "UNAUTHENTICATED");
          }
          return { body: SESSION };
        };
      })(),
      "POST /auth/login": { body: { user_id: SESSION.user_id, csrf_token: "csrf-login", expires_at: "x" } },
    });
    const user = userEvent.setup();
    renderApp(fetch);
    await fillLogin(user);
    expect(await screen.findAllByText("Ada Example")).toHaveLength(2);
    const overview = await screen.findAllByRole("definition");
    expect(overview[0]?.textContent).toContain("ada@example.test");
    const login = calls.find((c) => c.path === "/auth/login");
    expect(login?.body).toEqual({ email: "ada@example.test", password: "correct horse battery" });
    expect(JSON.stringify({ ...window.localStorage })).not.toMatch(/csrf|correct horse/);
    expect(JSON.stringify({ ...window.sessionStorage })).not.toMatch(/csrf|correct horse/);
  });

  it("validates empty fields locally and moves focus to the first invalid one", async () => {
    const { fetch, calls } = mockFetch(LOGGED_OUT);
    const user = userEvent.setup();
    renderApp(fetch);
    await user.click(await screen.findByRole("button", { name: "Log in" }));
    expect(await screen.findByText("Enter your e-mail address.")).toBeTruthy();
    expect(screen.getByText("Enter your password.")).toBeTruthy();
    await waitFor(() => expect(document.activeElement).toBe(screen.getByLabelText("E-mail address")));
    expect(calls.some((c) => c.path === "/auth/login")).toBe(false);
  });

  it("shows a neutral message for 401 and clears the password", async () => {
    const { fetch } = mockFetch({ ...LOGGED_OUT, "POST /auth/login": apiError(401, "INVALID_CREDENTIALS") });
    const user = userEvent.setup();
    renderApp(fetch);
    await fillLogin(user);
    expect((await screen.findByRole("alert")).textContent).toBe("E-mail address or password is incorrect.");
    expect((screen.getByLabelText("Password") as HTMLInputElement).value).toBe("");
  });

  it("explains 429 with the Retry-After time", async () => {
    const { fetch } = mockFetch({
      ...LOGGED_OUT,
      "POST /auth/login": apiError(429, "RATE_LIMITED", { headers: { "Retry-After": "90" } }),
    });
    const user = userEvent.setup();
    renderApp(fetch);
    await fillLogin(user);
    expect((await screen.findByRole("alert")).textContent).toContain("Try again in 90 seconds.");
  });

  it("explains 403 CSRF_FAILED", async () => {
    const { fetch } = mockFetch({ ...LOGGED_OUT, "POST /auth/login": apiError(403, "CSRF_FAILED") });
    const user = userEvent.setup();
    renderApp(fetch);
    await fillLogin(user);
    expect((await screen.findByRole("alert")).textContent).toContain("rejected for security reasons");
  });

  it("explains 503 and keeps the form usable", async () => {
    const { fetch } = mockFetch({ ...LOGGED_OUT, "POST /auth/login": apiError(503, "AUTH_UNAVAILABLE") });
    const user = userEvent.setup();
    renderApp(fetch);
    await fillLogin(user);
    expect((await screen.findByRole("alert")).textContent).toContain("temporarily unavailable");
    expect((screen.getByRole("button", { name: "Log in" }) as HTMLButtonElement).disabled).toBe(false);
  });
});

describe("setup", () => {
  const SETUP_ROUTES = { "GET /setup/status": { body: { setup_required: true } } };

  async function fillSetup(user: ReturnType<typeof userEvent.setup>, token = "t".repeat(40)) {
    await user.type(await screen.findByLabelText("Setup token"), token);
    await user.type(screen.getByLabelText("E-mail address"), "root@example.test");
    await user.type(screen.getByLabelText("Your name"), "Root Operator");
    await user.type(screen.getByLabelText("Password"), "a long enough password");
    await user.type(screen.getByLabelText("Organization name"), "Äcme Studio 2");
    await user.click(screen.getByRole("button", { name: "Complete setup" }));
  }

  it("suggests a slug, completes setup and continues at the login form with a notice", async () => {
    let done = false;
    const { fetch, calls } = mockFetch({
      "GET /setup/status": () => ({ body: { setup_required: !done } }),
      "GET /auth/session": apiError(401, "UNAUTHENTICATED"),
      "POST /setup/complete": () => {
        done = true;
        return { status: 201, body: { user_id: "u", organization_id: "o" } };
      },
    });
    const user = userEvent.setup();
    renderApp(fetch);
    await fillSetup(user);
    expect(await screen.findByRole("heading", { name: "Log in" })).toBeTruthy();
    expect(screen.getByText("Setup is complete. Log in with the new operator account.")).toBeTruthy();
    expect(calls.find((c) => c.path === "/setup/complete")?.body).toEqual({
      bootstrap_token: "t".repeat(40),
      email: "root@example.test",
      display_name: "Root Operator",
      password: "a long enough password",
      organization: { name: "Äcme Studio 2", slug: "acme-studio-2" },
    });
  });

  it("validates locally without calling the server", async () => {
    const { fetch, calls } = mockFetch(SETUP_ROUTES);
    const user = userEvent.setup();
    renderApp(fetch);
    await fillSetup(user, "short");
    expect(await screen.findByText("The setup token has at least 32 characters.")).toBeTruthy();
    expect(calls.some((c) => c.path === "/setup/complete")).toBe(false);
  });

  it("marks a rejected bootstrap token (403) on the token field", async () => {
    const { fetch } = mockFetch({ ...SETUP_ROUTES, "POST /setup/complete": apiError(403, "INVALID_BOOTSTRAP_TOKEN") });
    const user = userEvent.setup();
    renderApp(fetch);
    await fillSetup(user);
    expect(await screen.findByText("The setup token is not accepted.")).toBeTruthy();
    expect(screen.getByLabelText("Setup token").getAttribute("aria-invalid")).toBe("true");
    expect((screen.getByLabelText("Password") as HTMLInputElement).value).toBe("");
  });

  it("maps server field errors (422) to fields", async () => {
    const { fetch } = mockFetch({
      ...SETUP_ROUTES,
      "POST /setup/complete": apiError(422, "VALIDATION_ERROR", { details: { fields: ["password", "organization.slug"] } }),
    });
    const user = userEvent.setup();
    renderApp(fetch);
    await fillSetup(user);
    await waitFor(() => expect(screen.getByLabelText("Password").getAttribute("aria-invalid")).toBe("true"));
    expect(screen.getByLabelText("Organization address name").getAttribute("aria-invalid")).toBe("true");
    expect(screen.getByLabelText("E-mail address").getAttribute("aria-invalid")).toBeNull();
    expect(screen.getByRole("alert").textContent).toContain("Some values are not valid");
  });

  it("tells the operator to re-check after a timeout (503 TIMEOUT)", async () => {
    const { fetch } = mockFetch({ ...SETUP_ROUTES, "POST /setup/complete": apiError(503, "TIMEOUT") });
    const user = userEvent.setup();
    renderApp(fetch);
    await fillSetup(user);
    expect((await screen.findByRole("alert")).textContent).toContain("Setup may have completed");
  });

  it("falls back to the login form when setup was completed meanwhile (409)", async () => {
    let calls = 0;
    const { fetch } = mockFetch({
      "GET /setup/status": () => ({ body: { setup_required: calls++ === 0 } }),
      "GET /auth/session": apiError(401, "UNAUTHENTICATED"),
      "POST /setup/complete": apiError(409, "SETUP_ALREADY_COMPLETED"),
    });
    const user = userEvent.setup();
    renderApp(fetch);
    await fillSetup(user);
    expect(await screen.findByRole("heading", { name: "Log in" })).toBeTruthy();
  });
});

describe("organization and workspace selectors", () => {
  it("selects the first organization and workspace and shows roles", async () => {
    const { fetch } = mockFetch(loggedInRoutes());
    renderApp(fetch);
    await waitFor(() => {
      const definitions = screen.getAllByRole("definition");
      expect(definitions[1]?.textContent).toContain(`${ORG_A.name}Owner`);
      expect(definitions[2]?.textContent).toContain(`${WS_1.name}Admin`);
    });
  });

  it("switches organization and workspace with the keyboard", async () => {
    const { fetch, calls } = mockFetch(loggedInRoutes());
    const user = userEvent.setup();
    renderApp(fetch);
    await waitFor(() => expect(screen.getAllByRole("definition")[2]?.textContent).toContain(`${WS_1.name}Admin`));

    const workspaceButton = screen.getByRole("button", { name: `${WS_1.name} Workspace` });
    workspaceButton.focus();
    await user.keyboard("{Enter}");
    const workspaceOption = await screen.findByRole("option", { name: WS_2.name });
    await user.click(workspaceOption);
    await waitFor(() => expect(screen.getAllByRole("definition")[2]?.textContent).toContain(`${WS_2.name}Viewer`));

    const orgButton = screen.getByRole("button", { name: `${ORG_A.name} Organization` });
    orgButton.focus();
    await user.keyboard("{Enter}");
    await user.click(await screen.findByRole("option", { name: ORG_B.name }));
    await waitFor(() => expect(screen.getAllByRole("definition")[1]?.textContent).toContain(`${ORG_B.name}Member`));
    expect(await screen.findByText("This organization has no workspace you can see.")).toBeTruthy();
    expect(calls.some((c) => c.path === `/organizations/${ORG_B.id}/workspaces`)).toBe(true);
  });

  it("shows an empty state without organizations", async () => {
    const { fetch } = mockFetch(loggedInRoutes({ "GET /organizations": { body: { items: [], next_cursor: null } } }));
    renderApp(fetch);
    expect(await screen.findByText(/not a member of any organization/)).toBeTruthy();
  });

  it("loads further pages on request", async () => {
    const { fetch, calls } = mockFetch(
      loggedInRoutes({
        "GET /organizations": (call) =>
          call.query.get("cursor") === null
            ? { body: { items: [ORG_A], next_cursor: "page2" } }
            : { body: { items: [ORG_B], next_cursor: null } },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await user.click(await screen.findByRole("button", { name: "Load more organizations" }));
    await waitFor(() => expect(calls.filter((c) => c.path === "/organizations").length).toBe(2));
    expect(calls.filter((c) => c.path === "/organizations")[1]?.query.get("cursor")).toBe("page2");
    await waitFor(() => expect(screen.queryByRole("button", { name: "Load more organizations" })).toBeNull());
  });

  it("returns to the login form with a notice when the session ends (401)", async () => {
    let expired = false;
    const { fetch } = mockFetch(
      loggedInRoutes({
        [`GET /organizations/${ORG_A.id}/workspaces`]: () => {
          expired = true;
          return apiError(401, "UNAUTHENTICATED");
        },
      }),
    );
    renderApp(fetch);
    expect(await screen.findByRole("heading", { name: "Log in" })).toBeTruthy();
    expect(expired).toBe(true);
    expect(screen.getByText("Your session has ended. Log in again.")).toBeTruthy();
  });

  it("explains 403 and 503 on the lists and offers a retry", async () => {
    const { fetch } = mockFetch(
      loggedInRoutes({ [`GET /organizations/${ORG_A.id}/workspaces`]: apiError(403, "FORBIDDEN") }),
    );
    renderApp(fetch);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("You do not have permission");
    // 403 means the data on screen may be stale: reload, not blind retry.
    expect(screen.getByRole("button", { name: "Reload" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
  });

  it("offers a plain retry for 503 on the lists", async () => {
    const { fetch } = mockFetch(
      loggedInRoutes({ [`GET /organizations/${ORG_A.id}/workspaces`]: apiError(503, "UNAVAILABLE") }),
    );
    renderApp(fetch);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("temporarily unavailable");
    expect(screen.getByRole("button", { name: "Try again" })).toBeTruthy();
  });

  it("logs out and sends the CSRF token", async () => {
    const { fetch, calls } = mockFetch(loggedInRoutes());
    const user = userEvent.setup();
    renderApp(fetch);
    await user.click(await screen.findByRole("button", { name: "Log out" }));
    expect(await screen.findByRole("heading", { name: "Log in" })).toBeTruthy();
    expect(calls.find((c) => c.path === "/auth/logout")?.headers["x-csrf-token"]).toBe(SESSION.csrf_token);
  });

  it("stays logged in, says so and retries when logout fails", async () => {
    let attempts = 0;
    const { fetch, calls } = mockFetch(
      loggedInRoutes({
        "POST /auth/logout": () => (++attempts === 1 ? apiError(503, "AUTH_UNAVAILABLE") : { status: 204 }),
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await user.click(await screen.findByRole("button", { name: "Log out" }));

    // No login form: the session is still alive, and the page says so.
    const alert = await screen.findByRole("alert");
    // One alert carries both the sentence and the reason, so that it is announced together.
    expect(alert.textContent).toContain("Logging out did not work. You are still logged in on this device.");
    expect(alert.textContent).toContain("temporarily unavailable");
    expect(screen.queryByRole("heading", { name: "Log in" })).toBeNull();
    expect(screen.getByRole("button", { name: "Log out" })).toBeTruthy();

    // The retry reuses the CSRF token and ends the session.
    await user.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("heading", { name: "Log in" })).toBeTruthy();
    const logouts = calls.filter((c) => c.path === "/auth/logout");
    expect(logouts.map((c) => c.headers["x-csrf-token"])).toEqual([SESSION.csrf_token, SESSION.csrf_token]);
  });

  it("shows the login form when logout answers 401 (the session was already gone)", async () => {
    const { fetch } = mockFetch(loggedInRoutes({ "POST /auth/logout": apiError(401, "UNAUTHENTICATED") }));
    const user = userEvent.setup();
    renderApp(fetch);
    await user.click(await screen.findByRole("button", { name: "Log out" }));
    expect(await screen.findByRole("heading", { name: "Log in" })).toBeTruthy();
  });

  it("navigates settings and marks the current section", async () => {
    const { fetch } = mockFetch(loggedInRoutes({
      "GET /auth/devices": { body: { devices: [] } },
    }));
    const user = userEvent.setup();
    renderApp(fetch);
    await user.click(await screen.findByRole("button", { name: "Settings" }));
    expect(screen.getByRole("heading", { level: 1, name: "Settings" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Settings" }).getAttribute("aria-current")).toBe("page");
    expect(await screen.findByRole("heading", { level: 2, name: "Devices" })).toBeTruthy();
    expect(await screen.findByText("There are no registered devices.")).toBeTruthy();
  });
});

describe("language and theme", () => {
  it("switches to German and back, and sets the theme attribute", async () => {
    const { fetch } = mockFetch(LOGGED_OUT);
    const user = userEvent.setup();
    renderApp(fetch);
    await screen.findByRole("heading", { name: "Log in" });
    await user.selectOptions(screen.getByLabelText("Language"), "de");
    expect(await screen.findByRole("heading", { name: "Anmelden" })).toBeTruthy();
    expect(document.documentElement.lang).toBe("de");
    await user.selectOptions(screen.getByLabelText("Darstellung"), "dark");
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");
    expect(window.localStorage.getItem("craftcloud.admin.theme")).toBe("dark");
    await user.selectOptions(screen.getByLabelText("Darstellung"), "system");
    expect(document.documentElement.hasAttribute("data-theme")).toBe(false);
  });

  it("translates role names", async () => {
    const { fetch } = mockFetch(loggedInRoutes());
    renderApp(fetch, { language: "de" });
    await waitFor(() => {
      const definitions = screen.getAllByRole("definition");
      expect(definitions[1]?.textContent).toContain(ORG_A.name);
      expect(definitions[1]?.textContent).toContain("Eigentümer");
      expect(definitions[2]?.textContent).toContain(WS_1.name);
      expect(definitions[2]?.textContent).toContain("Administrator");
    });
  });

  it("renders error messages in German", async () => {
    const { fetch } = mockFetch({ ...LOGGED_OUT, "POST /auth/login": apiError(429, "RATE_LIMITED", { headers: { "Retry-After": "5" } }) });
    const user = userEvent.setup();
    renderApp(fetch, { language: "de" });
    await user.type(await screen.findByLabelText("E-Mail-Adresse"), "a@example.test");
    await user.type(screen.getByLabelText("Passwort"), "pw");
    await user.click(screen.getByRole("button", { name: "Anmelden" }));
    expect((await screen.findByRole("alert")).textContent).toContain("in 5 Sekunden");
  });
});

describe("invitation", () => {
  const INVITE = "invite-token-value";

  it("creates an account for a new address and continues in the joined organization", async () => {
    let session = false;
    const { fetch, calls } = mockFetch(
      loggedInRoutes({
        "GET /auth/session": () => (session ? { body: SESSION } : apiError(401, "UNAUTHENTICATED")),
        "POST /invitations/accept": () => {
          session = true;
          return {
            body: {
              user_id: SESSION.user_id,
              organization_id: ORG_B.id,
              workspace_id: null,
              account_created: true,
              workspace_role_applied: false,
              csrf_token: "csrf-new",
              expires_at: "x",
            },
          };
        },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch, { inviteToken: INVITE });
    await user.type(await screen.findByLabelText("Your name"), "New Person");
    await user.type(screen.getByLabelText("Password"), "a long enough password");
    await user.click(screen.getByRole("button", { name: "Accept invitation" }));
    expect(await screen.findByText("Invitation accepted.")).toBeTruthy();
    const definitions = await screen.findAllByRole("definition");
    expect(definitions[1]?.textContent).toContain(`${ORG_B.name}Member`);
    expect(calls.find((c) => c.path === "/invitations/accept")?.body).toEqual({
      token: INVITE,
      display_name: "New Person",
      password: "a long enough password",
    });
    expect(JSON.stringify({ ...window.localStorage, ...window.sessionStorage })).not.toContain(INVITE);
  });

  it("asks existing accounts to log in first, then accepts with the session", async () => {
    let session = false;
    const { fetch, calls } = mockFetch(
      loggedInRoutes({
        "GET /auth/session": () => (session ? { body: SESSION } : apiError(401, "UNAUTHENTICATED")),
        "POST /auth/login": () => {
          session = true;
          return { body: { user_id: SESSION.user_id, csrf_token: "csrf-login", expires_at: "x" } };
        },
        "POST /invitations/accept": {
          body: { user_id: SESSION.user_id, organization_id: ORG_A.id, workspace_id: WS_1.id, account_created: false, workspace_role_applied: true },
        },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch, { inviteToken: INVITE });
    await user.click(await screen.findByRole("button", { name: "I already have an account" }));
    expect(await screen.findByText("Log in with the invited account to accept the invitation.")).toBeTruthy();
    await fillLogin(user);
    expect(await screen.findByText(/continue with your current account/)).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Accept invitation" }));
    expect(await screen.findByText("Invitation accepted.")).toBeTruthy();
    const accept = calls.find((c) => c.path === "/invitations/accept");
    expect(accept?.body).toEqual({ token: INVITE });
    expect(accept?.headers["x-csrf-token"]).toBe(SESSION.csrf_token); // refreshed from /auth/session after login
  });

  it.each([
    [400, "INVALID_INVITATION", "not valid"],
    [401, "LOGIN_REQUIRED", "Log in with it first"],
    [403, "INVITATION_EMAIL_MISMATCH", "different account"],
    [409, "ALREADY_MEMBER", "already a member"],
    [429, "RATE_LIMITED", "Too many attempts"],
  ])("explains %i %s", async (status, code, text) => {
    const { fetch } = mockFetch({ ...LOGGED_OUT, "POST /invitations/accept": apiError(status, code) });
    const user = userEvent.setup();
    renderApp(fetch, { inviteToken: INVITE });
    await user.type(await screen.findByLabelText("Your name"), "New Person");
    await user.type(screen.getByLabelText("Password"), "a long enough password");
    await user.click(screen.getByRole("button", { name: "Accept invitation" }));
    expect((await screen.findByRole("alert")).textContent).toContain(text);
  });

  it("reads the token from the fragment and removes it from the address", () => {
    const replaced: unknown[][] = [];
    const history = { replaceState: (...args: unknown[]) => void replaced.push(args) };
    expect(takeInviteToken({ pathname: "/admin/invite", hash: "#token=abc-_123" }, history, "/admin/")).toBe("abc-_123");
    expect(replaced).toEqual([[null, "", "/admin/"]]);
    expect(takeInviteToken({ pathname: "/admin/invite/", hash: "#x=1" }, history, "/admin/")).toBeNull();
    expect(takeInviteToken({ pathname: "/admin/invite", hash: "#token=" }, history, "/admin/")).toBeNull();
    expect(takeInviteToken({ pathname: "/admin/", hash: "#token=abc" }, history, "/admin/")).toBeNull();
    expect(replaced).toHaveLength(3); // never touched the URL of non-invite pages
  });
});

describe("accessibility (axe-core)", () => {
  it("login screen has no violations", async () => {
    const { fetch } = mockFetch(LOGGED_OUT);
    renderApp(fetch);
    await screen.findByRole("heading", { name: "Log in" });
    await expectNoAxeViolations();
  });

  it("login screen with errors has no violations", async () => {
    const { fetch } = mockFetch({ ...LOGGED_OUT, "POST /auth/login": apiError(401, "INVALID_CREDENTIALS") });
    const user = userEvent.setup();
    renderApp(fetch);
    await user.click(await screen.findByRole("button", { name: "Log in" }));
    await screen.findByText("Enter your password.");
    await expectNoAxeViolations();
  });

  it("setup screen has no violations", async () => {
    const { fetch } = mockFetch({ "GET /setup/status": { body: { setup_required: true } } });
    renderApp(fetch);
    await screen.findByRole("heading", { name: "Set up CraftCloud" });
    await expectNoAxeViolations();
  });

  it("invitation screens have no violations", async () => {
    const { fetch } = mockFetch(LOGGED_OUT);
    renderApp(fetch, { inviteToken: "t" });
    await screen.findByRole("heading", { name: "Accept invitation" });
    await expectNoAxeViolations();
  });

  it("shell with selectors has no violations, also in German and with an open list", async () => {
    const { fetch } = mockFetch(loggedInRoutes());
    const user = userEvent.setup();
    renderApp(fetch, { language: "de" });
    await waitFor(() => expect(screen.getAllByRole("definition")[2]?.textContent).toContain(`${WS_1.name}Administrator`));
    await expectNoAxeViolations();
    await user.click(screen.getByRole("button", { name: `${ORG_A.name} Organisation` }));
    await screen.findByRole("option", { name: ORG_B.name });
    await expectNoAxeViolations();
  });

  it("unavailable and empty states have no violations", async () => {
    const { fetch } = mockFetch({ "GET /setup/status": apiError(503, "SETUP_UNAVAILABLE") });
    renderApp(fetch);
    await screen.findByRole("alert");
    await expectNoAxeViolations();
  });
});

describe("keyboard focus order", () => {
  it("focuses the heading, then tabs through the form in reading order and wraps to the skip link", async () => {
    const { fetch } = mockFetch(LOGGED_OUT);
    const user = userEvent.setup();
    renderApp(fetch);
    const heading = await screen.findByRole("heading", { name: "Log in" });
    await waitFor(() => expect(document.activeElement).toBe(heading));
    const sequence: string[] = [];
    for (let i = 0; i < 7; i += 1) {
      await user.tab();
      const el = document.activeElement as HTMLElement;
      sequence.push(el instanceof HTMLInputElement ? el.name : el.tagName.toLowerCase());
    }
    // Content first (the heading sits after the header in the DOM), then the
    // wrap-around: skip link, language and theme switches.
    expect(sequence).toEqual(["email", "password", "button", "body", "a", "select", "select"]);
  });
});
