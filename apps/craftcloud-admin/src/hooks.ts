import { useCallback, useEffect, useRef, useState } from "react";

export interface Action {
  pending: boolean;
  error: unknown;
  /** True once `run` failed; cleared by the next `run` or by `clear`. */
  failed: boolean;
  /**
   * Runs `work`. Resolves with its value, or `undefined` when it failed (the
   * error is kept in `error`) or when another run is still pending.
   */
  run: <T>(work: () => Promise<T>) => Promise<T | undefined>;
  clear: () => void;
}

/** State of one user-triggered request: pending flag, last error, no double submit. */
export function useAction(): Action {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [failed, setFailed] = useState(false);
  const busy = useRef(false);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const run = useCallback(async <T,>(work: () => Promise<T>): Promise<T | undefined> => {
    if (busy.current) {
      return undefined;
    }
    busy.current = true;
    setPending(true);
    setError(null);
    setFailed(false);
    try {
      return await work();
    } catch (failure) {
      if (mounted.current) {
        setError(failure);
        setFailed(true);
      }
      return undefined;
    } finally {
      busy.current = false;
      if (mounted.current) {
        setPending(false);
      }
    }
  }, []);

  const clear = useCallback(() => {
    setError(null);
    setFailed(false);
  }, []);

  return { pending, error, failed, run, clear };
}

/**
 * Counts down whole seconds from `seconds` (undefined: no countdown, 0 from
 * the start). Restarts when `seconds` or `key` changes.
 */
export function useCountdown(seconds: number | undefined, key: unknown): number {
  const [left, setLeft] = useState(seconds ?? 0);
  useEffect(() => {
    setLeft(seconds ?? 0);
    if (seconds === undefined || seconds <= 0) {
      return;
    }
    const timer = window.setInterval(() => {
      setLeft((current) => {
        if (current <= 1) {
          window.clearInterval(timer);
          return 0;
        }
        return current - 1;
      });
    }, 1000);
    return () => window.clearInterval(timer);
  }, [seconds, key]);
  return left;
}

/** Rechecks an edit lease at its server-provided expiry, then periodically if it remains. */
export function useLeaseRefresh(expiresAt: string | null | undefined, refresh: () => Promise<unknown>) {
  const [error, setError] = useState<unknown>(null);
  const refreshRef = useRef(refresh);
  refreshRef.current = refresh;
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    setError(null);
    if (expiresAt == null) return;
    const expiry = Date.parse(expiresAt);
    if (!Number.isFinite(expiry)) return;
    let timer = 0;
    let active = true;
    const check = async () => {
      try {
        await refreshRef.current();
        if (active) setError(null);
      } catch (failure) {
        if (active) setError(failure);
      }
      if (active) timer = window.setTimeout(() => void check(), 30_000);
    };
    timer = window.setTimeout(() => void check(), Math.max(0, expiry - Date.now()));
    return () => { active = false; window.clearTimeout(timer); };
  }, [expiresAt, attempt]);
  return {
    error,
    retry: async () => {
      setError(null);
      try {
        await refreshRef.current();
      } catch (failure) {
        setError(failure);
      }
      setAttempt((value) => value + 1);
    },
  };
}
