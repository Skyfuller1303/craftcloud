import { lazy, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Button } from "react-aria-components";
import type { Organization, Project, ServerInfo, SessionInfo, Workspace } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { ContextSelect } from "../components/ContextSelect.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { Badge } from "../components/Badge.tsx";
import { EmptyState } from "../components/EmptyState.tsx";
import { Alert, Field, ScreenHeading } from "../components/forms.tsx";
import { Preferences } from "../components/Preferences.tsx";
import { useI18n } from "../i18n/index.tsx";
import { useRoleNames } from "../roles.ts";
import type { ThemeChoice } from "../preferences.ts";
import { usePagedList } from "../usePagedList.ts";
import { ScreenBoundary } from "../components/ScreenBoundary.tsx";
import { ProfileSection } from "./ProfileSection.tsx";

// Every screen is its own chunk, loaded on first use, so the first paint only
// needs the shell (code splitting; the strict CSP forbids inline code, and a
// chunk is a plain same-origin script).
const ActivityScreen = lazy(() => import("./ActivityScreen.tsx").then((m) => ({ default: m.ActivityScreen })));
const FilesScreen = lazy(() => import("./FilesScreen.tsx").then((m) => ({ default: m.FilesScreen })));
const InvitationsScreen = lazy(() => import("./InvitationsScreen.tsx").then((m) => ({ default: m.InvitationsScreen })));
const MembersScreen = lazy(() => import("./MembersScreen.tsx").then((m) => ({ default: m.MembersScreen })));
const ProjectsScreen = lazy(() => import("./ProjectsScreen.tsx").then((m) => ({ default: m.ProjectsScreen })));
const SystemScreen = lazy(() => import("./SystemScreen.tsx").then((m) => ({ default: m.SystemScreen })));
const SharedWithMeScreen = lazy(() => import("./SharedWithMeScreen.tsx").then((m) => ({ default: m.SharedWithMeScreen })));
const TwoFactorSettingsScreen = lazy(() => import("./TwoFactorSettingsScreen.tsx").then((m) => ({ default: m.TwoFactorSettingsScreen })));
const AdminGrantsScreen = lazy(() => import("./AdminScreens.tsx").then((m) => ({ default: m.AdminGrantsScreen })));
const AdminLocksScreen = lazy(() => import("./AdminScreens.tsx").then((m) => ({ default: m.AdminLocksScreen })));
const AdminOperatorScreen = lazy(() => import("./AdminScreens.tsx").then((m) => ({ default: m.AdminOperatorScreen })));
const AdminOverview = lazy(() => import("./OverviewScreen.tsx").then((m) => ({ default: m.AdminOverview })));
const AdminPeopleScreen = lazy(() => import("./AdminScreens.tsx").then((m) => ({ default: m.AdminPeopleScreen })));
const AdminWorkspacesScreen = lazy(() => import("./AdminScreens.tsx").then((m) => ({ default: m.AdminWorkspacesScreen })));

/**
 * Dashboard sections. Settings follows with a later ticket and stays a
 * placeholder. "system" (CC-F15) is offered to the instance operator only.
 */
const CONTENT_SECTIONS = [
  "overview",
  "projects",
  "files",
  "members",
  "invitations",
  "activity",
  "shared",
] as const;
const ADMIN_SECTIONS = ["admin-overview", "admin-people", "admin-workspaces", "admin-grants", "admin-locks", "admin-activity", "org-settings", "settings", "instance-organizations", "instance-accounts", "system"] as const;
type Section = (typeof CONTENT_SECTIONS)[number] | (typeof ADMIN_SECTIONS)[number];

function sectionFromPath(path: string): Section {
  const tail = path.replace(/\/+$/, "").split("/").at(-1) ?? "";
  if (path.includes("/workspaces/") && (tail === "members" || tail === "invitations")) return tail;
  if (path.includes("/organizations/") && tail === "activity") return "admin-activity";
  const aliases: Record<string, Section> = { people: "admin-people", workspaces: "admin-workspaces", grants: "admin-grants", locks: "admin-locks", "org-settings": "org-settings", organizations: "instance-organizations", accounts: "instance-accounts", overview: "admin-overview", members: "admin-people", invitations: "admin-people", activity: "activity", settings: "settings", system: "system", projects: "projects", files: "files", shared: "shared" };
  return aliases[tail] ?? "overview";
}

function contextFromPath(path: string, kind: "organizations" | "workspaces"): string | null {
  const parts = path.split("/").filter(Boolean);
  const index = parts.indexOf(kind);
  return index >= 0 ? parts[index + 1] ?? null : null;
}

export function Shell({
  session,
  notice,
  preferredOrganizationId,
  theme,
  onThemeChange,
}: {
  session: SessionInfo;
  notice?: string | undefined;
  preferredOrganizationId?: string | null;
  theme: ThemeChoice;
  onThemeChange: (choice: ThemeChoice) => void;
}) {
  const { t } = useI18n();
  const { api, logout } = useAuth();
  const [section, setSection] = useState<Section>(() => sectionFromPath(window.location.pathname));
  const [organizationId, setOrganizationId] = useState<string | null>(() => contextFromPath(window.location.pathname, "organizations"));
  const [workspaceId, setWorkspaceId] = useState<string | null>(() => contextFromPath(window.location.pathname, "workspaces"));
  const [workspaceRouteResolved, setWorkspaceRouteResolved] = useState(() => !window.location.pathname.includes("/workspaces/"));
  const historyNavigation = useRef(false);
  const workspaceScopedRoute = useRef(window.location.pathname.startsWith("/admin/workspaces/"));
  const [projectId, setProjectId] = useState<string | null>(null);
  const names = useRoleNames();
  const [logoutError, setLogoutError] = useState<unknown>(null);
  const [serverInfo, setServerInfo] = useState<ServerInfo | null>(null);
  const [needsTwoFactorSetup, setNeedsTwoFactorSetup] = useState(false);
  const attemptLogout = useCallback(() => {
    setLogoutError(null);
    logout().catch((error: unknown) => setLogoutError(error));
  }, [logout]);

  const loadOrganizations = useCallback((cursor?: string) => api.listOrganizations(cursor), [api]);
  const organizations = usePagedList<Organization>(loadOrganizations);

  useEffect(() => {
    let current = true;
    api.serverInfo().then(
      (info) => { if (current) setServerInfo(info); },
      () => { if (current) setServerInfo({ features: [] }); },
    );
    return () => { current = false; };
  }, [api]);

  useEffect(() => {
    if (organizations.status !== "ready" || organizations.items.length === 0) {
      return;
    }
    setOrganizationId((current) => {
      if (current !== null && organizations.items.some((org) => org.id === current)) {
        return current;
      }
      const preferred = organizations.items.find((org) => org.id === preferredOrganizationId);
      return (preferred ?? organizations.items[0])?.id ?? null;
    });
  }, [organizations.status, organizations.items, preferredOrganizationId]);

  useEffect(() => {
    if (workspaceId === null || workspaceRouteResolved || organizations.status !== "ready") return;
    let live = true;
    void (async () => {
      let orgCursor: string | undefined;
      do {
        const orgPage = await api.listOrganizations(orgCursor);
        for (const org of orgPage.items) {
          let workspaceCursor: string | undefined;
          do {
            const page = await api.listWorkspaces(org.id, workspaceCursor);
            if (page.items.some((item) => item.id === workspaceId)) {
              if (live) { setOrganizationId(org.id); setWorkspaceRouteResolved(true); }
              return;
            }
            workspaceCursor = page.next_cursor ?? undefined;
          } while (workspaceCursor !== undefined);
        }
        orgCursor = orgPage.next_cursor ?? undefined;
      } while (orgCursor !== undefined);
      if (live) setWorkspaceRouteResolved(true);
    })().catch(() => undefined);
    return () => { live = false; };
  }, [api, workspaceId, organizations.status, workspaceRouteResolved]);

  const loadWorkspaces = useMemo(
    () =>
      organizationId === null
        ? null
        : (cursor?: string) => api.listWorkspaces(organizationId, cursor),
    [api, organizationId],
  );
  const workspaces = usePagedList<Workspace>(loadWorkspaces);

  useEffect(() => {
    if (organizations.status !== "ready") return;
    if (workspaceId !== null && !workspaceRouteResolved) return;
    if (workspaceId !== null && (!workspaceRouteResolved || workspaces.status !== "ready" || organizationId === null)) return;
    setWorkspaceId((current) => {
      if (workspaces.status !== "ready") {
        return current !== null && workspaces.items.some((ws) => ws.id === current) ? current : null;
      }
      if (current !== null && workspaces.items.some((ws) => ws.id === current)) {
        return current;
      }
      return workspaces.items[0]?.id ?? null;
    });
  }, [workspaces.status, workspaces.items]);

  const organization = organizations.items.find((org) => org.id === organizationId) ?? null;
  const workspace = workspaces.items.find((ws) => ws.id === workspaceId) ?? null;

  const scope = { userId: session.user_id, organization, workspace };
  const sectionLabel: Record<Section, string> = {
    overview: t("Overview"),
    projects: t("Projects"),
    files: t("Files"),
    members: t("Members"),
    invitations: t("Invitations"),
    settings: t("Settings"),
    activity: t("Activity"),
    shared: t("Shared with me"),
    system: t("System"),
    "admin-overview": t("Overview"),
    "admin-people": t("People"),
    "admin-workspaces": t("Workspaces"),
    "admin-grants": t("Grants"),
    "admin-locks": t("Locks"),
    "admin-activity": t("Activity"),
    "org-settings": t("Organization settings"),
    "instance-organizations": t("Organizations"),
    "instance-accounts": t("Accounts"),
  };
  const isOperator = session.instance_role === "operator";
  const isGuest = organization?.role === "guest";
  const orgAdmin = organization !== null && ["owner", "admin"].includes(organization.role);
  const workspaceAdmin = workspace?.workspace_role === "admin";
  const mayAdmin = orgAdmin || workspaceAdmin;
  useEffect(() => {
    const onPop = () => {
      const path = window.location.pathname;
      historyNavigation.current = true;
      workspaceScopedRoute.current = path.startsWith("/admin/workspaces/");
      setSection(sectionFromPath(path));
      setOrganizationId(contextFromPath(path, "organizations"));
      setWorkspaceId(contextFromPath(path, "workspaces"));
      setWorkspaceRouteResolved(!path.includes("/workspaces/"));
    };
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);
  useEffect(() => {
    if (historyNavigation.current) {
      historyNavigation.current = false;
      return;
    }
    if (organizations.status !== "ready") return;
    if (workspaceId !== null && (!workspaceRouteResolved || workspaces.status !== "ready" || organizationId === null)) return;
    const route = section.startsWith("instance-") || section === "system"
      ? `/admin/${section === "instance-organizations" ? "organizations" : section === "instance-accounts" ? "accounts" : "system"}`
      : section.startsWith("admin-") || section === "org-settings"
        ? workspaceAdmin && (workspaceScopedRoute.current || !orgAdmin) && ["admin-grants", "admin-locks"].includes(section)
          ? `/admin/workspaces/${workspaceId}/${section === "admin-grants" ? "grants" : "locks"}`
          : `/admin/organizations/${organizationId ?? ""}/${section.replace(/^admin-/, "")}`
        : section === "activity" && isOperator ? "/admin/activity"
        : section === "overview" || section === "settings" ? `/admin/${section === "overview" ? "" : "settings"}`
          : ["members", "invitations"].includes(section) && workspaceAdmin ? `/admin/workspaces/${workspaceId}/${section}`
            : `/admin/organizations/${organizationId ?? ""}/${section}`;
    if (window.location.pathname !== route) window.history.pushState(null, "", route);
  }, [section, organizationId, workspaceId, orgAdmin, workspaceAdmin, organizations.status, workspaces.status, workspaceRouteResolved]);
  useEffect(() => {
    let current = true;
    if (organization?.require_two_factor !== true) {
      setNeedsTwoFactorSetup(false);
      return () => { current = false; };
    }
    api.getTwoFactorStatus().then(
      (status) => { if (current) { setNeedsTwoFactorSetup(!status.enabled); } },
      () => { if (current) setNeedsTwoFactorSetup(false); },
    );
    return () => { current = false; };
  }, [api, organization?.id, organization?.require_two_factor]);
  const refreshTwoFactor = useCallback((enabled: boolean) => {
    setNeedsTwoFactorSetup(organization?.require_two_factor === true && !enabled);
    workspaces.refresh();
  }, [organization?.require_two_factor, workspaces.refresh]);
  useEffect(() => {
    if (needsTwoFactorSetup && section !== "settings" && section !== "system" && section !== "overview" && !(isOperator && section === "activity")) {
      setSection("settings");
    }
  }, [isOperator, needsTwoFactorSetup, section]);
  const limitedNavigation = isGuest || organization === null;
  useEffect(() => {
    if (isGuest && !(isOperator && (section.startsWith("instance-") || section === "system" || section === "activity")) && !["overview", "shared", "settings"].includes(section)) {
      setSection("shared");
    }
  }, [isGuest, isOperator, section]);
  const sharingEnabled = serverInfo?.features.includes("sharing.grants") === true;
  const visibleContentSections = CONTENT_SECTIONS.filter((key) =>
    key === "overview"
      // A guest has nothing to see on the account overview: "Shared with me" is the home page.
      ? !orgAdmin && !(isGuest && sharingEnabled && !isOperator)
      : key === "shared"
      ? sharingEnabled
      : key === "members"
        ? workspace !== null
        : key === "invitations"
          ? workspaceAdmin
      : key === "activity"
        ? false
      : needsTwoFactorSetup
        ? false
        : !limitedNavigation,
  );
  const visibleAdminSections = ADMIN_SECTIONS.filter((key) =>
    key === "settings" ? true : key.startsWith("instance-") || key === "system" ? isOperator :
      ["admin-grants", "admin-locks"].includes(key) ? mayAdmin : orgAdmin,
  );
  const validSection = visibleAdminSections.includes(section as (typeof ADMIN_SECTIONS)[number]) || visibleContentSections.includes(section as (typeof CONTENT_SECTIONS)[number]) || (section === "overview" && orgAdmin) || (section === "activity" && isOperator);
  useEffect(() => {
    if (workspaceId !== null && !workspaceRouteResolved) return;
    if (organizations.status === "ready" && organizationId !== null && organization === null) return;
    if (organizations.status === "ready" && !validSection) {
      setSection(isOperator ? "instance-organizations" : isGuest && sharingEnabled ? "shared" : "overview");
    }
  }, [validSection, isOperator, isGuest, sharingEnabled, organizations.status, organizationId, organization]);

  return (
    <div className="shell">
      <header className="shell__header">
        <div className="shell__brand">
          <span className="visually-hidden">{t("CraftCloud admin")}</span>
          <span aria-hidden="true" className="shell__brand-tag">CRAFTCLOUD</span>
          <span aria-hidden="true" className="shell__brand-label">ADMIN</span>
        </div>
        <div className="shell__context">
          <ContextSelect
            label={t("Organization")}
            placeholder={organizations.status === "loading" ? t("Loading…") : t("None available")}
            choices={organizations.items.map((org) => ({ id: org.id, label: org.name }))}
            selectedId={organizationId}
            onSelect={(id) => {
              workspaceScopedRoute.current = false;
              setOrganizationId(id);
              setWorkspaceId(null);
              setProjectId(null);
            }}
            isDisabled={organizations.items.length === 0}
          />
          <ContextSelect
            label={t("Workspace")}
            placeholder={
              organizationId === null || workspaces.status === "loading"
                ? t("Loading…")
                : t("None available")
            }
            choices={workspaces.items.map((ws) => ({ id: ws.id, label: ws.name }))}
            selectedId={workspaceId}
            onSelect={(id) => {
              workspaceScopedRoute.current = false;
              setWorkspaceId(id);
              setProjectId(null);
            }}
            isDisabled={workspaces.items.length === 0}
          />
        </div>
        <Preferences theme={theme} onThemeChange={onThemeChange} />
        <div className="shell__user">
          <span className="shell__user-name">{session.display_name}</span>
          <Button className="button" onPress={attemptLogout}>
            {t("Log out")}
          </Button>
        </div>
      </header>

      <div className="shell__body">
        <nav className="shell__nav" aria-label={t("Main navigation")}>
          <ul>
            {visibleContentSections.map((key) => (
              <li key={key}>
                <button
                  type="button"
                  className="nav__item"
                  aria-current={section === key ? "page" : undefined}
                  onClick={() => setSection(key)}
                >
                  {sectionLabel[key]}
                </button>
              </li>
            ))}
            {visibleAdminSections.includes("settings") && (
              <li>
                <button type="button" className="nav__item" aria-current={section === "settings" ? "page" : undefined} onClick={() => setSection("settings")}>
                  {sectionLabel.settings}
                </button>
              </li>
            )}
          </ul>
          {visibleAdminSections.some((key) => ["admin-overview", "admin-people", "admin-workspaces", "admin-grants", "admin-locks", "admin-activity", "org-settings"].includes(key)) && <>
            <h2 className="shell__nav-heading">{t("Administration")}</h2>
            <ul>
              {visibleAdminSections.filter((key) => ["admin-overview", "admin-people", "admin-workspaces", "admin-grants", "admin-locks", "admin-activity", "org-settings"].includes(key)).map((key) => (
                <li key={key}>
                  <button type="button" className="nav__item" aria-current={section === key ? "page" : undefined} onClick={() => setSection(key)}>
                    {sectionLabel[key]}
                  </button>
                </li>
              ))}
            </ul>
          </>}
          {visibleAdminSections.some((key) => ["instance-organizations", "instance-accounts", "system"].includes(key)) && <>
            <h2 className="shell__nav-heading">{t("Instance")}</h2>
            <ul>
              {visibleAdminSections.filter((key) => ["instance-organizations", "instance-accounts", "system"].includes(key)).map((key) => (
              <li key={key}>
                <button
                  type="button"
                  className="nav__item"
                  aria-current={section === key ? "page" : undefined}
                  onClick={() => setSection(key)}
                >
                  {sectionLabel[key]}
                </button>
              </li>
              ))}
            </ul>
          </>}
        </nav>

        <main id="main" className="shell__main" tabIndex={-1}>
          {logoutError !== null && (
            <ErrorNotice
              error={logoutError}
              context={t("Logging out did not work. You are still logged in on this device.")}
              onRetry={attemptLogout}
              onReload={() => window.location.reload()}
            />
          )}
          {notice !== undefined && <Alert tone="success">{notice}</Alert>}
          {needsTwoFactorSetup && organization !== null && (
            <Alert tone="info">
              {t("This organization requires two-factor authentication. Set it up to continue here; other organizations remain available.")}
              <Button className="button button--sm" onPress={() => setSection("settings")}>
                {t("Open two-factor settings")}
              </Button>
            </Alert>
          )}
          <ScreenHeading key={section}>{sectionLabel[section]}</ScreenHeading>

          {organizations.status === "error" && (
            <ErrorNotice error={organizations.error} onRetry={organizations.reload} onReload={organizations.reload} />
          )}
          {workspaces.status === "error" && (
            <ErrorNotice error={workspaces.error} onRetry={workspaces.reload} onReload={workspaces.reload} />
          )}
          {organizations.status === "loading" && (
            <p role="status" className="muted">
              {t("Loading organizations…")}
            </p>
          )}
          {organizations.status === "ready" && organizations.items.length === 0 && (
            <p className="muted">
              {t("You are not a member of any organization yet. Ask an organization owner for an invitation.")}
            </p>
          )}
          {organizations.hasMore && (
            <Button
              className="button"
              onPress={organizations.loadMore}
              isDisabled={organizations.loadingMore}
            >
              {t("Load more organizations")}
            </Button>
          )}
          {workspaces.hasMore && (
            <Button
              className="button"
              onPress={workspaces.loadMore}
              isDisabled={workspaces.loadingMore}
            >
              {t("Load more workspaces")}
            </Button>
          )}

          <ScreenBoundary>
          {section === "admin-overview" ? (
            <AdminOverview key={organizationId} organization={orgAdmin ? organization : null} workspaces={workspaces.items} />
          ) : section === "admin-people" ? (
            <AdminPeopleScreen key={organizationId} organization={orgAdmin ? organization : null} onChanged={organizations.refresh} />
          ) : section === "admin-workspaces" ? (
            <AdminWorkspacesScreen key={organizationId} organization={orgAdmin ? organization : null} workspaces={workspaces.items} hasMore={workspaces.hasMore} onLoadMore={workspaces.loadMore} onRefresh={workspaces.refresh} />
          ) : section === "admin-grants" ? (
            mayAdmin ? <AdminGrantsScreen key={`${organizationId}:${workspaceId}`} organization={organization} workspaces={workspaces.items} /> : <EmptyState>{t("This view is not available yet.")}</EmptyState>
          ) : section === "admin-locks" ? (
            mayAdmin ? <AdminLocksScreen key={`${organizationId}:${workspaceId}`} organization={organization} workspaces={workspaces.items} /> : <EmptyState>{t("This view is not available yet.")}</EmptyState>
          ) : section === "admin-activity" ? (
            orgAdmin ? <ActivityScreen key={organizationId} scope={scope} isOperator={isOperator} workspaces={workspaces.items} /> : <EmptyState>{t("This view is not available yet.")}</EmptyState>
          ) : section === "org-settings" ? (
            orgAdmin && organization !== null ? <OrganizationSettingsScreen organization={organization} onUpdated={(updated) => organizations.mutate((items) => items.map((item) => item.id === updated.id ? updated : item))} /> : <EmptyState>{t("This view is not available yet.")}</EmptyState>
          ) : section === "instance-organizations" ? (
            isOperator ? <AdminOperatorScreen key="organizations" section="organizations" /> : <EmptyState>{t("This view is not available yet.")}</EmptyState>
          ) : section === "instance-accounts" ? (
            isOperator ? <AdminOperatorScreen key="accounts" section="accounts" /> : <EmptyState>{t("This view is not available yet.")}</EmptyState>
          ) : limitedNavigation && section !== "overview" && section !== "shared" && section !== "settings" && !(isOperator && (section === "system" || section === "activity")) ? (
            <EmptyState>{t("This view is not available yet.")}</EmptyState>
          ) : section === "overview" ? (
            <dl className="facts">
              <dt>{t("Signed in as")}</dt>
              <dd>
                <span>{session.display_name}<span className="cell__sub">{session.email}</span></span>
                {session.instance_role === "operator" && <Badge>{t("Instance operator")}</Badge>}
              </dd>
              <dt>{t("Organization")}</dt>
              <dd>
                {organization === null
                  ? t("None selected")
                  : <>{organization.name}<Badge>{names.orgRole(organization.role)}</Badge></>}
              </dd>
              <dt>{t("Workspace")}</dt>
              <dd>
                {workspace === null
                  ? workspaces.status === "ready" && organization !== null
                    ? t("This organization has no workspace you can see.")
                    : t("None selected")
                  : <>{workspace.name}<Badge>{names.workspaceRole(workspace.workspace_role)}</Badge></>}
              </dd>
            </dl>
          ) : section === "projects" ? (
            <ProjectsScreen
              scope={scope}
              onOpenProject={(project: Project) => {
                setProjectId(project.id);
                setSection("files");
              }}
            />
          ) : section === "files" ? (
            <FilesScreen scope={scope} projectId={projectId} onProjectChange={setProjectId} />
          ) : section === "members" ? (
            <MembersScreen
              scope={scope}
              isOperator={isOperator}
              onOrganizationsChanged={organizations.refresh}
              onWorkspacesChanged={workspaces.refresh}
            />
          ) : section === "invitations" ? (
            <InvitationsScreen key={organizationId} scope={scope} workspaces={workspaces.items} />
          ) : section === "activity" ? (
            <ActivityScreen
              key={organizationId}
              scope={scope}
              isOperator={isOperator}
              workspaces={workspaces.items}
            />
          ) : section === "settings" ? (
            <div className="stack">
              <ProfileSection session={session} />
              <TwoFactorSettingsScreen
                organization={organization}
                onFactorChanged={refreshTwoFactor}
                onOrganizationUpdated={(updated) => {
                  organizations.mutate((items) => items.map((item) => item.id === updated.id ? updated : item));
                }}
              />
            </div>
          ) : section === "system" ? (
            <>
              <SystemScreen isOperator={isOperator} />
              {isOperator && <section className="section">
                <h2 className="section__title">{t("Instance activity")}</h2>
                <Button className="button" onPress={() => setSection("activity")}>{t("Open activity log")}</Button>
              </section>}
            </>
          ) : section === "shared" ? (
            <SharedWithMeScreen organizationId={organizationId} />
          ) : (
            <EmptyState>{t("This view is not available yet.")}</EmptyState>
          )}
          </ScreenBoundary>
        </main>
      </div>
    </div>
  );
}

function OrganizationSettingsScreen({ organization, onUpdated }: { organization: Organization; onUpdated: (updated: Organization) => void }) {
  const { t } = useI18n();
  const { api } = useAuth();
  const [name, setName] = useState(organization.name);
  const [invites, setInvites] = useState(organization.workspace_admin_invites_enabled);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [twoFactorAvailable, setTwoFactorAvailable] = useState(false);
  useEffect(() => {
    setName(organization.name);
    setInvites(organization.workspace_admin_invites_enabled);
  }, [organization]);
  useEffect(() => {
    let live = true;
    void api.serverInfo().then(
      (info) => { if (live) setTwoFactorAvailable(info.feature_flags?.two_factor === true); },
      () => { if (live) setTwoFactorAvailable(false); },
    );
    return () => { live = false; };
  }, [api]);
  async function save(patch: { name?: string; workspace_admin_invites_enabled?: boolean }) {
    if (pending) return;
    setPending(true);
    setError(null);
    try {
      const updated = await api.updateOrganizationSettings(organization.id, organization.version, patch);
      onUpdated(updated);
    } catch (failure) {
      setError(failure);
      setName(organization.name);
      setInvites(organization.workspace_admin_invites_enabled);
    } finally {
      setPending(false);
    }
  }
  return (
    <div className="stack">
      <section className="section">
        <h2 className="section__title">{t("Organization settings")}</h2>
        {error !== null && <ErrorNotice error={error} />}
        <Field label={t("Organization name")} name="organization-name" value={name} onChange={setName} />
        <Button
          className="button button--primary"
          isDisabled={pending || name.trim() === "" || name === organization.name}
          onPress={() => void save({ name: name.trim() })}
        >
          {t("Save name")}
        </Button>
        <label className="checkbox">
          <input
            type="checkbox"
            checked={invites}
            disabled={pending}
            onChange={(event) => {
              setInvites(event.currentTarget.checked);
              void save({ workspace_admin_invites_enabled: event.currentTarget.checked });
            }}
          />
          <span>{t("Allow workspace admins to invite people")}</span>
        </label>
      </section>
      {twoFactorAvailable
        ? <TwoFactorSettingsScreen organization={organization} onFactorChanged={() => undefined} onOrganizationUpdated={onUpdated} />
        : <p className="muted">{t("Two-factor policy is unavailable on this server.")}</p>}
    </div>
  );
}
