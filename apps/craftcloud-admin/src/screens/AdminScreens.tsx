import { useEffect, useMemo, useRef, useState } from "react";
import { Button } from "react-aria-components";
import type { AdminDevice, AdminOrganization, AdminUser, AuditEvent, Invitation, Organization, OrganizationGrant, OrganizationLock, OrgMember, Page, Workspace } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { useAnnounce } from "../components/Announcer.tsx";
import { useEmailDelivery, ResendInvitation } from "./EmailAccount.tsx";
import { AppModal, ConfirmDialog } from "../components/Dialog.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { ListStatus, LoadMoreButton } from "../components/ListStatus.tsx";
import { NativeSelect } from "../components/ContextSelect.tsx";
import { Field } from "../components/forms.tsx";
import { formatDateTime } from "../format.ts";
import { useI18n } from "../i18n/index.tsx";
import { useRoleNames } from "../roles.ts";
import { usePagedList } from "../usePagedList.ts";

function useDebounced(value: string, delay = 350) {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => { const timer = window.setTimeout(() => setDebounced(value), delay); return () => window.clearTimeout(timer); }, [value, delay]);
  return debounced;
}

async function collectPages<T>(load: (cursor?: string) => Promise<Page<T>>, maxPages = 100): Promise<{ items: T[]; truncated: boolean }> {
  const items: T[] = []; let cursor: string | undefined; let pageNo = 0;
  do { const page = await load(cursor); items.push(...page.items); cursor = page.next_cursor ?? undefined; pageNo += 1; } while (cursor !== undefined && pageNo < maxPages);
  return { items, truncated: cursor !== undefined };
}

export function AdminPeopleScreen({ organization, onChanged }: { organization: Organization | null; onChanged: () => void }) {
  const { t, language } = useI18n(); const { api, state: authState } = useAuth(); const names = useRoleNames();
  const [query, setQuery] = useState(""); const debounced = useDebounced(query.trim());
  const [role, setRole] = useState(""); const [status, setStatus] = useState("all"); const [twoFactor, setTwoFactor] = useState(""); const [sort, setSort] = useState("joined_desc");
  const [selected, setSelected] = useState<string[]>([]); const [action, setAction] = useState(""); const [results, setResults] = useState<Record<string, { name: string; result: string }>>({}); const [detail, setDetail] = useState<OrgMember | null>(null); const [roleToApply, setRoleToApply] = useState<"owner" | "admin" | "member" | "guest">("member"); const [confirmRemove, setConfirmRemove] = useState(false);
  const [appData, setAppData] = useState<import("../api/types.ts").OrganizationApps | null>(null); const [selectedApp, setSelectedApp] = useState("");
  const currentScope = useRef({ orgId: organization?.id ?? null, userId: authState.phase === "authenticated" ? authState.session.user_id : null });
  currentScope.current = { orgId: organization?.id ?? null, userId: authState.phase === "authenticated" ? authState.session.user_id : null };
  const [inviteText, setInviteText] = useState(""); const [inviteRole, setInviteRole] = useState<"admin" | "member" | "guest">("member"); const [inviteResults, setInviteResults] = useState<{ email: string; result: string; url?: string }[]>([]); const [inviting, setInviting] = useState(false);
  const load = useMemo(() => organization === null ? null : (cursor?: string) => api.listOrgMembers(organization.id, cursor, debounced.length >= 2 ? debounced : undefined, { status, ...(role ? { role } : {}), ...(twoFactor ? { two_factor: twoFactor } : {}), sort }), [api, organization?.id, debounced, role, status, twoFactor, sort]);
  const members = usePagedList<OrgMember>(load);
  useEffect(() => {
    if (organization === null) return;
    let live = true;
    void api.getOrganizationApps(organization.id).then((value) => { if (live) setAppData(value); }, () => { if (live) setAppData(null); });
    return () => { live = false; };
  }, [api, organization?.id]);
  useEffect(() => { setSelected([]); setDetail(null); }, [organization?.id, debounced, role, status, twoFactor, sort]);
  useEffect(() => { setResults({}); setInviteResults([]); setSelected([]); setDetail(null); setAction(""); }, [organization?.id, authState.phase === "authenticated" ? authState.session.user_id : null]);
  const toggle = (id: string) => setSelected((s) => s.includes(id) ? s.filter((x) => x !== id) : [...s, id]);
  async function bulk(kind: "remove" | "role") {
    if (organization === null || selected.length === 0 || action !== "") return;
    setAction(kind);
    setResults({});
    const next: Record<string, { name: string; result: string }> = {};
    for (const id of selected) {
      const member = members.items.find((item) => item.user_id === id);
      if (!member) continue;
      try {
        if (kind === "remove") await api.removeOrgMember(organization.id, id);
        else await api.changeOrgMemberRole(organization.id, id, roleToApply);
        next[id] = { name: member.display_name, result: t("Done") };
      } catch {
        next[id] = { name: member.display_name, result: t("Failed") };
      }
    }
    setResults(next);
    setAction("");
    setConfirmRemove(false);
    members.refresh();
    onChanged();
  }
  async function bulkApps(kind: "assign" | "remove") {
    if (organization === null || selected.length === 0 || selectedApp === "" || action !== "") return;
    const orgId = organization.id;
    const userId = authState.phase === "authenticated" ? authState.session.user_id : null;
    const scopeIsCurrent = () => currentScope.current.orgId === orgId && currentScope.current.userId === userId;
    setAction(`apps-${kind}`); setResults({});
    const next: Record<string, { name: string; result: string }> = {};
    for (const id of selected) {
      const person = members.items.find((item) => item.user_id === id);
      // Owners and admins always have every app and guests need none: only plain members carry assignments.
      if (!person || person.role !== "member" || person.status !== "active") continue;
      if (!scopeIsCurrent()) return;
      try {
        // Add/remove one app instead of replacing the list from possibly stale data.
        const updated = await api.changeMemberApps(orgId, id, kind === "assign" ? { add: [selectedApp] } : { remove: [selectedApp] });
        if (!scopeIsCurrent()) return;
        members.mutate((items) => items.map((item) => item.user_id === updated.user_id ? { ...item, apps: updated.apps } : item));
        next[id] = { name: person.display_name, result: t("Done") };
      } catch {
        if (!scopeIsCurrent()) return;
        next[id] = { name: person.display_name, result: t("Failed") };
      }
    }
    if (!scopeIsCurrent()) return;
    setResults(next); setAction("");
  }
  async function inviteBatch() {
    if (organization === null || inviting) return;
    const emails = inviteText.split(/\r?\n/).map((value) => value.trim()).filter(Boolean);
    const rows = emails.map((email) => ({ email, valid: /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email) }));
    if (rows.length > 50) {
      setInviteResults([{ email: "", result: t("Enter no more than 50 addresses.") }]);
      return;
    }
    setInviting(true);
    const next: { email: string; result: string; url?: string }[] = [];
    for (const row of rows) {
      if (!row.valid) {
        next.push({ email: row.email, result: t("Invalid email address") });
        continue;
      }
      try {
        const result = await api.createInvitation(organization.id, { email: row.email, org_role: inviteRole, language: language === "de" ? "de" : "en" });
        next.push({ email: row.email, result: result.sent_to !== undefined ? t("Invitation sent.") : t("Invitation created"), ...(result.invite_url === undefined ? {} : { url: result.invite_url }) });
      } catch {
        next.push({ email: row.email, result: t("Failed") });
      }
    }
    setInviteResults(next);
    setInviting(false);
  }
  if (organization === null) return <p className="muted">{t("Select an organization first.")}</p>;
  return (
    <div className="stack">
      <section className="section">
        <h2 className="section__title">{t("People")}</h2>
        <div className="toolbar">
          <Field
            label={t("Search people")}
            name="people-search"
            required={false}
            value={query}
            onChange={setQuery}
            description={t("Enter at least two characters to search.")}
          />
          <AdminSelect label={t("Role")} value={role} onChange={setRole} options={[
            ["", t("All roles")], ["owner", names.orgRole("owner")], ["admin", names.orgRole("admin")],
            ["member", names.orgRole("member")], ["guest", names.orgRole("guest")],
          ]} />
          <AdminSelect label={t("Status")} value={status} onChange={setStatus} options={[
            ["all", t("All statuses")], ["active", t("Active")], ["invited", t("Invited")], ["disabled", t("Disabled")],
          ]} />
          <AdminSelect label={t("Two-factor authentication")} value={twoFactor} onChange={setTwoFactor} options={[
            ["", t("Any")], ["enabled", t("Enabled")], ["disabled", t("Not enabled")],
          ]} />
          <AdminSelect label={t("Sort by")} value={sort} onChange={setSort} options={[
            ["joined_desc", t("Newest first")], ["joined_asc", t("Oldest first")],
            ["name_asc", t("Name A to Z")], ["name_desc", t("Name Z to A")],
          ]} />
        </div>
        <div className="row-actions">
          <AdminSelect label={t("Organization role")} value={roleToApply} onChange={(value) => setRoleToApply(value as typeof roleToApply)} options={[
            ...(organization.role === "owner" ? [["owner", names.orgRole("owner")] as [string, string]] : []),
            ["admin", names.orgRole("admin")], ["member", names.orgRole("member")], ["guest", names.orgRole("guest")],
          ]} />
          <Button className="button" isDisabled={selected.length === 0 || action !== ""} onPress={() => void bulk("role")}>
            {t("Change selected role to {role}", { role: names.orgRole(roleToApply) })}
          </Button>
          <Button className="button button--danger-quiet" isDisabled={selected.length === 0 || action !== ""} onPress={() => setConfirmRemove(true)}>
            {t("Remove selected")}
          </Button>
          {appData !== null && <>
            <AdminSelect label={t("App")} value={selectedApp} onChange={setSelectedApp} options={[["", t("Choose an app")], ...appData.apps.map((app) => [app.id, app.name] as [string, string])]} />
            <Button className="button" isDisabled={selected.length === 0 || selectedApp === "" || action !== ""} onPress={() => void bulkApps("assign")}>{t("Assign selected app")}</Button>
            <Button className="button" isDisabled={selected.length === 0 || selectedApp === "" || action !== ""} onPress={() => void bulkApps("remove")}>{t("Remove selected app")}</Button>
          </>}
        </div>
        <ListStatus list={members} empty={t("No people found.")} loadingLabel={t("Loading members…")} />
        {members.items.length > 0 && <div className="table-wrap">
          <table className="table">
            <thead><tr>
              <th>{t("Select")}</th><th>{t("Name")}</th><th>{t("Role")}</th><th>{t("Apps")}</th>
              <th>{t("Status")}</th><th>{t("Two-factor authentication")}</th><th>{t("Actions")}</th>
            </tr></thead>
            <tbody>{members.items.map((person) => <tr key={person.user_id}>
              <td data-label={t("Select")}>
                <input type="checkbox" aria-label={t("Select {name}", { name: person.display_name })} checked={selected.includes(person.user_id)} onChange={() => toggle(person.user_id)} />
              </td>
              <th>{person.display_name}<span className="cell__sub cell__sub--email">{person.email}</span></th>
              <td data-label={t("Role")}>{names.orgRole(person.role)}</td>
              <td data-label={t("Apps")}>{person.role === "guest" ? t("Not required for guests") : person.role === "owner" || person.role === "admin" ? t("Always all apps") : (person.apps ?? []).map((appId) => appData?.apps.find((app) => app.id === appId)?.name ?? appId).join(", ") || t("None assigned")}</td>
              <td data-label={t("Status")}>{person.status === "invited" ? t("Invited") : person.status === "disabled" ? t("Disabled") : t("Active")}</td>
              <td data-label={t("Two-factor authentication")}>{person.two_factor_enabled ? t("Enabled") : t("Not enabled")}</td>
              <td data-label={t("Actions")}><div className="row-actions">
                <Button className="button button--sm" onPress={() => setDetail(person)}>{t("Details")}</Button>
                <Button className="button button--sm" onPress={() => setSelected([person.user_id])}>{t("Select")}</Button>
              </div></td>
            </tr>)}</tbody>
          </table>
        </div>}
        <LoadMoreButton list={members} />
      </section>
      {Object.keys(results).length > 0 && <section className="section" aria-label={t("Batch results")}>
        <h2 className="section__title">{t("Batch results")}</h2>
        <ul>{Object.entries(results).map(([id, result]) => <li key={id}><strong>{result.name}</strong> · {result.result}</li>)}</ul>
      </section>}
      <section className="section">
        <h2 className="section__title">{t("Invite people")}</h2>
        <label className="field">
          <span className="field__label">{t("Email addresses, one per line")}</span>
          <textarea className="field__input" rows={5} value={inviteText} onChange={(event) => setInviteText(event.currentTarget.value)} />
        </label>
        <AdminSelect label={t("Organization role")} value={inviteRole} onChange={(value) => setInviteRole(value as typeof inviteRole)} options={[
          ["member", names.orgRole("member")], ["admin", names.orgRole("admin")], ["guest", names.orgRole("guest")],
        ]} />
        <Button className="button button--primary" isDisabled={inviting} onPress={() => void inviteBatch()}>
          {inviting ? t("Sending invitations…") : t("Send invitations")}
        </Button>
        {inviteResults.map((result, index) => <p key={`${result.email}-${index}`}>
          <strong>{result.email}</strong> · {result.result}
          {result.url && <><br /><code>{result.url}</code></>}
        </p>)}
      </section>
    <AdminInvitationList organization={organization} />
    {confirmRemove && <ConfirmDialog title={t("Remove selected members")} message={t("Selected people will lose access to this organization and its workspaces.")} confirmLabel={t("Remove selected")} danger busy={action !== ""} error={null} onClose={() => setConfirmRemove(false)} onConfirm={() => void bulk("remove")} />}
    {detail !== null && <PersonDetail organization={organization} member={detail} onClose={() => setDetail(null)} />}
    </div>
  );
}

export function AdminWorkspacesScreen({ organization, workspaces, hasMore, onLoadMore, onRefresh }: { organization: Organization | null; workspaces: Workspace[]; hasMore: boolean; onLoadMore: () => void; onRefresh: () => void }) {
  const { t } = useI18n(); const { api } = useAuth(); const [quota, setQuota] = useState<Record<string, string>>({}); const [names, setNames] = useState<Record<string, string>>({}); const [errors, setErrors] = useState<Record<string, unknown>>({}); const [search, setSearch] = useState("");
  const [newName, setNewName] = useState(""); const [newSlug, setNewSlug] = useState(""); const [creating, setCreating] = useState(false); const [savingName, setSavingName] = useState<string | null>(null); const [savingQuota, setSavingQuota] = useState<string | null>(null); const [loadingUsage, setLoadingUsage] = useState(true);
  useEffect(() => {
    if (organization === null) return;
    let live = true; setLoadingUsage(true);
    void (async () => {
      const values: Record<string, string> = {}; let cursor: string | undefined; let pages = 0;
      do { const page = await api.getOrganizationUsage(organization.id, cursor); for (const entry of page.workspaces) values[entry.workspace_id] = entry.quota_bytes?.toString() ?? ""; cursor = page.next_cursor ?? undefined; pages += 1; } while (cursor !== undefined && pages < 100);
      if (live) setQuota((current) => ({ ...values, ...current }));
    })().then(() => { if (live) setErrors((current) => ({ ...current, usage: null })); })
      .catch((error: unknown) => { if (live) setErrors((current) => ({ ...current, usage: error })); })
      .finally(() => { if (live) setLoadingUsage(false); });
    return () => { live = false; };
  }, [api, organization?.id]);
  const [selected, setSelected] = useState<Workspace | null>(null);
  async function save(ws: Workspace) {
    if (loadingUsage || errors.usage != null || !Object.prototype.hasOwnProperty.call(quota, ws.id) || savingQuota !== null) return;
    const raw = quota[ws.id] ?? "";
    const bytes = raw.trim() === "" ? null : Number(raw);
    if (bytes !== null && (!Number.isSafeInteger(bytes) || bytes < 0)) {
      setErrors((current) => ({ ...current, [ws.id]: new Error(t("Enter a whole number of bytes.")) }));
      return;
    }
    setSavingQuota(ws.id);
    try {
      await api.updateWorkspaceQuota(ws.id, bytes);
      setQuota((current) => ({ ...current, [ws.id]: bytes?.toString() ?? "" }));
      setErrors((current) => ({ ...current, [ws.id]: null }));
      onRefresh();
    } catch (error) {
      setErrors((current) => ({ ...current, [ws.id]: error }));
    } finally {
      setSavingQuota(null);
    }
  }
  async function create() {
    if (organization === null || creating) return;
    const name = newName.trim(); const slug = newSlug.trim();
    if (name === "" || !/^[a-z0-9][a-z0-9-]{0,62}$/.test(slug)) return;
    setCreating(true);
    try { await api.createWorkspace(organization.id, name, slug); setNewName(""); setNewSlug(""); onRefresh(); }
    catch (error) { setErrors((current) => ({ ...current, create: error })); }
    finally { setCreating(false); }
  }
  async function rename(workspace: Workspace) {
    const name = (names[workspace.id] ?? workspace.name).trim();
    if (name === "" || name === workspace.name || savingName !== null) return;
    setSavingName(workspace.id);
    try { await api.renameWorkspace(workspace.id, workspace.version, name); setErrors((current) => ({ ...current, [workspace.id]: null })); onRefresh(); }
    catch (error) { setErrors((current) => ({ ...current, [workspace.id]: error })); }
    finally { setSavingName(null); }
  }
  if (organization === null) return <p className="muted">{t("Select an organization first.")}</p>;
  const visible = workspaces.filter((ws) => `${ws.name} ${ws.slug}`.toLocaleLowerCase().includes(search.trim().toLocaleLowerCase()));
  return <div className="stack">
    <section className="section">
      <h2 className="section__title">{t("Create workspace")}</h2>
      {errors.create !== undefined && errors.create !== null && <ErrorNotice error={errors.create} />}
      <Field label={t("Workspace name")} name="workspace-name" value={newName} onChange={setNewName} />
      <Field label={t("Slug")} name="workspace-slug" value={newSlug} onChange={setNewSlug} description={t("Use lowercase letters, digits and hyphens; start with a letter or digit.")} />
      <Button className="button button--primary" isDisabled={creating || newName.trim() === "" || !/^[a-z0-9][a-z0-9-]{0,62}$/.test(newSlug.trim())} onPress={() => void create()}>{creating ? t("Creating…") : t("Create workspace")}</Button>
    </section>
    <section className="section">
      <Field label={t("Search workspaces")} name="workspace-search" required={false} value={search} onChange={setSearch} />
      {loadingUsage && <p role="status" className="muted">{t("Loading storage quotas…")}</p>}
      {errors.usage !== undefined && errors.usage !== null && <ErrorNotice error={errors.usage} />}
      <div className="table-wrap"><table className="table">
        <thead><tr><th>{t("Workspace")}</th><th>{t("Slug")}</th><th>{t("Quota in bytes")}</th><th>{t("Actions")}</th></tr></thead>
        <tbody>{visible.map((ws) => <tr key={ws.id}>
          <th><input className="field__input" aria-label={t("Workspace name for {name}", { name: ws.name })} value={names[ws.id] ?? ws.name} onChange={(e) => setNames((current) => ({ ...current, [ws.id]: e.currentTarget.value }))} /></th>
          <td data-label={t("Slug")}>{ws.slug}</td>
          <td data-label={t("Quota in bytes")}><input className="field__input" aria-label={t("Quota for {name}", { name: ws.name })} value={quota[ws.id] ?? ""} onChange={(e) => { const value = e.currentTarget.value; setQuota((current) => ({ ...current, [ws.id]: value })); }} placeholder={loadingUsage ? t("Loading…") : Object.prototype.hasOwnProperty.call(quota, ws.id) ? t("Unlimited") : t("Quota unavailable")} /></td>
          <td data-label={t("Actions")}><div className="row-actions">
            <Button className="button button--sm" isDisabled={savingName !== null} onPress={() => void rename(ws)}>{t("Rename")}</Button>
            <Button className="button button--sm" isDisabled={loadingUsage || errors.usage != null || !Object.prototype.hasOwnProperty.call(quota, ws.id) || savingQuota !== null} onPress={() => void save(ws)}>{savingQuota === ws.id ? t("Saving…") : t("Save quota")}</Button>
            <Button className="button button--sm" onPress={() => setSelected(ws)}>{t("Members")}</Button>
          </div>{errors[ws.id] !== undefined && errors[ws.id] !== null && <ErrorNotice error={errors[ws.id]} />}</td>
        </tr>)}</tbody>
      </table>{visible.length === 0 && <p className="muted">{t("No workspaces found.")}</p>}</div>
      {hasMore && <Button className="button" onPress={onLoadMore}>{t("Load more workspaces")}</Button>}
    </section>
    {selected && <WorkspaceMembers workspace={selected} onClose={() => setSelected(null)} />}
  </div>;
}

function WorkspaceMembers({ workspace, onClose }: { workspace: Workspace; onClose: () => void }) {
  const { t } = useI18n(); const { api } = useAuth(); const names = useRoleNames();
  const [rows, setRows] = useState<{ user_id: string; display_name: string; email: string; role: "admin" | "editor" | "viewer" }[]>([]);
  const [error, setError] = useState<unknown>(null); const [remove, setRemove] = useState<(typeof rows)[number] | null>(null); const [busy, setBusy] = useState(false);
  useEffect(() => { let live = true; void collectPages((cursor) => api.listWorkspaceMembers(workspace.id, cursor)).then((r) => { if (live) setRows(r.items); }, (e: unknown) => { if (live) setError(e); }); return () => { live = false; }; }, [api, workspace.id]);
  return <section className="admin-detail">
    <h2>{workspace.name} · {t("Members")}</h2>
    {error !== null && <ErrorNotice error={error} />}
    <ul>{rows.map((member) => <li key={member.user_id} className="workspace-member-row">
      {member.display_name} · {member.email}
      <AdminSelect label={t("Role for {name}", { name: member.display_name })} value={member.role} onChange={(role) => {
        if (busy) return; setBusy(true);
        void api.changeWorkspaceMemberRole(workspace.id, member.user_id, role as "admin" | "editor" | "viewer")
          .then((updated) => setRows((current) => current.map((row) => row.user_id === updated.user_id ? updated : row)), setError)
          .finally(() => setBusy(false));
      }} options={[["admin", names.workspaceRole("admin")], ["editor", names.workspaceRole("editor")], ["viewer", names.workspaceRole("viewer")]]} />
      <Button className="button button--danger-quiet button--sm" isDisabled={busy} onPress={() => setRemove(member)}>{t("Remove")}</Button>
    </li>)}</ul>
    <Button className="button" onPress={onClose}>{t("Close")}</Button>
    {remove && <ConfirmDialog title={t("Remove workspace member")} message={t("{name} loses access to the projects of this workspace.", { name: remove.display_name })} confirmLabel={t("Remove")} danger busy={busy} error={error} onClose={() => { setRemove(null); setError(null); }} onConfirm={() => {
      const member = remove; setBusy(true);
      void api.removeWorkspaceMember(workspace.id, member.user_id).then(() => { setRows((current) => current.filter((row) => row.user_id !== member.user_id)); setRemove(null); setError(null); }, setError).finally(() => setBusy(false));
    }} />}
  </section>;
}

function AdminInvitationList({ organization }: { organization: Organization }) {
  const emailDelivery = useEmailDelivery();
  const { t, language } = useI18n(); const { api } = useAuth(); const names = useRoleNames();
  const load = useMemo(() => (cursor?: string) => api.listInvitations(organization.id, cursor), [api, organization.id]);
  const list = usePagedList<Invitation>(load);
  const [pending, setPending] = useState<Invitation | null>(null); const [error, setError] = useState<unknown>(null); const [busy, setBusy] = useState(false);
  return <section className="section">
    <h2 className="section__title">{t("Invitations")}</h2>
    {error !== null && <ErrorNotice error={error} />}
    <ListStatus list={list} empty={t("There are no invitations yet.")} loadingLabel={t("Loading invitations…")} />
    {list.items.length > 0 && <div className="table-wrap"><table className="table">
      <thead><tr><th>{t("E-mail address")}</th><th>{t("Role")}</th><th>{t("Status")}</th><th>{t("Expires")}</th><th>{t("Actions")}</th></tr></thead>
      <tbody>{list.items.map((item) => <tr key={item.id}>
        <th>{item.email}</th><td data-label={t("Role")}>{names.orgRole(item.org_role)}</td>
        <td data-label={t("Status")}>{t(item.status === "pending" ? "Open" : item.status === "invalid" ? "No longer valid" : item.status.charAt(0).toUpperCase() + item.status.slice(1))}</td>
        <td data-label={t("Expires")}>{formatDateTime(language, item.expires_at)}</td>
        <td data-label={t("Actions")}>{item.status === "pending" && <>
          {emailDelivery && <ResendInvitation orgId={organization.id} invitationId={item.id} />}
          <Button className="button button--danger-quiet button--sm" onPress={() => setPending(item)}>{t("Revoke")}</Button>
        </>}</td>
      </tr>)}</tbody>
    </table></div>}
    <LoadMoreButton list={list} />
    {pending && <ConfirmDialog title={t("Revoke invitation")} message={t("The link for {email} stops working.", { email: pending.email })} confirmLabel={t("Revoke")} danger busy={busy} error={error} onClose={() => { setPending(null); setError(null); }} onConfirm={() => { setBusy(true); void api.revokeInvitation(organization.id, pending.id).then(() => { setPending(null); setError(null); list.refresh(); }, setError).finally(() => setBusy(false)); }} onReload={list.refresh} />}
  </section>;
}

function PersonDetail({ organization, member, onClose }: { organization: Organization; member: OrgMember; onClose: () => void }) {
  const { t, language } = useI18n(); const { api } = useAuth(); const names = useRoleNames();
  const [workspaceRoles, setWorkspaceRoles] = useState<{ workspace: string; role: string }[]>([]);
  const [grants, setGrants] = useState<OrganizationGrant[]>([]); const [locks, setLocks] = useState<OrganizationLock[]>([]);
  const [events, setEvents] = useState<AuditEvent[]>([]); const [errors, setErrors] = useState<unknown[]>([]); const [loading, setLoading] = useState(true);
  const [truncated, setTruncated] = useState({ workspaces: false, grants: false, locks: false });
  useEffect(() => {
    let live = true;
    setLoading(true);
    setWorkspaceRoles([]); setGrants([]); setLocks([]); setEvents([]); setErrors([]);
    setTruncated({ workspaces: false, grants: false, locks: false });
    void Promise.all([
      (async () => {
        const workspacePage = await collectPages((cursor) => api.listWorkspaces(organization.id, cursor));
        const roles: { workspace: string; role: string }[] = [];
        let memberPagesTruncated = false;
        for (let index = 0; index < workspacePage.items.length; index += 4) {
          const batch = workspacePage.items.slice(index, index + 4);
          const rows = await Promise.all(batch.map(async (workspace) => {
            try {
              const page = await collectPages((cursor) => api.listWorkspaceMembers(workspace.id, cursor));
              memberPagesTruncated ||= page.truncated;
              return page.items.filter((row) => row.user_id === member.user_id).map((row) => ({ workspace: workspace.name, role: row.role }));
            } catch (error) {
              if (live) setErrors((current) => [...current, error]);
              return [];
            }
          }));
          roles.push(...rows.flat());
        }
        if (live) {
          setWorkspaceRoles(roles);
          setTruncated((current) => ({ ...current, workspaces: workspacePage.truncated || memberPagesTruncated }));
        }
      })().catch((error: unknown) => { if (live) setErrors((current) => [...current, error]); }),
      collectPages((cursor) => api.listOrgGrants(organization.id, cursor, { user_id: member.user_id })).then((result) => {
        if (live) { setGrants(result.items); setTruncated((current) => ({ ...current, grants: result.truncated })); }
      }, (error: unknown) => { if (live) setErrors((current) => [...current, error]); }),
      collectPages((cursor) => api.listOrgLocks(organization.id, cursor, { user_id: member.user_id })).then((result) => {
        if (live) { setLocks(result.items); setTruncated((current) => ({ ...current, locks: result.truncated })); }
      }, (error: unknown) => { if (live) setErrors((current) => [...current, error]); }),
      api.listAudit({ kind: "organization", orgId: organization.id }, { actor: member.user_id }, undefined).then((r) => { if (live) setEvents(r.items.slice(0, 10)); }, (e: unknown) => { if (live) setErrors((x) => [...x, e]); }),
    ]).finally(() => { if (live) setLoading(false); });
    return () => { live = false; };
  }, [api, organization.id, member.user_id]);
  return <AppModal title={member.display_name} onClose={onClose} size="side">
    <div className="form__actions">
      <Button className="button" onPress={onClose}>{t("Close")}</Button>
    </div>
    <p>{member.email}</p>
    <p>{t("Organization role")}: {names.orgRole(member.role)}</p>
    {loading && <p role="status" className="muted">{t("Loading person details…")}</p>}
    {errors.map((error, index) => <ErrorNotice key={index} error={error} />)}
    {(truncated.workspaces || truncated.grants || truncated.locks) && <p className="muted">{t("Some person details are incomplete because the listing limit was reached.")}</p>}
    <h3>{t("Workspace roles")}</h3>
    {workspaceRoles.length > 0 ? <ul>{workspaceRoles.map((item) => <li key={item.workspace}>{item.workspace}: {names.workspaceRole(item.role as "admin" | "editor" | "viewer")}</li>)}</ul> : !loading && errors.length === 0 && <p className="muted">{t("No workspace roles.")}</p>}
    <h3>{t("Received grants")}</h3>
    {grants.length > 0 ? <ul>{grants.map((grant) => <li key={grant.id}>{grant.target_name}: {grant.role === "viewer" ? t("Viewer") : t("Editor")}</li>)}</ul> : !loading && errors.length === 0 && <p className="muted">{t("No received grants.")}</p>}
    <h3>{t("Held locks")}</h3>
    {locks.length > 0 ? <ul>{locks.map((lock) => <li key={lock.file_id}>{lock.file_name} · {formatDateTime(language, lock.expires_at)}</li>)}</ul> : !loading && errors.length === 0 && <p className="muted">{t("No held locks.")}</p>}
    <h3>{t("Last activity")}</h3>
    {events.length > 0 ? <ul>{events.map((event) => <li key={event.id}>{event.action} · {formatDateTime(language, event.created_at)}</li>)}</ul> : !loading && errors.length === 0 && <p className="muted">{t("No recent activity.")}</p>}
  </AppModal>;
}

export function AdminGrantsScreen({ organization, workspaces }: { organization: Organization | null; workspaces: Workspace[] }) {
  const { t } = useI18n(); const { api } = useAuth();
  const [workspace, setWorkspace] = useState(""); const [person, setPerson] = useState(""); const [kind, setKind] = useState("");
  const [pending, setPending] = useState<OrganizationGrant | null>(null); const [error, setError] = useState<unknown>(null); const [busy, setBusy] = useState(false);
  const loadMembers = useMemo(() => organization === null ? null : (cursor?: string) => api.listOrgMembers(organization.id, cursor), [api, organization?.id]);
  const members = usePagedList<OrgMember>(loadMembers);
  const load = useMemo(() => organization === null ? null : (cursor?: string) => api.listOrgGrants(organization.id, cursor, { ...(workspace ? { workspace_id: workspace } : {}), ...(person ? { user_id: person } : {}), ...(kind ? { target_kind: kind } : {}) }), [api, organization?.id, workspace, person, kind]);
  const list = usePagedList<OrganizationGrant>(load);
  if (organization === null) return <p className="muted">{t("Select an organization first.")}</p>;
  const peopleOptions: [string, string][] = [["", t("All people")], ...members.items.map((member): [string, string] => [member.user_id, member.display_name])];
  return <>
    <div className="toolbar">
      <AdminSelect label={t("Workspace")} value={workspace} onChange={setWorkspace} options={[["", t("All workspaces")], ...workspaces.map((item) => [item.id, item.name] as [string, string])]} />
      <AdminSelect label={t("Person")} value={person} onChange={setPerson} options={peopleOptions} />
      <AdminSelect label={t("Target type")} value={kind} onChange={setKind} options={[["", t("All target types")], ["project", t("Project")], ["file", t("File")]]} />
    </div>
    {members.hasMore && <LoadMoreButton list={members} />}
    {error !== null && <ErrorNotice error={error} />}
    <ListStatus list={list} empty={t("No active grants.")} loadingLabel={t("Loading grants…")} />
    {list.items.length > 0 && <div className="table-wrap"><table className="table">
      <thead><tr><th>{t("Target")}</th><th>{t("Person")}</th><th>{t("Role")}</th><th>{t("Actions")}</th></tr></thead>
      <tbody>{list.items.map((grant) => <tr key={grant.id}>
        <th>{grant.target_name}</th><td data-label={t("Person")}>{grant.grantee.display_name}</td>
        <td data-label={t("Role")}>{grant.role === "viewer" ? t("Viewer") : t("Editor")}</td>
        <td data-label={t("Actions")}><Button className="button button--danger-quiet button--sm" onPress={() => setPending(grant)}>{t("Revoke")}</Button></td>
      </tr>)}</tbody>
    </table></div>}
    <LoadMoreButton list={list} />
    {pending && <ConfirmDialog title={t("Revoke grant")} message={t("This person will lose access to the shared item.")} confirmLabel={t("Revoke")} danger busy={busy} error={error} onClose={() => { setPending(null); setError(null); }} onConfirm={() => { setBusy(true); void api.revokeGrant(pending.id).then(() => { setPending(null); setError(null); list.refresh(); }, setError).finally(() => setBusy(false)); }} onReload={list.refresh} />}
  </>;
}

export function AdminLocksScreen({ organization, workspaces }: { organization: Organization | null; workspaces: Workspace[] }) {
  const { t, language } = useI18n(); const { api } = useAuth();
  const [workspace, setWorkspace] = useState(""); const [pending, setPending] = useState<OrganizationLock | null>(null);
  const [error, setError] = useState<unknown>(null); const [busy, setBusy] = useState(false);
  const load = useMemo(() => organization === null ? null : (cursor?: string) => api.listOrgLocks(organization.id, cursor, workspace ? { workspace_id: workspace } : {}), [api, organization?.id, workspace]);
  const list = usePagedList<OrganizationLock>(load);
  if (organization === null) return <p className="muted">{t("Select an organization first.")}</p>;
  const workspaceOptions: [string, string][] = [["", t("All workspaces")], ...workspaces.map((item): [string, string] => [item.id, item.name])];
  return <>
    <AdminSelect label={t("Workspace")} value={workspace} onChange={setWorkspace} options={workspaceOptions} />
    {error !== null && <ErrorNotice error={error} />}
    <ListStatus list={list} empty={t("No active locks.")} loadingLabel={t("Loading locks…")} />
    {list.items.length > 0 && <div className="table-wrap"><table className="table">
      <thead><tr><th>{t("File")}</th><th>{t("Holder")}</th><th>{t("Since")}</th><th>{t("Expires")}</th><th>{t("Session type")}</th><th>{t("Actions")}</th></tr></thead>
      <tbody>{list.items.map((lock) => <tr key={lock.file_id}>
        <th>{lock.file_name}</th><td data-label={t("Holder")}>{lock.holder_display_name}</td>
        <td data-label={t("Since")}>{formatDateTime(language, lock.since)}</td><td data-label={t("Expires")}>{formatDateTime(language, lock.expires_at)}</td>
        <td data-label={t("Session type")}>{lock.session_kind === "device" ? t("Device") : t("Browser")}</td>
        <td data-label={t("Actions")}><Button className="button button--danger-quiet button--sm" onPress={() => setPending(lock)}>{t("Release lock")}</Button></td>
      </tr>)}</tbody>
    </table></div>}
    <LoadMoreButton list={list} />
    {pending && <ConfirmDialog title={t("Release lock")} message={t("The holder will lose this editing lock immediately.")} confirmLabel={t("Release lock")} danger busy={busy} error={error} onClose={() => { setPending(null); setError(null); }} onConfirm={() => { setBusy(true); void api.releaseFileLock(pending.file_id, true).then(() => { setPending(null); setError(null); list.refresh(); }, setError).finally(() => setBusy(false)); }} onReload={list.refresh} />}
  </>;
}

export function AdminOperatorScreen({ section }: { section: "organizations" | "accounts" }) {
  const announce = useAnnounce();
  const emailDelivery = useEmailDelivery();
  const { t, language } = useI18n(); const { api } = useAuth(); const [query, setQuery] = useState(""); const debounced = useDebounced(query.trim());
  const loadOrganizations = useMemo(() => section === "organizations" ? (cursor?: string) => api.listAdminOrganizations(cursor) : null, [api, section]);
  const loadUsers = useMemo(() => section === "accounts" ? (cursor?: string) => api.listAdminUsers(cursor, debounced.length >= 2 ? debounced : undefined) : null, [api, section, debounced]);
  const organizations = usePagedList<AdminOrganization>(loadOrganizations);
  const users = usePagedList<AdminUser>(loadUsers);
  const [devicesFor, setDevicesFor] = useState<AdminUser | null>(null); const [devices, setDevices] = useState<AdminDevice[]>([]); const [deviceError, setDeviceError] = useState<unknown>(null); const [deviceCursor, setDeviceCursor] = useState<string | null>(null); const [deviceLoadingMore, setDeviceLoadingMore] = useState(false);
  const deviceGeneration = useRef(0);
  const [confirmUser, setConfirmUser] = useState<AdminUser | null>(null); const [confirmDevice, setConfirmDevice] = useState<AdminDevice | null>(null); const [error, setError] = useState<unknown>(null); const [mutationBusy, setMutationBusy] = useState(false);
  const [resetFor, setResetFor] = useState<AdminUser | null>(null);
  const [resetUrl, setResetUrl] = useState<string | null>(null);
  const [resetExpiry, setResetExpiry] = useState<string | null>(null);
  const [resetBusy, setResetBusy] = useState(false);
  const [resetError, setResetError] = useState<unknown>(null);
  const [resetCopied, setResetCopied] = useState(false);
  const [resetCopyFailed, setResetCopyFailed] = useState(false);
  useEffect(() => {
    const generation = ++deviceGeneration.current;
    if (devicesFor === null) return;
    let live = true;
    setDevices([]); setDeviceCursor(null); setDeviceError(null);
    void api.listAdminUserDevices(devicesFor.id).then((page) => { if (live && generation === deviceGeneration.current) { setDevices(page.devices); setDeviceCursor(page.next_cursor); } }, (failure: unknown) => { if (live && generation === deviceGeneration.current) setDeviceError(failure); });
    return () => { live = false; };
  }, [api, devicesFor]);
  async function loadMoreDevices() {
    if (devicesFor === null || deviceCursor === null || deviceLoadingMore) return;
    const accountId = devicesFor.id;
    const generation = deviceGeneration.current;
    setDeviceLoadingMore(true);
    try {
      const page = await api.listAdminUserDevices(accountId, deviceCursor);
      if (generation !== deviceGeneration.current || devicesFor?.id !== accountId) return;
      setDevices((current) => [...current, ...page.devices.filter((item) => !current.some((old) => old.device_ref === item.device_ref))]);
      setDeviceCursor(page.next_cursor);
    } catch (failure) {
      if (generation === deviceGeneration.current && devicesFor?.id === accountId) setDeviceError(failure);
    } finally {
      if (generation === deviceGeneration.current && devicesFor?.id === accountId) setDeviceLoadingMore(false);
    }
  }
  function closeDevices() {
    deviceGeneration.current += 1;
    setDevicesFor(null); setDevices([]); setDeviceCursor(null); setDeviceError(null); setDeviceLoadingMore(false); setConfirmDevice(null); setError(null);
  }
  function openDevices(user: AdminUser) {
    if (devicesFor?.id === user.id) return;
    deviceGeneration.current += 1;
    setDevicesFor(user); setDevices([]); setDeviceCursor(null); setDeviceError(null); setDeviceLoadingMore(false); setConfirmDevice(null); setError(null);
  }
  if (section === "organizations") return <>
    <ListStatus list={organizations} empty={t("No organizations found.")} loadingLabel={t("Loading organizations…")} />
    {organizations.items.length > 0 && <div className="table-wrap"><table className="table">
      <thead><tr><th>{t("Organization")}</th><th>{t("People")}</th><th>{t("Storage")}</th><th>{t("Quota")}</th><th>{t("Seat limit")}</th></tr></thead>
      <tbody>{organizations.items.map((organization) => <tr key={organization.id}>
        <th>{organization.name}<span className="cell__sub">{organization.slug}</span></th>
        <td data-label={t("People")}>{organization.member_count}</td>
        <td data-label={t("Storage")}>{formatBytes(organization.committed_bytes + organization.reserved_bytes)}</td>
        <td data-label={t("Quota")}><QuotaEditor organization={organization} onSave={(bytes) => api.updateAdminOrganizationQuota(organization.id, bytes).then(() => organizations.refresh())} /></td>
        <td data-label={t("Seat limit")}><SeatLimitEditor organization={organization} onSave={(limit) => api.updateAdminOrganizationSeats(organization.id, limit).then(() => organizations.refresh())} /></td>
      </tr>)}</tbody>
    </table></div>}
    <LoadMoreButton list={organizations} />
  </>;
  return <div className="stack">
    <Field label={t("Search accounts")} name="account-search" required={false} value={query} onChange={setQuery} />
    <ListStatus list={users} empty={t("No accounts found.")} loadingLabel={t("Loading accounts…")} />
    {users.items.length > 0 && <div className="table-wrap"><table className="table">
      <thead><tr><th>{t("Account")}</th><th>{t("Status")}</th><th>{t("E-mail verified")}</th><th>{t("Actions")}</th></tr></thead>
      <tbody>{users.items.map((user) => <tr key={user.id}>
        <th>{user.display_name}<span className="cell__sub cell__sub--email">{user.email}</span></th>
        <td data-label={t("Status")}>{user.disabled ? t("Disabled") : t("Active")}</td>
        <td data-label={t("E-mail verified")}>{user.email_verified ? t("Yes") : t("No")}</td>
        <td data-label={t("Actions")}><div className="row-actions">
          <Button className="button button--sm" onPress={() => openDevices(user)}>{t("Devices")}</Button>
          <Button className={`button button--sm ${user.disabled ? "" : "button--danger-quiet"}`} onPress={() => setConfirmUser(user)}>{user.disabled ? t("Enable") : t("Disable")}</Button>
          <Button className="button button--sm" onPress={() => { setResetFor(user); setResetError(null); }}>{t("Reset password")}</Button>
        </div></td>
      </tr>)}</tbody>
    </table></div>}
    <LoadMoreButton list={users} />
    {error !== null && <ErrorNotice error={error} />}
    {resetFor && <ConfirmDialog
      title={t("Issue account reset link")}
      message={emailDelivery ? t("This signs out the account on all devices and sends a password reset email.") : t("Issuing a reset link signs out this account on all devices. The link is shown once and must be shared securely.")}
      confirmLabel={t("Issue reset link")}
      busy={resetBusy}
      error={resetError}
      onClose={() => { setResetFor(null); setResetError(null); }}
      onConfirm={() => {
        const account = resetFor;
        setResetBusy(true);
        void api.issuePasswordReset(account.id, {}).then((result) => {
          setResetFor(null);
          setResetError(null);
          setResetUrl(result.reset_url ?? null);
          if (result.sent_to !== undefined) announce(t("Password reset email sent."));
          setResetExpiry(result.expires_at);
          setResetCopied(false);
          setResetCopyFailed(false);
        }, setResetError).finally(() => setResetBusy(false));
      }}
    />}
    {resetUrl && <AppModal title={t("Password reset link")} onClose={() => { setResetUrl(null); setResetExpiry(null); setResetCopyFailed(false); }} size="md">
      <p>{t("Copy this link now. It is shown once and expires at {expires}.", { expires: resetExpiry === null ? "" : formatDateTime(language, resetExpiry) })}</p>
      <label className="field">
        <span className="field__label">{t("One-time reset link")}</span>
        <input className="field__input field__input--mono" readOnly value={resetUrl} onFocus={(event) => event.currentTarget.select()} />
      </label>
      {resetCopied && <p role="status">{t("Link copied.")}</p>}
      {resetCopyFailed && <p role="alert">{t("Copying failed. Select the link and copy it by hand.")}</p>}
      <div className="form__actions">
        <Button className="button" onPress={() => {
          void navigator.clipboard.writeText(resetUrl).then(() => {
            setResetCopied(true);
            setResetCopyFailed(false);
          }, () => {
            setResetCopied(false);
            setResetCopyFailed(true);
          });
        }}>{t("Copy link")}</Button>
        <Button className="button button--primary" onPress={() => { setResetUrl(null); setResetExpiry(null); setResetCopyFailed(false); }}>{t("Close")}</Button>
      </div>
    </AppModal>}
    {devicesFor && <section className="admin-detail">
      <h2>{t("Devices for {name}", { name: devicesFor.display_name })}</h2>
      {deviceError !== null && <ErrorNotice error={deviceError} />}
      <ul>{devices.map((device) => <li key={device.device_ref}>{device.name} · {device.client_kind} <Button className="button button--danger-quiet button--sm" onPress={() => setConfirmDevice(device)}>{t("Revoke")}</Button></li>)}</ul>
      {deviceCursor !== null && <Button className="button" onPress={() => void loadMoreDevices()} isDisabled={deviceLoadingMore}>{deviceLoadingMore ? t("Loading…") : t("Load more")}</Button>}
      <Button className="button" onPress={closeDevices}>{t("Close")}</Button>
    </section>}
    {confirmUser && <ConfirmDialog title={confirmUser.disabled ? t("Enable account") : t("Disable account")} message={confirmUser.disabled ? t("This account can sign in again.") : t("This revokes all sessions and blocks sign-in for this account.")} confirmLabel={confirmUser.disabled ? t("Enable") : t("Disable")} danger={!confirmUser.disabled} busy={mutationBusy} error={error} onClose={() => { setConfirmUser(null); setError(null); }} onConfirm={() => {
      const user = confirmUser; setMutationBusy(true);
      void (user.disabled ? api.enableAdminUser(user.id) : api.disableAdminUser(user.id)).then(() => { setConfirmUser(null); setError(null); users.refresh(); }, setError).finally(() => setMutationBusy(false));
    }} onReload={users.refresh} />}
    {confirmDevice && devicesFor && <ConfirmDialog title={t("Revoke device")} message={t("This device session will end and its editing locks will be released.")} confirmLabel={t("Revoke")} danger busy={mutationBusy} error={error} onClose={() => { setConfirmDevice(null); setError(null); }} onConfirm={() => {
      const device = confirmDevice; const account = devicesFor; const generation = deviceGeneration.current; setMutationBusy(true);
      void api.revokeAdminUserDevice(account.id, device.device_ref).then(() => { if (generation === deviceGeneration.current && devicesFor?.id === account.id) { setDevices((current) => current.filter((item) => item.device_ref !== device.device_ref)); setConfirmDevice(null); setError(null); } }, (failure: unknown) => { if (generation === deviceGeneration.current && devicesFor?.id === account.id) setError(failure); }).finally(() => setMutationBusy(false));
    }} />}
  </div>;
}

function QuotaEditor({ organization, onSave }: { organization: AdminOrganization; onSave: (bytes: number | null) => Promise<unknown> }) { const { t } = useI18n(); const [value, setValue] = useState(organization.quota_bytes?.toString() ?? ""); const [error, setError] = useState<unknown>(null); return <div className="quota-editor"><input className="field__input" aria-label={t("Quota for {name}", { name: organization.name })} value={value} onChange={(e) => setValue(e.currentTarget.value)} placeholder={t("Unlimited")}/><Button className="button button--sm" onPress={() => { const n = value.trim() === "" ? null : Number(value); if (n !== null && (!Number.isSafeInteger(n) || n < 0)) { setError(new Error(t("Enter a whole number of bytes."))); return; } void onSave(n).then(() => setError(null), setError); }}>{t("Save")}</Button>{error !== null && <ErrorNotice error={error}/>}</div>; }
function SeatLimitEditor({ organization, onSave }: { organization: AdminOrganization; onSave: (limit: number | null) => Promise<unknown> }) { const { t } = useI18n(); const [value, setValue] = useState(organization.seat_limit?.toString() ?? ""); const [error, setError] = useState<unknown>(null); return <div className="quota-editor"><input className="field__input" type="number" min="0" step="1" aria-label={t("Seat limit for {name}", { name: organization.name })} value={value} onChange={(e) => setValue(e.currentTarget.value)} placeholder={t("Unlimited")}/><Button className="button button--sm" onPress={() => { const n = value.trim() === "" ? null : Number(value); if (n !== null && (!Number.isSafeInteger(n) || n < 0)) { setError(new Error(t("Enter a whole number of seats."))); return; } void onSave(n).then(() => setError(null), setError); }}>{t("Save")}</Button>{error !== null && <ErrorNotice error={error}/>}</div>; }

function AdminSelect({ label, value, onChange, options }: { label: string; value: string; onChange: (value: string) => void; options: [string, string][] }) {
  return <NativeSelect label={label} value={value} onChange={onChange} options={options.map(([id, text]) => ({ id, label: text }))} />;
}

export function formatBytes(bytes: number): string { if (!Number.isFinite(bytes) || bytes < 0) return "—"; if (bytes < 1024) return `${bytes} B`; const units = ["KiB", "MiB", "GiB", "TiB"]; let value = bytes; let unit = "B"; for (const next of units) { value /= 1024; unit = next; if (value < 1024 || next === "TiB") break; } return `${value.toLocaleString(undefined, { maximumFractionDigits: 1 })} ${unit}`; }
