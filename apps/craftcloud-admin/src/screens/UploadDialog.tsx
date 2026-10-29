import { useEffect, useRef, useState } from "react";
import type { FormEvent } from "react";
import { Button, Form } from "react-aria-components";
import { ABORTED_ERROR_CODE, isApiError } from "../api/client.ts";
import { classifyError } from "../api/errors.ts";
import { sha256OfBlob } from "../api/sha256.ts";
import type { FileRecord, Project, Upload } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { useAnnounce } from "../components/Announcer.tsx";
import { AppModal } from "../components/Dialog.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { Alert, Field, focusFirstInvalid } from "../components/forms.tsx";
import { formatBytes } from "../format.ts";
import { useI18n } from "../i18n/index.tsx";
import {
  APP_PATTERN,
  DEFAULT_APP,
  MAX_NAME_BYTES,
  conflictCopyName,
  formatOf,
  nameBytes,
} from "./fileNames.ts";

export type UploadTarget =
  | { kind: "new-file"; project: Project }
  | { kind: "new-version"; file: FileRecord };

type Step = "creating" | "hashing" | "reserving" | "uploading" | "committing";

type Phase =
  | { name: "choose" }
  | { name: "working"; step: Step; done: number; total: number }
  | { name: "conflict"; currentVersionId: string }
  | { name: "done"; fileName: string; copy: boolean }
  | { name: "failed"; error: unknown };

interface FormErrors {
  file?: string | undefined;
  name?: string | undefined;
  app?: string | undefined;
}

/** Failures of the body where the content has to be sent again (and may need hashing again). */
const RESTART_CODES = new Set(["UPLOAD_INTERRUPTED", "CHECKSUM_MISMATCH", "LENGTH_MISMATCH"]);

/** How many "(conflict copy N)" names are tried before giving up. */
const COPY_NAME_ATTEMPTS = 5;

function newKey(): string {
  return globalThis.crypto.randomUUID();
}

/**
 * Uploads one file as a new file or as the next version of an existing one.
 *
 * Steps (API semantics in contracts/openapi.yaml): create the file record (new
 * files only) -> SHA-256 of the content, read in chunks -> `POST .../uploads`
 * with an Idempotency-Key (reserves quota) -> streamed `PUT` with progress ->
 * `POST .../versions` with `If-Match` (head) or `If-None-Match: *` (first
 * version). A failed commit with 412 VERSION_CONFLICT keeps the stored upload
 * and asks: keep both (save as a separate copy), overwrite (commit on top of
 * the current head) or cancel.
 */
export function UploadDialog({
  target,
  onChanged,
  onClose,
}: {
  target: UploadTarget;
  /** Something changed on the server (file created or version committed); reload lists. */
  onChanged: () => void;
  onClose: () => void;
}) {
  const { t, language } = useI18n();
  const { api } = useAuth();
  const announce = useAnnounce();
  const form = useRef<HTMLFormElement>(null);

  const [chosen, setChosen] = useState<File | null>(null);
  const [name, setName] = useState(target.kind === "new-version" ? target.file.name : "");
  const [app, setApp] = useState(DEFAULT_APP);
  const [errors, setErrors] = useState<FormErrors>({});
  const [phase, setPhase] = useState<Phase>({ name: "choose" });

  // Progress that survives a retry: what already exists on the server.
  const fileRecord = useRef<FileRecord | null>(target.kind === "new-version" ? target.file : null);
  const digest = useRef<{ key: string; hex: string } | null>(null);
  const session = useRef<Upload | null>(null);
  const stored = useRef(false);
  const committed = useRef(false);
  const sessionKey = useRef<string | null>(null);
  const changed = useRef(false);
  const controller = useRef<AbortController | null>(null);
  const alive = useRef(true);
  const copyRecord = useRef<FileRecord | null>(null);
  const retryRef = useRef<(() => void) | null>(null);
  const apiRef = useRef(api);
  apiRef.current = api;

  /** Gives the reserved space back; a session that was committed is left alone. */
  function releaseSession() {
    const open = session.current;
    if (open !== null && !committed.current) {
      session.current = null;
      void apiRef.current.abortUpload(open.upload_id).catch(() => undefined);
    }
  }

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
      controller.current?.abort();
      // Leaving the dialog in any way (also unmounting) must not keep a reservation for 24 h.
      releaseSession();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  function update(next: Phase) {
    if (alive.current) setPhase(next);
  }

  const stepLabel: Record<Step, string> = {
    creating: t("Creating the file…"),
    hashing: t("Checking the file…"),
    reserving: t("Reserving space…"),
    uploading: t("Uploading…"),
    committing: t("Saving the version…"),
  };

  function working(step: Step, done = 0, total = 0) {
    update({ name: "working", step, done, total });
    announce(stepLabel[step]);
  }

  /** Head the commit is based on: the one shown when the dialog opened, `null` for a new file. */
  const baseHead = useRef<string | null>(
    target.kind === "new-version" ? target.file.head_version_id : null,
  );

  async function commit(expectedHead: string | null) {
    const record = fileRecord.current;
    const upload = session.current;
    if (record === null || upload === null) return;
    working("committing");
    try {
      await api.commitVersion(record.id, upload.upload_id, expectedHead);
    } catch (error) {
      const current = isApiError(error) ? error.details?.["current_version_id"] : undefined;
      if (isApiError(error) && error.status === 412 && typeof current === "string") {
        update({ name: "conflict", currentVersionId: current });
        announce(t("Somebody else saved a new version meanwhile. Choose how to continue."));
        return;
      }
      throw error;
    }
    committed.current = true;
    changed.current = true;
    update({ name: "done", fileName: record.name, copy: false });
    announce(t("{name} was uploaded.", { name: record.name }));
  }

  async function run(file: File) {
    const abort = new AbortController();
    controller.current = abort;
    try {
      // 1. File record (new files only). It stays when a later step fails, so a retry reuses it.
      if (fileRecord.current === null && target.kind === "new-file") {
        working("creating");
        fileRecord.current = await api.createFile(target.project.id, {
          name: name.trim(),
          app,
          format: formatOf(name.trim()),
        });
        changed.current = true;
      }
      const record = fileRecord.current;
      if (record === null) return;

      // 2. SHA-256 of the content (the API wants it before the first byte).
      if (!stored.current) {
        const fingerprint = `${file.name}:${file.size}:${file.lastModified}`;
        if (digest.current?.key !== fingerprint) {
          working("hashing", 0, file.size);
          const hex = await sha256OfBlob(file, {
            signal: abort.signal,
            onProgress: (done, total) => update({ name: "working", step: "hashing", done, total }),
          });
          digest.current = { key: fingerprint, hex };
        }

        // 3. Reserve quota. The Idempotency-Key makes a repeated request return the same session.
        if (session.current === null) {
          working("reserving");
          sessionKey.current ??= newKey();
          const reserved = await api.createUpload(
            record.id,
            { size_bytes: file.size, sha256: digest.current.hex },
            sessionKey.current,
          );
          if (abort.signal.aborted || !alive.current) {
            // Cancelled while the reservation was in flight: release it right away.
            void api.abortUpload(reserved.upload_id).catch(() => undefined);
            return;
          }
          session.current = reserved;
        }

        // 4. Stream the content. A session that failed here cannot be resumed.
        working("uploading", 0, file.size);
        const upload = session.current;
        try {
          await api.uploadContent(upload.upload_id, file, {
            signal: abort.signal,
            onProgress: ({ sent, total }) =>
              update({ name: "working", step: "uploading", done: sent, total }),
          });
        } catch (error) {
          if (session.current === upload) {
            // The body failed on the client side: give the reservation back before dropping it.
            // A 409 "still receiving" answer is ignored; the server fails the session on its own.
            void api.abortUpload(upload.upload_id).catch(() => undefined);
          }
          session.current = null;
          sessionKey.current = null;
          if (isApiError(error) && RESTART_CODES.has(error.code)) {
            digest.current = null; // the file may have changed on disk: hash it again
          }
          throw error;
        }
        stored.current = true;
      }

      // 5. Make it a version.
      await commit(baseHead.current);
    } catch (error) {
      if (isApiError(error) && error.code === ABORTED_ERROR_CODE) {
        return;
      }
      if (error instanceof DOMException && error.name === "AbortError") {
        return;
      }
      if (
        fileRecord.current === null &&
        isApiError(error) &&
        (error.code === "FILE_NAME_TAKEN" || error.fields.some((f) => f === "name" || f === "app" || f === "format"))
      ) {
        // The name or app has to change: back to the form with the problem marked.
        setErrors(
          error.code === "FILE_NAME_TAKEN" || error.fields.includes("name")
            ? { name: error.code === "FILE_NAME_TAKEN" ? t("A file with this name already exists in this project.") : t("This value is not accepted.") }
            : { app: t("This value is not accepted.") },
        );
        update({ name: "choose" });
        window.setTimeout(() => focusFirstInvalid(form.current), 0);
        return;
      }
      retryRef.current = () => void run(file);
      update({ name: "failed", error });
    } finally {
      if (controller.current === abort) controller.current = null;
    }
  }

  async function overwrite(currentVersionId: string) {
    baseHead.current = currentVersionId;
    try {
      await commit(currentVersionId);
    } catch (error) {
      retryRef.current = () => void overwrite(currentVersionId);
      update({ name: "failed", error });
    }
  }

  async function keepBoth() {
    const original = fileRecord.current;
    const upload = session.current;
    if (original === null || upload === null) return;
    working("creating");
    try {
      let copy: FileRecord | null = copyRecord.current;
      for (let attempt = 1; attempt <= COPY_NAME_ATTEMPTS && copy === null; attempt += 1) {
        try {
          copy = await api.createFile(original.project_id, {
            name: conflictCopyName(original.name, attempt),
            app: original.app,
            format: original.format,
          });
        } catch (error) {
          if (!(isApiError(error) && error.code === "FILE_NAME_TAKEN") || attempt === COPY_NAME_ATTEMPTS) {
            throw error;
          }
        }
      }
      if (copy === null) return;
      copyRecord.current = copy; // a retry commits to this copy instead of creating another
      changed.current = true;
      working("committing");
      // The upload stays stored; commit it as the first version of the copy.
      await api.commitVersion(copy.id, upload.upload_id, null);
      committed.current = true;
      update({ name: "done", fileName: copy.name, copy: true });
      announce(t("Saved as a separate file: {name}.", { name: copy.name }));
    } catch (error) {
      retryRef.current = () => void keepBoth();
      update({ name: "failed", error });
    }
  }

  function finish() {
    controller.current?.abort();
    // Release the reserved space; the server also expires sessions on its own.
    releaseSession();
    if (changed.current) onChanged();
    onClose();
  }

  function submit(event: FormEvent) {
    event.preventDefault();
    const next: FormErrors = {};
    if (chosen === null) next.file = t("Choose a file.");
    if (target.kind === "new-file") {
      const bytes = nameBytes(name.trim());
      if (bytes < 1 || bytes > MAX_NAME_BYTES) {
        next.name = t("Enter a file name of 1 to {max} bytes.", { max: MAX_NAME_BYTES });
      }
      if (!APP_PATTERN.test(app)) {
        next.app = t("Use lowercase letters, digits and hyphens (1 to 32 characters).");
      }
    }
    setErrors(next);
    if (next.file !== undefined || next.name !== undefined || next.app !== undefined) {
      queueMicrotask(() => focusFirstInvalid(form.current));
      return;
    }
    if (chosen !== null) void run(chosen);
  }

  const title =
    target.kind === "new-file"
      ? t("Upload a file")
      : t("Upload a new version of {name}", { name: target.file.name });
  const isConflict = phase.name === "conflict";

  return (
    <AppModal
      title={isConflict ? t("The file was changed meanwhile") : title}
      onClose={finish}
      busy={phase.name === "working"}
    >
      {/* Stays mounted (hidden) so the chosen file survives a return to the form. */}
      <div hidden={phase.name !== "choose"}>
        <Form className="form" onSubmit={submit} ref={form} validationBehavior="aria">
          <ChooseFile
            onChoose={(file) => {
              setChosen(file);
              setErrors((current) => ({ ...current, file: undefined }));
              if (target.kind === "new-file" && file !== null && name === "") {
                setName(file.name);
              }
            }}
            error={errors.file}
          />
          {target.kind === "new-file" && (
            <>
              <Field label={t("File name")} name="file-name" value={name} onChange={setName} error={errors.name} />
              <Field
                label={t("App")}
                name="file-app"
                value={app}
                onChange={setApp}
                error={errors.app}
                description={t("Which Craft app the file belongs to, for example photocraft.")}
              />
            </>
          )}
          {chosen !== null && (
            <p className="muted">{t("Size: {size}", { size: formatBytes(language, chosen.size) })}</p>
          )}
          <div className="form__actions">
            <Button type="submit" className="button button--primary">
              {t("Upload")}
            </Button>
            <Button className="button" onPress={finish}>
              {t("Cancel")}
            </Button>
          </div>
        </Form>
      </div>

      {phase.name === "working" && (
        <div className="stack">
          <p>{stepLabel[phase.step]}</p>
          {phase.total > 0 && (phase.step === "hashing" || phase.step === "uploading") && (
            <>
              <progress
                className="progress"
                max={phase.total}
                value={phase.done}
                aria-label={stepLabel[phase.step]}
              />
              <p className="muted">
                {t("{done} of {total}", {
                  done: formatBytes(language, phase.done),
                  total: formatBytes(language, phase.total),
                })}
              </p>
            </>
          )}
          <div className="form__actions">
            {/* While the version is being recorded the outcome is decided on the server; cancelling would only guess. */}
            <Button className="button" onPress={finish} isDisabled={phase.step === "committing"}>
              {t("Cancel upload")}
            </Button>
          </div>
        </div>
      )}

      {phase.name === "conflict" && fileRecord.current !== null && (
        <div className="stack">
          <p>
            {t(
              "Somebody else saved a new version of {name} while you were uploading. Your upload is stored but not saved as a version yet.",
              { name: fileRecord.current.name },
            )}
          </p>
          <ul className="bullets">
            <li>{t("Keep both: save your upload as a separate file next to the original.")}</li>
            <li>
              {t("Overwrite: save your upload as the newest version. The other version stays in the history.")}
            </li>
            <li>{t("Cancel: discard your upload.")}</li>
          </ul>
          <div className="form__actions">
            <Button className="button button--primary" autoFocus onPress={() => void keepBoth()}>
              {t("Keep both")}
            </Button>
            <Button className="button" onPress={() => void overwrite(phase.currentVersionId)}>
              {t("Overwrite")}
            </Button>
            <Button className="button" onPress={finish}>
              {t("Cancel")}
            </Button>
          </div>
        </div>
      )}

      {phase.name === "done" && (
        <div className="stack">
          <Alert tone="success">
            {phase.copy
              ? t("Saved as a separate file: {name}.", { name: phase.fileName })
              : t("{name} was uploaded.", { name: phase.fileName })}
          </Alert>
          <div className="form__actions">
            <Button className="button button--primary" autoFocus onPress={finish}>
              {t("Close")}
            </Button>
          </div>
        </div>
      )}

      {phase.name === "failed" && (
        <div className="stack">
          <ErrorNotice
            error={phase.error}
            onRetry={
              retryRef.current ?? (chosen !== null ? () => void run(chosen) : undefined)
            }
            onReload={
              classifyError(phase.error) === "forbidden" || classifyError(phase.error) === "missing"
                ? () => {
                    if (changed.current) onChanged();
                  }
                : undefined
            }
          />
          <div className="form__actions">
            {/* 400/422 body failures carry no retry in ErrorNotice, but sending the file again is the fix. */}
            {isApiError(phase.error) && RESTART_CODES.has(phase.error.code) && chosen !== null && (
              <Button className="button button--primary" onPress={() => void run(chosen)}>
                {t("Try again")}
              </Button>
            )}
            <Button className="button" onPress={finish}>
              {t("Close")}
            </Button>
          </div>
        </div>
      )}
    </AppModal>
  );
}

function ChooseFile({
  onChoose,
  error,
}: {
  onChoose: (file: File | null) => void;
  error: string | undefined;
}) {
  const { t } = useI18n();
  const id = "upload-file-input";
  return (
    <div className="field">
      <label className="field__label" htmlFor={id}>
        {t("File")}
      </label>
      <input
        id={id}
        type="file"
        className="field__input"
        aria-invalid={error !== undefined ? true : undefined}
        aria-describedby={error !== undefined ? `${id}-error` : undefined}
        onChange={(event) => onChoose(event.currentTarget.files?.[0] ?? null)}
      />
      {error !== undefined && (
        <span id={`${id}-error`} className="field__error">
          {error}
        </span>
      )}
    </div>
  );
}
