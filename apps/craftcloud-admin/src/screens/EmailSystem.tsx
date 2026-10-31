import { useEffect, useState } from "react";
import { Button } from "react-aria-components";
import type { EmailDeliveryStatus } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { ConfirmDialog } from "../components/Dialog.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { Alert } from "../components/forms.tsx";
import { formatDateTime } from "../format.ts";
import { useAction, useCountdown } from "../hooks.ts";
import { useI18n } from "../i18n/index.tsx";

/** Counts and delivery timestamps only; never retrieves message content. */
export function EmailSystem({ revision }: { revision: number }) {
  const { api } = useAuth();
  const { t, language } = useI18n();
  const [status, setStatus] = useState<EmailDeliveryStatus | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [sent, setSent] = useState(false);
  const [verifyAll, setVerifyAll] = useState(false);
  const [queued, setQueued] = useState<number | null>(null);
  const [cursor, setCursor] = useState<string | null>(null);
  const [cooldown, setCooldown] = useState<{ seconds: number; round: number }>({ seconds: 0, round: 0 });
  const left = useCountdown(cooldown.seconds, cooldown.round);
  const test = useAction();
  const bulk = useAction();
  useEffect(() => {
    let active = true;
    setError(null);
    api.emailDeliveryStatus().then((result) => { if (active) setStatus(result); }, (failure: unknown) => { if (active) setError(failure); });
    return () => { active = false; };
  }, [api, revision]);
  return <section className="section" aria-labelledby="email-delivery-title">
    <h2 id="email-delivery-title" className="section__title">{t("E-mail delivery")}</h2>
    {error !== null && <ErrorNotice error={error} />}
    {status !== null && <>
      <dl className="facts">
        <dt>{t("Status")}</dt><dd>{status.configured ? t("Configured") : t("Not configured")}</dd>
        <dt>{t("Last successful delivery")}</dt><dd>{status.last_success_at === null ? t("None") : formatDateTime(language, status.last_success_at)}</dd>
        <dt>{t("Failed messages")}</dt><dd>{status.failed_count}</dd>
        <dt>{t("Suppressed security notices")}</dt><dd>{status.suppressed_count}</dd>
      </dl>
      {status.configured && <div className="stack">
        <p className="muted">{t("Test messages are sent only to your own address.")}</p>
        <div className="form__actions">
          <Button className="button" isDisabled={test.pending} onPress={() => {
            setSent(false);
            void test.run(async () => { await api.sendTestEmail(); return true; }).then((done) => { if (done) setSent(true); });
          }}>{t("Send test email")}</Button>
          <Button className="button" isDisabled={left > 0} onPress={() => setVerifyAll(true)}>{cursor === null ? t("Verify all unverified accounts…") : t("Continue sending confirmation links…")}</Button>
        </div>
        {test.failed && <ErrorNotice error={test.error} />}
        {sent && <Alert tone="success">{t("Test email queued. Check your inbox and refresh the delivery status.")}</Alert>}
        {queued !== null && <Alert tone="success">{t("Confirmation emails queued: {count}.", { count: queued })}</Alert>}
        {left > 0 && <p className="muted">{t("Send again in {seconds} s", { seconds: left })}</p>}
      </div>}
    </>}
    {verifyAll && <ConfirmDialog title={t("Verify all unverified accounts")} message={t("Send confirmation links to unverified accounts. Rate limits still apply.")} confirmLabel={t("Send confirmation links")} busy={bulk.pending} error={bulk.failed ? bulk.error : null} onClose={() => { setVerifyAll(false); bulk.clear(); }} onConfirm={() => {
      void bulk.run(() => api.requestAllEmailVerifications(cursor ?? undefined)).then((result) => {
        if (result !== undefined) {
          setQueued(result.queued);
          setCursor(result.next_cursor);
          setCooldown((value) => ({ seconds: result.limited ? result.retry_after_seconds ?? 3600 : 0, round: value.round + 1 }));
          setVerifyAll(false);
        }
      });
    }} />}
  </section>;
}
