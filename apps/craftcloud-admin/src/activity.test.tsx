import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import {
  ORG_A,
  SESSION,
  WS_1,
  apiError,
  expectNoAxeViolations,
  mockFetch,
  renderApp,
} from "./test/helpers.tsx";
import { BOB, ME, page, viewRoutes } from "./test/fixtures.ts";

type User = ReturnType<typeof userEvent.setup>;

const AUDIT = `/organizations/${ORG_A.id}/audit`;

const event = (n: number, extra: Record<string, unknown> = {}) => ({
  id: `00000000-0000-4000-8000-00000000f${String(n).padStart(3, "0")}`,
  created_at: `2026-10-0${n}T10:00:00Z`,
  last_at: `2026-10-0${n}T10:00:00Z`,
  count: 1,
  org_id: ORG_A.id,
  actor_id: BOB,
  action: "member.add",
  target: { type: "user", id: ME },
  workspace_id: null,
  result: "success",
  request_id: `req-${n}`,
  details: { role: "member" },
  ...extra,
});

const PLAIN = { ...SESSION, instance_role: null };

async function openActivity(user: User) {
  await user.click(await screen.findByRole("button", { name: /^(Activity|Aktivit\u00e4t)$/ }));
  return screen.findByRole("heading", { level: 1, name: /^(Activity|Aktivit\u00e4t)$/ });
}

describe("activity log", () => {
  it("lists events with people named from the member list and pages on request", async () => {
    const { fetch, calls } = mockFetch(
      viewRoutes({
        [`GET ${AUDIT}`]: (call) =>
          call.query.get("cursor") === "c2"
            ? page([event(3, { actor_id: null, action: "file.purge", target: null })])
            : page(
                [
                  event(2),
                  event(1, {
                    count: 4,
                    last_at: "2026-10-09T10:00:00Z",
                    action: "workspace.create",
                    result: "denied",
                    workspace_id: WS_1.id,
                    details: {},
                  }),
                ],
                "c2",
              ),
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openActivity(user);
    const table = await screen.findByRole("table");
    // Bob is named from the member list; the second row is the aggregated denial.
    expect(await within(table).findAllByText("Bob")).toHaveLength(2);
    const rows = within(table).getAllByRole("row");
    expect(rows).toHaveLength(3);
    expect(within(rows[1]!).getByText("member.add")).toBeTruthy();
    expect(within(rows[1]!).getByText("role: member")).toBeTruthy();
    expect(within(rows[2]!).getByText("Denied")).toBeTruthy();
    expect(within(rows[2]!).getByText(/4 times, latest/)).toBeTruthy();
    expect(within(rows[2]!).getByText("Main workspace")).toBeTruthy();
    // The first request asks for a page and applies no filter.
    const first = calls.find((c) => c.path === AUDIT)!;
    expect(first.query.get("limit")).toBe("50");
    expect(first.query.get("action")).toBeNull();

    await user.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() => expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(4));
    expect(screen.getByText("System or unknown")).toBeTruthy();
    expect(calls.filter((c) => c.path === AUDIT).at(-1)?.query.get("cursor")).toBe("c2");
  });

  it("applies and resets filters on the server side and keeps the export link in step", async () => {
    const { fetch, calls } = mockFetch(viewRoutes({ [`GET ${AUDIT}`]: page([event(1)]) }));
    const user = userEvent.setup();
    renderApp(fetch);
    await openActivity(user);
    await screen.findByRole("table");
    const exportLink = () => screen.getByRole("link", { name: "Export as NDJSON" }).getAttribute("href");
    expect(exportLink()).toBe(`/api/v1${AUDIT}/export`);

    await user.selectOptions(screen.getByLabelText("Action"), "member");
    await user.selectOptions(screen.getByLabelText("Result"), "denied");
    await user.selectOptions(within(screen.getByRole("main")).getByLabelText("Workspace"), WS_1.id);
    await user.type(screen.getByLabelText("Actor (user ID)"), BOB);
    await user.type(screen.getByLabelText("From"), "2026-10-01");
    await user.type(screen.getByLabelText("To"), "2026-10-03");
    await user.click(screen.getByRole("button", { name: "Apply filters" }));

    await waitFor(() => expect(calls.filter((c) => c.path === AUDIT)).toHaveLength(2));
    const filtered = calls.filter((c) => c.path === AUDIT).at(-1)!;
    expect(Object.fromEntries(filtered.query)).toMatchObject({
      action: "member",
      result: "denied",
      workspace: WS_1.id,
      actor: BOB,
      from: "2026-10-01T00:00:00.000Z",
      // the whole last day is included: the end is the start of the next day
      to: "2026-10-04T00:00:00.000Z",
    });
    const href = new URL(exportLink()!, "http://localhost");
    expect(href.pathname).toBe(`/api/v1${AUDIT}/export`);
    expect(href.searchParams.get("action")).toBe("member");
    expect(href.searchParams.get("limit")).toBeNull();

    await user.click(screen.getByRole("button", { name: "Reset filters" }));
    await waitFor(() => expect(calls.filter((c) => c.path === AUDIT)).toHaveLength(3));
    expect(calls.filter((c) => c.path === AUDIT).at(-1)!.query.get("action")).toBeNull();
    expect(exportLink()).toBe(`/api/v1${AUDIT}/export`);
  });

  it("rejects bad filter input in the form before asking the server", async () => {
    const { fetch, calls } = mockFetch(viewRoutes({ [`GET ${AUDIT}`]: page([event(1)]) }));
    const user = userEvent.setup();
    renderApp(fetch);
    await openActivity(user);
    await screen.findByRole("table");
    await user.type(screen.getByLabelText("Actor (user ID)"), "not-an-id");
    await user.type(screen.getByLabelText("From"), "2026-10-05");
    await user.type(screen.getByLabelText("To"), "2026-10-01");
    await user.click(screen.getByRole("button", { name: "Apply filters" }));
    expect(await screen.findByText("Enter a valid user ID.")).toBeTruthy();
    expect(screen.getByText("The end date must not be before the start date.")).toBeTruthy();
    expect(calls.filter((c) => c.path === AUDIT)).toHaveLength(1);
  });

  it("does not offer the log to plain members and makes no request for it", async () => {
    const { fetch, calls } = mockFetch(
      viewRoutes({
        "GET /auth/session": { body: PLAIN },
        "GET /organizations": { body: { items: [{ ...ORG_A, role: "member" }], next_cursor: null } },
      }),
    );
    renderApp(fetch);
    const nav = await screen.findByRole("navigation");
    expect(within(nav).queryByRole("button", { name: /^(Activity|Aktivit\u00e4t)$/ })).toBeNull();
    expect(screen.queryByRole("heading", { level: 1, name: /^(Activity|Aktivit\u00e4t)$/ })).toBeNull();
    expect(screen.queryByRole("link", { name: "Export as NDJSON" })).toBeNull();
    expect(screen.queryByLabelText("Log")).toBeNull();
    expect(calls.some((c) => c.path.includes("/audit"))).toBe(false);
  });

  it("shows the server's refusal when the role changed meanwhile", async () => {
    const { fetch } = mockFetch(
      viewRoutes({ "GET /auth/session": { body: PLAIN }, [`GET ${AUDIT}`]: apiError(403, "FORBIDDEN") }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openActivity(user);
    expect(await screen.findByText(/You do not have permission to do this\./)).toBeTruthy();
    expect(screen.queryByRole("table")).toBeNull();
  });

  it("lets an operator switch to the instance log, which has no workspace column", async () => {
    const instanceEvent = event(1, { org_id: null, actor_id: null, action: "auth.login", result: "failed", count: 3, target: null, details: {} });
    const { fetch, calls } = mockFetch(
      viewRoutes({
        [`GET ${AUDIT}`]: page([event(1)]),
        "GET /admin/audit": page([instanceEvent]),
      }),
    );
    const user = userEvent.setup();
    renderApp(fetch);
    await openActivity(user);
    await screen.findByRole("table");
    expect(within(screen.getByRole("table")).getByText("Workspace")).toBeTruthy();
    await user.selectOptions(screen.getByLabelText("Log"), "instance");
    await within(await screen.findByRole("table")).findByText("auth.login");
    expect(calls.some((c) => c.path === "/admin/audit")).toBe(true);
    expect(within(screen.getByRole("table")).queryByText("Workspace")).toBeNull();
    expect(screen.getByRole("link", { name: "Export as NDJSON" }).getAttribute("href")).toBe("/api/v1/admin/audit/export");
    expect(within(screen.getByRole("table")).getByText("Failed")).toBeTruthy();
    expect(within(screen.getByLabelText("Action")).getByRole("option", { name: "Sign-in and sessions" })).toBeTruthy();
  });

  it("falls back to the instance log for an operator who is no organization manager", async () => {
    const { fetch, calls } = mockFetch(
      viewRoutes({
        "GET /organizations": { body: { items: [{ ...ORG_A, role: "member" }], next_cursor: null } },
        "GET /admin/audit": page([]),
      }),
    );
    window.history.replaceState(null, "", "/admin/activity");
    renderApp(fetch);
    expect(await screen.findByText("There are no matching events.")).toBeTruthy();
    expect(calls.some((c) => c.path === AUDIT)).toBe(false);
    expect(calls.some((c) => c.path === "/admin/audit")).toBe(true);
  });

  it.each([
    ["without an organization", "system"],
    ["without an organization", "legacy"],
    ["as an organization guest with required 2FA", "system"],
    ["as an organization guest with required 2FA", "legacy"],
  ] as const)("opens only instance audit for an operator %s via %s", async (context, entry) => {
    const organization = context.startsWith("as an") ? [{ ...ORG_A, role: "guest", require_two_factor: true }] : [];
    const { fetch, calls } = mockFetch(viewRoutes({
      "GET /organizations": { body: { items: organization, next_cursor: null } },
      "GET /auth/2fa": { body: { enabled: false, available: true, recovery_codes_remaining: 0 } },
      "GET /admin/audit": page([]),
    }));
    window.history.replaceState(null, "", entry === "system" ? "/admin/system" : "/admin/activity");
    const user = userEvent.setup();
    renderApp(fetch);
    if (entry === "system") {
      await user.click(await screen.findByRole("button", { name: "Open activity log" }));
    }
    await screen.findByRole("heading", { level: 1, name: "Activity" });
    expect(await screen.findByText("There are no matching events.")).toBeTruthy();
    expect(calls.some((call) => call.path === "/admin/audit")).toBe(true);
    expect(calls.some((call) => call.path === AUDIT)).toBe(false);
  });
});

const languages = [
  ["en", "English"],
  ["de", "German"],
] as const;

describe.each(languages)("axe: activity log (%s)", (language) => {
  it("has no violations with events, filters and errors", async () => {
    const { fetch } = mockFetch(
      viewRoutes({ [`GET ${AUDIT}`]: page([event(2), event(1, { count: 2, result: "denied", actor_id: null })], "c2") }),
    );
    const user = userEvent.setup();
    renderApp(fetch, { language });
    const nav = await screen.findByRole("navigation");
      await user.click(await within(nav).findByRole("button", { name: /^(Activity|Aktivit\u00e4t)$/ }));
    await screen.findByRole("table");
    await expectNoAxeViolations();
    // Validation errors are exposed to assistive technology too.
    const actor = document.querySelector<HTMLInputElement>('input[name="audit-actor"]')!;
    await user.type(actor, "nope");
    await user.click(document.querySelector<HTMLButtonElement>('button[type="submit"]')!);
    await waitFor(() => expect(actor.getAttribute("aria-invalid")).toBe("true"));
    await expectNoAxeViolations();
  });

  it("has no violations in the error and plain-member states", async () => {
    const { fetch } = mockFetch(
      viewRoutes({ "GET /auth/session": { body: PLAIN }, [`GET ${AUDIT}`]: apiError(403, "FORBIDDEN") }),
    );
    const user = userEvent.setup();
    renderApp(fetch, { language });
    const nav = await screen.findByRole("navigation");
      await user.click(await within(nav).findByRole("button", { name: /^(Activity|Aktivit\u00e4t)$/ }));
    await screen.findByRole("alert");
    await expectNoAxeViolations();
  });

  it("is reachable and operable by keyboard", async () => {
    const { fetch } = mockFetch(viewRoutes({ [`GET ${AUDIT}`]: page([event(1)]) }));
    const user = userEvent.setup();
    renderApp(fetch, { language });
    const nav = await screen.findByRole("navigation");
      const activity = await within(nav).findByRole("button", { name: /^(Activity|Aktivit\u00e4t)$/ });
    activity.focus();
    await user.keyboard("{Enter}");
    await screen.findByRole("table");
    // The filter form is the first interactive content after the heading.
    await user.tab();
    expect(document.activeElement?.tagName).toBeTruthy();
    const apply = document.querySelector<HTMLButtonElement>('button[type="submit"]')!;
    apply.focus();
    await user.keyboard("{Enter}");
    await waitFor(() => expect(screen.getByRole("table")).toBeTruthy());
  });
});
