import { useCallback, useEffect, useMemo, useState } from "react";
import { Button } from "react-aria-components";
import { describeError } from "../api/errors.ts";
import type { OrgRole, Organization, SessionInfo, Workspace, WorkspaceRole } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { ContextSelect } from "../components/ContextSelect.tsx";
import { Alert, ScreenHeading } from "../components/forms.tsx";
import { Preferences } from "../components/Preferences.tsx";
import { useI18n } from "../i18n/index.tsx";
import type { ThemeChoice } from "../preferences.ts";
import { usePagedList } from "../usePagedList.ts";

/** Sections of the later dashboard views (CC-P03-03 and following); placeholders for now. */
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

  const orgRole = (role: OrgRole): string =>
    ({ owner: t("Owner"), admin: t("Admin"), member: t("Member") })[role];
  const workspaceRole = (role: WorkspaceRole | null): string =>
    role === null
      ? t("No workspace role")
      : { admin: t("Admin"), editor: t("Editor"), viewer: t("Viewer") }[role];
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
            onSelect={setWorkspaceId}
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
            <Alert>
              {describeError(t, organizations.error)}{" "}
              <Button className="button button--link" onPress={organizations.reload}>
                {t("Try again")}
              </Button>
            </Alert>
          )}
          {workspaces.status === "error" && (
            <Alert>
              {describeError(t, workspaces.error)}{" "}
              <Button className="button button--link" onPress={workspaces.reload}>
                {t("Try again")}
              </Button>
            </Alert>
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
                  : `${organization.name} · ${orgRole(organization.role)}`}
              </dd>
              <dt>{t("Workspace")}</dt>
              <dd>
                {workspace === null
                  ? workspaces.status === "ready" && organization !== null
                    ? t("This organization has no workspace you can see.")
                    : t("None selected")
                  : `${workspace.name} · ${workspaceRole(workspace.workspace_role)}`}
              </dd>
            </dl>
          ) : (
            <p className="muted">{t("This view is not available yet.")}</p>
          )}
        </main>
      </div>
    </div>
  );
}
