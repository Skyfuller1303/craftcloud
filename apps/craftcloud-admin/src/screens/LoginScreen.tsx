import { useRef, useState } from "react";
import type { FormEvent } from "react";
import { Button, Form } from "react-aria-components";
import { describeError } from "../api/errors.ts";
import { isApiError } from "../api/client.ts";
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
  const [challengeId, setChallengeId] = useState<string | null>(null);
  const [recoveryMode, setRecoveryMode] = useState(false);
  const [secondFactor, setSecondFactor] = useState("");
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
      if (isApiError(error) && error.code === "SECOND_FACTOR_REQUIRED") {
        const id = error.details?.["challenge_id"];
        if (typeof id === "string" && id !== "") {
          setChallengeId(id);
          setPassword("");
          setBusy(false);
          return;
        }
      }
      setFailure(
        describeError(t, error, {
          INVALID_CREDENTIALS: t("E-mail address or password is incorrect."),
        }),
      );
      setPassword("");
      setBusy(false);
    }
  }

  async function submitSecondFactor(event: FormEvent) {
    event.preventDefault();
    if (busy || challengeId === null || secondFactor.trim() === "") return;
    setFailure(null);
    setBusy(true);
    try {
      await api.loginSecondFactor({
        challenge_id: challengeId,
        ...(recoveryMode ? { recovery_code: secondFactor } : { code: secondFactor }),
      });
      setChallengeId(null);
      setSecondFactor("");
      await refresh();
    } catch (error) {
      setFailure(
        isApiError(error) && error.code === "INVALID_CREDENTIALS"
          ? t("The authentication code or recovery code is incorrect.")
          : describeError(t, error),
      );
      setSecondFactor("");
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
      {challengeId !== null ? (
        <Form className="form" onSubmit={(event) => void submitSecondFactor(event)}>
          <p>{t("Enter a code from your authenticator app to finish signing in.")}</p>
          <Field
            label={recoveryMode ? t("Recovery code") : t("Authentication code")}
            name="second-factor-code"
            type="text"
            autoComplete="one-time-code"
            value={secondFactor}
            onChange={setSecondFactor}
          />
          <Button type="submit" className="button button--primary" isDisabled={busy || secondFactor.trim() === ""}>
            {busy ? t("Checking…") : t("Verify and log in")}
          </Button>
          <Button type="button" className="button button--link" onPress={() => setRecoveryMode((value) => !value)}>
            {recoveryMode ? t("Use an authentication code") : t("Use a recovery code")}
          </Button>
          <Button type="button" className="button button--link" onPress={() => {
            setChallengeId(null);
            setSecondFactor("");
            setRecoveryMode(false);
          }}>
            {t("Back to password")}
          </Button>
        </Form>
      ) : (
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
      )}
    </div>
  );
}
