import { cleanup, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { ORG_A, SESSION, WS_1, expectNoAxeViolations, mockFetch } from "./test/helpers.tsx";
import { PROJECT_1, viewRoutes } from "./test/fixtures.ts";
import type { AdminSetup, SetupStepId } from "./api/types.ts";
import { renderApp } from "./test/helpers.tsx";

const stepIds: SetupStepId[] = [
  "account", "system_check", "two_factor", "email_delivery", "organization",
  "workspace_project", "invitations", "optional_services", "summary",
];

function setup(complete = false, done: SetupStepId[] = ["account"], skipped: SetupStepId[] = []): AdminSetup {
  return {
    organization: { id: ORG_A.id, name: ORG_A.name, slug: ORG_A.slug },
    complete,
    steps: stepIds.map((id) => ({
      id,
      status: done.includes(id) ? "done" : skipped.includes(id) ? "skipped" : "pending",
      updated_at: null,
      updated_by: null,
    })),
  };
}

const systemCheck = {
  checked_at: "2026-11-02T10:00:00Z",
  items: [
    { id: "public_url", status: "warning", message: "setup.check.public_url.origin_unavailable", docs_path: "/docs/06-operations/install.md#public-url" },
    { id: "https", status: "error", message: "setup.check.https.insecure", docs_path: "/docs/06-operations/install.md#https" },
    { id: "postgres_version", status: "error", message: "setup.check.postgres_version.unsupported", docs_path: "/docs/06-operations/install.md#database" },
    { id: "storage_free_space", status: "warning", message: "setup.check.storage_free_space.low", docs_path: "/docs/06-operations/install.md#host" },
    { id: "figcraft", status: "warning", message: "setup.check.figcraft.configured_unverified", docs_path: "/docs/06-operations/install.md#figcraft" },
    { id: "clock_drift", status: "warning", message: "setup.check.clock_drift.warning", docs_path: "/docs/06-operations/install.md#host" },
    { id: "maintenance", status: "warning", message: "setup.check.maintenance.disabled", docs_path: "/docs/06-operations/install.md#maintenance" },
    { id: "archive", status: "warning", message: "setup.check.archive.configured_unverified", docs_path: "/docs/06-operations/install.md#archive" },
  ],
};

const instanceSettings = {
  updated_at: "2026-11-02T10:00:00Z",
  settings: {
    "mail.transport": { value: "smtp", source: "environment", locked: true },
    "mail.host": { value: "smtp.env.test", source: "environment", locked: true },
    "mail.port": { value: 587, source: "environment", locked: true },
    "mail.tls": { value: "starttls", source: "environment", locked: true },
    "mail.username": { value: "operator", source: "database", locked: false },
    "mail.from": { value: "cloud@example.test", source: "database", locked: false },
    "mail.tenant_id": { value: null, source: "default", locked: false },
    "mail.client_id": { value: null, source: "default", locked: false },
    "mail.sender": { value: null, source: "default", locked: false },
    "mail.save_to_sent": { value: false, source: "default", locked: false },
    "mail.password": { configured_at: null, source: "environment", locked: true },
    "mail.client_secret": { configured_at: null, source: "database", locked: false },
    "mail.certificate": { configured_at: null, source: "default", locked: false },
    "mail.private_key": { configured_at: null, source: "default", locked: false },
    "mail.service_account": { configured_at: null, source: "default", locked: false },
    "retention.version_days": { value: 365, source: "default", locked: false },
    "versions.keep_all_hours": { value: 24, source: "default", locked: false },
    "versions.daily_days": { value: 7, source: "default", locked: false },
    "versions.weekly_weeks": { value: 4, source: "default", locked: false },
    "versions.max_per_file": { value: 0, source: "default", locked: false },
    "retention.trash_days": { value: 30, source: "default", locked: false },
    "retention.audit_days": { value: 90, source: "default", locked: false },
    "retention.quarantine_days": { value: 7, source: "default", locked: false },
    "storage.compression_level": { value: 3, source: "default", locked: false },
  },
};

const languages = [
  ["en", "Setup wizard", "System check", "Public URL", "The request origin could not be verified against the configured public URL."],
  ["de", "Setup-Assistent", "Systemprüfung", "Öffentliche URL", "Der Ursprung der Anfrage konnte nicht mit der konfigurierten öffentlichen URL abgeglichen werden."],
] as const;

describe.each(languages)("setup wizard (%s)", (language, wizardTitle, systemTitle, checkTitle, checkMessage) => {
  it("shows translated progress and system checks; step changes move focus", async () => {
    window.history.replaceState(null, "", "/admin/setup");
    const { fetch } = mockFetch(viewRoutes({
      "GET /admin/setup": { body: setup() },
      "GET /admin/setup/system-check": { body: systemCheck },
    }));
    const user = userEvent.setup();
    renderApp(fetch, { language });

    await screen.findByRole("heading", { level: 1, name: wizardTitle });
    const stepHeading = await screen.findByRole("heading", { level: 2, name: systemTitle });
    expect(document.activeElement).toBe(stepHeading);
    expect(await screen.findByText(checkTitle)).toBeTruthy();
    expect(await screen.findByText(checkMessage)).toBeTruthy();
    for (const title of language === "en"
      ? ["HTTPS connection", "PostgreSQL version", "Free storage space", "FigCraft service", "Database clock", "Scheduled maintenance", "Archive storage"]
      : ["HTTPS-Verbindung", "PostgreSQL-Version", "Freier Speicherplatz", "FigCraft-Dienst", "Datenbankuhr", "Geplante Wartung", "Archivspeicher"]) {
      expect(await screen.findByText(title)).toBeTruthy();
    }
    for (const message of language === "en"
      ? ["The request origin could not be verified against the configured public URL.", "The connection does not use HTTPS.", "The PostgreSQL version is not supported.", "The storage directory has little free space.", "FigCraft is configured, but its availability has not been verified.", "The server and database clocks differ.", "Scheduled maintenance is disabled.", "Archive storage is configured, but its availability has not been verified. This status is shown for reference only."]
      : ["Der Ursprung der Anfrage konnte nicht mit der konfigurierten öffentlichen URL abgeglichen werden.", "Die Verbindung verwendet kein HTTPS.", "Die PostgreSQL-Version wird nicht unterstützt.", "Im Speicherverzeichnis ist nur wenig Speicherplatz frei.", "FigCraft ist konfiguriert, seine Verfügbarkeit wurde jedoch nicht überprüft.", "Server- und Datenbankuhr weichen voneinander ab.", "Die geplante Wartung ist deaktiviert.", "Der Archivspeicher ist konfiguriert, seine Verfügbarkeit wurde jedoch nicht überprüft. Der Status dient nur zur Anzeige."]) {
      expect(await screen.findByText(message)).toBeTruthy();
    }
    await expectNoAxeViolations();

    const nav = screen.getByRole("navigation", { name: language === "en" ? "Setup steps" : "Einrichtungsschritte" });
    const nextStep = within(nav).getByRole("button", { name: /Two-factor authentication|Zwei-Faktor-Anmeldung/ });
    nextStep.focus();
    await user.keyboard("{Enter}");
    const nextHeading = await screen.findByRole("heading", { level: 2, name: /Two-factor authentication|Zwei-Faktor-Anmeldung/ });
    await waitFor(() => expect(document.activeElement).toBe(nextHeading));
  });

  it("localizes FigCraft WebSocket reachability results", async () => {
    window.history.replaceState(null, "", "/admin/setup");
    const expected = language === "en"
      ? ["The FigCraft WebSocket service is reachable.", "The FigCraft WebSocket service rejected the handshake.", "The FigCraft WebSocket handshake could not be completed.", "The FigCraft WebSocket service could not be reached."]
      : ["Der FigCraft-WebSocket-Dienst ist erreichbar.", "Der FigCraft-WebSocket-Dienst hat den Handshake abgelehnt.", "Der FigCraft-WebSocket-Handshake konnte nicht abgeschlossen werden.", "Der FigCraft-WebSocket-Dienst konnte nicht erreicht werden."] as const;
    const results = [
      [0, "setup.check.figcraft.reachable", "ok"],
      [1, "setup.check.figcraft.handshake_rejected", "warning"],
      [2, "setup.check.figcraft.handshake_failed", "warning"],
      [3, "setup.check.figcraft.unreachable", "error"],
    ] as const;
    for (const [index, message, status] of results) {
      const result = { ...systemCheck, items: systemCheck.items.map((item) => item.id === "figcraft" ? { ...item, status, message } : item) };
      const { fetch } = mockFetch(viewRoutes({
        "GET /admin/setup": { body: setup() },
        "GET /admin/setup/system-check": { body: result },
      }));
      renderApp(fetch, { language });
      expect(await screen.findByText(expected[index]!)).toBeTruthy();
      cleanup();
    }
  });

  it("persists the system check completion and continues to the next pending step", async () => {
    window.history.replaceState(null, "", "/admin/setup");
    let current = setup();
    const { fetch, calls } = mockFetch(viewRoutes({
      "GET /admin/setup": () => ({ body: current }),
      "GET /admin/setup/system-check": { body: systemCheck },
      "PATCH /admin/setup/steps/system_check": (call) => {
        expect(call.body).toEqual({ status: "done" });
        current = {
          ...current,
          steps: current.steps.map((step) => step.id === "system_check" ? { ...step, status: "done" } : step),
        };
        return { body: current.steps[1] };
      },
    }));
    const user = userEvent.setup();
    renderApp(fetch, { language });
    await screen.findByRole("heading", { level: 2, name: systemTitle });
    await user.click(await screen.findByRole("button", { name: language === "en" ? "Continue" : "Weiter" }));
    await screen.findByRole("heading", { level: 2, name: /Two-factor authentication|Zwei-Faktor-Anmeldung/ });
    expect(calls.some((call) => call.method === "PATCH" && call.path === "/admin/setup/steps/system_check")).toBe(true);
  });

  it("persists a skipped step and resumes from server progress", async () => {
    window.history.replaceState(null, "", "/admin/setup");
    let current = setup(false, ["account", "system_check"]);
    const { fetch, calls } = mockFetch(viewRoutes({
      "GET /admin/setup": () => ({ body: current }),
      "PATCH /admin/setup/steps/two_factor": (call) => {
        expect(call.body).toEqual({ status: "skipped" });
        current = {
          ...current,
          steps: current.steps.map((step) => step.id === "two_factor" ? { ...step, status: "skipped" } : step),
        };
        return { body: current.steps[2] };
      },
      "GET /admin/setup/system-check": { body: systemCheck },
    }));
    const user = userEvent.setup();
    renderApp(fetch, { language });
    await screen.findByRole("heading", { level: 2, name: /Two-factor authentication|Zwei-Faktor-Anmeldung/ });
    await user.click(await screen.findByRole("button", { name: language === "en" ? "Skip step" : "Schritt überspringen" }));
    await screen.findByRole("heading", { level: 2, name: language === "en" ? "E-mail delivery" : "E-Mail-Versand" });
    expect(calls.some((call) => call.method === "PATCH" && call.path === "/admin/setup/steps/two_factor")).toBe(true);

    cleanup();
    window.history.replaceState(null, "", "/admin/setup");
    renderApp(fetch, { language });
    await screen.findByRole("heading", { level: 2, name: language === "en" ? "E-mail delivery" : "E-Mail-Versand" });
    expect(screen.getByRole("button", { name: /Two-factor authentication|Zwei-Faktor-Anmeldung/ }).textContent).toContain(language === "en" ? "Skipped" : "Übersprungen");
  });

  it("finishes only after all other steps resolve and persists summary", async () => {
    window.history.replaceState(null, "", "/admin/setup");
    let current = setup(false, ["account", "system_check"], ["two_factor", "email_delivery", "organization", "workspace_project", "invitations", "optional_services"]);
    const { fetch, calls } = mockFetch(viewRoutes({
      "GET /admin/setup": () => ({ body: current }),
      "PATCH /admin/setup/steps/summary": (call) => {
        expect(call.body).toEqual({ status: "done" });
        current = { ...current, complete: true, steps: current.steps.map((step) => step.id === "summary" ? { ...step, status: "done" } : step) };
        return { body: current.steps.at(-1) };
      },
      "GET /admin/setup/system-check": { body: systemCheck },
    }));
    const user = userEvent.setup();
    renderApp(fetch, { language });
    await screen.findByRole("heading", { level: 2, name: language === "en" ? "Summary" : "Zusammenfassung" });
    await user.click(await screen.findByRole("button", { name: language === "en" ? "Finish" : "Fertigstellen" }));
    await waitFor(() => expect(window.location.pathname).toBe("/admin/system"));
    expect(calls.some((call) => call.method === "PATCH" && call.path === "/admin/setup/steps/summary")).toBe(true);
  });

  it("keeps environment values and secrets read-only; saves database values", async () => {
    window.history.replaceState(null, "", "/admin/setup");
    let savedSettings = instanceSettings;
    const { fetch, calls } = mockFetch(viewRoutes({
      "GET /admin/setup": { body: setup(false, ["account", "system_check"], ["two_factor"]) },
      "GET /admin/instance-settings": () => ({ body: savedSettings }),
      "PATCH /admin/instance-settings": (call) => {
        expect(call.body).toEqual({ settings: { "mail.from": { value: "new@example.test" } } });
        savedSettings = { ...instanceSettings, settings: { ...instanceSettings.settings, "mail.from": { value: "new@example.test", source: "database", locked: false } } };
        return { body: savedSettings };
      },
      "POST /admin/instance-settings/email-test": { status: 204 },
    }));
    const user = userEvent.setup();
    renderApp(fetch, { language });
    await screen.findByRole("heading", { level: 2, name: language === "en" ? "E-mail delivery" : "E-Mail-Versand" });
    const host = await screen.findByLabelText(language === "en" ? "SMTP host" : "SMTP-Host");
    const password = screen.getByLabelText(language === "en" ? "SMTP password" : "SMTP-Passwort");
    expect((host as HTMLInputElement).disabled).toBe(true);
    expect((host as HTMLInputElement).value).toBe("smtp.env.test");
    expect((password as HTMLInputElement).disabled).toBe(true);
    expect((password as HTMLInputElement).value).toBe("");
    expect(screen.getAllByText(language === "en" ? "Set by the configuration file." : "In der Konfigurationsdatei festgelegt.").length).toBeGreaterThan(0);
    await expectNoAxeViolations();

    const from = screen.getByLabelText(language === "en" ? "From address" : "Absenderadresse");
    await user.clear(from);
    await user.type(from, "new@example.test");
    await user.click(screen.getByRole("button", { name: language === "en" ? "Save settings" : "Einstellungen speichern" }));
    await waitFor(() => expect(calls.some((call) => call.method === "PATCH" && call.path === "/admin/instance-settings")).toBe(true));
    await screen.findByText(language === "en" ? "Settings saved." : "Einstellungen gespeichert.");
    await user.click(screen.getByRole("button", { name: language === "en" ? "Send test e-mail" : "Test-E-Mail senden" }));
    await screen.findByText(language === "en" ? "Test e-mail queued for delivery." : "Test-E-Mail zur Zustellung vorgemerkt.");
  });

  it("asks for confirmation before shortening a retention period and saves only after it", async () => {
    window.history.replaceState(null, "", "/admin/setup");
    let patched: unknown[] = [];
    const organizationRow = {
      id: ORG_A.id, slug: ORG_A.slug, name: ORG_A.name, member_count: 1,
      created_at: "2026-10-01T10:00:00Z", committed_bytes: 0, reserved_bytes: 0,
      quota_bytes: null, seat_limit: null,
    };
    const { fetch } = mockFetch(viewRoutes({
      "GET /admin/setup": { body: setup() },
      "GET /admin/setup/system-check": { body: systemCheck },
      "GET /admin/organizations": { body: { items: [organizationRow], next_cursor: null } },
      [`GET /organizations/${ORG_A.id}/apps`]: { status: 503, body: { error: "unavailable" } },
      "GET /admin/instance-settings": { body: instanceSettings },
      "PATCH /admin/instance-settings": (call) => {
        patched.push(call.body);
        return { body: instanceSettings };
      },
    }));
    const user = userEvent.setup();
    renderApp(fetch, { language });
    const nav = await screen.findByRole("navigation", { name: language === "en" ? "Setup steps" : "Einrichtungsschritte" });
    await user.click(within(nav).getByRole("button", { name: /Organization basics|Organisationsgrundlagen/ }));
    const trash = await screen.findByLabelText(language === "en" ? "Trash retention (days)" : "Aufbewahrung im Papierkorb (Tage)");
    const save = screen.getAllByRole("button", { name: language === "en" ? "Save settings" : "Einstellungen speichern" })[0]!;

    // Lengthening saves directly.
    await user.clear(trash);
    await user.type(trash, "60");
    await user.click(save);
    await waitFor(() => expect(patched).toEqual([{ settings: { "retention.trash_days": { value: 60 } } }]));
    patched = [];

    // Shortening opens the confirmation; cancelling sends nothing.
    await user.clear(trash);
    await user.type(trash, "7");
    await user.click(save);
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText(language === "en" ? "Shorten retention period?" : "Aufbewahrungsfrist verkürzen?")).toBeTruthy();
    expect(dialog.textContent).toContain("30 → 7");
    await user.click(within(dialog).getByRole("button", { name: language === "en" ? "Cancel" : "Abbrechen" }));
    expect(patched).toEqual([]);

    await user.click(save);
    const confirm = await screen.findByRole("dialog");
    await user.click(within(confirm).getByRole("button", { name: language === "en" ? "Shorten retention" : "Frist verkürzen" }));
    await waitFor(() => expect(patched).toEqual([{ settings: { "retention.trash_days": { value: 7 } } }]));
  });

  it("saves the instance compression level from the organization settings step", async () => {
    window.history.replaceState(null, "", "/admin/setup");
    let patchBody: unknown;
    const organizationRow = {
      id: ORG_A.id, slug: ORG_A.slug, name: ORG_A.name, member_count: 1,
      created_at: "2026-10-01T10:00:00Z", committed_bytes: 0, reserved_bytes: 0,
      quota_bytes: null, seat_limit: null,
    };
    const { fetch } = mockFetch(viewRoutes({
      "GET /admin/setup": { body: setup() },
      "GET /admin/setup/system-check": { body: systemCheck },
      "GET /admin/organizations": { body: { items: [organizationRow], next_cursor: null } },
      [`GET /organizations/${ORG_A.id}/apps`]: { status: 503, body: { error: "unavailable" } },
      "GET /admin/instance-settings": { body: instanceSettings },
      "PATCH /admin/instance-settings": (call) => { patchBody = call.body; return { body: instanceSettings }; },
    }));
    const user = userEvent.setup();
    renderApp(fetch);
    const nav = await screen.findByRole("navigation", { name: "Setup steps" });
    await user.click(within(nav).getByRole("button", { name: /Organization basics/ }));
    const level = await screen.findByLabelText("Storage compression level");
    await user.clear(level);
    await user.type(level, "0");
    await user.click(screen.getAllByRole("button", { name: "Save settings" })[1]!);
    await waitFor(() => expect(patchBody).toEqual({ settings: { "storage.compression_level": { value: 0 } } }));
  });

  it("asks before changing the unlimited per-file version cap", async () => {
    window.history.replaceState(null, "", "/admin/setup");
    let patchBody: unknown;
    const organizationRow = {
      id: ORG_A.id, slug: ORG_A.slug, name: ORG_A.name, member_count: 1,
      created_at: "2026-10-01T10:00:00Z", committed_bytes: 0, reserved_bytes: 0,
      quota_bytes: null, seat_limit: null,
    };
    const { fetch } = mockFetch(viewRoutes({
      "GET /admin/setup": { body: setup() },
      "GET /admin/setup/system-check": { body: systemCheck },
      "GET /admin/organizations": { body: { items: [organizationRow], next_cursor: null } },
      [`GET /organizations/${ORG_A.id}/apps`]: { status: 503, body: { error: "unavailable" } },
      "GET /admin/instance-settings": { body: instanceSettings },
      "PATCH /admin/instance-settings": (call) => { patchBody = call.body; return { body: instanceSettings }; },
    }));
    const user = userEvent.setup();
    renderApp(fetch);
    const nav = await screen.findByRole("navigation", { name: "Setup steps" });
    await user.click(within(nav).getByRole("button", { name: /Organization basics/ }));
    const cap = await screen.findByLabelText("Maximum unpinned versions per file");
    await user.clear(cap);
    await user.type(cap, "20");
    await user.click(screen.getAllByRole("button", { name: "Save settings" })[0]!);
    const dialog = await screen.findByRole("dialog");
    expect(dialog.textContent).toContain("0 → 20");
    expect(patchBody).toBeUndefined();
    await user.click(within(dialog).getByRole("button", { name: "Shorten retention" }));
    await waitFor(() => expect(patchBody).toEqual({ settings: { "versions.max_per_file": { value: 20 } } }));
  });

  it("warns that a changed mail target removes saved credentials and reports the removal", async () => {
    window.history.replaceState(null, "", "/admin/setup");
    const editable = {
      ...instanceSettings,
      settings: {
        ...instanceSettings.settings,
        "mail.transport": { value: "smtp", source: "database", locked: false },
        "mail.host": { value: "smtp.old.test", source: "database", locked: false },
        "mail.port": { value: 587, source: "database", locked: false },
        "mail.tls": { value: "starttls", source: "database", locked: false },
        "mail.password": { configured_at: "2026-11-01T10:00:00Z", source: "database", locked: false },
      },
    };
    const { fetch, calls } = mockFetch(viewRoutes({
      "GET /admin/setup": { body: setup(false, ["account", "system_check"], ["two_factor"]) },
      "GET /admin/instance-settings": { body: editable },
      "PATCH /admin/instance-settings": () => ({
        body: {
          ...editable,
          settings: {
            ...editable.settings,
            "mail.host": { value: "smtp.new.test", source: "database", locked: false },
            "mail.password": { configured_at: null, source: "default", locked: false },
          },
          credentials_cleared: ["mail.password"],
        },
      }),
    }));
    const user = userEvent.setup();
    renderApp(fetch, { language });
    const host = await screen.findByLabelText(language === "en" ? "SMTP host" : "SMTP-Host");
    const hint = language === "en"
      ? "Changing the mail server, transport or sign-in identity removes all saved credentials. Enter them again after saving."
      : "Wenn du Mailserver, Transport oder Anmeldeidentität änderst, werden alle gespeicherten Zugangsdaten entfernt. Gib sie nach dem Speichern erneut ein.";
    expect(screen.queryByText(hint)).toBeNull();
    await user.clear(host);
    await user.type(host, "smtp.new.test");
    expect(await screen.findByText(hint)).toBeTruthy();
    await user.click(screen.getByRole("button", { name: language === "en" ? "Save settings" : "Einstellungen speichern" }));
    await screen.findByText(language === "en"
      ? "Saved credentials were removed because the mail target changed. Enter the credentials again."
      : "Gespeicherte Zugangsdaten wurden entfernt, weil sich das Mail-Ziel geändert hat. Gib die Zugangsdaten erneut ein.");
    expect(calls.filter((call) => call.method === "PATCH").length).toBe(1);
  });

  it("blocks capacity saves until organization and app settings load", async () => {
    window.history.replaceState(null, "", "/admin/setup");
    const organizationRow = {
      id: ORG_A.id, slug: ORG_A.slug, name: ORG_A.name, member_count: 1,
      created_at: "2026-10-01T10:00:00Z", committed_bytes: 0, reserved_bytes: 0,
      quota_bytes: null, seat_limit: null,
    };
    const { fetch, calls } = mockFetch(viewRoutes({
      "GET /admin/setup": { body: setup() },
      "GET /admin/setup/system-check": { body: systemCheck },
      "GET /admin/organizations": { body: { items: [organizationRow], next_cursor: null } },
      [`GET /organizations/${ORG_A.id}/apps`]: { status: 503, body: { error: "unavailable" } },
    }));
    const user = userEvent.setup();
    renderApp(fetch, { language });
    const nav = await screen.findByRole("navigation", { name: language === "en" ? "Setup steps" : "Einrichtungsschritte" });
    await user.click(within(nav).getByRole("button", { name: /Organization basics|Organisationsgrundlagen/ }));
    const save = await screen.findByRole("button", { name: language === "en" ? "Save organization settings" : "Organisationseinstellungen speichern" });
    expect((save as HTMLButtonElement).disabled).toBe(true);
    await user.click(save);
    expect(calls.some((call) => call.method === "PATCH" && call.path.includes("/organizations/") )).toBe(false);
  });

  it("renders every step with an accessible structure", async () => {
    window.history.replaceState(null, "", "/admin/setup");
    const organizationRow = {
      id: ORG_A.id, slug: ORG_A.slug, name: ORG_A.name, member_count: 1,
      created_at: "2026-10-01T10:00:00Z", committed_bytes: 0, reserved_bytes: 0,
      quota_bytes: null, seat_limit: null,
    };
    const createdWorkspace = { ...WS_1, id: "00000000-0000-4000-8000-0000000000b9", slug: "setup-workspace", name: "Setup workspace" };
    const inviteUrl = "https://cloud.test/invite#token=one-time";
    const { fetch } = mockFetch(viewRoutes({
      "GET /admin/setup": { body: setup() },
      "GET /admin/setup/system-check": { body: systemCheck },
      "GET /admin/instance-settings": { body: instanceSettings },
      "GET /admin/organizations": { body: { items: [organizationRow], next_cursor: null } },
      [`GET /organizations/${ORG_A.id}/workspaces`]: { body: { items: [WS_1], next_cursor: null } },
      [`GET /organizations/${ORG_A.id}/apps`]: { body: { mode: "all", apps: [], seat_usage: { used: 1, limit: null } } },
      [`POST /organizations/${ORG_A.id}/workspaces`]: { status: 201, body: createdWorkspace },
      [`POST /workspaces/${createdWorkspace.id}/projects`]: { status: 201, body: { ...PROJECT_1, id: "00000000-0000-4000-8000-0000000000c9", workspace_id: createdWorkspace.id } },
      [`POST /organizations/${ORG_A.id}/invitations`]: { status: 201, body: { invitation_id: "00000000-0000-4000-8000-0000000000d9", invite_url: inviteUrl, expires_at: "2026-11-09T10:00:00Z" } },
    }));
    const user = userEvent.setup();
    renderApp(fetch, { language });
    const nav = await screen.findByRole("navigation", { name: language === "en" ? "Setup steps" : "Einrichtungsschritte" });
    const names = language === "en"
      ? ["Operator account", "System check", "Two-factor authentication", "E-mail delivery", "Organization basics", "First workspace and project", "Invite people", "Optional services", "Summary"]
      : ["Betreiberkonto", "Systemprüfung", "Zwei-Faktor-Anmeldung", "E-Mail-Versand", "Organisationsgrundlagen", "Erster Workspace und erstes Projekt", "Personen einladen", "Optionale Dienste", "Zusammenfassung"];
    for (const name of names) {
      await user.click(within(nav).getByRole("button", { name: new RegExp(name) }));
      await screen.findByRole("heading", { level: 2, name });
      if (name === (language === "en" ? "First workspace and project" : "Erster Workspace und erstes Projekt")) {
        const workspaceSelect = screen.getByRole("combobox", { name: language === "en" ? "Workspace" : "Workspace" });
        await waitFor(() => expect(within(workspaceSelect).getByRole("option", { name: WS_1.name })).toBeTruthy());
        await user.type(screen.getByLabelText(language === "en" ? "Workspace name" : "Workspace-Name"), "Setup workspace");
        await user.click(screen.getByRole("button", { name: language === "en" ? "Create workspace" : "Workspace erstellen" }));
        await screen.findByText(language === "en" ? "Workspace created." : "Workspace erstellt.");
        expect(within(workspaceSelect).getByRole("option", { name: "Setup workspace" })).toBeTruthy();
        expect(within(workspaceSelect).getByRole("option", { name: WS_1.name })).toBeTruthy();
        await user.type(screen.getByLabelText(language === "en" ? "Project name" : "Projektname"), "Setup project");
        await user.click(screen.getByRole("button", { name: language === "en" ? "Create project" : "Projekt anlegen" }));
        await screen.findByText(language === "en" ? "Project created." : "Projekt erstellt.");
      }
      if (name === (language === "en" ? "Invite people" : "Personen einladen")) {
        await user.type(screen.getByLabelText(language === "en" ? "E-mail address" : "E-Mail-Adresse"), "setup@example.test");
        await user.click(screen.getByRole("button", { name: language === "en" ? "Create invitation" : "Einladung erstellen" }));
        await screen.findByDisplayValue(inviteUrl);
      }
      if (name === (language === "en" ? "Optional services" : "Optionale Dienste")) {
        await screen.findByRole("heading", { level: 3, name: language === "en" ? "Archive storage status" : "Archivspeicherstatus" });
      }
      await expectNoAxeViolations();
    }
  });
});

describe("setup wizard operator visibility", () => {
  it("shows only the read-only system check after setup completion", async () => {
    window.history.replaceState(null, "", "/admin/setup");
    const completed = setup(true, stepIds);
    const { fetch, calls } = mockFetch(viewRoutes({
      "GET /admin/setup": { body: completed },
      "GET /admin/setup/system-check": { body: systemCheck },
    }));
    renderApp(fetch);
    await screen.findByRole("heading", { level: 2, name: "System check" });
    const nav = screen.getByRole("navigation", { name: "Setup steps" });
    expect((within(nav).getByRole("button", { name: /Organization basics/ }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.queryByRole("button", { name: "Continue" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Save settings" })).toBeNull();
    expect(calls.some((call) => call.method === "PATCH")).toBe(false);
  });

  it("shows the incomplete banner only to instance operators", async () => {
    const incomplete = setup();
    const operator = mockFetch(viewRoutes({ "GET /admin/setup": { body: incomplete } }));
    window.history.replaceState(null, "", "/admin");
    const user = userEvent.setup();
    renderApp(operator.fetch);
    expect(await screen.findByText("Setup is incomplete")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Close" }));
    expect(screen.queryByText("Setup is incomplete")).toBeNull();
    cleanup();

    window.history.replaceState(null, "", "/admin");
    const member = mockFetch(viewRoutes({ "GET /auth/session": { body: { ...SESSION, instance_role: null } } }));
    renderApp(member.fetch);
    await screen.findByRole("heading", { level: 1 });
    expect(screen.queryByText("Setup is incomplete")).toBeNull();
    expect(member.calls.some((call) => call.path === "/admin/setup")).toBe(false);
  });
});
