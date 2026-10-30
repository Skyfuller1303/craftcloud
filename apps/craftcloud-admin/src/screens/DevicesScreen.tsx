import { useEffect, useState } from "react";
import { Button } from "react-aria-components";
import type { DeviceSession } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { EmptyState } from "../components/EmptyState.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { formatDateTime } from "../format.ts";
import { useI18n } from "../i18n/index.tsx";

export function DevicesScreen() {
  const { t, language } = useI18n();
  const { api } = useAuth();
  const [devices, setDevices] = useState<DeviceSession[] | null>(null);
  const [loadError, setLoadError] = useState<unknown>(null);
  const [attempt, setAttempt] = useState(0);
  const [revoking, setRevoking] = useState<string | null>(null);
  const [actionError, setActionError] = useState<unknown>(null);
  const clientKind: Record<DeviceSession["client_kind"], string> = {
    app: t("Application"),
    launcher: t("Launcher"),
    cli: t("Command line"),
  };

  useEffect(() => {
    let current = true;
    setDevices(null);
    setLoadError(null);
    api.listDevices().then(
      (result) => { if (current) setDevices(result); },
      (error: unknown) => { if (current) setLoadError(error); },
    );
    return () => { current = false; };
  }, [api, attempt]);

  async function revoke(device: DeviceSession) {
    setRevoking(device.id);
    setActionError(null);
    try {
      await api.revokeDevice(device.id);
      setDevices((current) => current?.filter((item) => item.id !== device.id) ?? null);
    } catch (error) {
      setActionError(error);
    } finally {
      setRevoking(null);
    }
  }

  if (loadError !== null) {
    return (
      <section className="section" aria-labelledby="devices-title">
        <h2 id="devices-title" className="section__title">{t("Devices")}</h2>
        <ErrorNotice error={loadError} onRetry={() => setAttempt((value) => value + 1)} />
      </section>
    );
  }
  if (devices === null) {
    return (
      <section className="section" aria-labelledby="devices-title">
        <h2 id="devices-title" className="section__title">{t("Devices")}</h2>
        <p role="status" className="muted">{t("Loading devices…")}</p>
      </section>
    );
  }
  if (devices.length === 0) {
    return (
      <section className="section" aria-labelledby="devices-title">
        <h2 id="devices-title" className="section__title">{t("Devices")}</h2>
        <EmptyState>{t("There are no registered devices.")}</EmptyState>
      </section>
    );
  }

  return (
    <section className="section" aria-labelledby="devices-title">
      <h2 id="devices-title" className="section__title">{t("Devices")}</h2>
      {actionError !== null && <ErrorNotice error={actionError} onRetry={() => {
        setActionError(null);
        setAttempt((value) => value + 1);
      }} onReload={() => {
        setActionError(null);
        setAttempt((value) => value + 1);
      }} />}
      <div className="table-wrap">
        <table className="table">
          <caption className="visually-hidden">{t("Devices")}</caption>
          <thead><tr>
            <th scope="col">{t("Name")}</th>
            <th scope="col">{t("Kind")}</th>
            <th scope="col">{t("App")}</th>
            <th scope="col">{t("Last used")}</th>
            <th scope="col">{t("Created")}</th>
            <th scope="col">{t("Actions")}</th>
          </tr></thead>
          <tbody>{devices.map((device) => (
            <tr key={device.id}>
              <th scope="row" data-label={t("Name")}>{device.name}</th>
              <td data-label={t("Kind")}>{clientKind[device.client_kind]}</td>
              <td data-label={t("App")}>{device.client_id}</td>
              <td className="numeric" data-label={t("Last used")}>{device.last_used_at === null ? "—" : formatDateTime(language, device.last_used_at)}</td>
              <td className="numeric" data-label={t("Created")}>{formatDateTime(language, device.created_at)}</td>
              <td data-label={t("Actions")}>
                <Button
                  className="button button--danger-quiet button--sm"
                  onPress={() => void revoke(device)}
                  isDisabled={revoking !== null}
                >
                  {revoking === device.id ? t("Signing out…") : t("Sign out")}
                </Button>
              </td>
            </tr>
          ))}</tbody>
        </table>
      </div>
    </section>
  );
}
