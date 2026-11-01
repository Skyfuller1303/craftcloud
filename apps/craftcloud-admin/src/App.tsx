import { lazy, useEffect, useRef, useState } from "react";
import { Button } from "react-aria-components";
import { describeError } from "./api/errors.ts";
import { isApiError } from "./api/client.ts";
import type { ApiClient } from "./api/client.ts";
import { AuthProvider, useAuth } from "./auth.tsx";
import { AnnouncerProvider } from "./components/Announcer.tsx";
import { Alert, Field, ScreenHeading } from "./components/forms.tsx";
import { Preferences } from "./components/Preferences.tsx";
import { useI18n } from "./i18n/index.tsx";
import { readTheme, saveTheme } from "./preferences.ts";
import type { ThemeChoice } from "./preferences.ts";
import { InviteScreen } from "./screens/InviteScreen.tsx";
import { LoginScreen } from "./screens/LoginScreen.tsx";
import { ResetPasswordScreen } from "./screens/ResetPasswordScreen.tsx";
import { SetupScreen } from "./screens/SetupScreen.tsx";
import { EmailConfirmationScreen } from "./screens/EmailAccount.tsx";
import { ScreenBoundary } from "./components/ScreenBoundary.tsx";

// The signed-in dashboard is loaded on demand; login, setup and the token
// screens stay in the entry chunk.
const Shell = lazy(() => import("./screens/Shell.tsx").then((m) => ({ default: m.Shell })));

const INVITE_PATH = /\/invite\/?$/;
const RESET_PATH = /\/reset-password\/?$/;
export type EmailToken = { token: string; kind: "verify" | "change" };
export function takeOidcPending(location: Pick<Location, "pathname" | "hash">, history: Pick<History, "replaceState">, baseUrl: string): string | null {
  if (!location.hash.startsWith("#oidc=")) return null;
  const value = location.hash.slice(6);
  history.replaceState(null, "", baseUrl);
  return value !== "" && value.length <= 512 ? value : null;
}

export function takeEmailToken(location: Pick<Location, "pathname" | "hash">, history: Pick<History, "replaceState">, baseUrl: string): EmailToken | null {
  const kind = /\/verify-email\/?$/.test(location.pathname) ? "verify" : /\/change-email\/?$/.test(location.pathname) ? "change" : null;
  if (kind === null) return null;
  const token = takeFragmentToken(/\/(?:verify|change)-email\/?$/, location, history, baseUrl);
  return token === null ? null : { token, kind };
}

function takeFragmentToken(
  path: RegExp,
  location: Pick<Location, "pathname" | "hash">,
  history: Pick<History, "replaceState">,
  baseUrl: string,
): string | null {
  if (!path.test(location.pathname)) {
    return null;
  }
  const token = new URLSearchParams(location.hash.replace(/^#/, "")).get("token");
  history.replaceState(null, "", baseUrl);
  return token !== null && token !== "" ? token : null;
}

/**
 * Reads the invitation token from the URL fragment (`/admin/invite#token=...`)
 * and removes it from the address bar and history entry at once. The token
 * therefore never reaches the server, the Referer header or a bookmark. Call
 * it exactly once, before the first render.
 */
export function takeInviteToken(
  location: Pick<Location, "pathname" | "hash">,
  history: Pick<History, "replaceState">,
  baseUrl: string,
): string | null {
  return takeFragmentToken(INVITE_PATH, location, history, baseUrl);
}

/**
 * Same for the password reset link (`/admin/reset-password#token=...`). The
 * address is replaced before any request is made.
 */
export function takeResetToken(
  location: Pick<Location, "pathname" | "hash">,
  history: Pick<History, "replaceState">,
  baseUrl: string,
): string | null {
  return takeFragmentToken(RESET_PATH, location, history, baseUrl);
}

export function App({
  createClient,
  inviteToken: initialInviteToken = null,
  resetToken: initialResetToken = null,
  emailToken: initialEmailToken = null,
  oidcPending: initialOidcPending = null,
}: {
  createClient: (onUnauthenticated: () => void) => ApiClient;
  inviteToken?: string | null;
  resetToken?: string | null;
  emailToken?: EmailToken | null;
  oidcPending?: string | null;
}) {
  return (
    <AnnouncerProvider>
      <AuthProvider createClient={createClient}>
        <Screens initialInviteToken={initialInviteToken} initialResetToken={initialResetToken} initialEmailToken={initialEmailToken} initialOidcPending={initialOidcPending} />
      </AuthProvider>
    </AnnouncerProvider>
  );
}

function Screens({
  initialInviteToken,
  initialResetToken,
  initialEmailToken,
  initialOidcPending,
}: {
  initialInviteToken: string | null;
  initialResetToken: string | null;
  initialEmailToken: EmailToken | null;
  initialOidcPending: string | null;
}) {
  const { t } = useI18n();
  const { state, refresh } = useAuth();
  const [theme, setTheme] = useState<ThemeChoice>(readTheme);
  const [inviteToken, setInviteToken] = useState(initialInviteToken);
  const [resetToken, setResetToken] = useState(initialResetToken);
  const [emailToken, setEmailToken] = useState(initialEmailToken);
  const [oidcPending, setOidcPending] = useState(initialOidcPending);
  const [wantsLogin, setWantsLogin] = useState(false);
  const [loginNotice, setLoginNotice] = useState<string | undefined>(undefined);
  const [shellNotice, setShellNotice] = useState<string | undefined>(undefined);
  const [joinedOrganization, setJoinedOrganization] = useState<string | null>(null);

  function changeTheme(choice: ThemeChoice) {
    saveTheme(choice);
    setTheme(choice);
  }

  let content;
  if (state.phase === "loading") {
    content = (
      <div className="screen screen--narrow">
        <ScreenHeading>{t("CraftCloud admin")}</ScreenHeading>
        <p role="status" className="muted">
          {t("Loading…")}
        </p>
      </div>
    );
  } else if (state.phase === "unavailable") {
    content = (
      <div className="screen screen--narrow">
        <ScreenHeading>{t("The server is not available")}</ScreenHeading>
        <Alert>
          {state.error === null
            ? t("The server cannot be reached. Check the connection and try again.")
            : describeError(t, state.error)}
        </Alert>
        <Button className="button" onPress={() => void refresh()}>
          {t("Try again")}
        </Button>
      </div>
    );
  } else if (state.phase === "setup") {
    content = (
      <SetupScreen onDone={() => setLoginNotice(t("Setup is complete. Log in with the new operator account."))} />
    );
  } else if (emailToken !== null) {
    content = <EmailConfirmationScreen token={emailToken.token} kind={emailToken.kind} onFinished={() => {
      setEmailToken(null);
      setLoginNotice(t("E-mail address confirmed."));
      setShellNotice(t("E-mail address confirmed."));
      void refresh();
    }} />;
  } else if (resetToken !== null) {
    content = (
      <ResetPasswordScreen
        token={resetToken}
        onFinished={() => {
          setResetToken(null);
          setLoginNotice(t("Your password was changed. Log in with the new password."));
          void refresh();
        }}
      />
    );
  } else if (oidcPending !== null) {
    content = <OidcFinish token={oidcPending} onFinished={() => { setOidcPending(null); void refresh(); }} />;
  } else if (inviteToken !== null && (state.phase === "authenticated" || !wantsLogin)) {
    content = (
      <InviteScreen
        token={inviteToken}
        onHaveAccount={() => setWantsLogin(true)}
        onCancel={() => {
          setInviteToken(null);
          setWantsLogin(false);
        }}
        onAccepted={(result) => {
          setInviteToken(null);
          setWantsLogin(false);
          setJoinedOrganization(result.organization_id);
          setShellNotice(t("Invitation accepted."));
        }}
      />
    );
  } else if (state.phase === "anonymous") {
    content = <LoginScreen notice={loginNotice} invitePending={inviteToken !== null} />;
  } else {
    content = (
      <ScreenBoundary>
        <Shell
          session={state.session}
          notice={shellNotice}
          preferredOrganizationId={joinedOrganization}
          theme={theme}
          onThemeChange={changeTheme}
        />
      </ScreenBoundary>
    );
  }

  const framed = state.phase !== "authenticated" || inviteToken !== null || resetToken !== null || emailToken !== null || oidcPending !== null;
  return (
    <>
      <a className="skip-link" href="#main">
        {t("Skip to main content")}
      </a>
      {framed ? (
        <div className="frame">
          <header className="frame__bar">
            <span className="shell__brand">
              <span className="visually-hidden">{t("CraftCloud admin")}</span>
              <span aria-hidden="true" className="shell__brand-tag">CRAFTCLOUD</span>
              <span aria-hidden="true" className="shell__brand-label">ADMIN</span>
            </span>
            <Preferences theme={theme} onThemeChange={changeTheme} />
          </header>
          <main id="main" className="frame__main" tabIndex={-1}>
            {content}
          </main>
        </div>
      ) : (
        content
      )}
    </>
  );
}

class InvalidNativeOidcDestination extends Error {}

function OidcFinish({ token, onFinished }: { token: string; onFinished: () => void }) {
  const { api } = useAuth(); const { t } = useI18n();
  const [password, setPassword] = useState(""); const [factor, setFactor] = useState(""); const [mode, setMode] = useState<"password" | "code" | "recovery_code">("password");
  const [failure, setFailure] = useState<unknown>(null); const [busy, setBusy] = useState(false);
  const attempted = useRef(false);
  async function finish() {
    if (busy) return; setBusy(true); setFailure(null);
    try {
      const result = await api.finishOidc({ pending_id: token, ...(password ? { password } : {}), ...(factor ? mode === "code" ? { code: factor } : mode === "recovery_code" ? { recovery_code: factor } : {} : {}) });
      if ("redirect_url" in result && typeof result.redirect_url === "string") {
        const target = new URL(result.redirect_url);
        const loopback = target.hostname === "127.0.0.1" || target.hostname === "[::1]";
        const port = Number(target.port);
        const params = [...target.searchParams];
        if (target.protocol !== "http:" || target.hash !== "" || !loopback || target.username !== "" || target.password !== "" || target.pathname !== "/oidc/callback" || !Number.isInteger(port) || port < 1 || port > 65535 || params.length !== 2 || new Set(params.map(([key]) => key)).size !== 2 || !target.searchParams.has("code") || !target.searchParams.has("state") || params.some(([key, value]) => !["code", "state"].includes(key) || value.length === 0 || value.length > 2048)) throw new InvalidNativeOidcDestination();
        window.location.assign(target.href);
        return;
      }
      onFinished(); setPassword(""); setFactor("");
    }
    catch (error) {
      setFailure(error);
      if (isApiError(error) && error.code === "SSO_LINK_REQUIRED") setMode("password");
      else if (isApiError(error) && error.code === "SSO_SECOND_FACTOR_REQUIRED") setMode("code");
      setBusy(false);
    }
  }
  useEffect(() => { if (attempted.current) return; attempted.current = true; void finish(); }, []);
  const needsPassword = mode === "password" || (isApiError(failure) && failure.code === "SSO_LINK_REQUIRED");
  const setupRequired = isApiError(failure) && failure.code === "SSO_TWO_FACTOR_SETUP_REQUIRED";
  return <div className="screen screen--narrow"><ScreenHeading>{t("Finish organization sign-in")}</ScreenHeading>
    {failure !== null && <Alert>{failure instanceof InvalidNativeOidcDestination ? t("Invalid native sign-in destination.") : describeError(t, failure)}</Alert>}
    {setupRequired && <Alert tone="info">{t("Your organization requires a second factor. Contact your organization owner or administrator to restore local access and set up multi-factor authentication.")}</Alert>}
    <p>{t("If requested, confirm your local account or second factor.")}</p>
    {needsPassword && <Alert tone="info">{t("Link accounts: this company identity will be able to sign in to your CraftCloud account. Continue only if you trust this organization’s identity provider.")}</Alert>}
    {needsPassword && <Field label={t("Password")} name="oidc-link-password" type="password" value={password} onChange={setPassword} required={false} />}
    {mode !== "password" && <Field label={mode === "code" ? t("Authentication code") : t("Recovery code")} name="oidc-factor" type="text" value={factor} onChange={setFactor} required={false} />}
    <Button className="button button--primary" isDisabled={busy || setupRequired} onPress={() => void finish()}>{t("Continue")}</Button>
    {mode !== "password" && <Button className="button" onPress={() => { setMode(mode === "code" ? "recovery_code" : "code"); setFactor(""); }}>{mode === "code" ? t("Use a recovery code") : t("Use an authentication code")}</Button>}
  </div>;
}
