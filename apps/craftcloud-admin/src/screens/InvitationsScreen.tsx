import { useMemo, useRef, useState } from "react";
import type { FormEvent } from "react";
import { Button, Form } from "react-aria-components";
import { isApiError } from "../api/client.ts";
import type { CreatedInvitation, Invitation, Workspace, WorkspaceRole } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { useAnnounce } from "../components/Announcer.tsx";
import { Badge } from "../components/Badge.tsx";
import { NativeSelect } from "../components/ContextSelect.tsx";
import { ConfirmDialog } from "../components/Dialog.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { Alert, Field, focusFirstInvalid } from "../components/forms.tsx";
import { ListStatus, LoadMoreButton } from "../components/ListStatus.tsx";
import { formatDateTime } from "../format.ts";
import { useAction } from "../hooks.ts";
import { useI18n } from "../i18n/index.tsx";
import { canCreateInvitations, canListInvitations, isOrgManager } from "../permissions.ts";
import type { Scope } from "../permissions.ts";
import { useRoleNames } from "../roles.ts";
import { usePagedList } from "../usePagedList.ts";

const NO_WORKSPACE = "";

export function InvitationsScreen({
  scope,
  workspaces,
}: {
  scope: Scope;
  workspaces: Workspace[];
}) {
  const { t, language } = useI18n();
  const { api } = useAuth();
  const announce = useAnnounce();
  const names = useRoleNames();
  const organization = scope.organization;
  const orgId = organization?.id ?? null;
  const manager = isOrgManager(organization);
  const mayList = canListInvitations(organization);
  const mayCreate = canCreateInvitations(scope);

  const loadInvitations = useMemo(
    () =>
      orgId === null || !mayList ? null : (cursor?: string) => api.listInvitations(orgId, cursor),
    [api, orgId, mayList],
  );
  const invitations = usePagedList<Invitation>(loadInvitations);
  const [created, setCreated] = useState<CreatedInvitation | null>(null);
  const [revoking, setRevoking] = useState<Invitation | null>(null);
  const revoke = useAction();

  if (organization === null || orgId === null) {
    return <p className="muted">{t("Select an organization first.")}</p>;
  }

  const workspaceName = (id: string | null): string => {
    if (id === null) return t("None");
    return workspaces.find((candidate) => candidate.id === id)?.name ?? t("Another workspace");
  };
  const statusName: Record<Invitation["status"], string> = {
    pending: t("Open"),
    accepted: t("Accepted"),
    revoked: t("Revoked"),
    expired: t("Expired"),
    invalid: t("No longer valid"),
  };

  return (
    <>
      {!mayCreate && !mayList && (
        <p className="muted">{t("Only organization owners and admins can manage invitations.")}</p>
      )}

      {mayCreate && (
        <section className="section" aria-labelledby="invite-new-title">
          <h2 id="invite-new-title" className="section__title">
            {t("Invite a person")}
          </h2>
          {!manager && (
            <p className="muted">
              {t("As a workspace admin you can invite people into this workspace as editor or viewer.")}
            </p>
          )}
          <InviteForm
            scope={scope}
            workspaces={workspaces}
            onCreated={(result) => {
              setCreated(result);
              invitations.refresh();
              announce(t("Invitation created. Copy the link now; it is shown only once."));
            }}
          />
          {created !== null && <InviteLink invitation={created} onDismiss={() => setCreated(null)} />}
        </section>
      )}

      {mayList && (
        <section className="section" aria-labelledby="invite-list-title">
          <h2 id="invite-list-title" className="section__title">
            {t("Invitations")}
          </h2>
          <ListStatus
            list={invitations}
            empty={t("There are no invitations yet.")}
            loadingLabel={t("Loading invitations…")}
          />
          {invitations.items.length > 0 && (
            <div className="table-wrap">
              <table className="table">
                <caption className="visually-hidden">{t("Invitations")}</caption>
                <thead>
                  <tr>
                    <th scope="col">{t("E-mail address")}</th>
                    <th scope="col">{t("Organization role")}</th>
                    <th scope="col" className="col--optional">{t("Workspace")}</th>
                    <th scope="col">{t("Status")}</th>
                    <th scope="col">{t("Expires")}</th>
                    <th scope="col">{t("Actions")}</th>
                  </tr>
                </thead>
                <tbody>
                  {invitations.items.map((invitation) => (
                    <tr key={invitation.id}>
                      <th scope="row">{invitation.email}</th>
                      <td data-label={t("Organization role")}><Badge>{names.orgRole(invitation.org_role)}</Badge></td>
                      <td className="col--optional" data-label={t("Workspace")}>
                        {workspaceName(invitation.workspace_id)}
                        {invitation.workspace_role !== null
                          ? ` · ${names.workspaceRole(invitation.workspace_role)}`
                          : ""}
                      </td>
                      <td data-label={t("Status")}><Badge tone={invitation.status === "accepted" ? "good" : invitation.status === "pending" ? "info" : invitation.status === "revoked" ? "bad" : "neutral"}>{statusName[invitation.status]}</Badge></td>
                      <td className="numeric" data-label={t("Expires")}>{formatDateTime(language, invitation.expires_at)}</td>
                      <td data-label={t("Actions")}>
                        {invitation.status === "pending" && (
                          <Button
                            className="button button--danger-quiet button--sm"
                            aria-label={t("Revoke invitation for {email}", { email: invitation.email })}
                            onPress={() => setRevoking(invitation)}
                          >
                            {t("Revoke")}
                          </Button>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
          <LoadMoreButton list={invitations} />
        </section>
      )}

      {revoking !== null && (
        <ConfirmDialog
          title={t("Revoke invitation")}
          message={t("The link for {email} stops working.", { email: revoking.email })}
          confirmLabel={t("Revoke")}
          danger
          busy={revoke.pending}
          error={revoke.failed ? revoke.error : null}
          onReload={() => {
            invitations.refresh();
            setRevoking(null);
            revoke.clear();
          }}
          onClose={() => {
            setRevoking(null);
            revoke.clear();
          }}
          onConfirm={() => {
            const target = revoking;
            void revoke
              .run(async () => {
                await api.revokeInvitation(orgId, target.id);
                return true;
              })
              .then((done) => {
                if (done !== true) return;
                invitations.refresh();
                announce(t("Invitation for {email} revoked.", { email: target.email }));
                setRevoking(null);
              });
          }}
        />
      )}
    </>
  );
}

function InviteForm({
  scope,
  workspaces,
  onCreated,
}: {
  scope: Scope;
  workspaces: Workspace[];
  onCreated: (created: CreatedInvitation) => void;
}) {
  const { t } = useI18n();
  const { api } = useAuth();
  const names = useRoleNames();
  const manager = isOrgManager(scope.organization);
  const canInviteGuest = manager || (scope.workspace?.workspace_role === "admin" && scope.organization?.workspace_admin_invites_enabled === true);
  const fixedWorkspace = !manager ? scope.workspace : null;
  const [email, setEmail] = useState("");
  const [orgRole, setOrgRole] = useState<"admin" | "member" | "guest">("member");
  const [workspaceId, setWorkspaceId] = useState(NO_WORKSPACE);
  const [workspaceRole, setWorkspaceRole] = useState<WorkspaceRole>("viewer");
  const [errors, setErrors] = useState<{ email?: string }>({});
  const action = useAction();
  const form = useRef<HTMLFormElement>(null);

  // Workspace admins may invite guests to the own workspace as context, without membership.
  const effectiveWorkspace = fixedWorkspace !== null ? fixedWorkspace.id : workspaceId;
  const roleChoices: WorkspaceRole[] = manager ? ["admin", "editor", "viewer"] : ["editor", "viewer"];
  const effectiveRole = roleChoices.includes(workspaceRole) ? workspaceRole : (roleChoices[0] ?? "viewer");

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (action.pending || scope.organization === null) return;
    const trimmed = email.trim();
    if (trimmed === "" || !trimmed.includes("@") || trimmed.length > 320) {
      setErrors({ email: t("Enter a valid e-mail address.") });
      queueMicrotask(() => focusFirstInvalid(form.current));
      return;
    }
    setErrors({});
    const orgId = scope.organization.id;
    const created = await action.run(async () => {
      try {
        return await api.createInvitation(orgId, {
          email: trimmed,
          org_role: canInviteGuest ? orgRole : manager ? orgRole : "member",
          ...(effectiveWorkspace !== NO_WORKSPACE
            ? orgRole === "guest"
              ? { workspace_id: effectiveWorkspace }
              : { workspace_id: effectiveWorkspace, workspace_role: effectiveRole }
            : {}),
        });
      } catch (failure) {
        if (isApiError(failure) && failure.fields.includes("email")) {
          setErrors({ email: t("This value is not accepted.") });
        }
        throw failure;
      }
    });
    if (created !== undefined) {
      setEmail("");
      onCreated(created);
    }
  }

  return (
    <Form className="form form--grid" onSubmit={(event) => void submit(event)} ref={form} validationBehavior="aria">
      <Field
        label={t("E-mail address")}
        name="invite-email"
        type="email"
        autoComplete="off"
        value={email}
        onChange={setEmail}
        error={errors.email}
      />
      {canInviteGuest && (
        <NativeSelect
          label={t("Organization role")}
          value={orgRole}
          options={[
            { id: "member", label: names.orgRole("member") },
            ...(manager ? [{ id: "admin", label: names.orgRole("admin") }] : []),
            { id: "guest", label: names.orgRole("guest") },
          ]}
          onChange={(value) => setOrgRole(value === "guest" || (manager && value === "admin") ? value : "member")}
        />
      )}
      {manager ? (
        <NativeSelect
          label={t("Workspace")}
          value={workspaceId}
          options={[
            { id: NO_WORKSPACE, label: t("No workspace") },
            ...workspaces.map((workspace) => ({ id: workspace.id, label: workspace.name })),
          ]}
          onChange={setWorkspaceId}
        />
      ) : (
        <p className="muted">
          {t("Workspace")}: {fixedWorkspace?.name ?? ""}
        </p>
      )}
      {effectiveWorkspace !== NO_WORKSPACE && orgRole !== "guest" && (
        <NativeSelect
          label={t("Workspace role")}
          value={effectiveRole}
          options={roleChoices.map((choice) => ({ id: choice, label: names.workspaceRole(choice) }))}
          onChange={(value) => {
            const next = roleChoices.find((choice) => choice === value);
            if (next !== undefined) setWorkspaceRole(next);
          }}
        />
      )}
      {action.failed && errors.email === undefined && <ErrorNotice error={action.error} />}
      <Button type="submit" className="button button--primary" isDisabled={action.pending}>
        {action.pending ? t("Creating…") : t("Create invitation")}
      </Button>
    </Form>
  );
}

/**
 * The one-time invitation link. It only lives in this component's state: it is
 * not written to storage, the address bar or the history, and it disappears
 * when dismissed or when the view is left.
 */
function InviteLink({
  invitation,
  onDismiss,
}: {
  invitation: CreatedInvitation;
  onDismiss: () => void;
}) {
  const { t, language } = useI18n();
  const announce = useAnnounce();
  const input = useRef<HTMLInputElement>(null);
  const [copyFailed, setCopyFailed] = useState(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(invitation.invite_url);
      setCopyFailed(false);
      announce(t("Link copied."));
    } catch {
      setCopyFailed(true);
      input.current?.select();
    }
  }

  return (
    <div className="link-box" role="group" aria-labelledby="invite-link-title">
      <h3 id="invite-link-title" className="section__subtitle">
        {t("Invitation link")}
      </h3>
      <p className="muted">
        {t("Shown only once. Send it to the person yourself; it works once and expires on {date}.", {
          date: formatDateTime(language, invitation.expires_at),
        })}
      </p>
      <label className="field">
        <span className="field__label">{t("Link")}</span>
        <input
          ref={input}
          className="field__input field__input--mono"
          readOnly
          value={invitation.invite_url}
          autoComplete="off"
          spellCheck={false}
          onFocus={(event) => event.currentTarget.select()}
        />
      </label>
      {copyFailed && <Alert>{t("Copying failed. Select the link and copy it by hand.")}</Alert>}
      <div className="form__actions">
        <Button className="button button--primary" onPress={() => void copy()}>
          {t("Copy link")}
        </Button>
        <Button className="button" onPress={onDismiss}>
          {t("Hide link")}
        </Button>
      </div>
    </div>
  );
}
