import { useEffect, useState } from "react";
import { Button, Form } from "react-aria-components";
import { useAuth } from "../auth.tsx";
import { useAnnounce } from "../components/Announcer.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { Alert, Field, ScreenHeading } from "../components/forms.tsx";
import { NativeSelect } from "../components/ContextSelect.tsx";
import { useAction, useCountdown } from "../hooks.ts";
import { useI18n } from "../i18n/index.tsx";

/** An unavailable feature description never enables account-changing controls. */
export function useEmailDelivery() {
  const { api } = useAuth();
  const [enabled, setEnabled] = useState(false);
  useEffect(() => {
    let active = true;
    api.serverInfo().then((info) => {
      if (active) setEnabled(info.feature_flags?.email_delivery === true);
    }, () => { if (active) setEnabled(false); });
    return () => { active = false; };
  }, [api]);
  return enabled;
}

export function VerificationNotice() {
  const { t } = useI18n();
  const { api } = useAuth();
  const action = useAction();
  const [sent, setSent] = useState(0);
  const left = useCountdown(sent === 0 ? 0 : 60, sent);
  return <div className="stack">
    <Alert tone="info">{t("Your e-mail address is not verified yet.")}</Alert>
    {sent > 0 && <Alert tone="success">{t("Check your inbox for the confirmation link.")}</Alert>}
    {action.failed && <ErrorNotice error={action.error} />}
    <div><Button className="button" isDisabled={action.pending || left > 0} onPress={() => {
      void action.run(async () => { await api.requestEmailVerification(); return true; }).then((done) => {
        if (done) setSent((value) => value + 1);
      });
    }}>{left > 0 ? t("Send again in {seconds} s", { seconds: left }) : t("Send confirmation link")}</Button></div>
  </div>;
}

export function EmailChangeForm() {
  const { api } = useAuth();
  const { t } = useI18n();
  const action = useAction();
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [proof, setProof] = useState("");
  const [recovery, setRecovery] = useState(false);
  const [sent, setSent] = useState(false);
  return <Form className="stack" onSubmit={(event) => {
    event.preventDefault();
    if (action.pending) return;
    setSent(false);
    void action.run(async () => {
      try {
        await api.requestEmailChange({ email: email.trim(), password, ...(proof === "" ? {} : recovery ? { recovery_code: proof } : { code: proof }) });
        return true;
      } finally { setPassword(""); setProof(""); }
    }).then((done) => { if (done) { setSent(true); setEmail(""); } });
  }}>
    <h3 className="section__subtitle">{t("Change e-mail address")}</h3>
    <p className="muted">{t("Confirm the new address within 24 hours. Your current address receives a security notice.")}</p>
    <Field label={t("New e-mail address")} name="new-email" type="email" autoComplete="email" value={email} onChange={setEmail} />
    <Field label={t("Current password")} name="email-password" type="password" autoComplete="current-password" value={password} onChange={setPassword} />
    <NativeSelect label={t("Second factor (if enabled)")} value={recovery ? "recovery" : "code"} options={[{ id: "code", label: t("Authenticator code") }, { id: "recovery", label: t("Recovery code") }]} onChange={(value) => { setRecovery(value === "recovery"); setProof(""); }} />
    <Field label={recovery ? t("Recovery code") : t("Authenticator code")} name="email-proof" required={false} autoComplete="one-time-code" value={proof} onChange={setProof} />
    {action.failed && <ErrorNotice error={action.error} />}
    {sent && <Alert tone="success">{t("If the address can be used, a confirmation email will be sent.")}</Alert>}
    <div><Button type="submit" className="button button--primary" isDisabled={action.pending}>{action.pending ? t("Saving…") : t("Send confirmation link")}</Button></div>
  </Form>;
}

/** Explicit confirmation prevents link previews from consuming capabilities. */
export function EmailConfirmationScreen({ token, kind, onFinished }: { token: string; kind: "verify" | "change"; onFinished: () => void }) {
  const { api } = useAuth();
  const { t } = useI18n();
  const announce = useAnnounce();
  const action = useAction();
  return <div className="screen screen--narrow">
    <ScreenHeading>{t("Confirm e-mail address")}</ScreenHeading>
    <p>{t("Confirm this address for your CraftCloud account.")}</p>
    {action.failed && <ErrorNotice error={action.error} />}
    <Button className="button button--primary" isDisabled={action.pending} onPress={() => {
      void action.run(async () => {
        if (kind === "change") await api.confirmEmailChange(token);
        else await api.confirmEmailVerification(token);
        return true;
      }).then((done) => { if (done) { announce(t("E-mail address confirmed.")); onFinished(); } });
    }}>{t("Confirm e-mail address")}</Button>
  </div>;
}

export function ResendInvitation({ orgId, invitationId, initiallySent = false }: { orgId: string; invitationId: string; initiallySent?: boolean }) {
  const { api } = useAuth();
  const { t } = useI18n();
  const announce = useAnnounce();
  const action = useAction();
  const [round, setRound] = useState(initiallySent ? 1 : 0);
  const left = useCountdown(round === 0 ? 0 : 60, round);
  return <>
    <Button className="button button--sm" isDisabled={action.pending || left > 0} onPress={() => {
      void action.run(async () => { await api.resendInvitation(orgId, invitationId); return true; }).then((done) => {
        if (done) { setRound((value) => value + 1); announce(t("Invitation sent.")); }
      });
    }}>{left > 0 ? t("Send again in {seconds} s", { seconds: left }) : t("Resend invitation")}</Button>
    {action.failed && <ErrorNotice error={action.error} />}
  </>;
}
