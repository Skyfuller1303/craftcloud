import { useRef, useState } from "react";
import type { FormEvent } from "react";
import { Button, Form } from "react-aria-components";
import { isApiError } from "../api/client.ts";
import { describeError } from "../api/errors.ts";
import { useAuth } from "../auth.tsx";
import { Alert, Field, ScreenHeading, focusFirstInvalid } from "../components/forms.tsx";
import { useI18n } from "../i18n/index.tsx";

type FieldName = "bootstrap_token" | "email" | "display_name" | "password" | "org_name" | "org_slug";
type FieldErrors = Partial<Record<FieldName, string>>;

const SLUG_PATTERN = /^[a-z0-9][a-z0-9-]{0,62}$/;
const MIN_PASSWORD = 12;
const MAX_PASSWORD = 128;
const MIN_TOKEN = 32;

/** Suggests a slug from an organization name: lowercase ASCII letters, digits, hyphens. */
export function suggestSlug(name: string): string {
  return name
    .normalize("NFKD")
    .replace(/[̀-ͯ]/g, "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 63)
    .replace(/-+$/g, "");
}

const SERVER_FIELDS: Record<string, FieldName> = {
  bootstrap_token: "bootstrap_token",
  email: "email",
  display_name: "display_name",
  password: "password",
  "organization.name": "org_name",
  "organization.slug": "org_slug",
};

export function SetupScreen({ onDone }: { onDone: () => void }) {
  const { t } = useI18n();
  const { api, refresh } = useAuth();
  const [token, setToken] = useState("");
  const [email, setEmail] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [password, setPassword] = useState("");
  const [orgName, setOrgName] = useState("");
  const [orgSlug, setOrgSlug] = useState("");
  const [slugTouched, setSlugTouched] = useState(false);
  const [errors, setErrors] = useState<FieldErrors>({});
  const [failure, setFailure] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const form = useRef<HTMLFormElement>(null);

  function validate(): FieldErrors {
    const next: FieldErrors = {};
    if (token.trim().length < MIN_TOKEN) {
      next.bootstrap_token = t("The setup token has at least {count} characters.", {
        count: MIN_TOKEN,
      });
    }
    const trimmed = email.trim();
    if (trimmed === "" || trimmed.split("@").length !== 2 || /\s/.test(trimmed)) {
      next.email = t("Enter a valid e-mail address.");
    }
    if (displayName.trim() === "" || displayName.length > 100) {
      next.display_name = t("Enter a name of 1 to 100 characters.");
    }
    if ([...password].length < MIN_PASSWORD || [...password].length > MAX_PASSWORD) {
      next.password = t("The password needs {min} to {max} characters.", {
        min: MIN_PASSWORD,
        max: MAX_PASSWORD,
      });
    }
    if (orgName.trim() === "" || orgName.length > 100) {
      next.org_name = t("Enter a name of 1 to 100 characters.");
    }
    if (!SLUG_PATTERN.test(orgSlug)) {
      next.org_slug = t("Use lowercase letters, digits and hyphens; start with a letter or digit.");
    }
    return next;
  }

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) {
      return;
    }
    const found = validate();
    setErrors(found);
    setFailure(null);
    if (Object.keys(found).length > 0) {
      queueMicrotask(() => focusFirstInvalid(form.current));
      return;
    }
    setBusy(true);
    try {
      await api.setupComplete({
        bootstrap_token: token.trim(),
        email: email.trim(),
        display_name: displayName.trim(),
        password,
        organization: { name: orgName.trim(), slug: orgSlug },
      });
      setToken("");
      setPassword("");
      onDone();
      await refresh();
    } catch (error) {
      setPassword("");
      setBusy(false);
      if (isApiError(error)) {
        if (error.code === "INVALID_BOOTSTRAP_TOKEN") {
          setErrors({ bootstrap_token: t("The setup token is not accepted.") });
          queueMicrotask(() => focusFirstInvalid(form.current));
          return;
        }
        if (error.code === "VALIDATION_ERROR" && error.fields.length > 0) {
          const mapped: FieldErrors = {};
          for (const name of error.fields) {
            const field = SERVER_FIELDS[name];
            if (field !== undefined) {
              mapped[field] = t("This value is not accepted.");
            }
          }
          setErrors(mapped);
          queueMicrotask(() => focusFirstInvalid(form.current));
        }
        if (error.code === "SETUP_ALREADY_COMPLETED") {
          await refresh();
          return;
        }
      }
      setFailure(
        describeError(t, error, {
          TIMEOUT: t("The server did not answer in time. Setup may have completed: reload the page to check."),
          SETUP_UNAVAILABLE: t("Setup is not available: the server has no reachable database."),
        }),
      );
    }
  }

  return (
    <div className="screen screen--narrow screen--setup">
      <ScreenHeading>{t("Set up CraftCloud")}</ScreenHeading>
      <p className="screen__lead">
        {t("Create the first operator account and organization. The setup token is in the secret file that the server operator configured.")}
      </p>
      {failure !== null && <Alert>{failure}</Alert>}
      <Form className="form" onSubmit={submit} ref={form} validationBehavior="aria">
        <Field
          label={t("Setup token")}
          name="bootstrap_token"
          type="password"
          autoComplete="off"
          value={token}
          onChange={setToken}
          error={errors.bootstrap_token}
        />
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
          label={t("Your name")}
          name="display_name"
          autoComplete="name"
          value={displayName}
          onChange={setDisplayName}
          error={errors.display_name}
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
        <hr className="form__rule" />
        <Field
          label={t("Organization name")}
          name="organization_name"
          value={orgName}
          onChange={(value) => {
            setOrgName(value);
            if (!slugTouched) {
              setOrgSlug(suggestSlug(value));
            }
          }}
          error={errors.org_name}
        />
        <Field
          label={t("Organization address name")}
          name="organization_slug"
          description={t("Lowercase letters, digits and hyphens. Used in links.")}
          value={orgSlug}
          onChange={(value) => {
            setSlugTouched(true);
            setOrgSlug(value);
          }}
          error={errors.org_slug}
        />
        <Button type="submit" className="button button--primary" isDisabled={busy}>
          {busy ? t("Setting up…") : t("Complete setup")}
        </Button>
      </Form>
    </div>
  );
}
