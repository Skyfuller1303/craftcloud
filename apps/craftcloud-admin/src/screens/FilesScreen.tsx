import { useEffect, useMemo, useState } from "react";
import { Button, Checkbox } from "react-aria-components";
import type { FileRecord, Project } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { useAnnounce } from "../components/Announcer.tsx";
import { ContextSelect } from "../components/ContextSelect.tsx";
import { ConfirmDialog } from "../components/Dialog.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { ListStatus } from "../components/ListStatus.tsx";
import { formatBytes, formatDateTime } from "../format.ts";
import { useAction } from "../hooks.ts";
import { useI18n } from "../i18n/index.tsx";
import { canWriteContent } from "../permissions.ts";
import type { Scope } from "../permissions.ts";
import { usePagedList } from "../usePagedList.ts";
import { UploadDialog } from "./UploadDialog.tsx";
import type { UploadTarget } from "./UploadDialog.tsx";
import { VersionsDialog } from "./VersionsDialog.tsx";

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
  const trashAction = useAction();
  const restoreAction = useAction();

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
              <span className="checkbox__box" aria-hidden="true" />
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
                    <th scope="col">{t("Format")}</th>
                    <th scope="col">{t("Size")}</th>
                    <th scope="col">{showTrash ? t("Deleted") : t("Updated")}</th>
                    <th scope="col">{t("Actions")}</th>
                  </tr>
                </thead>
                <tbody>
                  {files.items.map((file) => (
                    <tr key={file.id}>
                      <th scope="row">{file.name}</th>
                      <td>
                        {file.app} · {file.format}
                      </td>
                      <td>
                        {file.head_version_id === null
                          ? t("No content yet")
                          : formatBytes(language, file.size_bytes)}
                      </td>
                      <td>{formatDateTime(language, file.deleted_at ?? file.updated_at)}</td>
                      <td>
                        <div className="row-actions">
                          {!showTrash && file.head_version_id !== null && (
                            <a
                              className="button"
                              href={api.downloadUrl(file.id)}
                              download
                              aria-label={t("Download {name}", { name: file.name })}
                            >
                              {t("Download")}
                            </a>
                          )}
                          {!showTrash && (
                            <Button
                              className="button"
                              aria-label={t("Versions of {name}", { name: file.name })}
                              onPress={() => setVersionsOf(file)}
                            >
                              {t("Versions")}
                            </Button>
                          )}
                          {!showTrash && mayWrite && (
                            <Button
                              className="button"
                              aria-label={t("Upload a new version of {name}", { name: file.name })}
                              onPress={() => setUploading({ kind: "new-version", file })}
                            >
                              {t("New version")}
                            </Button>
                          )}
                          {!showTrash && mayWrite && (
                            <Button
                              className="button"
                              aria-label={t("Move to trash: {name}", { name: file.name })}
                              onPress={() => setTrashing(file)}
                            >
                              {t("Move to trash")}
                            </Button>
                          )}
                          {showTrash && mayWrite && (
                            <Button
                              className="button"
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
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </>
      )}

      {uploading !== null && (
        <UploadDialog target={uploading} onChanged={files.refresh} onClose={() => setUploading(null)} />
      )}

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
    </>
  );
}
