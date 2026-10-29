import { act, fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AnnouncerProvider, useAnnounce } from "./Announcer.tsx";

function Controls() {
  const announce = useAnnounce();
  const [count, setCount] = useState(0);
  return (
    <button onClick={() => {
      const next = count + 1;
      setCount(next);
      announce(`Announcement ${next}`);
    }}>Announce</button>
  );
}

describe("announcer toasts", () => {
  afterEach(() => vi.useRealTimers());

  it("keeps one live region and queues only the newest four visual toasts", async () => {
    vi.useFakeTimers();
    const { container } = render(<AnnouncerProvider><Controls /></AnnouncerProvider>);
    const button = screen.getByRole("button", { name: "Announce" });
    for (let i = 0; i < 5; i += 1) fireEvent.click(button);
    await act(async () => { await vi.advanceTimersByTimeAsync(60); });

    const liveRegions = container.querySelectorAll('[aria-live="polite"]');
    expect(liveRegions).toHaveLength(1);
    expect(screen.getByTestId("announcer").textContent).toBe("Announcement 5");
    const toasts = container.querySelectorAll('.toasts[aria-hidden="true"] .toast');
    expect(toasts).toHaveLength(4);
    expect(container.querySelector(".toasts")?.textContent).not.toContain("Announcement 1");
  });

  it("pauses on pointer hover, expires after leaving, and closes on click", async () => {
    vi.useFakeTimers();
    const { container } = render(<AnnouncerProvider><Controls /></AnnouncerProvider>);
    fireEvent.click(screen.getByRole("button", { name: "Announce" }));
    const toast = container.querySelector<HTMLElement>(".toast");
    expect(toast).not.toBeNull();

    fireEvent.pointerEnter(toast!);
    await act(async () => { await vi.advanceTimersByTimeAsync(5000); });
    expect(container.querySelector(".toast")).not.toBeNull();

    fireEvent.pointerLeave(toast!);
    await act(async () => { await vi.advanceTimersByTimeAsync(4499); });
    expect(container.querySelector(".toast")).not.toBeNull();
    await act(async () => { await vi.advanceTimersByTimeAsync(1); });
    expect(container.querySelector(".toast")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Announce" }));
    const currentToast = container.querySelector<HTMLElement>(".toast");
    fireEvent.click(currentToast!);
    expect(container.querySelector(".toast")).toBeNull();
  });
});
