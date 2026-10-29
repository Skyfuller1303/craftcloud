import { useRef, useState } from "react";
import type { FormEvent } from "react";
import { Button, Form } from "react-aria-components";
import { describeError } from "../api/errors.ts";
import { useAuth } from "../auth.tsx";
import { Alert, Field, ScreenHeading, focusFirstInvalid } from "../components/forms.tsx";
import { useI18n } from "../i18n/index.tsx";

export function LoginScreen({
  notice,
  invitePending = false,
}: {
  notice?: string | undefined;
  /** An invitation link is waiting; logging in continues with accepting it. */
  invitePending?: boolean;
}) {
  const { t } = useI18n();
  const { api, refresh, state } = useAuth();
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [errors, setErrors] = useState<{ email?: string; password?: string }>({});
  const [failure, setFailure] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const form = useRef<HTMLFormElement>(null);
  const sessionEnded = state.phase === "anonymous" && state.sessionEnded;

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) {
      return;
    }
    const next: { email?: string; password?: string } = {};
    if (email.trim() === "") {
      next.email = t("Enter your e-mail address.");
    }
    if (password === "") {
      next.password = t("Enter your password.");
    }
    setErrors(next);
    setFailure(null);
    if (next.email !== undefined || next.password !== undefined) {
      queueMicrotask(() => focusFirstInvalid(form.current));
      return;
    }
    setBusy(true);
    try {
      await api.login({ email: email.trim(), password });
      setPassword("");
      await refresh();
    } catch (error) {
      setFailure(
        describeError(t, error, {
          INVALID_CREDENTIALS: t("E-mail address or password is incorrect."),
        }),
      );
      setPassword("");
      setBusy(false);
    }
  }

  return (
    <div className="screen screen--narrow">
      <ScreenHeading>{t("Log in")}</ScreenHeading>
      {sessionEnded && <Alert tone="info">{t("Your session has ended. Log in again.")}</Alert>}
      {notice !== undefined && <Alert tone="success">{notice}</Alert>}
      {invitePending && (
        <Alert tone="info">{t("Log in with the invited account to accept the invitation.")}</Alert>
      )}
      {failure !== null && <Alert>{failure}</Alert>}
      <Form className="form" onSubmit={submit} ref={form} validationBehavior="aria">
        <Field
          label={t("E-mail address")}
          name="email"
          type="email"
          autoComplete="username"
          value={email}
          onChange={setEmail}
          error={errors.email}
        />
        <Field
          label={t("Password")}
          name="password"
          type="password"
          autoComplete="current-password"
          value={password}
          onChange={setPassword}
          error={errors.password}
        />
        <Button type="submit" className="button button--primary" isDisabled={busy}>
          {busy ? t("Logging in…") : t("Log in")}
        </Button>
      </Form>
    </div>
  );
}
