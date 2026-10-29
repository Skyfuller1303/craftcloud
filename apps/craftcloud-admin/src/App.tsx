import { useState } from "react";
import { Button } from "react-aria-components";
import { describeError } from "./api/errors.ts";
import type { ApiClient } from "./api/client.ts";
import { AuthProvider, useAuth } from "./auth.tsx";
import { Alert, ScreenHeading } from "./components/forms.tsx";
import { Preferences } from "./components/Preferences.tsx";
import { useI18n } from "./i18n/index.tsx";
import { readTheme, saveTheme } from "./preferences.ts";
import type { ThemeChoice } from "./preferences.ts";
import { InviteScreen } from "./screens/InviteScreen.tsx";
import { LoginScreen } from "./screens/LoginScreen.tsx";
import { SetupScreen } from "./screens/SetupScreen.tsx";
import { Shell } from "./screens/Shell.tsx";

const INVITE_PATH = /\/invite\/?$/;

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
  if (!INVITE_PATH.test(location.pathname)) {
    return null;
  }
  const token = new URLSearchParams(location.hash.replace(/^#/, "")).get("token");
  history.replaceState(null, "", baseUrl);
  return token !== null && token !== "" ? token : null;
}

export function App({
  createClient,
  inviteToken: initialInviteToken = null,
}: {
  createClient: (onUnauthenticated: () => void) => ApiClient;
  inviteToken?: string | null;
}) {
  return (
    <AuthProvider createClient={createClient}>
      <Screens initialInviteToken={initialInviteToken} />
    </AuthProvider>
  );
}

function Screens({ initialInviteToken }: { initialInviteToken: string | null }) {
  const { t } = useI18n();
  const { state, refresh } = useAuth();
  const [theme, setTheme] = useState<ThemeChoice>(readTheme);
  const [inviteToken, setInviteToken] = useState(initialInviteToken);
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
      <Shell
        session={state.session}
        notice={shellNotice}
        preferredOrganizationId={joinedOrganization}
        theme={theme}
        onThemeChange={changeTheme}
      />
    );
  }

  const framed = state.phase !== "authenticated" || inviteToken !== null;
  return (
    <>
      <a className="skip-link" href="#main">
        {t("Skip to main content")}
      </a>
      {framed ? (
        <div className="frame">
          <header className="frame__bar">
            <span className="shell__brand">{t("CraftCloud admin")}</span>
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
