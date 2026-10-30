import { useEffect, useMemo, useRef, useState } from "react";
import { Button } from "react-aria-components";
import type { ShareGrant } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { AppModal, ConfirmDialog } from "../components/Dialog.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { NativeSelect } from "../components/ContextSelect.tsx";
import { Field } from "../components/forms.tsx";
import { LoadMoreButton, ListStatus } from "../components/ListStatus.tsx";
import { useAction } from "../hooks.ts";
import { useI18n } from "../i18n/index.tsx";
import { usePagedList } from "../usePagedList.ts";

export type ShareTarget = { kind: "file" | "project"; id: string; name: string };

export function SharingDialog({
  target,
  organizationId,
  onClose,
}: {
  target: ShareTarget;
  organizationId: string;
  onClose: () => void;
}) {
  const { t } = useI18n();
  const { api, state } = useAuth();
  const [search, setSearch] = useState("");
  const [userId, setUserId] = useState<string | null>(null);
  const [role, setRole] = useState<"viewer" | "editor">("viewer");
  const [remove, setRemove] = useState<ShareGrant | null>(null);
  const [retryChange, setRetryChange] = useState<{ grant: ShareGrant; role: "viewer" | "editor" } | null>(null);
  const action = useAction();
  const changeBusy = useRef(false);
  const revoke = useAction();
  const [debouncedSearch, setDebouncedSearch] = useState("");
  useEffect(() => {
    const timer = window.setTimeout(() => setDebouncedSearch(search.trim()), 250);
    return () => window.clearTimeout(timer);
  }, [search]);
  const grantsLoad = useMemo(
    () => (cursor?: string) => target.kind === "file"
      ? api.listFileGrants(target.id, cursor)
      : api.listProjectGrants(target.id, cursor),
    [api, target.id, target.kind],
  );
  const grants = usePagedList<ShareGrant>(grantsLoad);
  const query = debouncedSearch;
  const membersLoad = useMemo(
    () => query.length < 2 ? null : (cursor?: string) => api.listOrgMembers(organizationId, cursor, query),
    [api, organizationId, query],
  );
  const members = usePagedList(membersLoad);
  const memberChoices = members.items
    .filter((member) =>
      !(state.phase === "authenticated" && member.user_id === state.session.user_id) &&
      !grants.items.some((grant) => grant.grantee.user_id === member.user_id),
    )
    .map((member) => ({ id: member.user_id, label: `${member.display_name} · ${member.email}` }));

  async function add() {
    if (userId === null) return;
    setRetryChange(null);
    const added = await action.run(() => target.kind === "file"
      ? api.createFileGrant(target.id, userId, role)
      : api.createProjectGrant(target.id, userId, role));
    if (added !== undefined) {
      grants.refresh();
      setUserId(null);
      setSearch("");
    }
  }

  async function change(grant: ShareGrant, next: "viewer" | "editor") {
    if (changeBusy.current || action.pending) return;
    changeBusy.current = true;
    setRetryChange({ grant, role: next });
    try {
      const changed = await action.run(() => api.changeGrant(grant.id, grant.version, next));
      if (changed !== undefined) {
        setRetryChange(null);
        grants.mutate((rows) => rows.map((row) => row.id === changed.id ? changed : row));
      }
    } finally {
      changeBusy.current = false;
    }
  }

  function reloadGrants() {
    action.clear();
    setRetryChange(null);
    grants.reload();
  }

  return <>
    <AppModal title={t("Share {name}", { name: target.name })} onClose={onClose} busy={action.pending || revoke.pending} size="md">
      {action.failed && <ErrorNotice error={action.error} onRetry={() => {
        if (retryChange !== null) void change(retryChange.grant, retryChange.role);
        else void add();
      }} onReload={reloadGrants} />}
      <section className="section" aria-label={t("People with access")}>
        <h2 className="section__title">{t("People with access")}</h2>
        <ListStatus list={grants} empty={t("No one else has access yet.")} loadingLabel={t("Loading shares…")} />
        {grants.items.length > 0 && <div className="table-wrap"><table className="table">
          <caption className="visually-hidden">{t("People with access")}</caption>
          <thead><tr><th scope="col">{t("Name")}</th><th scope="col">{t("Role")}</th><th scope="col">{t("Actions")}</th></tr></thead>
          <tbody>{grants.items.map((grant) => <tr key={grant.id}>
            <th scope="row">{grant.grantee.display_name}<span className="cell__sub">{grant.grantee.email}</span></th>
            <td data-label={t("Role")}><NativeSelect label={t("Role for {name}", { name: grant.grantee.display_name })} value={grant.role} isDisabled={action.pending} options={[{ id: "viewer", label: t("Viewer") }, { id: "editor", label: t("Editor") }]} onChange={(value) => { if ((value === "viewer" || value === "editor") && value !== grant.role) void change(grant, value); }} /></td>
            <td data-label={t("Actions")}><div className="row-actions"><Button className="button button--danger-quiet button--sm" onPress={() => setRemove(grant)}>{t("Remove")}</Button></div></td>
          </tr>)}</tbody>
        </table></div>}
        <LoadMoreButton list={grants} />
      </section>
      <section className="section">
        <h2 className="section__title">{t("Add a person")}</h2>
        <Field label={t("Search organization members")} name="share-search" value={search} onChange={(value) => { setSearch(value); setUserId(null); }} />
        {search.trim().length >= 2 && search.trim() === query && <>
          {members.status === "error" && <ErrorNotice error={members.error} onRetry={members.reload} onReload={members.reload} />}
          {members.status === "loading" && <p role="status" className="muted">{t("Searching…")}</p>}
          {members.status === "ready" && members.items.length === 0 && <p className="muted">{t("No members found.")}</p>}
          {memberChoices.length > 0 && <NativeSelect label={t("Person")} value={userId ?? ""} options={[{ id: "", label: t("Choose a person") }, ...memberChoices]} onChange={(value) => setUserId(value === "" ? null : value)} />}
          <LoadMoreButton list={members} />
        </>}
        <NativeSelect label={t("Role")} value={role} options={[{ id: "viewer", label: t("Viewer") }, { id: "editor", label: t("Editor") }]} onChange={(value) => setRole(value === "editor" ? "editor" : "viewer")} />
        <div className="form__actions">
          <Button className="button" onPress={onClose}>{t("Close")}</Button>
          <Button className="button button--primary" isDisabled={userId === null || action.pending} onPress={() => void add()}>{t("Share")}</Button>
        </div>
      </section>
    </AppModal>
    {remove !== null && <ConfirmDialog
      title={t("Remove access")}
      message={t("{name} will lose access to {target}.", { name: remove.grantee.display_name, target: target.name })}
      confirmLabel={t("Remove")}
      danger
      busy={revoke.pending}
      error={revoke.failed ? revoke.error : null}
      onClose={() => { setRemove(null); revoke.clear(); }}
      onReload={() => { grants.reload(); setRemove(null); }}
      onConfirm={() => { void revoke.run(async () => { await api.revokeGrant(remove.id); return true; }).then((done) => { if (done === true) { grants.mutate((rows) => rows.filter((row) => row.id !== remove.id)); setRemove(null); } }); }}
    />}
  </>;
}
