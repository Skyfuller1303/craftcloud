import { useEffect, useRef, useState } from "react";
import type { FormEvent } from "react";
import { Button, Form } from "react-aria-components";
import { isApiError } from "../api/client.ts";
import type { SessionInfo } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { useAnnounce } from "../components/Announcer.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { Alert, Field, focusFirstInvalid } from "../components/forms.tsx";
import { useAction } from "../hooks.ts";
import { useI18n } from "../i18n/index.tsx";
import { EmailChangeForm, useEmailDelivery } from "./EmailAccount.tsx";

const NAME_MAX_CHARS = 100;
// Same limits as the server (craftcloud-auth PASSWORD_MIN_CHARS / PASSWORD_MAX_CHARS), counted in characters.
const MIN_PASSWORD = 12;
const MAX_PASSWORD = 128;

/** Returns true when `name` (already trimmed) is acceptable; the server decides again. */
function nameProblem(name: string): boolean {
  return name === "" || [...name].length > NAME_MAX_CHARS || /\p{Cc}/u.test(name);
}

/**
 * Self-service profile on the settings page: the display name can be changed
 * here; the e-mail address is shown read-only because only an instance
 * operator can change it until the server can send e-mail.
 */
export function ProfileSection({ session }: { session: SessionInfo }) {
  const emailDelivery = useEmailDelivery();
  const { t } = useI18n();
  const { api, replaceSession } = useAuth();
  const announce = useAnnounce();
  const form = useRef<HTMLFormElement>(null);
  const [name, setName] = useState(session.display_name);
  const [fieldError, setFieldError] = useState<string | undefined>(undefined);
  const [failure, setFailure] = useState<unknown>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    setName(session.display_name);
  }, [session.display_name]);

  const trimmed = name.trim();
  const unchanged = trimmed === session.display_name;

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy || unchanged) return;
    setFailure(null);
    if (nameProblem(trimmed)) {
      setFieldError(t("Enter a name of 1 to 100 characters without control characters."));
      queueMicrotask(() => focusFirstInvalid(form.current));
      return;
    }
    setFieldError(undefined);
    setBusy(true);
    try {
      const updated = await api.updateProfile(trimmed);
      replaceSession(updated);
      setName(updated.display_name);
      announce(t("Profile saved."));
    } catch (error) {
      if (isApiError(error) && error.code === "VALIDATION_ERROR" && error.fields.includes("display_name")) {
        setFieldError(t("Enter a name of 1 to 100 characters without control characters."));
        queueMicrotask(() => focusFirstInvalid(form.current));
      } else {
        setFailure(error);
      }
    } finally {
      setBusy(false);
    }
  }

  return (
    <section className="section" aria-labelledby="profile-title">
      <h2 id="profile-title" className="section__title">{t("Profile")}</h2>
      {failure !== null && <ErrorNotice error={failure} />}
      <Form ref={form} className="stack profile-form" onSubmit={(event) => void submit(event)} validationBehavior="aria">
        <Field
          label={t("Display name")}
          name="display-name"
          value={name}
          onChange={(value) => { setName(value); setFieldError(undefined); }}
          error={fieldError}
          description={t("Shown to other people in members lists, locks and activity.")}
          autoComplete="name"
        />
        <div>
          <Button type="submit" className="button button--primary" isDisabled={busy || unchanged}>
            {busy ? t("Saving…") : t("Save display name")}
          </Button>
        </div>
      </Form>
      <dl className="facts">
        <dt>{t("E-mail address")}</dt>
        <dd>{session.email}</dd>
      </dl>
      {emailDelivery ? <EmailChangeForm /> : <p className="muted">{t("Your e-mail address can only be changed by an instance operator for now.")}</p>}
      <PasswordForm />
      <nav aria-label={t("Account security")}>
        <ul className="link-list">
          <li><a href="#password-title">{t("Password")}</a></li>
          <li><a href="#two-factor-title">{t("Two-factor authentication")}</a></li>
          <li><a href="#devices-title">{t("Devices")}</a></li>
        </ul>
      </nav>
    </section>
  );
}

type PasswordErrors = { current?: string; password?: string; repeat?: string };

/**
 * Changes the own password (`POST /auth/password/change`). On success the server
 * keeps this browser session and ends every other browser session and every
 * registered device of the account, voids pending reset links and the
 * edit locks of the ended sessions.
 */
function PasswordForm() {
  const { t } = useI18n();
  const { api } = useAuth();
  const announce = useAnnounce();
  const form = useRef<HTMLFormElement>(null);
  const action = useAction();
  const [current, setCurrent] = useState("");
  const [password, setPassword] = useState("");
  const [repeat, setRepeat] = useState("");
  const [errors, setErrors] = useState<PasswordErrors>({});
  const [done, setDone] = useState(false);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (action.pending) return;
    setDone(false);
    const next: PasswordErrors = {};
    if (current === "") next.current = t("Enter your current password.");
    const length = [...password].length;
    if (length < MIN_PASSWORD || length > MAX_PASSWORD) {
      next.password = t("The password needs {min} to {max} characters.", { min: MIN_PASSWORD, max: MAX_PASSWORD });
    }
    if (repeat !== password) next.repeat = t("The two passwords are not the same.");
    setErrors(next);
    if (Object.keys(next).length > 0) {
      window.setTimeout(() => focusFirstInvalid(form.current), 0);
      return;
    }
    const result = await action.run(async () => {
      try {
        await api.changePassword({ current_password: current, new_password: password });
        return true;
      } catch (failure) {
        if (isApiError(failure) && failure.code === "INVALID_CURRENT_PASSWORD") {
          setErrors({ current: t("The current password is not correct.") });
          window.setTimeout(() => focusFirstInvalid(form.current), 0);
        } else if (isApiError(failure) && failure.fields.includes("new_password")) {
          setErrors({ password: t("This password is not accepted. Choose a longer or less common one.") });
          window.setTimeout(() => focusFirstInvalid(form.current), 0);
        }
        throw failure;
      }
    });
    setCurrent("");
    setPassword("");
    setRepeat("");
    if (result === true) {
      setErrors({});
      setDone(true);
      announce(t("Password changed"));
    }
  }

  const fieldFailure = Object.keys(errors).length > 0;
  return (
    <div className="stack password-form">
      <h3 id="password-title" className="section__subtitle">{t("Password")}</h3>
      {done && <Alert tone="success">{t("Password changed. Your other browser sessions and all registered devices were signed out; this browser stays signed in.")}</Alert>}
      {action.failed && !fieldFailure && <ErrorNotice error={action.error} />}
      <Form ref={form} className="stack" onSubmit={(event) => void submit(event)} validationBehavior="aria">
        <Field label={t("Old password")} name="current-password" type="password" autoComplete="current-password" value={current} onChange={setCurrent} error={errors.current} />
        <Field label={t("New password")} name="new-password" type="password" autoComplete="new-password" value={password} onChange={setPassword} error={errors.password} description={t("At least {min} characters.", { min: MIN_PASSWORD })} />
        <Field label={t("Repeat the new password")} name="repeat-password" type="password" autoComplete="new-password" value={repeat} onChange={setRepeat} error={errors.repeat} />
        <p className="muted">{t("Changing the password signs out your other browser sessions and all registered devices.")}</p>
        <div>
          <Button type="submit" className="button button--primary" isDisabled={action.pending}>
            {action.pending ? t("Saving…") : t("Change password")}
          </Button>
        </div>
      </Form>
    </div>
  );
}
