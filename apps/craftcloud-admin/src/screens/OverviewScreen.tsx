import { useCallback, useEffect, useId, useMemo, useState } from "react";
import type { AuditEvent, Organization, OrganizationOverview, Workspace, WorkspaceUsage } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { ListStatus, LoadMoreButton } from "../components/ListStatus.tsx";
import { formatBytes, formatDateTime } from "../format.ts";
import { useI18n } from "../i18n/index.tsx";
import { useRoleNames } from "../roles.ts";
import { usePagedList } from "../usePagedList.ts";

const CHART_WIDTH = 600;
const CHART_HEIGHT = 140;

function dayLabel(language: string, isoDate: string, long = false): string {
  const date = new Date(`${isoDate}T00:00:00Z`);
  if (Number.isNaN(date.getTime())) return isoDate;
  try {
    return new Intl.DateTimeFormat(language, {
      month: "short",
      day: "numeric",
      ...(long ? { year: "numeric" } : {}),
      timeZone: "UTC",
    }).format(date);
  } catch {
    return isoDate;
  }
}

/**
 * Bar chart of the audit events per UTC day. Plain SVG, no library: colors come
 * from CSS classes (design tokens), the svg has a text alternative and the same
 * numbers are available as a table.
 */
export function ActivityChart({ days }: { days: { date: string; count: number }[] }) {
  const { t, language } = useI18n();
  const id = useId();
  const total = days.reduce((sum, day) => sum + day.count, 0);
  const peak = days.reduce((best, day) => (day.count > best.count ? day : best), days[0] ?? { date: "", count: 0 });
  const max = Math.max(1, peak.count);
  const slot = days.length === 0 ? CHART_WIDTH : CHART_WIDTH / days.length;
  const first = days[0];
  const last = days[days.length - 1];
  const description =
    total === 0
      ? t("No activity in the last 30 days.")
      : t("{total} events in the last 30 days. Busiest day: {date} with {count}.", {
          total,
          date: dayLabel(language, peak.date, true),
          count: peak.count,
        });
  return (
    <figure className="chart">
      <figcaption id={`${id}-title`} className="chart__title">{t("Activity in the last 30 days")}</figcaption>
      <svg
        className="chart__svg"
        viewBox={`0 0 ${CHART_WIDTH} ${CHART_HEIGHT}`}
        preserveAspectRatio="none"
        role="img"
        aria-labelledby={`${id}-title ${id}-desc`}
        focusable="false"
      >
        <desc id={`${id}-desc`}>{description}</desc>
        <line className="chart__baseline" x1="0" y1={CHART_HEIGHT} x2={CHART_WIDTH} y2={CHART_HEIGHT} />
        {days.map((day, index) => {
          if (day.count === 0) return null;
          const height = (day.count / max) * (CHART_HEIGHT - 4);
          return (
            <rect
              key={day.date}
              className="chart__bar"
              x={index * slot + slot * 0.12}
              y={CHART_HEIGHT - height}
              width={slot * 0.76}
              height={height}
            />
          );
        })}
      </svg>
      <div className="chart__axis" aria-hidden="true">
        <span>{first === undefined ? "" : dayLabel(language, first.date)}</span>
        <span>{t("Peak: {count} per day", { count: peak.count })}</span>
        <span>{last === undefined ? "" : dayLabel(language, last.date)}</span>
      </div>
      <details className="chart__data">
        <summary>{t("Show data as table")}</summary>
        <div className="table-wrap">
          <table className="table">
            <caption className="visually-hidden">{t("Activity in the last 30 days")}</caption>
            <thead>
              <tr>
                <th scope="col">{t("Date")}</th>
                <th scope="col" className="numeric">{t("Events")}</th>
              </tr>
            </thead>
            <tbody>
              {[...days].reverse().map((day) => (
                <tr key={day.date}>
                  <th scope="row">{dayLabel(language, day.date, true)}</th>
                  <td className="numeric" data-label={t("Events")}>{day.count}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </details>
    </figure>
  );
}

export function AdminOverview({ organization, workspaces }: { organization: Organization | null; workspaces: Workspace[] }) {
  const { t, language } = useI18n();
  const { api } = useAuth();
  const names = useRoleNames();
  const organizationId = organization?.id ?? null;
  const [overview, setOverview] = useState<OrganizationOverview | null>(null);
  const [overviewError, setOverviewError] = useState<unknown>(null);
  const [attempt, setAttempt] = useState(0);
  const [twoFactorFeature, setTwoFactorFeature] = useState(false);
  const [events, setEvents] = useState<AuditEvent[]>([]);
  const [eventError, setEventError] = useState<unknown>(null);

  useEffect(() => {
    if (organizationId === null) return;
    let live = true;
    setOverview(null);
    setOverviewError(null);
    api.getOrganizationOverview(organizationId).then(
      (result) => { if (live) setOverview(result); },
      (error: unknown) => { if (live) setOverviewError(error); },
    );
    return () => { live = false; };
  }, [api, organizationId, attempt]);

  useEffect(() => {
    if (organizationId === null) return;
    let live = true;
    setEvents([]);
    setEventError(null);
    api.listAudit({ kind: "organization", orgId: organizationId }, {}, undefined).then(
      (result) => { if (live) setEvents(result.items.slice(0, 10)); },
      (error: unknown) => { if (live) setEventError(error); },
    );
    api.serverInfo().then(
      (info) => { if (live) setTwoFactorFeature(info.feature_flags?.two_factor === true); },
      () => undefined,
    );
    return () => { live = false; };
  }, [api, organizationId]);

  const loadUsage = useMemo(
    () =>
      organizationId === null
        ? null
        : (cursor?: string) =>
            api.getOrganizationUsage(organizationId, cursor).then((page) => ({ items: page.workspaces, next_cursor: page.next_cursor })),
    [api, organizationId],
  );
  const usage = usePagedList<WorkspaceUsage>(loadUsage, (item) => item.workspace_id);
  const workspaceNames = useMemo(() => new Map(workspaces.map((item) => [item.id, item.name])), [workspaces]);
  const reload = useCallback(() => setAttempt((value) => value + 1), []);

  if (organization === null) return <p className="muted">{t("Select an organization first.")}</p>;

  const number = (value: number) => value.toLocaleString(language);
  const coverage =
    overview !== null && overview.two_factor.active_members > 0
      ? `${Math.round((overview.two_factor.enrolled / overview.two_factor.active_members) * 100)}%`
      : "—";
  const tiles: [string, string][] =
    overview === null
      ? []
      : [
          [t("Active people"), number(overview.members.by_status.active)],
          ...(overview.seats === undefined ? [] : [[t("Seats used"), `${number(overview.seats.used)} / ${overview.seats.limit === null ? t("Unlimited") : number(overview.seats.limit)}`] as [string, string]]),
          [t("Open invitations"), number(overview.counts.pending_invitations)],
          [
            t("Storage used"),
            `${formatBytes(language, overview.storage.used_bytes)} / ${overview.storage.quota_bytes === null ? t("Unlimited") : formatBytes(language, overview.storage.quota_bytes)}`,
          ],
          [t("Active locks"), number(overview.counts.active_locks)],
          [t("Active grants"), number(overview.counts.active_grants)],
          [t("Workspaces"), number(overview.counts.workspaces)],
          [t("Projects"), number(overview.counts.projects)],
          [t("Files"), number(overview.counts.files)],
          ...(twoFactorFeature ? [[t("Two-factor coverage"), coverage] as [string, string]] : []),
        ];

  return (
    <div className="stack">
      {overview === null && overviewError === null && <p role="status" className="muted">{t("Loading overview…")}</p>}
      {overviewError !== null && <ErrorNotice error={overviewError} onRetry={reload} onReload={reload} />}
      {overview !== null && (
        <>
          <dl className="admin-metrics">
            {tiles.map(([label, value]) => (
              <div key={label}><dt>{label}</dt><dd>{value}</dd></div>
            ))}
          </dl>
          <section className="section" aria-labelledby="overview-activity-title">
            <h2 id="overview-activity-title" className="section__title">{t("Activity trend")}</h2>
            <ActivityChart days={overview.activity_daily} />
          </section>
          <section className="section" aria-labelledby="overview-people-title">
            <h2 id="overview-people-title" className="section__title">{t("People")}</h2>
            <div className="overview-people">
              <dl className="facts" aria-label={t("Members by role")}>
                {(["owner", "admin", "member", "guest"] as const).map((role) => (
                  <div className="facts__row" key={role}><dt>{names.orgRole(role)}</dt><dd>{number(overview.members.by_role[role])}</dd></div>
                ))}
              </dl>
              <dl className="facts" aria-label={t("Members by status")}>
                <div className="facts__row"><dt>{t("Active")}</dt><dd>{number(overview.members.by_status.active)}</dd></div>
                <div className="facts__row"><dt>{t("Invited")}</dt><dd>{number(overview.members.by_status.invited)}</dd></div>
                <div className="facts__row"><dt>{t("Disabled")}</dt><dd>{number(overview.members.by_status.disabled)}</dd></div>
              </dl>
            </div>
          </section>
        </>
      )}
      <section className="section" aria-labelledby="overview-storage-title">
        <h2 id="overview-storage-title" className="section__title">{t("Workspace storage")}</h2>
        <ListStatus list={usage} empty={t("No workspaces yet.")} loadingLabel={t("Loading overview…")} />
        {usage.items.length > 0 && (
          <div className="table-wrap">
            <table className="table">
              <thead><tr><th>{t("Workspace")}</th><th>{t("Used")}</th><th>{t("Physical storage")}</th><th>{t("Quota")}</th></tr></thead>
              <tbody>
                {usage.items.map((item) => (
                  <tr key={item.workspace_id}>
                    <th>{workspaceNames.get(item.workspace_id) ?? item.workspace_id.slice(0, 8)}</th>
                    <td data-label={t("Used")}>{formatBytes(language, item.committed_bytes + item.reserved_bytes)}</td>
                    <td data-label={t("Physical storage")}>{item.physical_bytes === undefined ? t("Unavailable") : formatBytes(language, item.physical_bytes)}</td>
                    <td data-label={t("Quota")}>{item.effective_limit_bytes === null ? t("Unlimited") : formatBytes(language, item.effective_limit_bytes)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        <LoadMoreButton list={usage} />
      </section>
      <section className="section" aria-labelledby="overview-recent-title">
        <h2 id="overview-recent-title" className="section__title">{t("Recent activity")}</h2>
        {eventError !== null && <ErrorNotice error={eventError} />}
        {events.length === 0 && eventError === null && <p className="muted">{t("No recent activity.")}</p>}
        {events.length > 0 && (
          <div className="table-wrap">
            <table className="table">
              <thead><tr><th>{t("Action")}</th><th>{t("Result")}</th><th>{t("Time")}</th></tr></thead>
              <tbody>
                {events.map((event) => (
                  <tr key={event.id}>
                    <th>{event.action}</th>
                    <td data-label={t("Result")}>{event.result}</td>
                    <td data-label={t("Time")}>{formatDateTime(language, event.created_at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>
    </div>
  );
}
