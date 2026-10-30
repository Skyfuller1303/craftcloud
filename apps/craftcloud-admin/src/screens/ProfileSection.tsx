import { useEffect, useRef, useState } from "react";
import type { FormEvent } from "react";
import { Button, Form } from "react-aria-components";
import { isApiError } from "../api/client.ts";
import type { SessionInfo } from "../api/types.ts";
import { useAuth } from "../auth.tsx";
import { useAnnounce } from "../components/Announcer.tsx";
import { ErrorNotice } from "../components/ErrorNotice.tsx";
import { Field, focusFirstInvalid } from "../components/forms.tsx";
import { useI18n } from "../i18n/index.tsx";

const NAME_MAX_CHARS = 100;

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
      <p className="muted">{t("Your e-mail address can only be changed by an instance operator for now.")}</p>
      <nav aria-label={t("Account security")}>
        <ul className="link-list">
          <li><a href="#two-factor-title">{t("Two-factor authentication")}</a></li>
          <li><a href="#devices-title">{t("Devices")}</a></li>
        </ul>
      </nav>
    </section>
  );
}
