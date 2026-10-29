import { useRef, useState } from "react";
import type { FormEvent } from "react";
import { Button, Form } from "react-aria-components";
import { isApiError } from "../api/client.ts";
import { describeError } from "../api/errors.ts";
import type { AcceptedInvitation } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { Alert, Field, ScreenHeading, focusFirstInvalid } from "../components/forms.tsx";
import { useI18n } from "../i18n/index.tsx";

const MIN_PASSWORD = 12;
const MAX_PASSWORD = 128;

/**
 * Accepting an invitation. The token was read from the URL fragment and
 * removed from the address bar by the caller; it only lives in memory.
 */
export function InviteScreen({
  token,
  onAccepted,
  onCancel,
  onHaveAccount,
}: {
  token: string;
  onAccepted: (result: AcceptedInvitation) => void;
  onCancel: () => void;
  onHaveAccount: () => void;
}) {
  const { t } = useI18n();
  const { api, state, refresh } = useAuth();
  const loggedIn = state.phase === "authenticated";
  const [displayName, setDisplayName] = useState("");
  const [password, setPassword] = useState("");
  const [errors, setErrors] = useState<{ name?: string; password?: string }>({});
  const [failure, setFailure] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const form = useRef<HTMLFormElement>(null);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) {
      return;
    }
    setFailure(null);
    if (!loggedIn) {
      const next: { name?: string; password?: string } = {};
      if (displayName.trim() === "" || displayName.length > 100) {
        next.name = t("Enter a name of 1 to 100 characters.");
      }
      const length = [...password].length;
      if (length < MIN_PASSWORD || length > MAX_PASSWORD) {
        next.password = t("The password needs {min} to {max} characters.", {
          min: MIN_PASSWORD,
          max: MAX_PASSWORD,
        });
      }
      setErrors(next);
      if (next.name !== undefined || next.password !== undefined) {
        queueMicrotask(() => focusFirstInvalid(form.current));
        return;
      }
    }
    setBusy(true);
    try {
      const result = await api.acceptInvitation(
        loggedIn ? { token } : { token, display_name: displayName.trim(), password },
      );
      setPassword("");
      onAccepted(result);
      await refresh();
    } catch (error) {
      setPassword("");
      setBusy(false);
      if (isApiError(error) && error.code === "VALIDATION_ERROR" && error.fields.length > 0) {
        setErrors({
          ...(error.fields.includes("display_name")
            ? { name: t("This value is not accepted.") }
            : {}),
          ...(error.fields.includes("password") ? { password: t("This value is not accepted.") } : {}),
        });
        queueMicrotask(() => focusFirstInvalid(form.current));
      }
      setFailure(
        describeError(t, error, {
          INVALID_INVITATION: t("This invitation link is not valid. It may have expired, been used or been withdrawn."),
          LOGIN_REQUIRED: t("An account exists for the invited address. Log in with it first."),
          INVITATION_EMAIL_MISMATCH: t("You are logged in with a different account than the one invited."),
          ALREADY_MEMBER: t("You are already a member of this organization."),
        }),
      );
    }
  }

  return (
    <div className="screen screen--narrow">
      <ScreenHeading>{t("Accept invitation")}</ScreenHeading>
      <p className="screen__lead">
        {loggedIn
          ? t("You are invited to join an organization. Accept to continue with your current account.")
          : t("You are invited to join an organization. Create your account to accept.")}
      </p>
      {failure !== null && <Alert>{failure}</Alert>}
      <Form className="form" onSubmit={submit} ref={form} validationBehavior="aria">
        {!loggedIn && (
          <>
            <Field
              label={t("Your name")}
              name="display_name"
              autoComplete="name"
              value={displayName}
              onChange={setDisplayName}
              error={errors.name}
            />
            <Field
              label={t("Password")}
              name="password"
              type="password"
              autoComplete="new-password"
              description={t("At least {min} characters.", { min: MIN_PASSWORD })}
              value={password}
              onChange={setPassword}
              error={errors.password}
            />
          </>
        )}
        <div className="form__actions">
          <Button type="submit" className="button button--primary" isDisabled={busy}>
            {busy ? t("Accepting…") : t("Accept invitation")}
          </Button>
          {!loggedIn && (
            <Button type="button" className="button" onPress={onHaveAccount}>
              {t("I already have an account")}
            </Button>
          )}
          <Button type="button" className="button button--quiet" onPress={onCancel}>
            {t("Discard invitation")}
          </Button>
        </div>
      </Form>
    </div>
  );
}
