import { useCallback, useEffect, useMemo, useState } from "react";
import { Button } from "react-aria-components";
import type { FileRecord, SharedItem } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { Badge } from "../components/Badge.tsx";
import { ConfirmDialog } from "../components/Dialog.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { ListStatus, LoadMoreButton } from "../components/ListStatus.tsx";
import { formatBytes, formatDateTime } from "../format.ts";
import { useAction, useLeaseRefresh } from "../hooks.ts";
import { useI18n } from "../i18n/index.tsx";
import { usePagedList } from "../usePagedList.ts";
import { VersionsDialog } from "./VersionsDialog.tsx";

/**
 * `hideWorkspace` is set for guests: they get the items shared with them, not the
 * name of the workspace around them.
 */
export function SharedWithMeScreen({ organizationId, hideWorkspace = false }: { organizationId: string | null; hideWorkspace?: boolean }) {
  const { t } = useI18n();
  const { api } = useAuth();
  const load = useMemo(
    () => organizationId === null ? null : (cursor?: string) => api.listSharedWithMe(organizationId, cursor),
    [api, organizationId],
  );
  const shares = usePagedList<SharedItem>(load);
  const groups = new Map<string, SharedItem[]>();
  for (const item of shares.items) {
    const group = groups.get(item.workspace.id) ?? [];
    group.push(item);
    groups.set(item.workspace.id, group);
  }

  return <>
    <ListStatus list={shares} empty={t("Nothing has been shared with you yet.")} loadingLabel={t("Loading shared items…")} />
    {[...groups.entries()].map(([workspaceId, items]) => <section className="section" key={workspaceId}>
      {!hideWorkspace && <h2 className="section__title">{items[0]?.workspace.name}</h2>}
      {items.filter((item) => item.target_kind === "project" && item.project !== null).map((item) =>
        item.project === null ? null : <SharedProject key={item.target_id} item={item} />,
      )}
      {items.filter((item) => item.target_kind === "file" && item.file !== null).map((item) =>
        item.file === null ? null : <SharedFile key={item.target_id} fileId={item.target_id} name={item.file.name} role={item.role} />,
      )}
    </section>)}
    <LoadMoreButton list={shares} />
  </>;
}

function SharedProject({ item }: { item: SharedItem }) {
  const { t } = useI18n();
  const { api } = useAuth();
  const projectId = item.project?.id ?? item.target_id;
  const load = useMemo(() => (cursor?: string) => api.listFiles(projectId, false, cursor), [api, projectId]);
  const files = usePagedList<FileRecord>(load);
  const lockExpiry = files.items.reduce<string | null>((soonest, file) => {
    const expiry = file.lock?.expires_at;
    if (expiry == null) return soonest;
    return soonest === null || Date.parse(expiry) < Date.parse(soonest) ? expiry : soonest;
  }, null);
  const lease = useLeaseRefresh(lockExpiry, async () => {
    await api.listFiles(projectId, false);
    files.refresh();
  });
  return <section className="section">
    <h3 className="section__subtitle">{item.project?.name}</h3>
    {lease.error !== null && <ErrorNotice error={lease.error} onRetry={() => void lease.retry()} onReload={() => void lease.retry()} />}
    <ListStatus list={files} empty={t("This project has no files yet.")} loadingLabel={t("Loading files…")} />
    {files.items.map((file) => <SharedFile key={file.id} fileId={file.id} name={file.name} role={item.role} record={file} />)}
    <LoadMoreButton list={files} />
  </section>;
}

function SharedFile({ fileId, name, role, record }: {
  fileId: string;
  name: string;
  role: "viewer" | "editor";
  record?: FileRecord;
}) {
  const { t, language } = useI18n();
  const { api } = useAuth();
  const [file, setFile] = useState<FileRecord | null>(record ?? null);
  const [loadError, setLoadError] = useState<unknown>(null);
  const [versionsOpen, setVersionsOpen] = useState(false);
  const [forceOpen, setForceOpen] = useState(false);
  const force = useAction();
  const reloadFile = useCallback(async () => {
    const updated = await api.getFile(fileId);
    setFile(updated);
    setLoadError(null);
  }, [api, fileId]);
  useEffect(() => {
    if (record !== undefined) {
      setFile(record);
      return;
    }
    let current = true;
    setFile(null);
    setLoadError(null);
    api.getFile(fileId).then(
      (result) => { if (current) setFile(result); },
      (failure: unknown) => { if (current) setLoadError(failure); },
    );
    return () => { current = false; };
  }, [api, fileId, record]);

  const lockedElsewhere = file?.lock !== null && file?.lock !== undefined && !file.lock.held_by_current_session;
  const lock = file?.lock ?? null;
  const loadAgain = () => {
    setLoadError(null);
    api.getFile(fileId).then(setFile, setLoadError);
  };
  const lease = useLeaseRefresh(file?.lock?.expires_at, reloadFile);
  return <div className="section">
    <div className="section__head">
      <strong>{name}</strong>
      <div className="row-actions">
        <Badge>{role === "editor" ? t("Editor") : t("Viewer")}</Badge>
        {file?.head_version_id !== null && file !== null && <a className="button button--sm" href={api.downloadUrl(fileId)} download aria-label={t("Download {name}", { name })}>{t("Download")}</a>}
        {file !== null && <Button className="button button--sm" onPress={() => setVersionsOpen(true)}>{t("Versions")}</Button>}
      </div>
    </div>
    {file !== null && <p className="muted">{[
      file.format,
      t("Size: {size}", { size: formatBytes(language, file.size_bytes) }),
      t("Updated {time}", { time: formatDateTime(language, file.updated_at) }),
    ].join(" · ")}</p>}
    {loadError !== null && <ErrorNotice error={loadError} onRetry={loadAgain} onReload={loadAgain} />}
    {lease.error !== null && <ErrorNotice error={lease.error} onRetry={() => void lease.retry()} onReload={() => void lease.retry()} />}
    {lock !== null && <p className="muted">
      <Badge tone="warn">{t("In progress")}</Badge>{" "}
      {t("In progress by {name}, since {time}.", { name: lock.holder_display_name, time: formatDateTime(language, lock.since) })}
      {lockedElsewhere && role === "editor" && <span className="cell__sub">{t("A new version is unavailable until this edit lock is released.")}</span>}
      {lockedElsewhere && file?.can_force_lock === true && <Button className="button button--danger-quiet button--sm" onPress={() => setForceOpen(true)}>{t("Release lock")}</Button>}
    </p>}
    {versionsOpen && file !== null && <VersionsDialog file={file} canRestore={role === "editor"} onChanged={() => undefined} onClose={() => setVersionsOpen(false)} />}
    {forceOpen && lock !== null && <ConfirmDialog
      title={t("Release edit lock")}
      message={t("Release the edit lock held by {name}?", { name: lock.holder_display_name })}
      confirmLabel={t("Release lock")}
      danger
      busy={force.pending}
      error={force.failed ? force.error : null}
      onClose={() => { setForceOpen(false); force.clear(); }}
      onReload={() => { setForceOpen(false); force.clear(); }}
      onConfirm={() => { void force.run(async () => { await api.releaseFileLock(fileId, true); const updated = await api.getFile(fileId); setFile(updated); return true; }).then((done) => { if (done === true) setForceOpen(false); }); }}
    />}
  </div>;
}
