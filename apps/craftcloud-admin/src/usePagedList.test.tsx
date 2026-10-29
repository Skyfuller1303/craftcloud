import { act, renderHook, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { Page } from "./api/types.ts";
import { usePagedList } from "./usePagedList.ts";

interface Row {
  id: string;
  label: string;
}
const row = (n: number, label = `row ${n}`): Row => ({ id: `r${n}`, label });
const rows = (from: number, to: number) => Array.from({ length: to - from + 1 }, (_, i) => row(from + i));

/** A list endpoint whose answers the test releases by hand. */
function controlled() {
  const waiting: Array<{ cursor: string | undefined; resolve: (page: Page<Row>) => void }> = [];
  const load = (cursor?: string) =>
    new Promise<Page<Row>>((resolve) => {
      waiting.push({ cursor, resolve });
    });
  return {
    load,
    waiting,
    answer: async (index: number, page: Page<Row>) => {
      await act(async () => {
        waiting[index]?.resolve(page);
      });
    },
  };
}

describe("usePagedList", () => {
  it("clears loadingMore when a refresh makes the pending load-more stale", async () => {
    const server = controlled();
    const { result } = renderHook(() => usePagedList<Row>(server.load));
    await server.answer(0, { items: rows(1, 2), next_cursor: "c1" });
    await waitFor(() => expect(result.current.status).toBe("ready"));

    act(() => result.current.loadMore());
    expect(result.current.loadingMore).toBe(true);
    act(() => result.current.refresh());
    await waitFor(() => expect(result.current.loadingMore).toBe(false));

    // The stale answer arrives later and changes nothing.
    await server.answer(1, { items: rows(3, 4), next_cursor: null });
    await server.answer(2, { items: rows(1, 2), next_cursor: "c1" });
    expect(result.current.loadingMore).toBe(false);
    expect(result.current.items.map((r) => r.id)).toEqual(["r1", "r2"]);
    expect(result.current.hasMore).toBe(true);
  });

  it("clears loadingMore when a local change makes the pending load-more stale", async () => {
    const server = controlled();
    const { result } = renderHook(() => usePagedList<Row>(server.load));
    await server.answer(0, { items: rows(1, 2), next_cursor: "c1" });
    await waitFor(() => expect(result.current.status).toBe("ready"));
    act(() => result.current.loadMore());
    act(() => result.current.mutate((items) => items.filter((r) => r.id !== "r1")));
    await waitFor(() => expect(result.current.loadingMore).toBe(false));
    await server.answer(1, { items: rows(3, 4), next_cursor: null });
    expect(result.current.loadingMore).toBe(false);
  });

  it("keeps pages the user loaded when the first page is refreshed, and applies removals on page one", async () => {
    const server = controlled();
    const { result } = renderHook(() => usePagedList<Row>(server.load));
    await server.answer(0, { items: rows(1, 50), next_cursor: "c1" });
    await waitFor(() => expect(result.current.items.length).toBe(50));
    act(() => result.current.loadMore());
    await server.answer(1, { items: rows(51, 60), next_cursor: null });
    await waitFor(() => expect(result.current.items.length).toBe(60));

    // Edit row 60 (page two), then the background re-read of page one arrives
    // without row 1, which was removed on the server.
    act(() => result.current.mutate((items) => items.map((r) => (r.id === "r60" ? { ...r, label: "edited" } : r))));
    await server.answer(2, { items: rows(2, 50), next_cursor: "c1" });

    await waitFor(() => expect(result.current.items.length).toBe(59));
    expect(result.current.items.some((r) => r.id === "r1")).toBe(false);
    expect(result.current.items.find((r) => r.id === "r60")?.label).toBe("edited");
    expect(new Set(result.current.items.map((r) => r.id)).size).toBe(59);
    // The cursor of the loaded pages is kept: nothing more to load.
    expect(result.current.hasMore).toBe(false);
  });

  it("does not duplicate a row that moved from page two to page one", async () => {
    const server = controlled();
    const { result } = renderHook(() => usePagedList<Row>(server.load));
    await server.answer(0, { items: rows(1, 2), next_cursor: "c1" });
    await waitFor(() => expect(result.current.status).toBe("ready"));
    act(() => result.current.loadMore());
    await server.answer(1, { items: rows(3, 4), next_cursor: null });
    await waitFor(() => expect(result.current.items.length).toBe(4));
    act(() => result.current.refresh());
    await server.answer(2, { items: rows(2, 3), next_cursor: "c2" }); // r1 gone, r3 moved up
    await waitFor(() => expect(result.current.items.map((r) => r.id)).toEqual(["r2", "r3", "r4"]));
  });
});
