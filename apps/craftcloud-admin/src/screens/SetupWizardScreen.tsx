import { useEffect, useMemo, useRef, useState } from "react";
import type { FormEvent } from "react";
import { Button } from "react-aria-components";
import type { AdminSetup, AdminSetupStep, SetupStepId, SetupSystemCheck } from "../api/types.ts";
import type { Organization, SessionInfo, Workspace } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { Badge } from "../components/Badge.tsx";
import { Alert, Field } from "../components/forms.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { useI18n } from "../i18n/index.tsx";
import type { Translate } from "../i18n/index.tsx";
import { InvitationsScreen } from "./InvitationsScreen.tsx";
import { TwoFactorSettingsScreen } from "./TwoFactorSettingsScreen.tsx";
import { InstanceSettingsWizardSection } from "./InstanceSettingsWizardSection.tsx";
import { suggestSlug } from "./SetupScreen.tsx";

const STEP_IDS: SetupStepId[] = [
  "account",
  "system_check",
  "two_factor",
  "email_delivery",
  "organization",
  "workspace_project",
  "invitations",
  "optional_services",
  "summary",
];

function stepTitle(t: Translate, id: SetupStepId): string {
  switch (id) {
    case "account": return t("Operator account");
    case "system_check": return t("System check");
    case "two_factor": return t("Two-factor authentication");
    case "email_delivery": return t("E-mail delivery");
    case "organization": return t("Organization basics");
    case "workspace_project": return t("First workspace and project");
    case "invitations": return t("Invite people");
    case "optional_services": return t("Optional services");
    case "summary": return t("Summary");
  }
}

function checkTitle(t: Translate, id: string): string {
  switch (id) {
    case "public_url": return t("Public URL");
    case "https": return t("HTTPS connection");
    case "postgres_version": return t("PostgreSQL version");
    case "storage_free_space": return t("Free storage space");
    case "figcraft": return t("FigCraft service");
    case "clock_drift": return t("Database clock");
    case "maintenance": return t("Scheduled maintenance");
    case "archive": return t("Archive storage");
    default: return t("System check");
  }
}

function checkMessage(t: Translate, key: string): string {
  switch (key) {
    case "setup.check.public_url.ok": return t("The configured public URL matches this browser address.");
    case "setup.check.public_url.mismatch": return t("The configured public URL does not match this browser address.");
    case "setup.check.public_url.unconfigured": return t("Set the public URL in the server configuration file.");
    case "setup.check.public_url.origin_unavailable": return t("The request origin could not be verified against the configured public URL.");
    case "setup.check.https.ok": return t("The connection uses HTTPS.");
    case "setup.check.https.unverified": return t("The server could not verify that the connection uses HTTPS.");
    case "setup.check.https.insecure": return t("The connection does not use HTTPS.");
    case "setup.check.postgres_version.ok": return t("The PostgreSQL version is supported.");
    case "setup.check.postgres_version.unsupported": return t("The PostgreSQL version is not supported.");
    case "setup.check.postgres_version.unavailable": return t("The PostgreSQL version could not be checked.");
    case "setup.check.storage_free_space.ok": return t("The storage directory has sufficient free space.");
    case "setup.check.storage_free_space.low": return t("The storage directory has little free space.");
    case "setup.check.storage_free_space.unavailable": return t("Free storage space could not be checked.");
    case "setup.check.figcraft.configured_unverified": return t("FigCraft is configured, but its availability has not been verified.");
    case "setup.check.figcraft.reachable": return t("The FigCraft WebSocket service is reachable.");
    case "setup.check.figcraft.handshake_rejected": return t("The FigCraft WebSocket service rejected the handshake.");
    case "setup.check.figcraft.handshake_failed": return t("The FigCraft WebSocket handshake could not be completed.");
    case "setup.check.figcraft.unreachable": return t("The FigCraft WebSocket service could not be reached.");
    case "setup.check.figcraft.not_configured": return t("FigCraft is not configured.");
    case "setup.check.clock_drift.ok": return t("The server and database clocks are synchronized.");
    case "setup.check.clock_drift.warning": return t("The server and database clocks differ.");
    case "setup.check.maintenance.enabled": return t("Scheduled maintenance is enabled.");
    case "setup.check.maintenance.disabled": return t("Scheduled maintenance is disabled.");
    case "setup.check.archive.configured_unverified": return t("Archive storage is configured, but its availability has not been verified. This status is shown for reference only.");
    case "setup.check.archive.not_configured": return t("Archive storage is not configured.");
    default: return t("No explanation is available for this check.");
  }
}

function checkDocReference(t: Translate, id: string): string {
  switch (id) {
    case "public_url": return t("Public URL and server configuration");
    case "https": return t("HTTPS proxy");
    case "postgres_version": return t("Database selection");
    case "storage_free_space": return t("Host, software and network");
    case "figcraft": return t("FigCraft service");
    case "clock_drift": return t("Host, software and network");
    case "maintenance": return t("Maintenance");
    case "archive": return t("Archive storage");
    default: return t("First-run setup");
  }
}

function checkStatusLabel(t: Translate, status: string): string {
  switch (status) {
    case "ok": return t("OK");
    case "warning": return t("Warning");
    case "error": return t("Error");
    case "not_configured": return t("Not configured");
    default: return t("Unknown status");
  }
}

function statusLabel(t: Translate, status: AdminSetupStep["status"]): string {
  switch (status) {
    case "done": return t("Complete");
    case "skipped": return t("Skipped");
    case "pending": return t("Pending");
  }
}

type LoadState = { kind: "loading" } | { kind: "ready"; setup: AdminSetup } | { kind: "error"; error: unknown };

export function SetupWizardScreen({ onOpenSso, onFinishWizard, onSelectOrganization, onFactorChanged, organization, session, workspaces }: { onOpenSso: () => void; onFinishWizard: () => void; onSelectOrganization: (id: string) => void; onFactorChanged: (enabled: boolean) => void; organization: Organization | null; session: SessionInfo; workspaces: Workspace[] }) {
  const { t } = useI18n();
  const { api } = useAuth();
  const [state, setState] = useState<LoadState>({ kind: "loading" });
  const [activeStep, setActiveStep] = useState<SetupStepId>("system_check");
  const [check, setCheck] = useState<SetupSystemCheck | null>(null);
  const [checkError, setCheckError] = useState<unknown>(null);
  const [busy, setBusy] = useState(false);
  const heading = useRef<HTMLHeadingElement>(null);
  const steps = state.kind === "ready" ? state.setup.steps : [];
  const stepById = useMemo(() => new Map(steps.map((step) => [step.id, step])), [steps]);
  const doneCount = steps.filter((step) => step.status !== "pending").length;

  async function reload() {
    setState({ kind: "loading" });
    try {
      const setup = await api.getAdminSetup();
      setState({ kind: "ready", setup });
      setActiveStep(setup.complete ? "system_check" : setup.steps.find((step) => step.id !== "account" && step.status === "pending")?.id ?? "summary");
    } catch (error) {
      setState({ kind: "error", error });
    }
  }

  useEffect(() => { void reload(); }, [api]);
  useEffect(() => {
    if (state.kind === "ready" && state.setup.organization !== null && organization?.id !== state.setup.organization.id) {
      onSelectOrganization(state.setup.organization.id);
    }
  }, [onSelectOrganization, organization?.id, state]);
  useEffect(() => {
    if (state.kind === "ready") heading.current?.focus();
  }, [activeStep, state.kind]);
  useEffect(() => {
    if (activeStep !== "system_check") return;
    let live = true;
    setCheck(null);
    setCheckError(null);
    void api.getAdminSetupSystemCheck().then(
      (result) => { if (live) setCheck(result); },
      (error: unknown) => { if (live) setCheckError(error); },
    );
    return () => { live = false; };
  }, [activeStep, api]);

  async function updateStep(status: "done" | "skipped") {
    if (busy) return;
    setBusy(true);
    try {
      await api.updateAdminSetupStep(activeStep, status);
      const setup = await api.getAdminSetup();
      setState({ kind: "ready", setup });
      if (activeStep === "summary" && setup.complete) onFinishWizard();
      const currentIndex = STEP_IDS.indexOf(activeStep);
      const next = STEP_IDS.slice(currentIndex + 1).find((id) => setup.steps.find((step) => step.id === id)?.status === "pending");
      if (next !== undefined) setActiveStep(next);
    } catch (error) {
      setState({ kind: "error", error });
    } finally {
      setBusy(false);
    }
  }

  function retryCheck() {
    setCheck(null);
    setCheckError(null);
    void api.getAdminSetupSystemCheck().then(setCheck, (error: unknown) => setCheckError(error));
  }

  if (state.kind === "loading") return <p role="status" className="muted">{t("Loading setup progress…")}</p>;
  if (state.kind === "error") return <ErrorNotice error={state.error} onRetry={() => void reload()} onReload={() => void reload()} />;

  const active = stepById.get(activeStep);
  const canSkip = active?.status === "pending" && activeStep !== "account" && activeStep !== "system_check" && activeStep !== "summary";
  const canMarkDone = active?.status === "pending" && activeStep !== "account" && activeStep !== "system_check" && activeStep !== "summary";
  const completed = state.setup.complete;
  const readyToFinish = steps.filter((step) => step.id !== "summary").every((step) => step.status !== "pending");
  return (
    <div className="setup-wizard">
      <p className="screen__lead">{t("Continue the first-run setup. Progress is saved for every instance operator.")}</p>
      <div className="setup-wizard__progress" role="group" aria-label={t("Setup progress")}>{t("{done} of {total} steps resolved", { done: String(doneCount), total: String(steps.length) })}</div>
      <div className="setup-wizard__layout">
        <nav aria-label={t("Setup steps")}>
          <ol className="setup-wizard__steps">
            {STEP_IDS.map((id, index) => {
              const status = stepById.get(id)?.status ?? "pending";
              return <li key={id}>
                <button type="button" className="setup-wizard__step" disabled={completed && id !== "system_check"} aria-current={activeStep === id ? "step" : undefined} onClick={() => setActiveStep(id)}>
                  <span className="setup-wizard__number" aria-hidden="true">{index + 1}</span>
                  <span className="setup-wizard__step-name">{stepTitle(t, id)}</span>
                  <Badge tone={status === "done" ? "good" : status === "skipped" ? "neutral" : "info"}>{statusLabel(t, status)}</Badge>
                </button>
              </li>;
            })}
          </ol>
        </nav>
        <section className="setup-wizard__panel" aria-labelledby="setup-wizard-step-title">
          <h2 id="setup-wizard-step-title" ref={heading} tabIndex={-1}>{stepTitle(t, activeStep)}</h2>
          <StepContent
            step={activeStep}
            setup={state.setup}
            current={active}
            check={check}
            checkError={checkError}
            onCheckRetry={retryCheck}
            onOpenSso={onOpenSso}
            organization={state.setup.organization !== null && organization?.id === state.setup.organization.id ? organization : null}
            session={session}
            workspaces={workspaces}
            onFactorChanged={onFactorChanged}
          />
          <div className="setup-wizard__actions">
            {canSkip && !completed && <Button className="button" isDisabled={busy} onPress={() => void updateStep("skipped")}>{t("Skip step")}</Button>}
            {canMarkDone && !completed && <Button className="button button--primary" isDisabled={busy} onPress={() => void updateStep("done")}>{t("Mark complete")}</Button>}
            {activeStep === "system_check" && !completed && <Button className="button button--primary" isDisabled={busy || check === null} onPress={() => void updateStep("done")}>{t("Continue")}</Button>}
            {activeStep === "summary" && !completed && <Button className="button button--primary" isDisabled={busy || !readyToFinish} onPress={() => void updateStep("done")}>{t("Finish")}</Button>}
          </div>
        </section>
      </div>
    </div>
  );
}

function StepContent({
  step, setup, current, check, checkError, onCheckRetry, onOpenSso, organization, session, workspaces, onFactorChanged,
}: {
  step: SetupStepId;
  setup: AdminSetup;
  current: AdminSetupStep | undefined;
  check: SetupSystemCheck | null;
  checkError: unknown;
  onCheckRetry: () => void;
  onOpenSso: () => void;
  organization: Organization | null;
  session: SessionInfo;
  workspaces: Workspace[];
  onFactorChanged: (enabled: boolean) => void;
}) {
  const { t } = useI18n();
  if (step === "system_check") {
    if (checkError !== null) return <ErrorNotice error={checkError} onRetry={onCheckRetry} onReload={onCheckRetry} />;
    if (check === null) return <p role="status" className="muted">{t("Checking system requirements…")}</p>;
    const tone = (status: string) => status === "ok" ? "good" as const : status === "warning" ? "warn" as const : status === "error" ? "bad" as const : "neutral" as const;
    return <ul className="setup-wizard__checks">{check.items.map((item) => <li key={item.id}>
      <div><Badge tone={tone(item.status)}>{checkStatusLabel(t, item.status)}</Badge> <strong>{checkTitle(t, item.id)}</strong></div>
      <p>{checkMessage(t, item.message)}</p>
      <p className="setup-wizard__doc-reference">{t("Installation guide section: {section}", { section: checkDocReference(t, item.id) })}</p>
    </li>)}</ul>;
  }
  if (step === "summary") return <>
    <p>{setup.complete ? t("Setup is complete. Review any skipped steps below.") : t("Setup is not complete yet. Return to pending steps to continue.")}</p>
    <ul className="setup-wizard__summary">{setup.steps.map((item) => <li key={item.id}><span>{stepTitle(t, item.id)}</span><Badge tone={item.status === "done" ? "good" : item.status === "skipped" ? "neutral" : "warn"}>{statusLabel(t, item.status)}</Badge></li>)}</ul>
  </>;
  if (step === "account") return <p>{t("The first operator account and organization were created during the one-time setup.")}</p>;
  if (step === "two_factor") return <>
    <p>{t("Protect the instance operator account with an authenticator app and recovery codes.")}</p>
    <TwoFactorSettingsScreen organization={organization} onFactorChanged={onFactorChanged} onOrganizationUpdated={() => undefined} headingId="setup-wizard-step-title" />
  </>;
  if (step === "email_delivery") return <>
    <p>{t("Configure e-mail delivery or choose to share invitation links by hand.")}</p>
    <InstanceSettingsWizardSection section="mail" />
  </>;
  if (step === "organization") return <>
    <p>{t("Set organization seats, storage quota, app access, and instance retention periods.")}</p>
    {organization !== null && <OrganizationSetupControls organization={organization} />}
    <InstanceSettingsWizardSection section="retention" />
    <InstanceSettingsWizardSection section="storage" />
  </>;
  if (step === "workspace_project") return <WorkspaceProjectSetup organization={organization} workspaces={workspaces} />;
  if (step === "invitations") return <>
    <p>{t("Invite people to the organization. If e-mail is not configured, copy and share invitation links.")}</p>
    <InvitationsScreen scope={{ userId: session.user_id, organization, workspace: null }} workspaces={workspaces} />
  </>;
  if (step === "optional_services") return <>
    <p>{t("Company sign-in and archive storage are optional. Archive configuration stays in the server configuration file.")}</p>
    <Button className="button" onPress={onOpenSso}>{t("Open single sign-on settings")}</Button>
    <ArchiveStatus />
  </>;
  return <p>{current?.status === "done" ? t("This step is complete.") : t("Continue setup using the available CraftCloud controls.")}</p>;
}

function ArchiveStatus() {
  const { t } = useI18n();
  const { api } = useAuth();
  const [item, setItem] = useState<SetupSystemCheck["items"][number] | null>(null);
  const [error, setError] = useState<unknown>(null);
  useEffect(() => {
    let live = true;
    void api.getAdminSetupSystemCheck().then((result) => {
      if (!live) return;
      setItem(result.items.find((check) => check.id === "archive") ?? null);
    }, (failure: unknown) => { if (live) setError(failure); });
    return () => { live = false; };
  }, [api]);
  if (error !== null) return <ErrorNotice error={error} />;
  if (item === null) return <p role="status" className="muted">{t("Loading archive status…")}</p>;
  const tone = item.status === "ok" ? "good" : item.status === "warning" ? "warn" : item.status === "error" ? "bad" : "neutral";
  return <section className="section setup-wizard__archive" aria-label={t("Archive storage status")}>
    <h3>{t("Archive storage status")}</h3>
    <Badge tone={tone}>{checkStatusLabel(t, item.status)}</Badge>
    <p>{checkMessage(t, item.message)}</p>
    <p className="field__hint">{t("Installation guide section: {section}", { section: checkDocReference(t, "archive") })}</p>
    <p className="field__hint">{t("Archive storage settings remain in the server configuration file.")}</p>
  </section>;
}


function WorkspaceProjectSetup({ organization, workspaces }: { organization: Organization | null; workspaces: Workspace[] }) {
  const { t } = useI18n();
  const { api } = useAuth();
  const [name, setName] = useState("");
  const [createdWorkspaces, setCreatedWorkspaces] = useState<Array<{ organizationId: string; workspace: Workspace }>>([]);
  const [slug, setSlug] = useState("");
  const organizationWorkspaces = workspaces.filter((item) => item.organization_id === organization?.id);
  const availableWorkspaces = [...organizationWorkspaces, ...createdWorkspaces.filter((item) => item.organizationId === organization?.id).map((item) => item.workspace).filter((item) => !organizationWorkspaces.some((workspace) => workspace.id === item.id))];
  const [workspaceId, setWorkspaceId] = useState("");
  const [projectName, setProjectName] = useState("");
  const [error, setError] = useState<unknown>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const available = organization !== null;
  useEffect(() => {
    setWorkspaceId((current) => availableWorkspaces.some((item) => item.id === current) ? current : availableWorkspaces[0]?.id ?? "");
  }, [organization?.id, workspaces, createdWorkspaces]);

  async function createWorkspace(event: FormEvent) {
    event.preventDefault();
    if (organization === null || busy || name.trim() === "" || slug === "") return;
    setBusy(true); setError(null); setNotice(null);
    try {
      const created = await api.createWorkspace(organization.id, name.trim(), slug);
      setCreatedWorkspaces((current) => [...current.filter((item) => item.workspace.id !== created.id), { organizationId: organization.id, workspace: created }]);
      setWorkspaceId(created.id); setName(""); setSlug("");
      setNotice(t("Workspace created."));
    } catch (failure) { setError(failure); }
    finally { setBusy(false); }
  }

  async function createProject(event: FormEvent) {
    event.preventDefault();
    if (workspaceId === "" || busy || projectName.trim() === "") return;
    setBusy(true); setError(null); setNotice(null);
    try {
      await api.createProject(workspaceId, projectName.trim());
      setProjectName(""); setNotice(t("Project created."));
    } catch (failure) { setError(failure); }
    finally { setBusy(false); }
  }

  return <div className="stack">
    {error !== null && <ErrorNotice error={error} />}
    {notice !== null && <p role="status" className="muted">{notice}</p>}
    {!available && <Alert tone="info">{t("Select the first organization to create a workspace.")}</Alert>}
    <form className="form" onSubmit={(event) => void createWorkspace(event)}>
      <h3>{t("Create first workspace")}</h3>
      <Field label={t("Workspace name")} name="setup-workspace-name" value={name} onChange={(value) => { setName(value); setSlug(suggestSlug(value)); }} />
      <Field label={t("Workspace address name")} name="setup-workspace-slug" value={slug} onChange={setSlug} />
      <Button className="button" type="submit" isDisabled={!available || busy || name.trim() === "" || slug === ""}>{t("Create workspace")}</Button>
    </form>
    <form className="form" onSubmit={(event) => void createProject(event)}>
      <h3>{t("Create first project")}</h3>
      <label className="field"><span className="field__label">{t("Workspace")}</span><select className="select__native" value={workspaceId} onChange={(event) => setWorkspaceId(event.currentTarget.value)}><option value="">{t("Select a workspace")}</option>{availableWorkspaces.map((item) => <option key={item.id} value={item.id}>{item.name}</option>)}</select></label>
      <Field label={t("Project name")} name="setup-project-name" value={projectName} onChange={setProjectName} />
      <Button className="button" type="submit" isDisabled={workspaceId === "" || busy || projectName.trim() === ""}>{t("Create project")}</Button>
    </form>
  </div>;
}

function OrganizationSetupControls({ organization }: { organization: Organization }) {
  const { t } = useI18n();
  const { api } = useAuth();
  const [seatLimit, setSeatLimit] = useState("");
  const [quotaBytes, setQuotaBytes] = useState("");
  const [mode, setMode] = useState<"all" | "assigned">("all");
  const [error, setError] = useState<unknown>(null);
  const [saved, setSaved] = useState(false);
  const [busy, setBusy] = useState(false);
  const [loaded, setLoaded] = useState(false);
  useEffect(() => {
    let live = true;
    setLoaded(false);
    setError(null);
    void (async () => {
      let cursor: string | undefined;
      let row;
      do {
        const page = await api.listAdminOrganizations(cursor);
        row = page.items.find((item) => item.id === organization.id);
        cursor = page.next_cursor ?? undefined;
      } while (row === undefined && cursor !== undefined);
      if (row === undefined) throw new Error(t("Organization settings could not be loaded."));
      const appSettings = await api.getOrganizationApps(organization.id);
      if (!live) return;
      setSeatLimit(row.seat_limit === null || row.seat_limit === undefined ? "" : String(row.seat_limit));
      setQuotaBytes(row.quota_bytes === null ? "" : String(row.quota_bytes));
      setMode(appSettings.mode);
      setLoaded(true);
    })().catch((failure: unknown) => { if (live) setError(failure); });
    return () => { live = false; };
  }, [api, organization.id, t]);
  async function save() {
    if (busy || !loaded) return;
    const seats = seatLimit === "" ? null : Number(seatLimit);
    const quota = quotaBytes === "" ? null : Number(quotaBytes);
    if ((seats !== null && (!Number.isSafeInteger(seats) || seats < 1)) || (quota !== null && (!Number.isSafeInteger(quota) || quota < 0))) {
      setError(new Error(t("Enter a valid non-negative whole number. Seats must be greater than zero.")));
      return;
    }
    setBusy(true); setError(null); setSaved(false);
    try {
      await api.updateAdminOrganizationSeats(organization.id, seats);
      await api.updateAdminOrganizationQuota(organization.id, quota);
      await api.updateOrganizationAppMode(organization.id, mode);
      setSaved(true);
    } catch (failure) { setError(failure); }
    finally { setBusy(false); }
  }
  return <section className="section">
    <h3>{t("Organization capacity and app access")}</h3>
    {error !== null && <ErrorNotice error={error} />}
    {saved && <p role="status" className="muted">{t("Settings saved.")}</p>}
    <Field label={t("Seat limit")} name="setup-seat-limit" value={seatLimit} onChange={setSeatLimit} required={false} />
    <Field label={t("Storage quota in bytes")} name="setup-storage-quota" value={quotaBytes} onChange={setQuotaBytes} required={false} />
    <label className="field"><span className="field__label">{t("App access mode")}</span><select className="select__native" value={mode} onChange={(event) => setMode(event.currentTarget.value as "all" | "assigned")}><option value="all">{t("All apps")}</option><option value="assigned">{t("Assigned apps only")}</option></select></label>
    {!loaded && error === null && <p role="status" className="muted">{t("Loading organization settings…")}</p>}
    <Button className="button" isDisabled={busy || !loaded} onPress={() => void save()}>{busy ? t("Saving…") : t("Save organization settings")}</Button>
  </section>;
}
