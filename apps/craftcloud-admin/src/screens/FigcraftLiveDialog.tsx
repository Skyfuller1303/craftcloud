import { useEffect, useRef, useState } from "react";
import { Button } from "react-aria-components";
import type { FigcraftLiveLink, FigcraftLiveStatus } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { AppModal, ConfirmDialog } from "../components/Dialog.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { useAnnounce } from "../components/Announcer.tsx";
import { useI18n } from "../i18n/index.tsx";
import { formatDateTime } from "../format.ts";

/** Discovery failure keeps the optional feature unavailable. No request publishes a file here. */
export function useFigcraftAvailable() {
  const { api } = useAuth();
  const [available, setAvailable] = useState(false);
  useEffect(() => {
    let current = true;
    setAvailable(false);
    api.serverInfo().then(
      (info) => { if (current) setAvailable(info.feature_flags?.figcraft_live === true); },
      () => { if (current) setAvailable(false); },
    );
    return () => { current = false; };
  }, [api]);
  return available;
}

/** A bearer link lives only in this mounted dialog. Never put it in browser storage or history. */
export function FigcraftLiveDialog({ fileId, name, onClose, onChanged }: {
  fileId: string;
  name: string;
  onClose: () => void;
  onChanged: () => void;
}) {
  const { api } = useAuth();
  const { t, language } = useI18n();
  const announce = useAnnounce();
  const [status, setStatus] = useState<FigcraftLiveStatus | null>(null);
  const [link, setLink] = useState<FigcraftLiveLink | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState(false);
  const [stopOpen, setStopOpen] = useState(false);
  const [copyFailed, setCopyFailed] = useState(false);
  const pending = useRef(false);
  const input = useRef<HTMLInputElement>(null);
  const generation = useRef(0);

  useEffect(() => {
    const current = ++generation.current;
    setLink(null);
    setStatus(null);
    setError(null);
    api.getFigcraftLiveStatus(fileId).then(
      (value) => { if (current === generation.current) setStatus(value); },
      (failure: unknown) => { if (current === generation.current) setError(failure); },
    );
    return () => { generation.current++; };
  }, [api, fileId]);

  async function run(action: () => Promise<void>) {
    if (pending.current) return;
    pending.current = true;
    setBusy(true);
    setError(null);
    try { await action(); }
    catch (failure) { setLink(null); setError(failure); }
    finally { pending.current = false; setBusy(false); }
  }

  async function showLink(renew: boolean) {
    const current = generation.current;
    // Hide the old token before rotation; even a lost response may have revoked it.
    setLink(null);
    setCopyFailed(false);
    const value = await api.getFigcraftLiveLink(fileId, renew);
    if (current !== generation.current) return;
    const parsed = new URL(value.join_url);
    if (parsed.protocol !== "figcraft:" || parsed.hostname !== "join") {
      throw new Error(t("The server returned an invalid FigCraft link."));
    }
    setLink(value);
    const refreshed = await api.getFigcraftLiveStatus(fileId);
    if (current !== generation.current) return;
    setStatus(refreshed);
    onChanged();
  }

  async function copy() {
    if (link === null) return;
    try {
      await navigator.clipboard.writeText(link.join_url);
      setCopyFailed(false);
      announce(t("Link copied."));
    } catch {
      setCopyFailed(true);
      input.current?.select();
    }
  }

  return <>
    <AppModal title={t("Live in FigCraft: {name}", { name })} onClose={onClose} busy={busy}>
      <p>{t("Your personal link follows your CraftCloud permissions. Anyone you give this link to can use your access. Renew it to invalidate the old link.")}</p>
      <p className="muted">{t("While this file is live, save changes in FigCraft. Regular cloud uploads are unavailable until live collaboration ends.")}</p>
      {error !== null && <ErrorNotice error={error} onRetry={() => void run(async () => setStatus(await api.getFigcraftLiveStatus(fileId)))} onReload={() => void run(async () => setStatus(await api.getFigcraftLiveStatus(fileId)))} />}
      {status === null && error === null && <p role="status">{t("Loading live status…")}</p>}
      {status !== null && <>
        <p>{status.state === "active" ? t("Live collaboration is active.") : status.state === "publishing" || status.state === "stopping" ? t("Live collaboration is changing. Refresh the status shortly.") : t("Creating a link starts live collaboration from the current version.")}</p>
        <p>{status.access === "edit" ? t("Your access: edit") : t("Your access: view")}</p>
        {status.last_snapshot_at !== null && <p>{t("Last cloud snapshot: {time}", { time: formatDateTime(language, status.last_snapshot_at) })}</p>}
        {status.participants !== null && <p>{t("Connected participants: {count}", { count: status.participants })}</p>}
      </>}
      {link !== null && <div className="link-box">
        <label className="field"><span className="field__label">{t("Personal FigCraft link")}</span>
          <input ref={input} className="field__input field__input--mono" readOnly value={link.join_url} autoComplete="off" spellCheck={false} onFocus={(event) => event.currentTarget.select()} />
        </label>
        {copyFailed && <p role="alert">{t("Copying failed. Select the link and copy it by hand.")}</p>}
        <div className="form__actions">
          <Button className="button" onPress={() => void copy()} isDisabled={busy}>{t("Copy link")}</Button>
          <a className="button button--primary" href={link.join_url} rel="noreferrer">{t("Open in FigCraft")}</a>
        </div>
      </div>}
      <div className="form__actions">
        {status !== null && (status.state === "publishing" || status.state === "stopping") && <Button className="button" isDisabled={busy} onPress={() => void run(async () => setStatus(await api.getFigcraftLiveStatus(fileId)))}>{t("Reload")}</Button>}
        <Button className="button button--primary" isDisabled={busy || status === null || status.state === "publishing" || status.state === "stopping"} onPress={() => void run(() => showLink(false))}>{t("Show personal link")}</Button>
        {link !== null && <Button className="button" isDisabled={busy} onPress={() => void run(() => showLink(true))}>{t("Renew link")}</Button>}
        {status?.can_snapshot === true && status.state === "active" && <Button className="button" isDisabled={busy} onPress={() => void run(async () => { await api.snapshotFigcraftLive(fileId); setStatus(await api.getFigcraftLiveStatus(fileId)); onChanged(); announce(t("Cloud snapshot saved.")); })}>{t("Save cloud snapshot")}</Button>}
        {status?.can_stop === true && status.state === "active" && <Button className="button button--danger-quiet" isDisabled={busy} onPress={() => setStopOpen(true)}>{t("End live collaboration")}</Button>}
        <Button className="button" isDisabled={busy} onPress={onClose}>{t("Close")}</Button>
      </div>
    </AppModal>
    {stopOpen && <ConfirmDialog title={t("End live collaboration")} message={t("Disconnect everyone and save the final live state as a cloud version?")} confirmLabel={t("End live collaboration")} busy={busy} error={error} danger onClose={() => setStopOpen(false)} onConfirm={() => void run(async () => { setLink(null); await api.stopFigcraftLive(fileId); setStopOpen(false); setStatus(await api.getFigcraftLiveStatus(fileId)); onChanged(); })} />}
  </>;
}
