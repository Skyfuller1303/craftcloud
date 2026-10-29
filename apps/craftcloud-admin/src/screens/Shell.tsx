import { useCallback, useEffect, useMemo, useState } from "react";
import { Button } from "react-aria-components";
import type { Organization, Project, SessionInfo, Workspace } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { ContextSelect } from "../components/ContextSelect.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { Alert, ScreenHeading } from "../components/forms.tsx";
import { Preferences } from "../components/Preferences.tsx";
import { useI18n } from "../i18n/index.tsx";
import { useRoleNames } from "../roles.ts";
import type { ThemeChoice } from "../preferences.ts";
import { usePagedList } from "../usePagedList.ts";
import { ActivityScreen } from "./ActivityScreen.tsx";
import { FilesScreen } from "./FilesScreen.tsx";
import { InvitationsScreen } from "./InvitationsScreen.tsx";
import { MembersScreen } from "./MembersScreen.tsx";
import { ProjectsScreen } from "./ProjectsScreen.tsx";

/** Dashboard sections. Settings follows with a later ticket and stays a placeholder. */
const SECTIONS = ["overview", "projects", "files", "members", "invitations", "settings", "activity"] as const;
type Section = (typeof SECTIONS)[number];

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
  const [section, setSection] = useState<Section>("overview");
  const [organizationId, setOrganizationId] = useState<string | null>(null);
  const [workspaceId, setWorkspaceId] = useState<string | null>(null);
  const [projectId, setProjectId] = useState<string | null>(null);
  const names = useRoleNames();

  const loadOrganizations = useCallback((cursor?: string) => api.listOrganizations(cursor), [api]);
  const organizations = usePagedList<Organization>(loadOrganizations);

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

  const loadWorkspaces = useMemo(
    () =>
      organizationId === null
        ? null
        : (cursor?: string) => api.listWorkspaces(organizationId, cursor),
    [api, organizationId],
  );
  const workspaces = usePagedList<Workspace>(loadWorkspaces);

  useEffect(() => {
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
  };

  return (
    <div className="shell">
      <header className="shell__header">
        <div className="shell__brand">{t("CraftCloud admin")}</div>
        <div className="shell__context">
          <ContextSelect
            label={t("Organization")}
            placeholder={organizations.status === "loading" ? t("Loading…") : t("None available")}
            choices={organizations.items.map((org) => ({ id: org.id, label: org.name }))}
            selectedId={organizationId}
            onSelect={(id) => {
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
              setWorkspaceId(id);
              setProjectId(null);
            }}
            isDisabled={workspaces.items.length === 0}
          />
        </div>
        <Preferences theme={theme} onThemeChange={onThemeChange} />
        <div className="shell__user">
          <span className="shell__user-name">{session.display_name}</span>
          <Button className="button" onPress={() => void logout()}>
            {t("Log out")}
          </Button>
        </div>
      </header>

      <div className="shell__body">
        <nav className="shell__nav" aria-label={t("Main navigation")}>
          <ul>
            {SECTIONS.map((key) => (
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
        </nav>

        <main id="main" className="shell__main" tabIndex={-1}>
          {notice !== undefined && <Alert tone="success">{notice}</Alert>}
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

          {section === "overview" ? (
            <dl className="facts">
              <dt>{t("Signed in as")}</dt>
              <dd>
                {session.display_name} ({session.email})
                {session.instance_role === "operator" ? ` · ${t("Instance operator")}` : ""}
              </dd>
              <dt>{t("Organization")}</dt>
              <dd>
                {organization === null
                  ? t("None selected")
                  : `${organization.name} · ${names.orgRole(organization.role)}`}
              </dd>
              <dt>{t("Workspace")}</dt>
              <dd>
                {workspace === null
                  ? workspaces.status === "ready" && organization !== null
                    ? t("This organization has no workspace you can see.")
                    : t("None selected")
                  : `${workspace.name} · ${names.workspaceRole(workspace.workspace_role)}`}
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
              onOrganizationsChanged={organizations.refresh}
              onWorkspacesChanged={workspaces.refresh}
            />
          ) : section === "invitations" ? (
            <InvitationsScreen key={organizationId} scope={scope} workspaces={workspaces.items} />
          ) : section === "activity" ? (
            <ActivityScreen
              key={organizationId}
              scope={scope}
              isOperator={session.instance_role === "operator"}
              workspaces={workspaces.items}
            />
          ) : (
            <p className="muted">{t("This view is not available yet.")}</p>
          )}
        </main>
      </div>
    </div>
  );
}
