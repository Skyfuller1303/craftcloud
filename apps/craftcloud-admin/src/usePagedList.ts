import { useCallback, useEffect, useRef, useState } from "react";
import type { Page } from "./api/types.ts";

export interface PagedList<T> {
  items: T[];
  status: "loading" | "ready" | "error";
  error: unknown;
  hasMore: boolean;
  loadingMore: boolean;
  loadMore: () => void;
  /** Starts over and shows the loading state. */
  reload: () => void;
  /**
   * Fetches the first page again and swaps it in when it arrives; the current
   * rows (and the keyboard focus inside them) stay until then. Use it after a
   * change made by this page.
   */
  refresh: () => void;
  /**
   * Edits the rows already loaded, for example to replace one after a role change;
   * the first page is then read again in the background.
   */
  mutate: (change: (items: T[]) => T[]) => void;
}

/** Identity of a row for merging refreshed pages: `id`, or `user_id` for member rows. */
function defaultKey(item: unknown): string {
  const row = item as { id?: unknown; user_id?: unknown };
  return String(row.id ?? row.user_id ?? JSON.stringify(item));
}

/**
 * Loads a cursor-paginated list. `load` must be stable per scope (wrap it in
 * `useCallback` with the scope as dependency); a new `load` starts over and
 * answers of the previous scope are ignored.
 */
export function usePagedList<T>(
  load: ((cursor?: string) => Promise<Page<T>>) | null,
  keyOf: (item: T) => string = defaultKey,
): PagedList<T> {
  const [items, setItems] = useState<T[]>([]);
  const [status, setStatus] = useState<PagedList<T>["status"]>("loading");
  const [error, setError] = useState<unknown>(null);
  const [cursor, setCursor] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const generation = useRef(0);
  const [softAttempt, setSoftAttempt] = useState(0);
  const softSeen = useRef(0);
  // Rows and cursor as last rendered, and the keys the first page had, for merging a refresh.
  const itemsRef = useRef<T[]>([]);
  itemsRef.current = items;
  const cursorRef = useRef<string | null>(null);
  cursorRef.current = cursor;
  const firstPageKeys = useRef<Set<string>>(new Set());

  useEffect(() => {
    generation.current += 1;
    const mine = generation.current;
    if (softAttempt !== softSeen.current && load !== null) {
      // Soft refresh: keep what is shown until the new first page arrives.
      softSeen.current = softAttempt;
      setLoadingMore(false); // a "load more" that this refresh made stale must not stay "loading"
      load().then(
        (page) => {
          if (generation.current !== mine) return;
          // Later pages the user already loaded stay (the server's first page only replaces
          // the old first page); rows that left the first page disappear, none appear twice.
          const fresh = new Set(page.items.map(keyOf));
          const later = itemsRef.current.filter(
            (item) => !firstPageKeys.current.has(keyOf(item)) && !fresh.has(keyOf(item)),
          );
          firstPageKeys.current = fresh;
          setItems([...page.items, ...later]);
          setCursor(later.length > 0 ? cursorRef.current : page.next_cursor);
          setError(null);
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
    }
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
        firstPageKeys.current = new Set(page.items.map(keyOf));
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
  }, [load, attempt, softAttempt]);

  const loadMore = useCallback(() => {
    if (load === null || cursor === null) return;
    const mine = generation.current;
    setLoadingMore(true);
    load(cursor).then(
      (page) => {
        if (generation.current !== mine) {
          setLoadingMore(false);
          return;
        }
        setItems((current) => {
          const have = new Set(current.map(keyOf));
          return [...current, ...page.items.filter((item) => !have.has(keyOf(item)))];
        });
        setCursor(page.next_cursor);
        setLoadingMore(false);
      },
      (failure: unknown) => {
        if (generation.current !== mine) {
          setLoadingMore(false);
          return;
        }
        setError(failure);
        setStatus("error");
        setLoadingMore(false);
      },
    );
  }, [load, cursor, keyOf]);

  return {
    items,
    status,
    error,
    hasMore: cursor !== null,
    loadingMore,
    loadMore,
    reload: () => setAttempt((n) => n + 1),
    refresh: () => setSoftAttempt((n) => n + 1),
    mutate: (change) => {
      // A refresh that started before this change may carry the old rows: drop its
      // answer and read the list again, so a stale page cannot bring a removed row back.
      generation.current += 1;
      setItems(change);
      setSoftAttempt((n) => n + 1);
    },
  };
}
