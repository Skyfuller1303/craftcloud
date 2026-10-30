import type { OrgRole, WorkspaceRole } from "./api/types.ts";
import { useI18n } from "./i18n/index.tsx";

/** Translated role names, shared by every view. */
export function useRoleNames(): {
  orgRole: (role: OrgRole) => string;
  workspaceRole: (role: WorkspaceRole | null) => string;
} {
  const { t } = useI18n();
  return {
    orgRole: (role) => ({ owner: t("Owner"), admin: t("Admin"), member: t("Member"), guest: t("Guest") })[role],
    workspaceRole: (role) =>
      role === null
        ? t("No workspace role")
        : { admin: t("Admin"), editor: t("Editor"), viewer: t("Viewer") }[role],
  };
}
