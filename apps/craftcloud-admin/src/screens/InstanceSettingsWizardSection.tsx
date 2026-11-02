import { useEffect, useState } from "react";
import { Button } from "react-aria-components";
import type { FormEvent } from "react";
import { isApiError } from "../api/client.ts";
import type { InstanceSecretSetting, InstanceSetting, InstanceSettings, InstanceSettingsPatch } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { Banner } from "../components/Banner.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { useI18n } from "../i18n/index.tsx";
import type { Translate } from "../i18n/index.tsx";

type FieldName =
  | "mail.transport" | "mail.host" | "mail.port" | "mail.tls" | "mail.username" | "mail.from"
  | "mail.tenant_id" | "mail.client_id" | "mail.sender" | "mail.save_to_sent"
  | "retention.version_days" | "retention.trash_days" | "retention.audit_days" | "retention.quarantine_days";
type SecretName = "mail.password" | "mail.client_secret" | "mail.certificate" | "mail.private_key" | "mail.service_account";
type NormalField = { name: FieldName; label: string; kind: "text" | "number" | "select" | "checkbox"; choices?: string[]; min?: number; max?: number };
const MAIL_FIELDS: NormalField[] = [
  { name: "mail.transport", label: "Mail transport", kind: "select", choices: ["none", "smtp", "graph", "smtp_oauth_microsoft", "smtp_oauth_google"] },
  { name: "mail.host", label: "SMTP host", kind: "text" },
  { name: "mail.port", label: "SMTP port", kind: "number", min: 1, max: 65535 },
  { name: "mail.tls", label: "SMTP encryption", kind: "select", choices: ["starttls", "implicit"] },
  { name: "mail.username", label: "SMTP username", kind: "text" },
  { name: "mail.from", label: "From address", kind: "text" },
  { name: "mail.tenant_id", label: "Microsoft tenant ID", kind: "text" },
  { name: "mail.client_id", label: "OAuth client ID", kind: "text" },
  { name: "mail.sender", label: "Graph sender", kind: "text" },
  { name: "mail.save_to_sent", label: "Save sent messages to Sent Items", kind: "checkbox" },
];

const SECRET_FIELDS: { name: SecretName; label: string; multiline?: boolean }[] = [
  { name: "mail.password", label: "SMTP password" },
  { name: "mail.client_secret", label: "OAuth client secret" },
  { name: "mail.certificate", label: "OAuth certificate", multiline: true },
  { name: "mail.private_key", label: "OAuth private key", multiline: true },
  { name: "mail.service_account", label: "Google service account", multiline: true },
];

const RETENTION_FIELDS: NormalField[] = [
  { name: "retention.version_days", label: "Version retention (days)", kind: "number", min: 1, max: 3650 },
  { name: "retention.trash_days", label: "Trash retention (days)", kind: "number", min: 1, max: 3650 },
  { name: "retention.audit_days", label: "Audit retention (days)", kind: "number", min: 30, max: 3650 },
  { name: "retention.quarantine_days", label: "Quarantine retention (days)", kind: "number", min: 1, max: 3650 },
];

function isSecret(value: InstanceSetting | InstanceSecretSetting | undefined): value is InstanceSecretSetting {
  return value !== undefined && "configured_at" in value;
}

function stringValue(value: InstanceSettings["settings"][string] | undefined): string {
  if (value === undefined || isSecret(value)) return "";
  return value.value === null ? "" : String(value.value);
}

function translateFieldLabel(t: Translate, label: string): string {
  switch (label) {
    case "Mail transport": return t("Mail transport");
    case "SMTP host": return t("SMTP host");
    case "SMTP port": return t("SMTP port");
    case "SMTP encryption": return t("SMTP encryption");
    case "SMTP username": return t("SMTP username");
    case "From address": return t("From address");
    case "Microsoft tenant ID": return t("Microsoft tenant ID");
    case "OAuth client ID": return t("OAuth client ID");
    case "Graph sender": return t("Graph sender");
    case "Save sent messages to Sent Items": return t("Save sent messages to Sent Items");
    case "SMTP password": return t("SMTP password");
    case "OAuth client secret": return t("OAuth client secret");
    case "OAuth certificate": return t("OAuth certificate");
    case "OAuth private key": return t("OAuth private key");
    case "Google service account": return t("Google service account");
    case "Version retention (days)": return t("Version retention (days)");
    case "Trash retention (days)": return t("Trash retention (days)");
    case "Audit retention (days)": return t("Audit retention (days)");
    case "Quarantine retention (days)": return t("Quarantine retention (days)");
    default: return label;
  }
}

export function InstanceSettingsWizardSection({ section, onEmailTest }: { section: "mail" | "retention"; onEmailTest?: () => void }) {
  const { t } = useI18n();
  const { api } = useAuth();
  const [settings, setSettings] = useState<InstanceSettings | null>(null);
  const [draft, setDraft] = useState<Record<string, string>>({});
  const [secrets, setSecrets] = useState<Record<string, string>>({});
  const [removeSecrets, setRemoveSecrets] = useState<Set<SecretName>>(new Set());
  const [error, setError] = useState<unknown>(null);
  const [saved, setSaved] = useState(false);
  const [busy, setBusy] = useState(false);
  const [mailTest, setMailTest] = useState<"idle" | "sent" | "error">("idle");

  async function load() {
    setError(null);
    try {
      const result = await api.getAdminInstanceSettings();
      setSettings(result);
      const values = Object.fromEntries([...MAIL_FIELDS, ...RETENTION_FIELDS].map((field) => [field.name, stringValue(result.settings[field.name])]));
      setDraft(values);
      setSecrets({});
      setRemoveSecrets(new Set());
    } catch (failure) { setError(failure); }
  }

  useEffect(() => { void load(); }, [api]);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy || settings === null) return;
    setBusy(true);
    setError(null);
    setSaved(false);
    const changes: InstanceSettingsPatch["settings"] = {};
    const fields = section === "mail" ? MAIL_FIELDS : RETENTION_FIELDS;
    for (const field of fields) {
      const existing = settings.settings[field.name];
      if (existing === undefined || isSecret(existing) || existing.locked) continue;
      const next = draft[field.name] ?? "";
      const before = stringValue(existing);
      if (next === before) continue;
      if (field.kind === "number" && next !== "") {
        const numeric = Number(next);
        const min = field.min ?? 1;
        const max = field.max ?? 3650;
        if (!Number.isSafeInteger(numeric) || numeric < min || numeric > max) {
          setError(new Error(t("Enter a whole number from {min} to {max}.", { min: String(min), max: String(max) })));
          setBusy(false);
          return;
        }
      }
      changes[field.name] = { value: field.kind === "number" ? (next === "" ? null : Number(next)) : field.kind === "checkbox" ? next === "true" : next };
    }
    if (section === "mail") {
      for (const field of SECRET_FIELDS) {
        const item = settings.settings[field.name];
        if (!isSecret(item) || item.locked === true || item.source === "environment") continue;
        const value = secrets[field.name] ?? "";
        if (removeSecrets.has(field.name)) changes[field.name] = { remove: true };
        else if (value !== "") changes[field.name] = { set: value };
      }
    }
    try {
      if (Object.keys(changes).length > 0) {
        const result = await api.patchAdminInstanceSettings({ settings: changes });
        setSettings(result);
      }
      setSecrets({});
      setRemoveSecrets(new Set());
      setSaved(true);
    } catch (failure) { setError(failure); }
    finally { setBusy(false); }
  }

  async function sendTest() {
    if (busy) return;
    setBusy(true); setError(null); setMailTest("idle");
    try { await api.testAdminInstanceEmail(); setMailTest("sent"); onEmailTest?.(); }
    catch (failure) { setError(failure); setMailTest("error"); }
    finally { setBusy(false); }
  }

  if (settings === null) return error === null
    ? <p role="status" className="muted">{t("Loading instance settings…")}</p>
    : <ErrorNotice error={error} onRetry={() => void load()} onReload={() => void load()} />;

  const fields = section === "mail" ? MAIL_FIELDS : RETENTION_FIELDS;
  const dirtyFields = fields.some((field) => {
    const item = settings.settings[field.name];
    return item !== undefined && !isSecret(item) && !item.locked && (draft[field.name] ?? "") !== stringValue(item);
  });
  const dirtySecrets = SECRET_FIELDS.some((field) => (secrets[field.name] ?? "") !== "") || removeSecrets.size > 0;
  return <form className="form" onSubmit={(event) => void submit(event)}>
    {error !== null && <>
      {isApiError(error) && error.code === "FRESH_AUTH_REQUIRED"
        ? <Banner tone="warn">{t("Sign in again to change this sensitive setting, then retry.")}</Banner>
        : isApiError(error) && error.code === "MFA_REAUTH_REQUIRED"
          ? <Banner tone="warn">{t("Sign in again with your two-factor authentication, then retry.")}</Banner>
          : <ErrorNotice error={error} />}
    </>}
    {saved && <Banner tone="good">{t("Settings saved.")}</Banner>}
    {section === "mail" && <>
      <p>{t("Choose no e-mail delivery to share invitation links by hand, or configure a mail transport.")}</p>
      {fields.map((field) => {
        const item = settings.settings[field.name];
        if (item === undefined || isSecret(item)) return null;
        const locked = item.locked;
        const value = draft[field.name] ?? "";
        const label = translateFieldLabel(t, field.label);
        const inputId = `instance-setting-${field.name.replaceAll(".", "-")}`;
        return <label className="field" key={field.name}>
          <span className="field__label">{label}</span>
          {field.kind === "select" ? <select id={inputId} aria-label={label} className="select__native" value={value} disabled={locked} onChange={(event) => { const value = event.currentTarget.value; setDraft((current) => ({ ...current, [field.name]: value })); }}>
            {(field.choices ?? []).map((choice) => <option key={choice} value={choice}>{choice === "none" ? t("None") : choice.toUpperCase().replaceAll("_", " ")}</option>)}
          </select> : field.kind === "checkbox" ? <input id={inputId} aria-label={label} type="checkbox" checked={value === "true"} disabled={locked} onChange={(event) => { const value = String(event.currentTarget.checked); setDraft((current) => ({ ...current, [field.name]: value })); }} /> : <input id={inputId} aria-label={label} className="field__input" type={field.kind} min={"min" in field ? field.min : undefined} max={"max" in field ? field.max : undefined} value={value} disabled={locked} onChange={(event) => { const value = event.currentTarget.value; setDraft((current) => ({ ...current, [field.name]: value })); }} />}
          {locked && <span className="field__hint">{t("Set by the configuration file.")}</span>}
        </label>;
      })}
      {SECRET_FIELDS.map((field) => {
        const item = settings.settings[field.name];
        if (!isSecret(item)) return null;
        const configured = item.configured_at !== null || item.source === "environment";
        const locked = item.locked === true || item.source === "environment";
        const inputId = `instance-setting-${field.name.replaceAll(".", "-")}`;
        return <label className="field" key={field.name}>
          <span className="field__label">{translateFieldLabel(t, field.label)}</span>
          {field.multiline
            ? <textarea id={inputId} aria-label={translateFieldLabel(t, field.label)} className="field__input field__input--multiline field__input--mono" autoComplete="off" value={secrets[field.name] ?? ""} disabled={locked} onChange={(event) => { const value = event.currentTarget.value; setSecrets((current) => ({ ...current, [field.name]: value })); }} />
            : <input id={inputId} aria-label={translateFieldLabel(t, field.label)} className="field__input" type="password" autoComplete="new-password" value={secrets[field.name] ?? ""} disabled={locked} onChange={(event) => { const value = event.currentTarget.value; setSecrets((current) => ({ ...current, [field.name]: value })); }} />}
          <span className="field__hint">{locked ? t("Secret is set by the configuration file.") : configured ? t("Secret configured; last set {date}.", { date: item.configured_at ?? "—" }) : t("No secret configured.")}</span>
          {locked && <span className="field__hint">{t("Set by the configuration file.")}</span>}
          {configured && !locked && <Button className="button button--sm button--danger-quiet" type="button" onPress={() => setRemoveSecrets((current) => {
            const next = new Set(current);
            if (next.has(field.name)) next.delete(field.name); else next.add(field.name);
            return next;
          })}>{removeSecrets.has(field.name) ? t("Secret will be removed") : t("Remove saved secret")}</Button>}
        </label>;
      })}
      <Button className="button" type="button" isDisabled={busy || dirtyFields || dirtySecrets} onPress={() => void sendTest()}>{t("Send test e-mail")}</Button>
      {(dirtyFields || dirtySecrets) && <p className="field__hint">{t("Save settings before sending a test e-mail.")}</p>}
      {mailTest === "sent" && <p role="status" className="muted">{t("Test e-mail queued for delivery.")}</p>}
    </>}
    {section === "retention" && <p>{t("These values control instance maintenance and retention.")}</p>}
    {section === "retention" && fields.map((field) => {
      const item = settings.settings[field.name];
      if (item === undefined || isSecret(item)) return null;
      return <label className="field" key={field.name}>
        <span className="field__label">{translateFieldLabel(t, field.label)}</span>
        <input id={`instance-setting-${field.name.replaceAll(".", "-")}`} aria-label={translateFieldLabel(t, field.label)} className="field__input" type="number" min={field.min} max={field.max} value={draft[field.name] ?? ""} disabled={item.locked} onChange={(event) => { const value = event.currentTarget.value; setDraft((current) => ({ ...current, [field.name]: value })); }} />
        {item.locked && <span className="field__hint">{t("Set by the configuration file.")}</span>}
      </label>;
    })}
    <Button className="button button--primary" type="submit" isDisabled={busy}>{busy ? t("Saving…") : t("Save settings")}</Button>
  </form>;
}
