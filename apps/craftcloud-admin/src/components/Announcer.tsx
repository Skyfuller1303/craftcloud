import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from "react";
import type { ReactNode } from "react";
import { Toaster } from "./Toaster.tsx";
import type { ToastMessage } from "./Toaster.tsx";

type Announce = (message: string, options?: { toast?: boolean }) => void;

const AnnouncerContext = createContext<Announce>(() => undefined);

/**
 * One polite live region for the whole app. Successful actions, loaded lists
 * and countdown milestones are announced through it, so screen reader users
 * learn about results that only change the page visually. Errors use
 * `role="alert"` on their own element instead.
 */
export function AnnouncerProvider({ children }: { children: ReactNode }) {
  const [message, setMessage] = useState("");
  const [toasts, setToasts] = useState<ToastMessage[]>([]);
  const nextToastId = useRef(0);
  const timer = useRef<number | undefined>(undefined);

  const announce = useCallback<Announce>((text, options) => {
    // Clear first so that an identical message is announced again.
    setMessage("");
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setMessage(text), 60);
    if (options?.toast !== false) {
      const toast = { id: ++nextToastId.current, message: text };
      setToasts((current) => [...current, toast].slice(-4));
    }
  }, []);

  useEffect(() => () => window.clearTimeout(timer.current), []);

  const value = useMemo(() => announce, [announce]);
  return (
    <AnnouncerContext.Provider value={value}>
      {children}
      <div className="visually-hidden" aria-live="polite" aria-atomic="true" data-testid="announcer">
        {message}
      </div>
      <Toaster toasts={toasts} onDismiss={(id) => setToasts((current) => current.filter((toast) => toast.id !== id))} />
    </AnnouncerContext.Provider>
  );
}

export function useAnnounce(): Announce {
  return useContext(AnnouncerContext);
}
