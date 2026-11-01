import { useCallback, useEffect, useState } from "react";
import { Button } from "react-aria-components";
import type { AdminHealth, HealthLevel, StorageBinding } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { Badge } from "../components/Badge.tsx";
import { Banner } from "../components/Banner.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { Alert } from "../components/forms.tsx";
import { formatBytes, formatDateTime } from "../format.ts";
import { useI18n } from "../i18n/index.tsx";
import { EmailSystem } from "./EmailSystem.tsx";

type State =
  | { status: "loading" }
  | { status: "ready"; health: AdminHealth }
  | { status: "error"; error: unknown };

/**
 * The system view for the instance operator (CC-F15): what `GET /admin/health`
 * reports. The server decides who may read it (403 is shown as an error);
 * this screen only does not offer it to everybody else. Numbers, flags and
 * codes only: the view never shows tenant data.
 */
export function SystemScreen({ isOperator }: { isOperator: boolean }) {
  const { t } = useI18n();
  const { api } = useAuth();
  const [state, setState] = useState<State>({ status: "loading" });
  const [round, setRound] = useState(0);
  const [ssoEnabled, setSsoEnabled] = useState<boolean | null>(null);
  const reload = useCallback(() => setRound((current) => current + 1), []);

  useEffect(() => {
    if (!isOperator) {
      return;
    }
    let current = true;
    setState({ status: "loading" });
    api
      .getAdminHealth()
      .then((health) => {
        if (current) setState({ status: "ready", health });
      })
      .catch((error: unknown) => {
        if (current) setState({ status: "error", error });
      });
    return () => {
      current = false;
    };
  }, [api, isOperator, round]);
  useEffect(() => {
    if (!isOperator) return;
    let live = true;
    void api.getAdminSso().then((value) => { if (live) setSsoEnabled(value.enabled); }, () => { if (live) setSsoEnabled(null); });
    return () => { live = false; };
  }, [api, isOperator, round]);

  if (!isOperator) {
    return <p className="muted">{t("Only the instance operator can see the system status.")}</p>;
  }

  return (
    <div className="stack">
      <section className="section">
        <h2 className="section__title">{t("Single sign-on")}</h2>
        <label className="checkbox">
          <input type="checkbox" checked={ssoEnabled === true} disabled={ssoEnabled === null} onChange={(event) => {
            const enabled = event.currentTarget.checked;
            void api.updateAdminSso(enabled).then((value) => setSsoEnabled(value.enabled), () => setSsoEnabled(!enabled));
          }} />
          <span>{t("Allow organization single sign-on")}</span>
        </label>
      </section>
      <EmailSystem revision={round} />
      <p>
        <Button className="button" onPress={reload} isDisabled={state.status === "loading"}>
          {t("Refresh")}
        </Button>
      </p>
      {state.status === "loading" && (
        <>
          <div className="progress progress--indeterminate" aria-hidden="true"><progress /></div>
          <p role="status" className="muted">{t("Loading system status…")}</p>
        </>
      )}
      {state.status === "error" && (
        <ErrorNotice error={state.error} onRetry={reload} onReload={reload} />
      )}
      {state.status === "ready" && (
        <HealthView health={state.health} />
      )}
    </div>
  );
}

function HealthView({ health }: { health: AdminHealth }) {
  const { t, language } = useI18n();
  const level: Record<HealthLevel, string> = {
    ok: t("OK"),
    degraded: t("Degraded"),
    down: t("Down"),
    not_configured: t("Not configured"),
  };
  const tone = (value: HealthLevel) => {
    if (value === "ok") return "good" as const;
    if (value === "degraded") return "warn" as const;
    if (value === "down") return "bad" as const;
    return "neutral" as const;
  };
  const binding: Record<StorageBinding, string> = {
    matches: t("Paired with this database"),
    unbound: t("Not paired yet (pairs on first use)"),
    epoch_conflict: t("Epoch conflict: a restore is suspected"),
    mismatch: t("Belongs to another database"),
    error: t("Could not be checked"),
    unknown: t("Not checked (database unreachable)"),
    not_configured: t("Not configured"),
  };

  function uptime(seconds: number): string {
    const minutes = Math.floor(seconds / 60);
    const hours = Math.floor(minutes / 60);
    const days = Math.floor(hours / 24);
    if (days > 0) {
      return t("{days} d {hours} h", { days: String(days), hours: String(hours % 24) });
    }
    if (hours > 0) {
      return t("{hours} h {minutes} min", { hours: String(hours), minutes: String(minutes % 60) });
    }
    return t("{minutes} min", { minutes: String(minutes) });
  }

  const count = (value: number | null): string => (value === null ? t("Not available") : String(value));

  const { components, uploads, maintenance, audit, server } = health;
  const pool = components.database.pool;
  const run = maintenance.last_run;
  return (
    <>
      {health.status === "down" && (
        <Alert>{t("At least one component is down. The server does not take traffic.")}</Alert>
      )}
      {maintenance.epoch_conflict && (
        <Banner tone="warn" role="alert">
          {t(
            "The database and the object store disagree about their epoch, so a restore is suspected. Destructive maintenance is blocked. Verify the pairing, then run the storage-bind command described in the operations guide.",
          )}
        </Banner>
      )}
      <Banner tone={tone(health.status)}>
        <Badge tone={tone(health.status)}>{level[health.status]}</Badge>
      </Banner>
      <div className="system-grid">
      <section className="section" aria-labelledby="system-overview-title">
        <h2 id="system-overview-title" className="section__title">
          {t("Overall status")}
        </h2>
        <dl className="facts">
          <dt>{t("Checked")}</dt>
          <dd>{formatDateTime(language, health.checked_at)}</dd>
          <dt>{t("Version")}</dt>
          <dd>{server.version}</dd>
          <dt>{t("Uptime")}</dt>
          <dd>{uptime(server.uptime_seconds)}</dd>
        </dl>
      </section>

      <section className="section" aria-labelledby="system-components-title">
        <h2 id="system-components-title" className="section__title">
          {t("Components")}
        </h2>
        <dl className="facts">
          <dt>{t("Database")}</dt>
          <dd>
            <Badge tone={tone(components.database.status)}>
              {level[components.database.status]}
            </Badge>
          </dd>
          <dt>{t("Database connections")}</dt>
          <dd>
            {t("{used} of {max} in use, {idle} idle", {
              used: String(pool.in_use),
              max: String(pool.max),
              idle: String(pool.idle),
            })}
          </dd>
          <dt>{t("Database schema")}</dt>
          <dd>
            <Badge tone={tone(components.migrations.status)}>
              {level[components.migrations.status]}
            </Badge>
          </dd>
          <dt>{t("Object storage")}</dt>
          <dd>
            <Badge tone={tone(components.storage.status)}>
              {level[components.storage.status]}
            </Badge>
          </dd>
          <dt>{t("Storage pairing")}</dt>
          <dd>{binding[components.storage.binding]}</dd>
          <dt>{t("Free disk space")}</dt>
          <dd>
            {components.storage.free_bytes === null
              ? t("Not available")
              : formatBytes(language, components.storage.free_bytes)}
          </dd>
        </dl>
      </section>

      <section className="section" aria-labelledby="system-uploads-title">
        <h2 id="system-uploads-title" className="section__title">
          {t("Uploads")}
        </h2>
        <dl className="facts">
          <dt>{t("Open upload sessions")}</dt>
          <dd>{count(uploads.open_sessions)}</dd>
          <dt>{t("Overdue upload sessions")}</dt>
          <dd>{count(uploads.overdue_sessions)}</dd>
        </dl>
      </section>

      <section className="section" aria-labelledby="system-maintenance-title">
        <h2 id="system-maintenance-title" className="section__title">
          {t("Maintenance")}
        </h2>
        <dl className="facts">
          <dt>{t("Last run")}</dt>
          <dd>{run === null ? t("No run recorded yet.") : formatDateTime(language, run.finished_at)}</dd>
          {run !== null && (
            <>
              <dt>{t("Result")}</dt>
              <dd>
                {maintenance.last_run_ok === true
                  ? t("Succeeded")
                  : t("Failed: {codes}", { codes: run.error_codes.join(", ") })}
              </dd>
            </>
          )}
        </dl>
      </section>

      <section className="section" aria-labelledby="system-audit-title">
        <h2 id="system-audit-title" className="section__title">
          {t("Audit log")}
        </h2>
        <dl className="facts">
          <dt>{t("Dropped events since start")}</dt>
          <dd>
            {t("{total} in total ({lossy} denials and failed sign-ins, {important} downloads and exports)", {
              total: String(audit.dropped_events_total),
              lossy: String(audit.dropped_events_lossy),
              important: String(audit.dropped_events_important),
            })}
          </dd>
        </dl>
      </section>

      <section className="section" aria-labelledby="system-backup-title">
        <h2 id="system-backup-title" className="section__title">
          {t("Backups")}
        </h2>
        <dl className="facts">
          <dt>{t("Last backup")}</dt>
          <dd>
            {health.backup.last_success_age_seconds === null
              ? t("No successful backup recorded yet.")
              : uptime(health.backup.last_success_age_seconds)}
          </dd>
        </dl>
      </section>
      </div>
    </>
  );
}
