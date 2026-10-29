import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { takeResetToken } from "./App.tsx";
import { ApiError } from "./api/client.ts";
import { classifyError } from "./api/errors.ts";
import { AnnouncerProvider } from "./components/Announcer.tsx";
import { ErrorNotice } from "./components/ErrorNotice.tsx";
import { I18nProvider } from "./i18n/index.tsx";
import { ORG_A, WS_1, apiError, mockFetch, renderApp } from "./test/helpers.tsx";
import { BOB, CARA, PROJECT_1, viewRoutes } from "./test/fixtures.ts";

const failure = (status: number, code: string, extra: Partial<ConstructorParameters<typeof ApiError>[0]> = {}) =>
  new ApiError({ status, code, message: "server text", requestId: "req-9", ...extra });

function renderNotice(error: unknown, handlers: { onRetry?: () => void; onReload?: () => void } = {}) {
  return render(
    <I18nProvider initial="en">
      <AnnouncerProvider>
        <ErrorNotice error={error} {...handlers} />
      </AnnouncerProvider>
    </I18nProvider>,
  );
}

afterEach(() => {
  vi.useRealTimers();
});

describe("error classification", () => {
  it.each([
    [401, "UNAUTHENTICATED", "session"],
    [403, "FORBIDDEN", "forbidden"],
    [404, "NOT_FOUND", "missing"],
    [409, "ALREADY_MEMBER", "conflict"],
    [412, "VERSION_CONFLICT", "precondition"],
    [413, "QUOTA_EXCEEDED", "too-large"],
    [507, "INSUFFICIENT_STORAGE", "too-large"],
    [422, "VALIDATION_ERROR", "invalid"],
    [429, "RATE_LIMITED", "rate-limited"],
    [503, "UNAVAILABLE", "unavailable"],
    [500, "INTERNAL", "server"],
    [0, "NETWORK_ERROR", "offline"],
    [0, "ABORTED", "cancelled"],
  ])("%i %s is %s", (status, code, kind) => {
    expect(classifyError(failure(status, code))).toBe(kind);
  });
});

describe("ErrorNotice", () => {
  it("shows the request reference but never the server's English text", () => {
    renderNotice(failure(500, "INTERNAL"));
    const alert = screen.getByRole("alert");
    expect(alert.textContent).toContain("internal error");
    expect(alert.textContent).toContain("Reference: req-9");
    expect(alert.textContent).not.toContain("server text");
  });

  it("offers Reload (not a blind retry) for 403, 404, 409 and 412", () => {
    for (const [status, code] of [
      [403, "FORBIDDEN"],
      [404, "NOT_FOUND"],
      [409, "ALREADY_MEMBER"],
      [412, "VERSION_CONFLICT"],
    ] as const) {
      const onReload = vi.fn();
      const onRetry = vi.fn();
      const { unmount } = renderNotice(failure(status, code), { onReload, onRetry });
      expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
      screen.getByRole("button", { name: "Reload" }).click();
      expect(onReload).toHaveBeenCalledTimes(1);
      expect(onRetry).not.toHaveBeenCalled();
      unmount();
    }
  });

  it("offers nothing to repeat for input problems (413, 422)", () => {
    for (const [status, code] of [
      [413, "PAYLOAD_TOO_LARGE"],
      [422, "VALIDATION_ERROR"],
    ] as const) {
      const { unmount } = renderNotice(failure(status, code), { onRetry: vi.fn(), onReload: vi.fn() });
      expect(screen.queryByRole("button")).toBeNull();
      unmount();
    }
  });

  it("retries 503 and an unreachable server", async () => {
    for (const error of [failure(503, "UNAVAILABLE"), failure(0, "NETWORK_ERROR")]) {
      const onRetry = vi.fn();
      const { unmount } = renderNotice(error, { onRetry });
      await userEvent.setup().click(screen.getByRole("button", { name: "Try again" }));
      expect(onRetry).toHaveBeenCalledTimes(1);
      unmount();
    }
  });

  it("renders nothing for a cancellation by the user", () => {
    const { container } = renderNotice(failure(0, "ABORTED"));
    expect(container.querySelector(".error-notice")).toBeNull();
  });

  it("counts down Retry-After, keeps retry disabled until zero and announces the end", async () => {
    vi.useFakeTimers();
    const onRetry = vi.fn();
    renderNotice(failure(429, "RATE_LIMITED", { retryAfterSeconds: 3 }), { onRetry });
    const retry = () => screen.getByRole("button", { name: /Try again/ }) as HTMLButtonElement;
    expect(retry().textContent).toBe("Try again in 3 s");
    expect(retry().disabled).toBe(true);
    // The alert itself does not tick: it is not re-announced every second.
    const alertText = screen.getByRole("alert").textContent;
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2000);
    });
    expect(retry().textContent).toBe("Try again in 1 s");
    expect(screen.getByRole("alert").textContent).toBe(alertText);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1100);
    });
    expect(retry().textContent).toBe("Try again");
    expect(retry().disabled).toBe(false);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(100); // the live region fills after a short delay
    });
    expect(screen.getByTestId("announcer").textContent).toBe("You can try again now.");
    retry().click();
    expect(onRetry).toHaveBeenCalledTimes(1);
  });

  it("allows retry at once when 429 carries no Retry-After", () => {
    renderNotice(failure(429, "RATE_LIMITED"), { onRetry: vi.fn() });
    expect((screen.getByRole("button", { name: "Try again" }) as HTMLButtonElement).disabled).toBe(false);
  });
});

describe("401 during an action", () => {
  it("returns to the login form with the session notice and drops the dialog", async () => {
    const { fetch } = mockFetch(
      viewRoutes({ [`PATCH /organizations/${ORG_A.id}/members/${CARA}`]: apiError(401, "UNAUTHENTICATED") }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await user.click(await screen.findByRole("button", { name: "Members" }));
    await user.click(await screen.findByRole("button", { name: "Change role: Cara" }));
    const dialog = await screen.findByRole("dialog");
    await user.selectOptions(within(dialog).getByRole("combobox", { name: "Role" }), "admin");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    expect(await screen.findByRole("heading", { name: "Log in" })).toBeTruthy();
    expect(screen.getByText("Your session has ended. Log in again.")).toBeTruthy();
    expect(screen.queryByRole("dialog")).toBeNull();
  });
});

describe("429 and 503 on actions", () => {
  it("shows the Retry-After countdown inside the dialog", async () => {
    const { fetch } = mockFetch(
      viewRoutes({
        [`DELETE /organizations/${ORG_A.id}/members/${BOB}`]: apiError(429, "RATE_LIMITED", { headers: { "Retry-After": "30" } }),
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await user.click(await screen.findByRole("button", { name: "Members" }));
    await user.click(await screen.findByRole("button", { name: "Remove Bob" }));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Remove" }));
    const retry = await within(dialog).findByRole("button", { name: /Try again in \d+ s/ });
    expect((retry as HTMLButtonElement).disabled).toBe(true);
    expect(within(dialog).getByRole("alert").textContent).toContain("Wait a moment");
  });

  it("retries a 503 on a list", async () => {
    let attempts = 0;
    const { fetch } = mockFetch(
      viewRoutes({
        [`GET /workspaces/${WS_1.id}/projects`]: () => {
          attempts += 1;
          return attempts === 1 ? apiError(503, "UNAVAILABLE") : { body: { items: [PROJECT_1], next_cursor: null } };
        },
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await user.click(await screen.findByRole("button", { name: "Projects" }));
    await user.click(await screen.findByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("row", { name: /Brand kit/ })).toBeTruthy();
  });

  it("reports an unreachable server on an action and recovers", async () => {
    let up = false;
    const { fetch } = mockFetch(
      viewRoutes({
        [`POST /workspaces/${WS_1.id}/projects`]: () => {
          if (!up) {
            throw new TypeError("offline");
          }
          return { status: 201, body: { ...PROJECT_1, id: "p9", name: "Flyer" } };
        },
      }),
    );
    // mockFetch turns a throwing handler into a rejected fetch, which the client reports as NETWORK_ERROR.
    const user = userEvent.setup();
    renderApp(fetch);
    await user.click(await screen.findByRole("button", { name: "Projects" }));
    await user.type(await screen.findByLabelText("New project name"), "Flyer");
    await user.click(screen.getByRole("button", { name: "Create project" }));
    await waitFor(() => expect(screen.getAllByRole("alert").length).toBeGreaterThan(0));
    up = true;
    await user.click(screen.getByRole("button", { name: "Create project" }));
    await waitFor(() => expect(screen.getByTestId("announcer").textContent).toBe("Project Flyer created."));
  });
});

describe("password reset page", () => {
  const ANON = {
    "GET /setup/status": { body: { setup_required: false } },
    "GET /auth/session": apiError(401, "UNAUTHENTICATED"),
  };

  it("takes the token from the fragment and replaces the address before anything else", () => {
    const calls: string[] = [];
    const history = { replaceState: (_s: unknown, _t: string, url: string) => calls.push(url) };
    expect(takeResetToken({ pathname: "/admin/reset-password", hash: "#token=abc" }, history, "/admin/")).toBe("abc");
    expect(calls).toEqual(["/admin/"]);
    // Other pages and empty tokens yield nothing.
    expect(takeResetToken({ pathname: "/admin/", hash: "#token=abc" }, history, "/admin/")).toBeNull();
    expect(takeResetToken({ pathname: "/admin/reset-password/", hash: "" }, history, "/admin/")).toBeNull();
    expect(calls).toEqual(["/admin/", "/admin/"]);
  });

  async function fill(user: ReturnType<typeof userEvent.setup>, password = "a long enough password", repeat = password) {
    await user.type(await screen.findByLabelText("New password"), password);
    await user.type(screen.getByLabelText("Repeat the new password"), repeat);
    await user.click(screen.getByRole("button", { name: "Set password" }));
  }

  it("sets the password with the token in the body only, then leads to the login form", async () => {
    const { fetch, calls } = mockFetch({ ...ANON, "POST /auth/password-reset": { status: 204 } });
    const user = userEvent.setup();
    renderApp(fetch, { resetToken: "T".repeat(43) });
    await fill(user);
    expect(await screen.findByRole("heading", { name: "Password changed" })).toBeTruthy();
    const post = calls.find((c) => c.path === "/auth/password-reset");
    expect(post?.body).toEqual({ token: "T".repeat(43), new_password: "a long enough password" });
    expect(post?.query.toString()).toBe("");
    expect(JSON.stringify({ ...window.localStorage, ...window.sessionStorage })).not.toContain("TTTT");
    await user.click(screen.getByRole("button", { name: "Go to log in" }));
    expect(await screen.findByRole("heading", { name: "Log in" })).toBeTruthy();
    expect(screen.getByText("Your password was changed. Log in with the new password.")).toBeTruthy();
  });

  it("validates locally: length and repeat", async () => {
    const { fetch, calls } = mockFetch({ ...ANON });
    const user = userEvent.setup();
    renderApp(fetch, { resetToken: "tok" });
    await fill(user, "short", "other");
    expect(await screen.findByText("The password needs 12 to 128 characters.")).toBeTruthy();
    expect(screen.getByText("The two passwords are not the same.")).toBeTruthy();
    await waitFor(() => expect(document.activeElement).toBe(screen.getByLabelText("New password")));
    expect(calls.some((c) => c.method === "POST")).toBe(false);
  });

  it("says so when the link is invalid or used (400 INVALID_RESET_TOKEN)", async () => {
    const { fetch } = mockFetch({ ...ANON, "POST /auth/password-reset": apiError(400, "INVALID_RESET_TOKEN") });
    const user = userEvent.setup();
    renderApp(fetch, { resetToken: "tok" });
    await fill(user);
    expect((await screen.findByRole("alert")).textContent).toContain("not valid any more");
    expect(screen.queryByLabelText("New password")).toBeNull();
  });

  it("marks a rejected password (422) and keeps the form", async () => {
    const { fetch } = mockFetch({
      ...ANON,
      "POST /auth/password-reset": apiError(422, "VALIDATION_ERROR", { details: { fields: ["new_password"] } }),
    });
    const user = userEvent.setup();
    renderApp(fetch, { resetToken: "tok" });
    await fill(user);
    expect(await screen.findByText(/This password is not accepted/)).toBeTruthy();
    expect(screen.getByLabelText("New password").getAttribute("aria-invalid")).toBe("true");
  });

  it("explains 429 with the countdown and 503", async () => {
    const replies = [apiError(429, "RATE_LIMITED", { headers: { "Retry-After": "20" } }), apiError(503, "AUTH_UNAVAILABLE")];
    const { fetch } = mockFetch({ ...ANON, "POST /auth/password-reset": () => replies.shift() ?? { status: 204 } });
    const user = userEvent.setup();
    renderApp(fetch, { resetToken: "tok" });
    await fill(user);
    expect((await screen.findByRole("alert")).textContent).toContain("Wait a moment");
    await user.type(screen.getByLabelText("New password"), "a long enough password");
    await user.type(screen.getByLabelText("Repeat the new password"), "a long enough password");
    await user.click(screen.getByRole("button", { name: "Set password" }));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("temporarily unavailable"));
  });
});
