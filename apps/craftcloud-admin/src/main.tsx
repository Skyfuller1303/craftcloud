import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App, takeInviteToken, takeResetToken, takeEmailToken, takeOidcPending } from "./App.tsx";
import { createApiClient } from "./api/client.ts";
import { I18nProvider } from "./i18n/index.tsx";
import { applyTheme, readTheme } from "./preferences.ts";
import "./styles/main.scss";

applyTheme(readTheme());
const inviteToken = takeInviteToken(window.location, window.history, import.meta.env.BASE_URL);
const resetToken = takeResetToken(window.location, window.history, import.meta.env.BASE_URL);
const emailToken = takeEmailToken(window.location, window.history, import.meta.env.BASE_URL);
const oidcPending = takeOidcPending(window.location, window.history, import.meta.env.BASE_URL);

const container = document.getElementById("root");
if (container === null) {
  throw new Error("missing #root element");
}

createRoot(container).render(
  <StrictMode>
    <I18nProvider>
      <App
        inviteToken={inviteToken}
        resetToken={resetToken}
        emailToken={emailToken}
        oidcPending={oidcPending}
        createClient={(onUnauthenticated) => createApiClient({ onUnauthenticated })}
      />
    </I18nProvider>
  </StrictMode>,
);
