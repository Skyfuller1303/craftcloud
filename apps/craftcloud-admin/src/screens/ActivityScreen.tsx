import { useMemo, useRef, useState } from "react";
import type { FormEvent } from "react";
import { Button, Form } from "react-aria-components";
import type { AuditEvent, AuditFilters, AuditResult, AuditSource, OrgMember, Workspace } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { Badge } from "../components/Badge.tsx";
import { Banner } from "../components/Banner.tsx";
import { NativeSelect } from "../components/ContextSelect.tsx";
import { focusFirstInvalid } from "../components/forms.tsx";
import { ListStatus, LoadMoreButton } from "../components/ListStatus.tsx";
import { formatDateTime } from "../format.ts";
import { useI18n } from "../i18n/index.tsx";
import { canReadAudit } from "../permissions.ts";
import type { Scope } from "../permissions.ts";
import { usePagedList } from "../usePagedList.ts";

const ALL = "";
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const DAY_MS = 24 * 60 * 60 * 1000;

/** First eight characters of an id: enough to recognise it, never a name. */
function shortId(id: string): string {
  return id.slice(0, 8);
}

/** `YYYY-MM-DD` to the start of that day in UTC (RFC 3339), or null if it is not a date. */
function dayStart(day: string): string | null {
  const time = Date.parse(`${day}T00:00:00Z`);
  return Number.isNaN(time) ? null : new Date(time).toISOString();
}

/** The start of the day after `day`, so that the whole last day is included. */
function dayAfter(day: string): string | null {
  const time = Date.parse(`${day}T00:00:00Z`);
  return Number.isNaN(time) ? null : new Date(time + DAY_MS).toISOString();
}

interface Draft {
  action: string;
  result: string;
  workspace: string;
  actor: string;
  from: string;
  to: string;
}

const EMPTY_DRAFT: Draft = { action: ALL, result: ALL, workspace: ALL, actor: ALL, from: ALL, to: ALL };

/**
 * The activity log (CC-F11): the audit events of the selected organization for
 * its owners and admins, and the instance-level events for the operator. The
 * server decides who may read (403/404 are shown as errors); this screen only
 * hides what the viewer cannot use. Events carry ids and codes, so people are
 * named from the member list when it is available.
 */
export function ActivityScreen({
  scope,
  isOperator,
  workspaces,
}: {
  scope: Scope;
  isOperator: boolean;
  workspaces: Workspace[];
}) {
  const { t, language } = useI18n();
  const { api } = useAuth();
  const organization = scope.organization;
  const mayOrg = canReadAudit(organization);
  const [view, setView] = useState<"organization" | "instance">("organization");
  const instanceView = isOperator && (view === "instance" || !mayOrg);
  const mayRead = instanceView || mayOrg;

  const [draft, setDraft] = useState<Draft>(EMPTY_DRAFT);
  const [applied, setApplied] = useState<AuditFilters>({});
  const [errors, setErrors] = useState<{ actor?: string; to?: string }>({});
  const form = useRef<HTMLFormElement>(null);

  const source: AuditSource | null = instanceView
    ? { kind: "instance" }
    : mayOrg && organization !== null
      ? { kind: "organization", orgId: organization.id }
      : null;
  const sourceKey = source === null ? "" : source.kind === "instance" ? "instance" : source.orgId;

  const loadEvents = useMemo(
    () => (source === null ? null : (cursor?: string) => api.listAudit(source, applied, cursor)),
    // `source` is derived from `sourceKey`; the filters apply only on submit.
    [api, sourceKey, applied],
  );
  const events = usePagedList<AuditEvent>(loadEvents);

  // People are named from the first page of the member list, when the viewer may see it.
  const orgId = !instanceView && mayOrg ? (organization?.id ?? null) : null;
  const loadMembers = useMemo(
    () => (orgId === null ? null : (cursor?: string) => api.listOrgMembers(orgId, cursor)),
    [api, orgId],
  );
  const members = usePagedList<OrgMember>(loadMembers);
  const memberName = (id: string): string =>
    members.items.find((member) => member.user_id === id)?.display_name ?? shortId(id);

  if (!mayRead || source === null) {
    return (
      <p className="muted">
        {organization === null && !isOperator
          ? t("Select an organization first.")
          : t("Only organization owners and admins can see the activity log.")}
      </p>
    );
  }

  const groups: Array<[string, string]> = instanceView
    ? [
        ["auth", t("Sign-in and sessions")],
        ["operator", t("Instance setup")],
        ["user", t("Account status")],
        ["password_reset", t("Password resets")],
        ["storage", t("Storage binding")],
        ["maintenance", t("Maintenance")],
        ["audit", t("Audit exports")],
      ]
    : [
        ["member", t("Organization members")],
        ["workspace", t("Workspaces and workspace members")],
        ["invitation", t("Invitations")],
        ["project", t("Projects")],
        ["file", t("Files")],
        ["version", t("Versions")],
        ["organization", t("Organization")],
        ["audit", t("Audit exports")],
      ];
  const resultName: Record<AuditResult, string> = {
    success: t("Success"),
    denied: t("Denied"),
    failed: t("Failed"),
  };

  function apply(event: FormEvent) {
    event.preventDefault();
    const next: { actor?: string; to?: string } = {};
    const actor = draft.actor.trim();
    if (actor !== "" && !UUID.test(actor)) {
      next.actor = t("Enter a valid user ID.");
    }
    const from = draft.from === "" ? null : dayStart(draft.from);
    const to = draft.to === "" ? null : dayAfter(draft.to);
    if (from !== null && to !== null && from >= to) {
      next.to = t("The end date must not be before the start date.");
    }
    setErrors(next);
    if (next.actor !== undefined || next.to !== undefined) {
      queueMicrotask(() => focusFirstInvalid(form.current));
      return;
    }
    const filters: AuditFilters = {};
    if (draft.action !== ALL) filters.action = draft.action;
    if (draft.result === "success" || draft.result === "denied" || draft.result === "failed") {
      filters.result = draft.result;
    }
    if (draft.workspace !== ALL) filters.workspace = draft.workspace;
    if (actor !== "") filters.actor = actor;
    if (from !== null) filters.from = from;
    if (to !== null) filters.to = to;
    setApplied(filters);
  }

  function reset() {
    setDraft(EMPTY_DRAFT);
    setErrors({});
    setApplied({});
  }

  const workspaceName = (id: string | null): string => {
    if (id === null) return "";
    return workspaces.find((candidate) => candidate.id === id)?.name ?? shortId(id);
  };

  return (
    <>
      <Banner tone="info">
        {t("Events contain identifiers and codes only, never names, e-mail addresses or file contents.")}
      </Banner>
      {isOperator && (
        <NativeSelect
          label={t("Log")}
          value={instanceView ? "instance" : "organization"}
          options={[
            ...(mayOrg ? [{ id: "organization", label: t("This organization") }] : []),
            { id: "instance", label: t("Instance (operator)") },
          ]}
          onChange={(value) => {
            setView(value === "instance" ? "instance" : "organization");
            setDraft(EMPTY_DRAFT);
            setErrors({});
            setApplied({});
          }}
        />
      )}

      <section className="section" aria-labelledby="activity-filter-title">
        <h2 id="activity-filter-title" className="section__title">
          {t("Filters")}
        </h2>
        <Form className="form form--filters toolbar--framed" onSubmit={apply} ref={form} validationBehavior="aria">
          <NativeSelect
            label={t("Action")}
            value={draft.action}
            options={[{ id: ALL, label: t("All actions") }, ...groups.map(([id, label]) => ({ id, label }))]}
            onChange={(action) => setDraft({ ...draft, action })}
          />
          <NativeSelect
            label={t("Result")}
            value={draft.result}
            options={[
              { id: ALL, label: t("All results") },
              { id: "success", label: resultName.success },
              { id: "denied", label: resultName.denied },
              { id: "failed", label: resultName.failed },
            ]}
            onChange={(result) => setDraft({ ...draft, result })}
          />
          {!instanceView && (
            <NativeSelect
              label={t("Workspace")}
              value={draft.workspace}
              options={[
                { id: ALL, label: t("All workspaces") },
                ...workspaces.map((workspace) => ({ id: workspace.id, label: workspace.name })),
              ]}
              onChange={(workspace) => setDraft({ ...draft, workspace })}
            />
          )}
          <label className="field">
            <span className="field__label">{t("Actor (user ID)")}</span>
            <input
              className="field__input"
              type="text"
              name="audit-actor"
              autoComplete="off"
              spellCheck={false}
              value={draft.actor}
              aria-invalid={errors.actor !== undefined ? true : undefined}
              aria-describedby={errors.actor !== undefined ? "audit-actor-error" : undefined}
              onChange={(event) => setDraft({ ...draft, actor: event.target.value })}
            />
            {errors.actor !== undefined && (
              <span id="audit-actor-error" className="field__error">
                {errors.actor}
              </span>
            )}
          </label>
          <label className="field">
            <span className="field__label">{t("From")}</span>
            <input
              className="field__input"
              type="date"
              name="audit-from"
              value={draft.from}
              onChange={(event) => setDraft({ ...draft, from: event.target.value })}
            />
          </label>
          <label className="field">
            <span className="field__label">{t("To")}</span>
            <input
              className="field__input"
              type="date"
              name="audit-to"
              value={draft.to}
              aria-invalid={errors.to !== undefined ? true : undefined}
              aria-describedby={errors.to !== undefined ? "audit-to-error" : undefined}
              onChange={(event) => setDraft({ ...draft, to: event.target.value })}
            />
            {errors.to !== undefined && (
              <span id="audit-to-error" className="field__error">
                {errors.to}
              </span>
            )}
          </label>
          <div className="form__actions">
            <Button type="submit" className="button button--primary">
              {t("Apply filters")}
            </Button>
            <Button className="button button--quiet" onPress={reset}>
              {t("Reset filters")}
            </Button>
          </div>
        </Form>
      </section>

      <section className="section" aria-labelledby="activity-list-title">
        <div className="section__head">
          <h2 id="activity-list-title" className="section__title">
            {t("Events")}
          </h2>
          <a className="button button--sm" href={api.auditExportUrl(source, applied)} download>
            {t("Export as NDJSON")}
          </a>
        </div>
        <ListStatus
          list={events}
          empty={t("There are no matching events.")}
          loadingLabel={t("Loading activity…")}
        />
        {events.items.length > 0 && (
          <div className="table-wrap">
            <table className="table">
              <caption className="visually-hidden">{t("Activity")}</caption>
              <thead>
                <tr>
                  <th scope="col">{t("Time")}</th>
                  <th scope="col">{t("Action")}</th>
                  <th scope="col">{t("Result")}</th>
                  <th scope="col">{t("Actor")}</th>
                  <th scope="col" className="col--optional">{t("Target")}</th>
                  {!instanceView && <th scope="col" className="col--optional">{t("Workspace")}</th>}
                  <th scope="col" className="col--optional">{t("Details")}</th>
                  <th scope="col" className="col--optional">{t("Request ID")}</th>
                </tr>
              </thead>
              <tbody>
                {events.items.map((event) => (
                  <tr key={event.id}>
                    <th scope="row" className="numeric">
                      {formatDateTime(language, event.created_at)}
                      {event.count > 1 && (
                        <>
                          <br />
                          <span className="muted">
                            {t("{count} times, latest {time}", {
                              count: String(event.count),
                              time: formatDateTime(language, event.last_at),
                            })}
                          </span>
                        </>
                      )}
                    </th>
                    <td className="hash nowrap" data-label={t("Action")}>{event.action}</td>
                    <td className="nowrap" data-label={t("Result")}><Badge tone={event.result === "success" ? "good" : "bad"}>{resultName[event.result]}</Badge></td>
                    <td data-label={t("Actor")}>
                      {event.actor_id === null ? t("System or unknown") : memberName(event.actor_id)}
                    </td>
                    <td className="hash col--optional" data-label={t("Target")}>
                      {event.target === null
                        ? "—"
                        : `${event.target.type ?? ""} ${shortId(event.target.id)}`.trim()}
                    </td>
                    {!instanceView && <td className="col--optional" data-label={t("Workspace")}>{workspaceName(event.workspace_id)}</td>}
                    <td className="hash col--optional" data-label={t("Details")}>
                      {Object.entries(event.details)
                        .map(([key, value]) => `${key}: ${String(value)}`)
                        .join(", ") || "—"}
                    </td>
                    <td className="hash col--optional" data-label={t("Request ID")}>{event.request_id ?? "—"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        <LoadMoreButton list={events} />
      </section>
    </>
  );
}
