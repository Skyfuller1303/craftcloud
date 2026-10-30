import { useCallback, useEffect, useMemo, useState } from "react";
import { Button } from "react-aria-components";
import type { IssuedPasswordReset, OrgMember, Page, WorkspaceMember, WorkspaceRole } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { useAnnounce } from "../components/Announcer.tsx";
import { Badge } from "../components/Badge.tsx";
import { NativeSelect } from "../components/ContextSelect.tsx";
import { AppModal, ConfirmDialog } from "../components/Dialog.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { Alert } from "../components/forms.tsx";
import { ListStatus, LoadMoreButton } from "../components/ListStatus.tsx";
import { RoleDialog } from "../components/RoleDialog.tsx";
import { formatDateTime } from "../format.ts";
import { useAction } from "../hooks.ts";
import { useI18n } from "../i18n/index.tsx";
import {
  assignableOrgRoles,
  assignableWorkspaceRoles,
  canManageWorkspaceMembers,
  canRemoveOrgMember,
  canRemoveWorkspaceMember,
  grantableWorkspaceRoles,
} from "../permissions.ts";
import type { Scope } from "../permissions.ts";
import { useRoleNames } from "../roles.ts";
import { usePagedList } from "../usePagedList.ts";

type Dialog =
  | { kind: "org-role"; member: OrgMember }
  | { kind: "org-remove"; member: OrgMember }
  | { kind: "ws-role"; member: WorkspaceMember }
  | { kind: "ws-remove"; member: WorkspaceMember }
  | { kind: "account-reset"; member: OrgMember }
  | { kind: "ws-add" };

export function MembersScreen({
  scope,
  onOrganizationsChanged,
  onWorkspacesChanged,
  isOperator,
}: {
  scope: Scope;
  /** The own organization role or membership changed (role change, leaving). */
  onOrganizationsChanged: () => void;
  /** The own workspace role or membership changed, or the own organization role did. */
  onWorkspacesChanged: () => void;
  isOperator: boolean;
}) {
  const { t, language } = useI18n();
  const { api } = useAuth();
  const announce = useAnnounce();
  const names = useRoleNames();
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [resetTwoFactor, setResetTwoFactor] = useState(false);
  const [resetResult, setResetResult] = useState<{ member: OrgMember; result: IssuedPasswordReset } | null>(null);
  const [resetError, setResetError] = useState<unknown>(null);
  const action = useAction();

  const orgId = scope.organization?.id ?? null;
  const workspaceId = scope.workspace?.id ?? null;
  const orgActor = scope.organization?.role;

  const loadOrgMembers = useMemo(
    () => (orgId === null ? null : (cursor?: string) => api.listOrgMembers(orgId, cursor)),
    [api, orgId],
  );
  const orgMembers = usePagedList<OrgMember>(loadOrgMembers);
  const loadWsMembers = useMemo(
    () =>
      workspaceId === null ? null : (cursor?: string) => api.listWorkspaceMembers(workspaceId, cursor),
    [api, workspaceId],
  );
  const wsMembers = usePagedList<WorkspaceMember>(loadWsMembers);

  const close = useCallback(() => {
    setDialog(null);
    action.clear();
    setResetTwoFactor(false);
    setResetError(null);
  }, [action]);

  if (scope.organization === null || orgId === null) {
    return <p className="muted">{t("Select an organization first.")}</p>;
  }

  const canManageWs = canManageWorkspaceMembers(scope);

  async function issueMemberReset(member: OrgMember) {
    if (action.pending) return;
    setResetError(null);
    let requestError: unknown = null;
    const result = await action.run(async () => {
      try {
        return await api.issuePasswordReset(member.user_id, {
          reset_two_factor: resetTwoFactor,
        });
      } catch (error) {
        requestError = error;
        throw error;
      }
    });
    if (result === undefined) {
      setResetError(requestError ?? action.error);
      return;
    }
    action.clear();
    setDialog(null);
    setResetTwoFactor(false);
    setResetResult({ member, result });
  }

  return (
    <>
      <section className="section" aria-labelledby="org-members-title">
        <h2 id="org-members-title" className="section__title">
          {t("Organization members")}
        </h2>
        <ListStatus list={orgMembers} empty={t("This organization has no members.")} loadingLabel={t("Loading members…")} />
        {orgMembers.items.length > 0 && (
          <div className="table-wrap">
            <table className="table">
              <caption className="visually-hidden">{t("Organization members")}</caption>
              <thead>
                <tr>
                  <th scope="col">{t("Name")}</th>
                  <th scope="col" className="col--optional">{t("E-mail address")}</th>
                  <th scope="col">{t("Role")}</th>
                  <th scope="col">{t("Joined")}</th>
                  <th scope="col">{t("Actions")}</th>
                </tr>
              </thead>
              <tbody>
                {orgMembers.items.map((member) => {
                  const roles = assignableOrgRoles(orgActor, member);
                  const removable = canRemoveOrgMember(orgActor, scope.userId, member);
                  const self = member.user_id === scope.userId;
                  return (
                    <tr key={member.user_id}>
                      <th scope="row">{member.display_name}<span className="cell__sub cell__sub--email">{member.email}</span></th>
                      <td className="col--optional" data-label={t("E-mail address")}>{member.email}</td>
                      <td data-label={t("Role")}><Badge>{names.orgRole(member.role)}</Badge></td>
                      <td className="numeric" data-label={t("Joined")}>{formatDateTime(language, member.joined_at)}</td>
                      <td data-label={t("Actions")}>
                        <div className="row-actions">
                          {roles.length > 0 && (
                            <Button
                              className="button button--sm"
                              aria-label={t("Change role: {name}", { name: member.display_name })}
                              onPress={() => setDialog({ kind: "org-role", member })}
                            >
                              {t("Change role")}
                            </Button>
                          )}
                          {removable && (
                            <Button
                              className="button button--danger-quiet button--sm"
                              aria-label={
                                self
                                  ? t("Leave organization")
                                  : t("Remove {name}", { name: member.display_name })
                              }
                              onPress={() => setDialog({ kind: "org-remove", member })}
                            >
                              {self ? t("Leave") : t("Remove")}
                            </Button>
                          )}
                          {isOperator && (
                            <Button
                              className="button button--sm"
                              aria-label={t("Reset account: issue a password reset link for {name}", { name: member.display_name })}
                              onPress={() => {
                                setResetTwoFactor(false);
                                setResetError(null);
                                setDialog({ kind: "account-reset", member });
                              }}
                            >
                              {t("Reset account")}
                            </Button>
                          )}
                        </div>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
        <LoadMoreButton list={orgMembers} />
      </section>

      <section className="section" aria-labelledby="ws-members-title">
        <h2 id="ws-members-title" className="section__title">
          {t("Workspace members")}
        </h2>
        {scope.workspace === null || workspaceId === null ? (
          <p className="muted">{t("Select a workspace to see its members.")}</p>
        ) : (
          <>
            <p className="muted">{scope.workspace.name}</p>
            {canManageWs && (
              <Button className="button" onPress={() => setDialog({ kind: "ws-add" })}>
                {t("Add to workspace")}
              </Button>
            )}
            <ListStatus
              list={wsMembers}
              empty={t("This workspace has no members.")}
              loadingLabel={t("Loading members…")}
            />
            {wsMembers.items.length > 0 && (
              <div className="table-wrap">
                <table className="table">
                  <caption className="visually-hidden">{t("Workspace members")}</caption>
                  <thead>
                    <tr>
                      <th scope="col">{t("Name")}</th>
                      <th scope="col" className="col--optional">{t("E-mail address")}</th>
                      <th scope="col">{t("Role")}</th>
                      <th scope="col">{t("Joined")}</th>
                      <th scope="col">{t("Actions")}</th>
                    </tr>
                  </thead>
                  <tbody>
                    {wsMembers.items.map((member) => {
                      const roles = assignableWorkspaceRoles(scope, member);
                      const removable = canRemoveWorkspaceMember(scope, member);
                      const self = member.user_id === scope.userId;
                      return (
                        <tr key={member.user_id}>
                          <th scope="row">{member.display_name}<span className="cell__sub cell__sub--email">{member.email}</span></th>
                          <td className="col--optional" data-label={t("E-mail address")}>{member.email}</td>
                          <td data-label={t("Role")}><Badge>{names.workspaceRole(member.role)}</Badge></td>
                          <td className="numeric" data-label={t("Joined")}>{formatDateTime(language, member.joined_at)}</td>
                          <td data-label={t("Actions")}>
                            <div className="row-actions">
                              {roles.length > 0 && (
                                <Button
                                  className="button button--sm"
                                  aria-label={t("Change role in workspace: {name}", {
                                    name: member.display_name,
                                  })}
                                  onPress={() => setDialog({ kind: "ws-role", member })}
                                >
                                  {t("Change role")}
                                </Button>
                              )}
                              {removable && (
                                <Button
                                  className="button button--danger-quiet button--sm"
                                  aria-label={
                                    self
                                      ? t("Leave workspace")
                                      : t("Remove {name} from the workspace", {
                                          name: member.display_name,
                                        })
                                  }
                                  onPress={() => setDialog({ kind: "ws-remove", member })}
                                >
                                  {self ? t("Leave") : t("Remove")}
                                </Button>
                              )}
                            </div>
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            )}
              <LoadMoreButton list={wsMembers} />
          </>
        )}
      </section>

      {dialog?.kind === "org-role" && (
        <RoleDialog
          title={t("Change role")}
          who={`${dialog.member.display_name} (${dialog.member.email})`}
          current={dialog.member.role}
          choices={assignableOrgRoles(orgActor, dialog.member)}
          roleName={names.orgRole}
          onReload={() => {
            orgMembers.refresh();
            close();
          }}
          onClose={close}
          onSave={async (role) => {
            const updated = await api.changeOrgMemberRole(orgId, dialog.member.user_id, role);
            orgMembers.mutate((items) =>
              items.map((item) => (item.user_id === updated.user_id ? updated : item)),
            );
            announce(
              t("{name} is now {role}.", { name: updated.display_name, role: names.orgRole(updated.role) }),
            );
            if (updated.user_id === scope.userId) {
              // The own role changed what may be done here and in the workspaces list.
              onOrganizationsChanged();
              onWorkspacesChanged();
            }
          }}
        />
      )}

      {dialog?.kind === "org-remove" && (
        <ConfirmDialog
          title={dialog.member.user_id === scope.userId ? t("Leave organization") : t("Remove member")}
          message={
            dialog.member.user_id === scope.userId
              ? t("You lose access to this organization and all its workspaces.")
              : t("{name} loses access to this organization and all its workspaces.", {
                  name: dialog.member.display_name,
                })
          }
          confirmLabel={dialog.member.user_id === scope.userId ? t("Leave") : t("Remove")}
          danger
          busy={action.pending}
          error={action.failed ? action.error : null}
          onReload={() => {
            orgMembers.refresh();
            close();
          }}
          onClose={close}
          onConfirm={() => {
            const member = dialog.member;
            void action
              .run(async () => {
                await api.removeOrgMember(orgId, member.user_id);
                return true;
              })
              .then((done) => {
                if (done !== true) return;
                orgMembers.mutate((items) => items.filter((item) => item.user_id !== member.user_id));
                announce(t("{name} was removed.", { name: member.display_name }));
                close();
                if (member.user_id === scope.userId) {
                  onOrganizationsChanged();
                  onWorkspacesChanged();
                }
              });
          }}
        />
      )}

      {dialog?.kind === "ws-role" && scope.workspace !== null && (
        <RoleDialog<WorkspaceRole>
          title={t("Change workspace role")}
          who={`${dialog.member.display_name} (${dialog.member.email})`}
          current={dialog.member.role}
          choices={assignableWorkspaceRoles(scope, dialog.member)}
          roleName={names.workspaceRole}
          onReload={() => {
            wsMembers.refresh();
            close();
          }}
          onClose={close}
          onSave={async (role) => {
            const updated = await api.changeWorkspaceMemberRole(
              scope.workspace!.id,
              dialog.member.user_id,
              role,
            );
            wsMembers.mutate((items) =>
              items.map((item) => (item.user_id === updated.user_id ? updated : item)),
            );
            announce(
              t("{name} is now {role}.", {
                name: updated.display_name,
                role: names.workspaceRole(updated.role),
              }),
            );
            if (updated.user_id === scope.userId) {
              onWorkspacesChanged(); // the own workspace role decides which controls appear
            }
          }}
        />
      )}

      {dialog?.kind === "ws-remove" && scope.workspace !== null && (
        <ConfirmDialog
          title={dialog.member.user_id === scope.userId ? t("Leave workspace") : t("Remove from workspace")}
          message={t("{name} loses access to the projects of this workspace.", {
            name: dialog.member.display_name,
          })}
          confirmLabel={dialog.member.user_id === scope.userId ? t("Leave") : t("Remove")}
          danger
          busy={action.pending}
          error={action.failed ? action.error : null}
          onReload={() => {
            wsMembers.refresh();
            close();
          }}
          onClose={close}
          onConfirm={() => {
            const member = dialog.member;
            const workspace = scope.workspace;
            if (workspace === null) return;
            void action
              .run(async () => {
                await api.removeWorkspaceMember(workspace.id, member.user_id);
                return true;
              })
              .then((done) => {
                if (done !== true) return;
                wsMembers.mutate((items) => items.filter((item) => item.user_id !== member.user_id));
                announce(t("{name} was removed.", { name: member.display_name }));
                close();
                if (member.user_id === scope.userId) {
                  onWorkspacesChanged(); // the workspace may no longer be visible
                }
              });
          }}
        />
      )}

      {dialog?.kind === "ws-add" && scope.workspace !== null && (
        <AddWorkspaceMemberDialog
          scope={scope}
          onAdded={(added) => {
            wsMembers.refresh();
            announce(t("{name} was added to the workspace.", { name: added.display_name }));
            close();
          }}
          onClose={close}
        />
      )}

      {dialog?.kind === "account-reset" && (
        <AppModal title={t("Issue account reset link")} onClose={close} busy={action.pending} size="md">
          <p>{t("Issuing a reset link signs out this account on all devices. The link is shown once and must be shared securely.")}</p>
          <label className="two-factor-policy">
            <input
              type="checkbox"
              checked={resetTwoFactor}
              disabled={action.pending}
              onChange={(event) => setResetTwoFactor(event.currentTarget.checked)}
            />
            <span>{t("Also reset two-factor authentication")}</span>
          </label>
          {resetError !== null && <ErrorNotice error={resetError} onRetry={() => void issueMemberReset(dialog.member)} />}
          <div className="form__actions">
            <Button className="button" onPress={close} isDisabled={action.pending}>{t("Cancel")}</Button>
            <Button className="button button--primary" onPress={() => void issueMemberReset(dialog.member)} isDisabled={action.pending}>
              {action.pending ? t("Issuing…") : t("Issue reset link")}
            </Button>
          </div>
        </AppModal>
      )}

      {resetResult !== null && (
        <AppModal title={t("Password reset link")} onClose={() => setResetResult(null)} size="md">
          <Alert tone="info">
            {t("Copy this link now. It is shown once and expires at {expires}.", {
              expires: formatDateTime(language, resetResult.result.expires_at),
            })}
          </Alert>
          <label className="field">
            <span className="field__label">{t("One-time reset link")}</span>
            <input className="field__input field__input--mono" readOnly value={resetResult.result.reset_url} onFocus={(event) => event.currentTarget.select()} />
          </label>
          <div className="form__actions">
            <Button className="button button--primary" onPress={() => setResetResult(null)}>{t("Close")}</Button>
          </div>
        </AppModal>
      )}
    </>
  );
}

/** Reads every page of a list (members of one organization or workspace are few; capped for safety). */
async function loadAllPages<T>(fetchPage: (cursor?: string) => Promise<Page<T>>): Promise<T[]> {
  const all: T[] = [];
  let cursor: string | undefined;
  for (let pages = 0; pages < 200; pages += 1) {
    const page = await fetchPage(cursor);
    all.push(...page.items);
    if (page.next_cursor === null) break;
    cursor = page.next_cursor;
  }
  return all;
}

function AddWorkspaceMemberDialog({
  scope,
  onAdded,
  onClose,
}: {
  scope: Scope;
  onAdded: (member: WorkspaceMember) => void;
  onClose: () => void;
}) {
  const { t } = useI18n();
  const { api } = useAuth();
  const names = useRoleNames();
  const roles = grantableWorkspaceRoles(scope);
  const [userId, setUserId] = useState("");
  const [role, setRole] = useState<WorkspaceRole>(roles.includes("viewer") ? "viewer" : (roles[0] ?? "viewer"));
  const action = useAction();

  // Who can be added is "organization members minus workspace members": both lists are read
  // completely first, so a person on a later page is neither offered twice nor missed.
  const [people, setPeople] = useState<{ candidates: OrgMember[] } | null>(null);
  const [loadError, setLoadError] = useState<unknown>(null);
  const [attempt, setAttempt] = useState(0);
  const orgId = scope.organization?.id ?? null;
  const workspaceId = scope.workspace?.id ?? null;
  useEffect(() => {
    if (orgId === null || workspaceId === null) return;
    let current = true;
    setPeople(null);
    setLoadError(null);
    Promise.all([
      loadAllPages((cursor) => api.listOrgMembers(orgId, cursor)),
      loadAllPages((cursor) => api.listWorkspaceMembers(workspaceId, cursor)),
    ]).then(
      ([orgList, wsList]) => {
        if (!current) return;
        const inWorkspace = new Set(wsList.map((m) => m.user_id));
        setPeople({ candidates: orgList.filter((m) => !inWorkspace.has(m.user_id)) });
      },
      (failure: unknown) => {
        if (current) setLoadError(failure);
      },
    );
    return () => {
      current = false;
    };
  }, [api, orgId, workspaceId, attempt]);
  const candidates = people?.candidates ?? [];
  const selected = candidates.some((candidate) => candidate.user_id === userId) ? userId : "";

  async function submit() {
    const workspace = scope.workspace;
    if (workspace === null || selected === "") return;
    const added = await action.run(() => api.addWorkspaceMember(workspace.id, selected, role));
    if (added !== undefined) {
      onAdded(added);
    }
  }

  return (
    <AppModal title={t("Add to workspace")} onClose={onClose} busy={action.pending} size="md">
      {loadError !== null && (
        <ErrorNotice error={loadError} onRetry={() => setAttempt((n) => n + 1)} onReload={() => setAttempt((n) => n + 1)} />
      )}
      {people === null && loadError === null && (
        <p role="status" className="muted">
          {t("Loading people…")}
        </p>
      )}
      {people !== null && candidates.length === 0 ? (
        <p className="muted">{t("Everybody in the organization is already in this workspace.")}</p>
      ) : people === null ? null : (
        <NativeSelect
          label={t("Person")}
          value={selected}
          options={[
            { id: "", label: t("Choose a person") },
            ...candidates.map((candidate) => ({
              id: candidate.user_id,
              label: `${candidate.display_name} (${candidate.email})`,
            })),
          ]}
          onChange={setUserId}
        />
      )}
      <NativeSelect
        label={t("Role")}
        value={role}
        options={roles.map((choice) => ({ id: choice, label: names.workspaceRole(choice) }))}
        onChange={(value) => {
          const next = roles.find((choice) => choice === value);
          if (next !== undefined) setRole(next);
        }}
      />
      {action.failed && <ErrorNotice error={action.error} onRetry={() => void submit()} />}
      <div className="form__actions">
        <Button className="button" onPress={onClose} isDisabled={action.pending}>
          {t("Cancel")}
        </Button>
        <Button
          className="button button--primary"
          onPress={() => void submit()}
          isDisabled={action.pending || selected === ""}
        >
          {t("Add")}
        </Button>
      </div>
    </AppModal>
  );
}
