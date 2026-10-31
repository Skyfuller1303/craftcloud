import { useEffect, useState } from "react";
import { Button } from "react-aria-components";
import type { Organization, OrganizationApps } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { formatBytes } from "../format.ts";
import { useI18n } from "../i18n/index.tsx";

export function AppsScreen({ organization }: { organization: Organization | null }) {
  const { t, language } = useI18n();
  const { api } = useAuth();
  const [data, setData] = useState<OrganizationApps | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [saving, setSaving] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const orgId = organization?.id ?? null;
  useEffect(() => {
    if (orgId === null) return;
    let live = true;
    setData(null); setError(null);
    void api.getOrganizationApps(orgId).then((value) => { if (live) setData(value); }, (failure: unknown) => { if (live) setError(failure); });
    return () => { live = false; };
  }, [api, orgId, attempt]);
  async function setMode(mode: "all" | "assigned") {
    if (orgId === null || saving) return;
    setSaving(true);
    try { await api.updateOrganizationAppMode(orgId, mode); setData((current) => current === null ? current : { ...current, mode }); setError(null); }
    catch (failure) { setError(failure); }
    finally { setSaving(false); }
  }
  if (organization === null) return <p className="muted">{t("Select an organization first.")}</p>;
  if (error !== null) return <ErrorNotice error={error} onRetry={() => setAttempt((value) => value + 1)} />;
  if (data === null) return <p className="muted" role="status">{t("Loading apps…")}</p>;
  const number = (value: number) => value.toLocaleString(language);
  return <div className="stack">
    <section className="section" aria-labelledby="apps-access-title">
      <h2 id="apps-access-title" className="section__title">{t("App access")}</h2>
      <p>{data.mode === "all" ? t("All apps are available to organization members by default.") : t("Members can use only apps assigned to them.")}</p>
      <div className="row-actions">
        <Button className={`button ${data.mode === "all" ? "button--primary" : ""}`} isDisabled={saving || data.mode === "all"} onPress={() => void setMode("all")}>{t("All apps for all members")}</Button>
        <Button className={`button ${data.mode === "assigned" ? "button--primary" : ""}`} isDisabled={saving || data.mode === "assigned"} onPress={() => void setMode("assigned")}>{t("Only assigned apps")}</Button>
      </div>
    </section>
    <section className="section" aria-labelledby="apps-usage-title">
      <h2 id="apps-usage-title" className="section__title">{t("Apps usage")}</h2>
      <div className="table-wrap"><table className="table"><caption className="visually-hidden">{t("Apps usage")}</caption>
        <thead><tr><th scope="col">{t("App")}</th><th scope="col" className="numeric">{t("Assigned people")}</th><th scope="col" className="numeric">{t("Active devices")}</th><th scope="col" className="numeric">{t("Storage incl. all versions")}</th></tr></thead>
        <tbody>{data.apps.map((app) => <tr key={app.id}><th scope="row">{app.name}</th><td className="numeric" data-label={t("Assigned people")}>{number(app.assigned_members)}</td><td className="numeric" data-label={t("Active devices")}>{number(app.active_devices)}</td><td className="numeric" data-label={t("Storage incl. all versions")}>{formatBytes(language, app.storage_bytes)}</td></tr>)}</tbody>
      </table></div>
      <p className="muted">{t("Storage counts the logical size of all retained versions of the app's files, including files in the trash. It is the same basis as the storage quota.")}</p>
    </section>
  </div>;
}
