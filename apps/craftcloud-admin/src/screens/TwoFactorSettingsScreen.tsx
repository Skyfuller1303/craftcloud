import { useEffect, useState } from "react";
import { Button } from "react-aria-components";
import { QRCodeSVG } from "qrcode.react";
import { describeError } from "../api/errors.ts";
import type { Organization } from "../api/types.ts";
import { isApiError } from "../api/client.ts";
import { useAuth } from "../auth.tsx";
import { Badge } from "../components/Badge.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { Alert, Field } from "../components/forms.tsx";
import { useI18n } from "../i18n/index.tsx";
import { DevicesScreen } from "./DevicesScreen.tsx";

type StatusState =
  | { state: "loading" }
  | { state: "ready"; enabled: boolean; available: boolean; remaining: number }
  | { state: "error"; error: unknown };

function saveCodes(codes: string[], fileName: string) {
  const blob = new Blob([codes.join("\n") + "\n"], { type: "text/plain;charset=utf-8" });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = fileName;
  document.body.append(link);
  link.click();
  link.remove();
  window.setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export function TwoFactorSettingsScreen({
  organization,
  onOrganizationUpdated,
  onFactorChanged,
}: {
  organization: Organization | null;
  onOrganizationUpdated: (organization: Organization) => void;
  onFactorChanged: (enabled: boolean) => void;
}) {
  const { t } = useI18n();
  const { api } = useAuth();
  const [status, setStatus] = useState<StatusState>({ state: "loading" });
  const [attempt, setAttempt] = useState(0);
  const [setup, setSetup] = useState<{ secret: string; uri: string } | null>(null);
  const [codes, setCodes] = useState<string[] | null>(null);
  const [password, setPassword] = useState("");
  const [confirmation, setConfirmation] = useState("");
  const [proof, setProof] = useState("");
  const [proofIsRecovery, setProofIsRecovery] = useState(false);
  const [required, setRequired] = useState(false);
  const [policyRetryTarget, setPolicyRetryTarget] = useState<boolean | null>(null);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<unknown>(null);
  const [policyError, setPolicyError] = useState<unknown>(null);

  useEffect(() => {
    let current = true;
    setStatus({ state: "loading" });
    api.getTwoFactorStatus().then(
      (result) => {
        if (current) setStatus({
          state: "ready",
          enabled: result.enabled,
          available: result.available,
          remaining: result.recovery_codes_remaining,
        });
      },
      (error: unknown) => {
        if (current) setStatus({ state: "error", error });
      },
    );
    return () => { current = false; };
  }, [api, attempt]);

  useEffect(() => {
    setRequired(organization?.require_two_factor === true);
    setPolicyError(null);
    setPolicyRetryTarget(null);
  }, [organization?.id, organization?.require_two_factor]);

  async function beginSetup() {
    if (busy || password === "") return;
    setBusy(true);
    setFailure(null);
    try {
      const result = await api.setupTwoFactor(password);
      setSetup({ secret: result.secret, uri: result.otpauth_url });
      setPassword("");
    } catch (error) {
      setFailure(error);
      setPassword("");
    } finally {
      setBusy(false);
    }
  }

  async function confirmSetup() {
    if (busy || setup === null || !/^\d{6}$/.test(confirmation)) return;
    setBusy(true);
    setFailure(null);
    try {
      const result = await api.confirmTwoFactor(confirmation);
      setCodes(result.recovery_codes);
      setSetup(null);
      setConfirmation("");
      setStatus({ state: "ready", available: true, enabled: true, remaining: result.recovery_codes.length });
      onFactorChanged(true);
    } catch (error) {
      setFailure(error);
      setConfirmation("");
    } finally {
      setBusy(false);
    }
  }

  async function regenerateCodes() {
    if (busy || proof.trim() === "") return;
    setBusy(true);
    setFailure(null);
    try {
      const result = await api.regenerateRecoveryCodes(
        proofIsRecovery ? { recovery_code: proof } : { code: proof },
      );
      setCodes(result.recovery_codes);
      setProof("");
      setStatus((current) => current.state === "ready"
        ? { ...current, remaining: result.recovery_codes.length }
        : current);
    } catch (error) {
      setFailure(error);
      setProof("");
    } finally {
      setBusy(false);
    }
  }

  async function disableTwoFactor() {
    if (busy || password === "" || proof.trim() === "") return;
    setBusy(true);
    setFailure(null);
    try {
      await api.disableTwoFactor({
        password,
        ...(proofIsRecovery ? { recovery_code: proof } : { code: proof }),
      });
      setPassword("");
      setProof("");
      setProofIsRecovery(false);
      setCodes(null);
      setStatus({ state: "ready", available: true, enabled: false, remaining: 0 });
      onFactorChanged(false);
    } catch (error) {
      setFailure(error);
      setPassword("");
      setProof("");
    } finally {
      setBusy(false);
    }
  }

  async function changePolicy(next: boolean) {
    if (organization === null || busy) return;
    setBusy(true);
    setPolicyError(null);
    setPolicyRetryTarget(null);
    try {
      const updated = await api.updateOrganizationTwoFactor(organization.id, organization.version, next);
      setRequired(updated.require_two_factor === true);
      onOrganizationUpdated(updated);
    } catch (error) {
      setPolicyError(error);
      setPolicyRetryTarget(next);
      setRequired(organization.require_two_factor === true);
    } finally {
      setBusy(false);
    }
  }

  const unavailable = status.state === "ready" && !status.available;
  const enabled = status.state === "ready" && status.enabled;

  return (
    <div className="stack">
      <section className="section" aria-labelledby="two-factor-title">
        <h2 id="two-factor-title" className="section__title">{t("Two-factor authentication")}</h2>
        {status.state === "loading" && <p role="status" className="muted">{t("Loading two-factor status…")}</p>}
        {status.state === "error" && <ErrorNotice error={status.error} onRetry={() => setAttempt((value) => value + 1)} />}
        {status.state === "ready" && (
          <>
            <p>{t("Use an authenticator app to protect browser and device sign-ins.")}</p>
            <p>
              <Badge tone={enabled ? "good" : "neutral"}>{enabled ? t("Enabled") : t("Not enabled")}</Badge>
              {enabled && <span className="cell__sub">{t("{count} recovery codes remain.", { count: status.remaining })}</span>}
            </p>
            {unavailable && (
              <Alert>{t("Two-factor authentication is unavailable because the server secret key is not configured.")}</Alert>
            )}
            {failure !== null && <Alert>{describeError(t, failure)}</Alert>}
            {codes !== null ? (
              <div className="two-factor-codes">
                <Alert tone="info">{t("Save these recovery codes now. They will only be shown once.")}</Alert>
                <label className="field">
                  <span className="field__label">{t("Recovery codes")}</span>
                  <textarea className="field__input field__input--mono" rows={10} readOnly value={codes.join("\n")} />
                </label>
                <Button className="button button--primary" onPress={() => saveCodes(codes, "craftcloud-recovery-codes.txt")}>
                  {t("Download recovery codes")}
                </Button>
                <Button className="button" onPress={() => setCodes(null)}>{t("I have saved the codes")}</Button>
              </div>
            ) : !enabled && !unavailable ? (
              setup === null ? (
                <div className="stack two-factor-form">
                  <Field label={t("Current password")} name="two-factor-password" type="password" autoComplete="current-password" value={password} onChange={setPassword} />
                  <Button className="button button--primary" onPress={() => void beginSetup()} isDisabled={busy || password === ""}>
                    {busy ? t("Preparing…") : t("Set up two-factor authentication")}
                  </Button>
                </div>
              ) : (
                <div className="stack two-factor-enrollment">
                  <p>{t("Scan this QR code with your authenticator app, or enter the secret manually.")}</p>
                  <div className="two-factor-qr">
                    <QRCodeSVG value={setup.uri} size={192} level="M" title={t("Authenticator setup QR code")} />
                  </div>
                  <dl className="facts">
                    <dt>{t("Secret")}</dt>
                    <dd><code className="two-factor-secret">{setup.secret}</code></dd>
                  </dl>
                  <Field label={t("Authentication code")} name="confirm-two-factor-code" value={confirmation} onChange={setConfirmation} autoComplete="one-time-code" />
                  <Button className="button button--primary" onPress={() => void confirmSetup()} isDisabled={busy || !/^\d{6}$/.test(confirmation)}>
                    {busy ? t("Checking…") : t("Confirm and show recovery codes")}
                  </Button>
                  <Button className="button" onPress={() => { setSetup(null); setConfirmation(""); }} isDisabled={busy}>{t("Cancel setup")}</Button>
                </div>
              )
            ) : (
              <div className="stack two-factor-form">
                <Field label={proofIsRecovery ? t("Recovery code") : t("Authentication code")} name="two-factor-proof" value={proof} onChange={setProof} autoComplete="one-time-code" />
                <Button className="button" onPress={() => void regenerateCodes()} isDisabled={busy || proof.trim() === ""}>
                  {t("Generate new recovery codes")}
                </Button>
                <Button className="button button--link" onPress={() => setProofIsRecovery((value) => !value)}>
                  {proofIsRecovery ? t("Use an authentication code") : t("Use a recovery code")}
                </Button>
                <hr className="form__rule" />
                <Field label={t("Current password")} name="disable-two-factor-password" type="password" autoComplete="current-password" value={password} onChange={setPassword} />
                <Button className="button button--danger-quiet" onPress={() => void disableTwoFactor()} isDisabled={busy || password === "" || proof.trim() === ""}>
                  {t("Disable two-factor authentication")}
                </Button>
              </div>
            )}
          </>
        )}
      </section>

      <section className="section" aria-labelledby="two-factor-policy-title">
        <h2 id="two-factor-policy-title" className="section__title">{t("Organization policy")}</h2>
        {organization === null ? <p className="muted">{t("Select an organization to manage its sign-in policy.")}</p> : (
          <>
            <p>{t("Require two-factor authentication for organization access.")}</p>
            {policyError !== null && (
              <ErrorNotice
                error={policyError}
                context={isApiError(policyError) && policyError.code === "TWO_FACTOR_SETUP_REQUIRED"
                  ? t("Set up two-factor authentication on your account before enabling this policy.")
                  : undefined}
                onRetry={() => void changePolicy(policyRetryTarget ?? required)}
              />
            )}
            <label className="two-factor-policy">
              <input
                type="checkbox"
                checked={required}
                disabled={busy || !["owner", "admin"].includes(organization.role)}
                onChange={(event) => void changePolicy(event.currentTarget.checked)}
              />
              <span>{t("Make two-factor authentication mandatory")}</span>
            </label>
            {!["owner", "admin"].includes(organization.role) && (
              <p className="muted">{t("Only organization owners and admins can change this policy.")}</p>
            )}
          </>
        )}
      </section>

      <DevicesScreen />
    </div>
  );
}

