import { render } from "@testing-library/react";
import axe from "axe-core";
import { expect, vi } from "vitest";
import { App } from "../App.tsx";
import { createApiClient } from "../api/client.ts";
import { I18nProvider } from "../i18n/index.tsx";

export interface RecordedCall {
  method: string;
  path: string;
  query: URLSearchParams;
  headers: Record<string, string>;
  body: unknown;
}

export type Reply = { status?: number; body?: unknown; headers?: Record<string, string> };
export type Handler = Reply | ((call: RecordedCall) => Reply | Promise<Reply>);

/**
 * A fetch double. Handlers are keyed by `"METHOD /path"` (path without query).
 * Unknown routes answer 599 so that a forgotten route fails loudly.
 */
export function mockFetch(routes: Record<string, Handler>) {
  const calls: RecordedCall[] = [];
  const impl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(String(input), "http://localhost");
    const headers: Record<string, string> = {};
    new Headers(init?.headers).forEach((value, key) => {
      headers[key.toLowerCase()] = value;
    });
    const call: RecordedCall = {
      method: init?.method ?? "GET",
      path: url.pathname.replace(/^\/api\/v1/, ""),
      query: url.searchParams,
      headers,
      body: typeof init?.body === "string" ? JSON.parse(init.body) : undefined,
    };
    calls.push(call);
    const handler = routes[`${call.method} ${call.path}`];
    if (handler === undefined) {
      return new Response(`{"unexpected":"${call.method} ${call.path}"}`, { status: 599 });
    }
    const reply = await (typeof handler === "function" ? handler(call) : handler);
    const status = reply.status ?? 200;
    const text = status === 204 || reply.body === undefined ? null : JSON.stringify(reply.body);
    return new Response(text, {
      status,
      headers: { "content-type": "application/json", ...(reply.headers ?? {}) },
    });
  });
  return { fetch: impl as unknown as typeof fetch, calls, impl };
}

export function apiError(
  status: number,
  code: string,
  extra: { details?: Record<string, unknown>; headers?: Record<string, string> } = {},
): Reply {
  return {
    status,
    body: {
      error: {
        code,
        message: `server text for ${code}`,
        request_id: "req-1",
        ...(extra.details === undefined ? {} : { details: extra.details }),
      },
    },
    ...(extra.headers === undefined ? {} : { headers: extra.headers }),
  };
}

export const SESSION = {
  user_id: "00000000-0000-4000-8000-000000000001",
  email: "ada@example.test",
  display_name: "Ada Example",
  instance_role: "operator",
  csrf_token: "csrf-from-session",
  created_at: "2026-10-01T10:00:00Z",
  expires_at: "2026-10-08T10:00:00Z",
};

export const ORG_A = {
  id: "00000000-0000-4000-8000-0000000000a1",
  slug: "acme",
  name: "Acme Studio",
  role: "owner",
  version: 1,
  workspace_admin_invites_enabled: false,
  created_at: "2026-10-01T10:00:00Z",
};
export const ORG_B = { ...ORG_A, id: "00000000-0000-4000-8000-0000000000a2", slug: "beta", name: "Beta Lab", role: "member" };

export const WS_1 = {
  id: "00000000-0000-4000-8000-0000000000b1",
  organization_id: ORG_A.id,
  slug: "main",
  name: "Main workspace",
  workspace_role: "admin",
  version: 1,
  created_at: "2026-10-01T10:00:00Z",
};
export const WS_2 = { ...WS_1, id: "00000000-0000-4000-8000-0000000000b2", slug: "side", name: "Side workspace", workspace_role: "viewer" };

/** Routes of a healthy server with a logged-in user. */
export function loggedInRoutes(overrides: Record<string, Handler> = {}): Record<string, Handler> {
  return {
    "GET /setup/status": { body: { setup_required: false } },
    "GET /auth/session": { body: SESSION },
    "GET /auth/2fa": { body: { enabled: false, available: true, recovery_codes_remaining: 0 } },
    "GET /admin/email-delivery": { body: { configured: false, last_success_at: null, failed_count: 0, suppressed_count: 0 } },
    "GET /organizations": { body: { items: [ORG_A, ORG_B], next_cursor: null } },
    [`GET /organizations/${ORG_A.id}/workspaces`]: { body: { items: [WS_1, WS_2], next_cursor: null } },
    [`GET /organizations/${ORG_B.id}/workspaces`]: { body: { items: [], next_cursor: null } },
    "POST /auth/logout": { status: 204 },
    ...overrides,
  };
}

export function renderApp(
  fetchImpl: typeof fetch,
  options: {
    inviteToken?: string | null;
    resetToken?: string | null;
    oidcPending?: string | null;
    language?: string;
    xhrFactory?: () => XMLHttpRequest;
  } = {},
) {
  return render(
    <I18nProvider initial={options.language ?? "en"}>
      <App
        inviteToken={options.inviteToken ?? null}
        resetToken={options.resetToken ?? null}
        oidcPending={options.oidcPending ?? null}
        createClient={(onUnauthenticated) =>
          createApiClient({
            fetchImpl,
            onUnauthenticated,
            ...(options.xhrFactory === undefined ? {} : { xhrFactory: options.xhrFactory }),
          })
        }
      />
    </I18nProvider>,
  );
}

/**
 * Runs axe-core on the current document. jsdom has no layout engine, so the
 * colour-contrast rule cannot run here; contrast of the design tokens is
 * covered by tokens.test.ts instead.
 */
export async function expectNoAxeViolations(container: Element = document.body): Promise<void> {
  const results = await axe.run(container, {
    rules: {
      "color-contrast": { enabled: false },
      // Experimental in axe, but WCAG 2.5.3 (Label in Name): a control's accessible name
      // must contain its visible text. It caught "Move Brand kit to the trash" vs "Move to trash".
      "label-content-name-mismatch": { enabled: true },
    },
  });
  const summary = results.violations.map(
    (violation) =>
      `${violation.id}: ${violation.help}\n` +
      violation.nodes.map((node) => `  ${node.html}`).join("\n"),
  );
  expect(summary, summary.join("\n")).toEqual([]);
}
