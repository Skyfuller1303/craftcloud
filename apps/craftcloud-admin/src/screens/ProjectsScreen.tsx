import { useMemo, useRef, useState } from "react";
import type { FormEvent } from "react";
import { Button, Checkbox, Form } from "react-aria-components";
import { isApiError } from "../api/client.ts";
import type { Project } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { useAnnounce } from "../components/Announcer.tsx";
import { AppModal, ConfirmDialog } from "../components/Dialog.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { Field, focusFirstInvalid } from "../components/forms.tsx";
import { ListStatus } from "../components/ListStatus.tsx";
import { formatDateTime } from "../format.ts";
import { useAction } from "../hooks.ts";
import { useI18n } from "../i18n/index.tsx";
import { canTrashProjects, canWriteContent } from "../permissions.ts";
import type { Scope } from "../permissions.ts";
import { usePagedList } from "../usePagedList.ts";

const MAX_NAME = 100;

function validName(name: string): boolean {
  const length = [...name.trim()].length;
  return length >= 1 && length <= MAX_NAME;
}

export function ProjectsScreen({
  scope,
  onOpenProject,
}: {
  scope: Scope;
  onOpenProject: (project: Project) => void;
}) {
  const { t, language } = useI18n();
  const { api } = useAuth();
  const announce = useAnnounce();
  const [showTrash, setShowTrash] = useState(false);
  const [renaming, setRenaming] = useState<Project | null>(null);
  const [trashing, setTrashing] = useState<Project | null>(null);
  const trashAction = useAction();
  const restoreAction = useAction();

  const workspaceId = scope.workspace?.id ?? null;
  const load = useMemo(
    () =>
      workspaceId === null
        ? null
        : (cursor?: string) => api.listProjects(workspaceId, showTrash, cursor),
    [api, workspaceId, showTrash],
  );
  const projects = usePagedList<Project>(load);
  const mayWrite = canWriteContent(scope);
  const mayTrash = canTrashProjects(scope);

  if (scope.workspace === null || workspaceId === null) {
    return <p className="muted">{t("Select a workspace to see its projects.")}</p>;
  }

  return (
    <>
      {mayWrite && !showTrash && (
        <CreateProjectForm
          workspaceId={workspaceId}
          onCreated={(project) => {
            projects.refresh();
            announce(t("Project {name} created.", { name: project.name }));
          }}
        />
      )}
      {!mayWrite && (
        <p className="muted">{t("You can read projects here but not change them.")}</p>
      )}

      <Checkbox className="checkbox" isSelected={showTrash} onChange={setShowTrash}>
        <span className="checkbox__box" aria-hidden="true" />
        {t("Show trash")}
      </Checkbox>

      {restoreAction.failed && (
        <ErrorNotice error={restoreAction.error} onReload={projects.refresh} />
      )}
      <ListStatus
        list={projects}
        empty={showTrash ? t("The trash has no projects.") : t("This workspace has no projects yet.")}
        loadingLabel={t("Loading projects…")}
      />
      {projects.items.length > 0 && (
        <div className="table-wrap">
          <table className="table">
            <caption className="visually-hidden">
              {showTrash ? t("Projects in the trash") : t("Projects")}
            </caption>
            <thead>
              <tr>
                <th scope="col">{t("Name")}</th>
                <th scope="col">{showTrash ? t("Deleted") : t("Updated")}</th>
                <th scope="col">{t("Actions")}</th>
              </tr>
            </thead>
            <tbody>
              {projects.items.map((project) => (
                <tr key={project.id}>
                  <th scope="row">{project.name}</th>
                  <td>{formatDateTime(language, project.deleted_at ?? project.updated_at)}</td>
                  <td>
                    <div className="row-actions">
                      {!showTrash && (
                        <Button
                          className="button"
                          aria-label={t("Open files of {name}", { name: project.name })}
                          onPress={() => onOpenProject(project)}
                        >
                          {t("Files")}
                        </Button>
                      )}
                      {!showTrash && mayWrite && (
                        <Button
                          className="button"
                          aria-label={t("Rename {name}", { name: project.name })}
                          onPress={() => setRenaming(project)}
                        >
                          {t("Rename")}
                        </Button>
                      )}
                      {!showTrash && mayTrash && (
                        <Button
                          className="button"
                          aria-label={t("Move to trash: {name}", { name: project.name })}
                          onPress={() => setTrashing(project)}
                        >
                          {t("Move to trash")}
                        </Button>
                      )}
                      {showTrash && mayTrash && (
                        <Button
                          className="button"
                          aria-label={t("Restore {name}", { name: project.name })}
                          isDisabled={restoreAction.pending}
                          onPress={() => {
                            void restoreAction
                              .run(() => api.restoreProject(project.id))
                              .then((restored) => {
                                if (restored === undefined) return;
                                projects.mutate((items) =>
                                  items.filter((item) => item.id !== project.id),
                                );
                                announce(t("Project {name} restored.", { name: restored.name }));
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

      {renaming !== null && (
        <RenameProjectDialog
          project={renaming}
          onClose={() => setRenaming(null)}
          onReload={() => {
            projects.refresh();
            setRenaming(null);
          }}
          onRenamed={(renamed) => {
            projects.mutate((items) => items.map((item) => (item.id === renamed.id ? renamed : item)));
            announce(t("Project renamed to {name}.", { name: renamed.name }));
            setRenaming(null);
          }}
        />
      )}

      {trashing !== null && (
        <ConfirmDialog
          title={t("Move project to the trash")}
          message={t("{name} and its files become read-only and can be restored from the trash.", {
            name: trashing.name,
          })}
          confirmLabel={t("Move to trash")}
          danger
          busy={trashAction.pending}
          error={trashAction.failed ? trashAction.error : null}
          onReload={() => {
            projects.refresh();
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
                await api.trashProject(target.id);
                return true;
              })
              .then((done) => {
                if (done !== true) return;
                projects.mutate((items) => items.filter((item) => item.id !== target.id));
                announce(t("Project {name} moved to the trash.", { name: target.name }));
                setTrashing(null);
              });
          }}
        />
      )}
    </>
  );
}

function CreateProjectForm({
  workspaceId,
  onCreated,
}: {
  workspaceId: string;
  onCreated: (project: Project) => void;
}) {
  const { t } = useI18n();
  const { api } = useAuth();
  const [name, setName] = useState("");
  const [nameError, setNameError] = useState<string | undefined>(undefined);
  const action = useAction();
  const form = useRef<HTMLFormElement>(null);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (action.pending) return;
    if (!validName(name)) {
      setNameError(t("Enter a name of 1 to {max} characters.", { max: MAX_NAME }));
      queueMicrotask(() => focusFirstInvalid(form.current));
      return;
    }
    setNameError(undefined);
    const created = await action.run(async () => {
      try {
        return await api.createProject(workspaceId, name.trim());
      } catch (failure) {
        if (isApiError(failure) && failure.code === "PROJECT_NAME_TAKEN") {
          setNameError(t("A project with this name already exists."));
        } else if (isApiError(failure) && failure.fields.includes("name")) {
          setNameError(t("This value is not accepted."));
        }
        throw failure;
      }
    });
    if (created !== undefined) {
      setName("");
      onCreated(created);
    } else {
      // After the re-render that marks the field invalid.
      window.setTimeout(() => focusFirstInvalid(form.current), 0);
    }
  }

  return (
    <Form className="form form--inline" onSubmit={(event) => void submit(event)} ref={form} validationBehavior="aria">
      <Field label={t("New project name")} name="project-name" value={name} onChange={setName} error={nameError} />
      {action.failed && nameError === undefined && <ErrorNotice error={action.error} />}
      <Button type="submit" className="button button--primary" isDisabled={action.pending}>
        {action.pending ? t("Creating…") : t("Create project")}
      </Button>
    </Form>
  );
}

function RenameProjectDialog({
  project,
  onRenamed,
  onClose,
  onReload,
}: {
  project: Project;
  onRenamed: (project: Project) => void;
  onClose: () => void;
  onReload: () => void;
}) {
  const { t } = useI18n();
  const { api } = useAuth();
  const [name, setName] = useState(project.name);
  const [nameError, setNameError] = useState<string | undefined>(undefined);
  const action = useAction();
  const form = useRef<HTMLFormElement>(null);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (action.pending) return;
    if (!validName(name)) {
      setNameError(t("Enter a name of 1 to {max} characters.", { max: MAX_NAME }));
      queueMicrotask(() => focusFirstInvalid(form.current));
      return;
    }
    setNameError(undefined);
    const renamed = await action.run(async () => {
      try {
        return await api.renameProject(project, name.trim());
      } catch (failure) {
        if (isApiError(failure) && failure.code === "PROJECT_NAME_TAKEN") {
          setNameError(t("A project with this name already exists."));
        }
        throw failure;
      }
    });
    if (renamed !== undefined) {
      onRenamed(renamed);
    } else {
      // After the re-render that marks the field invalid.
      window.setTimeout(() => focusFirstInvalid(form.current), 0);
    }
  }

  return (
    <AppModal title={t("Rename project")} onClose={onClose} busy={action.pending}>
      <Form className="form" onSubmit={(event) => void submit(event)} ref={form} validationBehavior="aria">
        <Field label={t("Project name")} name="rename-project" value={name} onChange={setName} error={nameError} />
        {action.failed && nameError === undefined && <ErrorNotice error={action.error} onReload={onReload} />}
        <div className="form__actions">
          <Button type="submit" className="button button--primary" isDisabled={action.pending}>
            {t("Save")}
          </Button>
          <Button className="button" onPress={onClose} isDisabled={action.pending}>
            {t("Cancel")}
          </Button>
        </div>
      </Form>
    </AppModal>
  );
}
