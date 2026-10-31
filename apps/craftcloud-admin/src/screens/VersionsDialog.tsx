import { useEffect, useMemo, useState } from "react";
import { Button } from "react-aria-components";
import type { FileRecord, FileVersion } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { useAnnounce } from "../components/Announcer.tsx";
import { AppModal } from "../components/Dialog.tsx";
import { Badge } from "../components/Badge.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { ListStatus, LoadMoreButton } from "../components/ListStatus.tsx";
import { formatBytes, formatDateTime } from "../format.ts";
import { useAction, useLeaseRefresh } from "../hooks.ts";
import { useI18n } from "../i18n/index.tsx";
import { usePagedList } from "../usePagedList.ts";

/**
 * Version history of a file (newest first) with download of each version and,
 * for writers, "restore as new version". Restoring never deletes anything: it
 * creates a new version that points at the old content.
 */
export function VersionsDialog({
  file,
  canRestore,
  onChanged,
  onClose,
}: {
  file: FileRecord;
  canRestore: boolean;
  /** A restore created a version; the file list should be reloaded. */
  onChanged: () => void;
  onClose: () => void;
}) {
  const { t, language } = useI18n();
  const { api } = useAuth();
  const announce = useAnnounce();
  const action = useAction();
  const [changed, setChanged] = useState(false);
  const [lock, setLock] = useState(file.lock ?? null);
  useEffect(() => setLock(file.lock ?? null), [file.lock]);
  const lease = useLeaseRefresh(lock?.expires_at, async () => {
    const updated = await api.getFile(file.id);
    setLock(updated.lock ?? null);
  });

  const load = useMemo(() => (cursor?: string) => api.listVersions(file.id, cursor), [api, file.id]);
  const versions = usePagedList<FileVersion>(load);
  // The list is newest first, so the first row is the head the restore is based on.
  const head = versions.items[0]?.id ?? null;

  function close() {
    if (changed) onChanged();
    onClose();
  }

  return (
    <AppModal title={t("Versions of {name}", { name: file.name })} onClose={close} busy={action.pending} size="lg">
      {lock != null && <p className="muted"><Badge tone="warn">{t("In progress")}</Badge>{" "}{t("In progress by {name}, since {time}.", { name: lock.holder_display_name, time: formatDateTime(language, lock.since) })}</p>}
      {lock != null && !lock.held_by_current_session && canRestore && <p className="muted">{t("A new version is unavailable until this edit lock is released.")}</p>}
      {lease.error !== null && <ErrorNotice error={lease.error} onRetry={() => void lease.retry()} onReload={() => void lease.retry()} />}
      <ListStatus
        list={versions}
        empty={t("This file has no versions yet.")}
        loadingLabel={t("Loading versions…")}
      />
      {action.failed && (
        <ErrorNotice
          error={action.error}
          onReload={() => {
            action.clear();
            versions.refresh();
          }}
        />
      )}
      {versions.items.length > 0 && (
        <div className="table-wrap">
          <table className="table">
            <caption className="visually-hidden">{t("Versions of {name}", { name: file.name })}</caption>
            <thead>
              <tr>
                <th scope="col">{t("Version")}</th>
                <th scope="col">{t("Size")}</th>
                <th scope="col">{t("Saved")}</th>
                <th scope="col" className="col--optional">{t("SHA-256")}</th>
                <th scope="col">{t("Actions")}</th>
              </tr>
            </thead>
            <tbody>
              {versions.items.map((version) => (
                <tr key={version.id}>
                  <th scope="row">
                    {version.number}
                    {version.id === head ? ` · ${t("Current")}` : ""}
                    {version.restored_from_version_id !== null ? ` · ${t("Restored")}` : ""}
                    {version.actor_kind === "figcraft_live" ? ` · ${t("FigCraft Live")}` : ""}
                  </th>
                  <td className="numeric" data-label={t("Size")}>{formatBytes(language, version.size_bytes)}</td>
                  <td className="numeric" data-label={t("Saved")}>{formatDateTime(language, version.created_at)}</td>
                  <td className="col--optional" data-label={t("SHA-256")}>
                    <code className="hash" title={version.sha256}>
                      {version.sha256.slice(0, 12)}
                    </code>
                  </td>
                  <td data-label={t("Actions")}>
                    <div className="row-actions">
                      <a
                        className="button button--sm"
                        href={api.downloadUrl(file.id, version.id)}
                        download
                        aria-label={t("Download version {number}", { number: version.number })}
                      >
                        {t("Download")}
                      </a>
                      {canRestore && !(lock != null && !lock.held_by_current_session) && version.id !== head && head !== null && (
                        <Button
                          className="button button--sm"
                          isDisabled={action.pending}
                          aria-label={t("Restore version {number} as a new version", {
                            number: version.number,
                          })}
                          onPress={() => {
                            void action
                              .run(() => api.restoreVersion(file.id, version.id, head))
                              .then((created) => {
                                if (created === undefined) return;
                                setChanged(true);
                                versions.refresh();
                                announce(
                                  t("Version {from} restored as version {to}.", {
                                    from: version.number,
                                    to: created.number,
                                  }),
                                );
                              });
                          }}
                        >
                          {t("Restore")}
                        </Button>
                      )}
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <LoadMoreButton list={versions} />
      <div className="form__actions">
        <Button className="button" onPress={close}>
          {t("Close")}
        </Button>
      </div>
    </AppModal>
  );
}
