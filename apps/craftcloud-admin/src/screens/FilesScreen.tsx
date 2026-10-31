import { useEffect, useMemo, useRef, useState } from "react";
import { Button, Checkbox } from "react-aria-components";
import type { FileLock, FileRecord, Project } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { useAnnounce } from "../components/Announcer.tsx";
import { Badge } from "../components/Badge.tsx";
import { ContextSelect } from "../components/ContextSelect.tsx";
import { ConfirmDialog } from "../components/Dialog.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { Icon } from "../components/Icon.tsx";
import { ListStatus, LoadMoreButton } from "../components/ListStatus.tsx";
import { formatBytes, formatDateTime } from "../format.ts";
import { useAction, useLeaseRefresh } from "../hooks.ts";
import { useI18n } from "../i18n/index.tsx";
import { canWriteContent } from "../permissions.ts";
import type { Scope } from "../permissions.ts";
import { usePagedList } from "../usePagedList.ts";
import { UploadDialog } from "./UploadDialog.tsx";
import type { UploadTarget } from "./UploadDialog.tsx";
import { VersionsDialog } from "./VersionsDialog.tsx";
import { SharingDialog } from "./SharingDialog.tsx";
import type { ShareTarget } from "./SharingDialog.tsx";
import { FigcraftLiveDialog, useFigcraftAvailable } from "./FigcraftLiveDialog.tsx";

export function FilesScreen({
  scope,
  projectId,
  onProjectChange,
}: {
  scope: Scope;
  projectId: string | null;
  onProjectChange: (projectId: string | null) => void;
}) {
  const { t, language } = useI18n();
  const { api } = useAuth();
  const announce = useAnnounce();
  const [showTrash, setShowTrash] = useState(false);
  const [uploading, setUploading] = useState<UploadTarget | null>(null);
  const [versionsOf, setVersionsOf] = useState<FileRecord | null>(null);
  const [trashing, setTrashing] = useState<FileRecord | null>(null);
  const [unlocking, setUnlocking] = useState<FileRecord | null>(null);
  const [sharing, setSharing] = useState<ShareTarget | null>(null);
  const [liveFile, setLiveFile] = useState<FileRecord | null>(null);
  const liveAvailable = useFigcraftAvailable();
  useEffect(() => { setLiveFile(null); }, [projectId, scope.organization?.id]);
  const trashAction = useAction();
  const restoreAction = useAction();
  const unlockAction = useAction();

  const workspaceId = scope.workspace?.id ?? null;
  const loadProjects = useMemo(
    () => (workspaceId === null ? null : (cursor?: string) => api.listProjects(workspaceId, false, cursor)),
    [api, workspaceId],
  );
  const projects = usePagedList<Project>(loadProjects);
  const listed = projects.items.find((candidate) => candidate.id === projectId) ?? null;

  // A project chosen elsewhere (for example "Files" in the project list) may lie beyond the
  // first page of the selector: read it directly instead of pretending it does not exist.
  const [fetched, setFetched] = useState<Project | null>(null);
  useEffect(() => {
    setFetched(null);
    if (projectId === null || listed !== null || projects.status !== "ready") {
      return;
    }
    let current = true;
    api.getProject(projectId).then(
      (found) => {
        if (current && found.workspace_id === workspaceId && found.deleted_at === null) setFetched(found);
      },
      () => undefined, // the selector then simply shows no selection
    );
    return () => {
      current = false;
    };
  }, [api, projectId, listed, projects.status, workspaceId]);
  const project = listed ?? (fetched?.id === projectId ? fetched : null);

  const selectedId = project?.id ?? null;
  const loadFiles = useMemo(
    () =>
      selectedId === null
        ? null
        : (cursor?: string) => api.listFiles(selectedId, showTrash, cursor),
    [api, selectedId, showTrash],
  );
  const files = usePagedList<FileRecord>(loadFiles);
  const [lockOverrides, setLockOverrides] = useState<Record<string, { source: FileRecord; lock: FileLock | null }>>({});
  const currentFileScope = useRef("");
  currentFileScope.current = `${selectedId ?? ""}:${showTrash}`;
  useEffect(() => {
    setLockOverrides((current) => {
      const stillCurrent = Object.entries(current).filter(([id, override]) =>
        files.items.some((file) => file.id === id && file === override.source),
      );
      return stillCurrent.length === Object.keys(current).length ? current : Object.fromEntries(stillCurrent);
    });
  }, [files.items]);
  const lockForFile = (file: FileRecord): FileLock | null =>
    lockOverrides[file.id]?.source === file
      ? lockOverrides[file.id]?.lock ?? null
      : file.lock ?? null;
  const lockExpiry = files.items.reduce<string | null>((soonest, file) => {
    const expiry = lockForFile(file)?.expires_at;
    if (expiry == null) return soonest;
    return soonest === null || Date.parse(expiry) < Date.parse(soonest) ? expiry : soonest;
  }, null);
  const lease = useLeaseRefresh(lockExpiry, async () => {
    const scope = currentFileScope.current;
    const expired = files.items.filter((file) => {
      const lock = lockForFile(file);
      return lock !== null && Date.parse(lock.expires_at) <= Date.now();
    });
    const refreshed = await Promise.all(expired.map(async (file) => {
      const updated = await api.getFile(file.id);
      return [file.id, updated.lock ?? null] as const;
    }));
    if (currentFileScope.current === scope) {
      const sourceById = new Map(files.items.map((file) => [file.id, file]));
      setLockOverrides((current) => ({
        ...current,
        ...Object.fromEntries(refreshed.flatMap(([id, lock]) => {
          const source = sourceById.get(id);
          return source === undefined ? [] : [[id, { source, lock }]];
        })),
      }));
    }
  });
  const mayWrite = canWriteContent(scope);

  if (scope.workspace === null || workspaceId === null) {
    return <p className="muted">{t("Select a workspace to see its files.")}</p>;
  }

  return (
    <>
      <div className="toolbar">
        <ContextSelect
          label={t("Project")}
          placeholder={projects.status === "loading" ? t("Loading…") : t("Choose a project")}
          choices={[
            ...projects.items,
            ...(project !== null && listed === null ? [project] : []),
          ].map((item) => ({ id: item.id, label: item.name }))}
          selectedId={project?.id ?? null}
          onSelect={onProjectChange}
          isDisabled={projects.items.length === 0 && project === null}
        />
        {projects.hasMore && (
          <Button className="button" onPress={projects.loadMore} isDisabled={projects.loadingMore}>
            {t("Load more projects")}
          </Button>
        )}
      </div>
      {projects.status === "error" && (
        <ErrorNotice error={projects.error} onRetry={projects.reload} onReload={projects.reload} />
      )}
      {lease.error !== null && <ErrorNotice error={lease.error} onRetry={() => void lease.retry()} onReload={() => void lease.retry()} />}
      {projects.status === "ready" && projects.items.length === 0 && (
        <p className="muted">{t("This workspace has no projects yet.")}</p>
      )}
      {projects.status === "ready" && projects.items.length > 0 && project === null && (
        <p className="muted">{t("Choose a project to see its files.")}</p>
      )}

      {project !== null && (
        <>
          <div className="toolbar">
            {mayWrite && !showTrash && (
              <Button
                className="button button--primary"
                onPress={() => setUploading({ kind: "new-file", project })}
              >
                {t("Upload file")}
              </Button>
            )}
            <Checkbox className="checkbox" isSelected={showTrash} onChange={setShowTrash}>
              <span className="checkbox__box" aria-hidden="true"><Icon name="check" size={12} /></span>
              {t("Show trash")}
            </Checkbox>
          </div>
          {!mayWrite && <p className="muted">{t("You can download files here but not change them.")}</p>}
          {restoreAction.failed && <ErrorNotice error={restoreAction.error} onReload={files.refresh} />}
          <ListStatus
            list={files}
            empty={showTrash ? t("The trash has no files.") : t("This project has no files yet.")}
            loadingLabel={t("Loading files…")}
          />
          {files.items.length > 0 && (
            <div className="table-wrap">
              <table className="table">
                <caption className="visually-hidden">
                  {showTrash ? t("Files in the trash") : t("Files")}
                </caption>
                <thead>
                  <tr>
                    <th scope="col">{t("Name")}</th>
                    <th scope="col" className="col--optional">{t("Format")}</th>
                    <th scope="col">{t("Size")}</th>
                    <th scope="col">{showTrash ? t("Deleted") : t("Updated")}</th>
                    <th scope="col">{t("Actions")}</th>
                  </tr>
                </thead>
                <tbody>
                  {files.items.map((file) => {
                    const fileLock = lockForFile(file);
                    return (
                    <tr key={file.id}>
                      <th scope="row">
                        {file.name}
                        {fileLock !== null && <span className="cell__sub"><Badge tone="warn">{t("In progress")}</Badge> {t("In progress by {name}, since {time}.", { name: fileLock.holder_display_name, time: formatDateTime(language, fileLock.since) })}</span>}
                      </th>
                      <td className="hash col--optional" data-label={t("Format")}>
                        {file.app} · {file.format}
                      </td>
                      <td className="numeric" data-label={t("Size")}>
                        {file.head_version_id === null
                          ? <Badge>{t("No content yet")}</Badge>
                          : formatBytes(language, file.size_bytes)}
                      </td>
                      <td className="numeric" data-label={showTrash ? t("Deleted") : t("Updated")}>{formatDateTime(language, file.deleted_at ?? file.updated_at)}</td>
                      <td data-label={t("Actions")}>
                        <div className="row-actions">
                          {!showTrash && file.head_version_id !== null && (
                            <a
                              className="button button--sm"
                              href={api.downloadUrl(file.id)}
                              download
                              aria-label={t("Download {name}", { name: file.name })}
                            >
                              {t("Download")}
                            </a>
                          )}
                          {!showTrash && (
                            <Button
                              className="button button--sm"
                              aria-label={t("Versions of {name}", { name: file.name })}
                              onPress={() => setVersionsOf({ ...file, lock: fileLock })}
                            >
                              {t("Versions")}
                            </Button>
                          )}
                          {!showTrash && file.can_share === true && (
                            <Button className="button button--sm" aria-label={t("Share {name}", { name: file.name })} onPress={() => setSharing({ kind: "file", id: file.id, name: file.name })}>
                              {t("Share")}
                            </Button>
                          )}
                          {!showTrash && liveAvailable && file.app === "figcraft" && file.head_version_id !== null && (
                            <Button className="button button--sm" aria-label={t("Open {name} live", { name: file.name })} onPress={() => setLiveFile(file)}>{t("Open live")}</Button>
                          )}
                          {!showTrash && mayWrite && (
                            <Button
                              className="button button--sm"
                              aria-label={t("Upload a new version of {name}", { name: file.name })}
                              isDisabled={fileLock !== null && !fileLock.held_by_current_session}
                              {...(fileLock !== null && !fileLock.held_by_current_session ? { "aria-describedby": `file-lock-note-${file.id}` } : {})}
                              onPress={() => setUploading({ kind: "new-version", file: { ...file, lock: fileLock } })}
                            >
                              {t("New version")}
                            </Button>
                          )}
                          {!showTrash && mayWrite && fileLock !== null && !fileLock.held_by_current_session && (
                            <span id={`file-lock-note-${file.id}`} className="cell__sub">{t("A new version is unavailable until this edit lock is released.")}</span>
                          )}
                          {!showTrash && file.can_force_lock === true && fileLock !== null && (
                            <Button className="button button--danger-quiet button--sm" aria-label={t("Release lock for {name}", { name: file.name })} onPress={() => setUnlocking({ ...file, lock: fileLock })}>
                              {t("Release lock")}
                            </Button>
                          )}
                          {!showTrash && mayWrite && (
                            <Button
                              className="button button--danger-quiet button--sm"
                              aria-label={t("Move to trash: {name}", { name: file.name })}
                              onPress={() => setTrashing(file)}
                            >
                              {t("Move to trash")}
                            </Button>
                          )}
                          {showTrash && mayWrite && (
                            <Button
                              className="button button--sm"
                              aria-label={t("Restore {name}", { name: file.name })}
                              isDisabled={restoreAction.pending}
                              onPress={() => {
                                void restoreAction
                                  .run(() => api.restoreFile(file.id))
                                  .then((restored) => {
                                    if (restored === undefined) return;
                                    files.mutate((items) => items.filter((item) => item.id !== file.id));
                                    announce(t("File {name} restored.", { name: restored.name }));
                                  });
                              }}
                            >
                              {t("Restore")}
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
        </>
      )}
      {project !== null && <LoadMoreButton list={files} />}

      {uploading !== null && (
        <UploadDialog target={uploading} onChanged={files.refresh} onClose={() => setUploading(null)} />
      )}

      {liveFile !== null && <FigcraftLiveDialog key={liveFile.id} fileId={liveFile.id} name={liveFile.name} onClose={() => setLiveFile(null)} onChanged={() => files.refresh()} />}
      {versionsOf !== null && (
        <VersionsDialog
          file={versionsOf}
          canRestore={mayWrite}
          onChanged={files.refresh}
          onClose={() => setVersionsOf(null)}
        />
      )}

      {trashing !== null && (
        <ConfirmDialog
          title={t("Move file to the trash")}
          message={t("{name} can be restored from the trash.", { name: trashing.name })}
          confirmLabel={t("Move to trash")}
          danger
          busy={trashAction.pending}
          error={trashAction.failed ? trashAction.error : null}
          onReload={() => {
            files.refresh();
            setTrashing(null);
            trashAction.clear();
          }}
          onClose={() => {
            setTrashing(null);
            trashAction.clear();
          }}
          onConfirm={() => {
            const target = trashing;
            void trashAction
              .run(async () => {
                await api.trashFile(target.id);
                return true;
              })
              .then((done) => {
                if (done !== true) return;
                files.mutate((items) => items.filter((item) => item.id !== target.id));
                announce(t("File {name} moved to the trash.", { name: target.name }));
                setTrashing(null);
              });
          }}
        />
      )}

      {unlocking !== null && unlocking.lock != null && (
        <ConfirmDialog
          title={t("Release edit lock")}
          message={t("Release the edit lock held by {name}?", { name: unlocking.lock.holder_display_name })}
          confirmLabel={t("Release lock")}
          danger
          busy={unlockAction.pending}
          error={unlockAction.failed ? unlockAction.error : null}
          onClose={() => { setUnlocking(null); unlockAction.clear(); }}
          onReload={() => { files.refresh(); setUnlocking(null); unlockAction.clear(); }}
          onConfirm={() => {
            const target = unlocking;
            void unlockAction.run(async () => {
              await api.releaseFileLock(target.id, true);
              return true;
            }).then((done) => {
              if (done === true) {
                const source = files.items.find((item) => item.id === target.id);
                if (source !== undefined) {
                  setLockOverrides((current) => ({ ...current, [target.id]: { source, lock: null } }));
                }
                files.refresh();
                setUnlocking(null);
              }
            });
          }}
        />
      )}

      {sharing !== null && scope.organization !== null && (
        <SharingDialog target={sharing} organizationId={scope.organization.id} onClose={() => setSharing(null)} />
      )}
    </>
  );
}
