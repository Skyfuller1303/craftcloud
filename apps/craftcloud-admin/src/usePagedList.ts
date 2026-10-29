import { useCallback, useEffect, useRef, useState } from "react";
import type { Page } from "./api/types.ts";

export interface PagedList<T> {
  items: T[];
  status: "loading" | "ready" | "error";
  error: unknown;
  hasMore: boolean;
  loadingMore: boolean;
  loadMore: () => void;
  reload: () => void;
}

/**
 * Loads a cursor-paginated list. `load` must be stable per scope (wrap it in
 * `useCallback` with the scope as dependency); a new `load` starts over and
 * answers of the previous scope are ignored.
 */
export function usePagedList<T>(
  load: ((cursor?: string) => Promise<Page<T>>) | null,
): PagedList<T> {
  const [items, setItems] = useState<T[]>([]);
  const [status, setStatus] = useState<PagedList<T>["status"]>("loading");
  const [error, setError] = useState<unknown>(null);
  const [cursor, setCursor] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const generation = useRef(0);

  useEffect(() => {
    generation.current += 1;
    const mine = generation.current;
    setItems([]);
    setCursor(null);
    setError(null);
    setLoadingMore(false);
    if (load === null) {
      setStatus("ready");
      return;
    }
    setStatus("loading");
    load().then(
      (page) => {
        if (generation.current !== mine) return;
        setItems(page.items);
        setCursor(page.next_cursor);
        setStatus("ready");
      },
      (failure: unknown) => {
        if (generation.current !== mine) return;
        setError(failure);
        setStatus("error");
      },
    );
    return () => {
      generation.current += 1;
    };
  }, [load, attempt]);

  const loadMore = useCallback(() => {
    if (load === null || cursor === null) return;
    const mine = generation.current;
    setLoadingMore(true);
    load(cursor).then(
      (page) => {
        if (generation.current !== mine) return;
        setItems((current) => [...current, ...page.items]);
        setCursor(page.next_cursor);
        setLoadingMore(false);
      },
      (failure: unknown) => {
        if (generation.current !== mine) return;
        setError(failure);
        setStatus("error");
        setLoadingMore(false);
      },
    );
  }, [load, cursor]);

  return {
    items,
    status,
    error,
    hasMore: cursor !== null,
    loadingMore,
    loadMore,
    reload: () => setAttempt((n) => n + 1),
  };
}
