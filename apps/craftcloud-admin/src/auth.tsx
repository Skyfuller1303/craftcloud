import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import type { ReactNode } from "react";
import { isApiError } from "./api/client.ts";
import type { ApiClient, ApiError } from "./api/client.ts";
import type { SessionInfo } from "./api/types.ts";

export type AuthState =
  | { phase: "loading" }
  | { phase: "unavailable"; error: ApiError | null }
  | { phase: "setup" }
  | { phase: "anonymous"; sessionEnded: boolean }
  | { phase: "authenticated"; session: SessionInfo };

interface AuthValue {
  state: AuthState;
  api: ApiClient;
  /** Re-reads setup status and session, for example after login. */
  refresh: () => Promise<void>;
  /**
   * Ends the session. Resolves only after the server confirmed it (or said the
   * session was already gone); on failure it rejects and the state stays
   * `authenticated`, so the caller can show the error and offer a retry.
   */
  logout: () => Promise<void>;
  /** Replaces the signed-in session data, for example after a profile change. */
  replaceSession: (session: SessionInfo) => void;
}

const AuthContext = createContext<AuthValue | null>(null);

/**
 * Owns the one `ApiClient` and the authentication phase. The client is
 * created by the caller so tests and `main.tsx` decide on `fetch`; it must call
 * the provided `onUnauthenticated` hook via {@link AuthProvider}'s `signal`.
 */
export function AuthProvider({
  children,
  createClient,
}: {
  children: ReactNode;
  createClient: (onUnauthenticated: () => void) => ApiClient;
}) {
  const [state, setState] = useState<AuthState>({ phase: "loading" });
  const clientRef = useRef<ApiClient | null>(null);
  if (clientRef.current === null) {
    clientRef.current = createClient(() =>
      setState((current) =>
        current.phase === "authenticated" ? { phase: "anonymous", sessionEnded: true } : current,
      ),
    );
  }
  const api = clientRef.current;

  const refresh = useCallback(async () => {
    try {
      const status = await api.setupStatus();
      if (status.setup_required) {
        setState({ phase: "setup" });
        return;
      }
      try {
        const session = await api.session();
        setState({ phase: "authenticated", session });
      } catch (error) {
        if (isApiError(error) && error.status === 401) {
          setState({ phase: "anonymous", sessionEnded: false });
        } else {
          setState({ phase: "unavailable", error: isApiError(error) ? error : null });
        }
      }
    } catch (error) {
      setState({ phase: "unavailable", error: isApiError(error) ? error : null });
    }
  }, [api]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const logout = useCallback(async () => {
    await api.logout();
    setState({ phase: "anonymous", sessionEnded: false });
  }, [api]);

  const replaceSession = useCallback((session: SessionInfo) => {
    setState((current) => (current.phase === "authenticated" ? { phase: "authenticated", session } : current));
  }, []);

  const value = useMemo(
    () => ({ state, api, refresh, logout, replaceSession }),
    [state, api, refresh, logout, replaceSession],
  );
  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function useAuth(): AuthValue {
  const value = useContext(AuthContext);
  if (value === null) {
    throw new Error("useAuth needs an AuthProvider");
  }
  return value;
}
