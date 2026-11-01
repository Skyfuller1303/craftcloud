import { useEffect, useState } from "react";
import { Button } from "react-aria-components";
import type { Organization, Workspace } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { Field } from "../components/forms.tsx";
import { NativeSelect } from "../components/ContextSelect.tsx";
import { useI18n } from "../i18n/index.tsx";

/** Owner-only organization identity settings. No data calls for other roles. */
export function SsoScimSettings({ organization, workspaces }: { organization: Organization; workspaces: Workspace[] }) {
  const { t, language } = useI18n();
  const { api } = useAuth();
  const owner = organization.role === "owner";
  const [config, setConfig] = useState<Record<string, unknown> | null>(null);
  const [domains, setDomains] = useState("");
  const claims = Array.isArray(config?.domain_claims) ? config.domain_claims as { domain: string; status: string }[] : [];
  const [provider, setProvider] = useState({ discovery_url: "", client_id: "", enabled: false, jit_enabled: false, sso_required: false, trust_mfa: true, version: null as number | null });
  const [clientSecret, setClientSecret] = useState("");
  const [mappings, setMappings] = useState<{ group_id: string; workspace_id: string; role: "viewer" | "editor" }[]>([]);
  const [events, setEvents] = useState<Record<string, unknown>[]>([]);
  const [scimStatus, setScimStatus] = useState<Record<string, unknown> | null>(null);
  const [testResult, setTestResult] = useState<string | null>(null);
  const [token, setToken] = useState<string | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    if (!owner) return;
    let live = true;
    setToken(null);
    setTestResult(null);
    setConfig(null);
    setScimStatus(null);
    setMappings([]);
    setEvents([]);
    Promise.all([api.getOrganizationSso(organization.id), api.getScimStatus(organization.id), api.getScimMappings(organization.id), api.getScimEvents(organization.id)])
      .then(([sso, status, map, log]) => {
        if (!live) return;
        setToken(null);
        setTestResult(null);
        const loaded = sso.configured === false ? { discovery_url: "", client_id: "", allowed_domains: [], enabled: false, jit_enabled: false, sso_required: false, trust_mfa: true, version: null } : sso;
        setConfig(loaded);
        setProvider({ discovery_url: String(loaded.discovery_url ?? ""), client_id: String(loaded.client_id ?? ""), enabled: loaded.enabled === true, jit_enabled: loaded.jit_enabled === true, sso_required: loaded.sso_required === true, trust_mfa: loaded.trust_mfa === true, version: typeof loaded.version === "number" ? loaded.version : null });
        setDomains(Array.isArray(loaded.allowed_domains) ? loaded.allowed_domains.join(", ") : "");
        setScimStatus(status);
        setMappings(Array.isArray(map.items) ? map.items as { group_id: string; workspace_id: string; role: "viewer" | "editor" }[] : []);
        setEvents(log.items);
      }).catch((failure: unknown) => { if (live) setError(failure); });
    return () => { live = false; };
  }, [api, organization.id, owner]);
  if (!owner) return <p className="muted">{t("Only organization owners can manage single sign-on and SCIM. Contact the organization owner for help.")}</p>;
  async function action(run: () => Promise<unknown>) {
    setBusy(true); setError(null);
    try { await run(); } catch (failure) { setError(failure); } finally { setBusy(false); }
  }
  const tokenConfigured = scimStatus?.token_configured === true;
  const providerLabels = { enabled: t("Enable single sign-on"), jit_enabled: t("Create accounts automatically on first sign-in"), sso_required: t("Require single sign-on"), trust_mfa: t("Trust provider multi-factor authentication") };
  const formatEventDate = (value: unknown) => {
    if (typeof value !== "string") return "";
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? value : new Intl.DateTimeFormat(language, { dateStyle: "medium", timeStyle: "short" }).format(date);
  };
  return <div className="stack">
    <section className="section">
      <h2 className="section__title">{t("Single sign-on")}</h2>
      {error !== null && <ErrorNotice error={error} />}
      <Field label={t("OIDC discovery URL")} name="sso-discovery-url" value={provider.discovery_url} onChange={(value) => setProvider({ ...provider, discovery_url: value })} />
      <Field label={t("Client ID")} name="sso-client-id" value={provider.client_id} onChange={(value) => setProvider({ ...provider, client_id: value })} />
      <Field label={t("Allowed e-mail domains (comma separated)")} name="sso-domains" value={domains} onChange={setDomains} />
      <p>{t("A domain only takes effect after the instance operator approved it.")}</p>
      {claims.length > 0 && <ul aria-label={t("Domain approval status")}>{claims.map((claim) => <li key={claim.domain}>{claim.domain}: {claim.status === "approved" ? t("Approved") : claim.status === "rejected" ? t("Rejected by the instance operator") : t("Pending operator approval")}</li>)}</ul>}
      <Field label={t("Client secret (leave blank to keep current secret)")} name="sso-client-secret" type="password" required={false} value={clientSecret} onChange={setClientSecret} />
      {(["enabled", "jit_enabled", "sso_required", "trust_mfa"] as const).map((key) => <label className="checkbox" key={key}><input type="checkbox" checked={provider[key]} onChange={(event) => setProvider({ ...provider, [key]: event.currentTarget.checked })} /><span>{providerLabels[key]}</span></label>)}
      {provider.sso_required && <p role="note">{t("Warning: local password sign-in will be blocked for this organization while single sign-on is required. Instance operator recovery remains available.")}</p>}
      <p>{t("Callback URL")}: <code>{window.location.origin}/api/v1/auth/oidc/callback</code></p>
      <Button className="button button--primary" isDisabled={busy || config === null} onPress={() => void action(async () => { try { const next = await api.saveOrganizationSso(organization.id, { ...provider, allowed_domains: domains.split(",").map((value) => value.trim()).filter(Boolean), client_secret: clientSecret || undefined }); setConfig(next); setProvider({ ...provider, version: typeof next.version === "number" ? next.version : provider.version }); } finally { setClientSecret(""); } })}>{t("Save single sign-on")}</Button>
      <Button className="button" isDisabled={busy || config === null} onPress={() => void action(async () => { const result = await api.testOrganizationSso(organization.id); setTestResult(typeof result.issuer === "string" ? result.issuer : t("Connection test succeeded.")); })}>{t("Test provider connection")}</Button>
      {testResult !== null && <p role="status">{t("Provider issuer")}: {testResult}</p>}
      <Button className="button" isDisabled={busy || config === null} onPress={() => void action(async () => { await api.deleteOrganizationSso(organization.id); const empty = { discovery_url: "", client_id: "", allowed_domains: [], enabled: false, jit_enabled: false, sso_required: false, trust_mfa: true, version: null }; setConfig(empty); setProvider({ ...empty, version: null }); setDomains(""); setClientSecret(""); })}>{t("Remove configuration")}</Button>
    </section>
    <section className="section">
      <h2 className="section__title">{t("SCIM provisioning")}</h2>
      <p role="status">{t("SCIM status")}: {scimStatus === null ? t("Loading…") : tokenConfigured ? t("SCIM token configured") : t("SCIM token not configured")}</p>
      <p>{t("SCIM base URL")}: <code>{window.location.origin}/scim/v2/organizations/{organization.id}</code></p>
      <p>{t("A generated SCIM token is shown once. Copy it now and store it securely.")}</p>
      {token !== null && <div><Field label={t("SCIM token — copy now")} name="scim-token" value={token} onChange={() => undefined} /><Button className="button" onPress={() => { void navigator.clipboard?.writeText(token).catch(setError); }}>{t("Copy token")}</Button><Button className="button" onPress={() => setToken(null)}>{t("Dismiss token")}</Button></div>}
      <Button className="button button--primary" isDisabled={busy} onPress={() => void action(async () => { const created = await api.createScimToken(organization.id); setToken(created.token); setScimStatus((current) => current === null ? current : { ...current, token_configured: true }); })}>{t("Generate SCIM token")}</Button>
      <Button className="button" isDisabled={busy} onPress={() => void action(async () => { await api.deleteScimToken(organization.id); setToken(null); setScimStatus((current) => current === null ? current : { ...current, token_configured: false }); })}>{t("Revoke SCIM token")}</Button>
      {mappings.map((mapping, index) => <div className="toolbar" key={`${mapping.group_id}-${index}`}>
        <NativeSelect label={t("SCIM group")} value={mapping.group_id} onChange={(group_id) => setMappings(mappings.map((row, i) => i === index ? { ...row, group_id } : row))} options={[{ id: "", label: t("Choose a group") }, ...(Array.isArray(scimStatus?.groups) ? scimStatus.groups as { id: string; display_name: string }[] : []).map((group) => ({ id: group.id, label: group.display_name }))]} />
        <NativeSelect label={t("Workspace")} value={mapping.workspace_id} onChange={(workspace_id) => setMappings(mappings.map((row, i) => i === index ? { ...row, workspace_id } : row))} options={[{ id: "", label: t("Choose a workspace") }, ...workspaces.map((workspace) => ({ id: workspace.id, label: workspace.name }))]} />
        <NativeSelect label={t("Workspace role")} value={mapping.role} onChange={(role) => setMappings(mappings.map((row, i) => i === index ? { ...row, role: role as "viewer" | "editor" } : row))} options={[{ id: "viewer", label: t("Viewer") }, { id: "editor", label: t("Editor") }]} />
        <Button className="button" onPress={() => setMappings(mappings.filter((_, i) => i !== index))}>{t("Remove mapping")}</Button>
      </div>)}
      <Button className="button" onPress={() => setMappings([...mappings, { group_id: "", workspace_id: "", role: "viewer" }])}>{t("Add mapping")}</Button>
      <Button className="button" isDisabled={busy} onPress={() => void action(async () => { await api.updateScimMappings(organization.id, { items: mappings }); })}>{t("Save mappings")}</Button>
      <h3>{t("Recent SCIM events")}</h3>
      {events.length === 0 ? <p>{t("No SCIM events yet.")}</p> : <table><thead><tr><th scope="col">{t("Action")}</th><th scope="col">{t("Date")}</th></tr></thead><tbody>{events.map((event, index) => <tr key={typeof event.id === "string" ? event.id : index}><td>{typeof event.action === "string" ? event.action.replace(/[._]/g, " ") : t("Unknown action")}</td><td>{formatEventDate(event.created_at)}</td></tr>)}</tbody></table>}
    </section>
  </div>;
}
