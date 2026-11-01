import { useRef, useState } from "react";
import type { FormEvent } from "react";
import { Button, Form } from "react-aria-components";
import { isApiError } from "../api/client.ts";
import { useAuth } from "../auth.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { Alert, Field, ScreenHeading, focusFirstInvalid } from "../components/forms.tsx";
import { useAction } from "../hooks.ts";
import { useI18n } from "../i18n/index.tsx";

const MIN_PASSWORD = 12;
const MAX_PASSWORD = 128;

/**
 * Sets a new password with the one-time token from a reset link. The token was
 * read from the URL fragment and removed from the address bar before the first
 * request; it only lives in memory and is dropped after success.
 */
export function ResetPasswordScreen({
  token,
  onFinished,
}: {
  token: string;
  /** Leaves the screen (success or "back to log in"). */
  onFinished: () => void;
}) {
  const { t } = useI18n();
  const { api } = useAuth();
  const [password, setPassword] = useState("");
  const [repeat, setRepeat] = useState("");
  const [errors, setErrors] = useState<{ password?: string; repeat?: string }>({});
  const [done, setDone] = useState(false);
  const [tokenRejected, setTokenRejected] = useState(false);
  const action = useAction();
  const form = useRef<HTMLFormElement>(null);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (action.pending) return;
    const length = [...password].length;
    const next: { password?: string; repeat?: string } = {};
    if (length < MIN_PASSWORD || length > MAX_PASSWORD) {
      next.password = t("The password needs {min} to {max} characters.", {
        min: MIN_PASSWORD,
        max: MAX_PASSWORD,
      });
    }
    if (repeat !== password) {
      next.repeat = t("The two passwords are not the same.");
    }
    setErrors(next);
    if (next.password !== undefined || next.repeat !== undefined) {
      queueMicrotask(() => focusFirstInvalid(form.current));
      return;
    }
    const result = await action.run(async () => {
      try {
        await api.resetPassword({ token, new_password: password });
        return true;
      } catch (failure) {
        if (isApiError(failure) && failure.code === "INVALID_RESET_TOKEN") {
          setTokenRejected(true);
        } else if (isApiError(failure) && failure.fields.includes("new_password")) {
          setErrors({ password: t("This password is not accepted. Choose a longer or less common one.") });
          queueMicrotask(() => focusFirstInvalid(form.current));
        }
        throw failure;
      }
    });
    setPassword("");
    setRepeat("");
    if (result === undefined) {
      return;
    }
    setDone(true);
  }

  if (done) {
    return (
      <div className="screen screen--narrow">
        <ScreenHeading>{t("Password changed")}</ScreenHeading>
        <Alert tone="success">{t("Your password was changed. Log in with the new password.")}</Alert>
        <Button className="button button--primary" onPress={onFinished}>
          {t("Go to log in")}
        </Button>
      </div>
    );
  }

  return (
    <div className="screen screen--narrow">
      <ScreenHeading>{t("Choose a new password")}</ScreenHeading>
      {tokenRejected ? (
        <>
          <Alert>{t("This reset link is not valid any more. Ask for a new one.")}</Alert>
          <Button className="button button--primary" onPress={onFinished}>
            {t("Go to log in")}
          </Button>
        </>
      ) : (
        <Form className="form" onSubmit={(event) => void submit(event)} ref={form} validationBehavior="aria">
          {action.failed && errors.password === undefined && <ErrorNotice error={action.error} />}
          <Field
            label={t("New password")}
            name="new-password"
            type="password"
            autoComplete="new-password"
            value={password}
            onChange={setPassword}
            error={errors.password}
          />
          <Field
            label={t("Repeat the new password")}
            name="repeat-password"
            type="password"
            autoComplete="new-password"
            value={repeat}
            onChange={setRepeat}
            error={errors.repeat}
          />
          <p className="muted">{t("All other sessions of this account end when the password is set.")}</p>
          <p className="muted">{t("Company sign-in links will be disconnected. Link them again after signing in with your new password.")}</p>
          <Button type="submit" className="button button--primary" isDisabled={action.pending}>
            {action.pending ? t("Saving…") : t("Set password")}
          </Button>
        </Form>
      )}
    </div>
  );
}
