import { cleanup } from "@testing-library/react";
import { afterEach, beforeEach } from "vitest";

// Some test files run in the plain node environment (file system checks).
if (typeof window !== "undefined") {
  // jsdom lacks a few browser APIs that React Aria and axe touch.
  if (typeof window.matchMedia !== "function") {
    window.matchMedia = (query: string): MediaQueryList =>
      ({
        matches: false,
        media: query,
        onchange: null,
        addEventListener: () => undefined,
        removeEventListener: () => undefined,
        addListener: () => undefined,
        removeListener: () => undefined,
        dispatchEvent: () => false,
      }) as MediaQueryList;
  }
  if (typeof window.ResizeObserver !== "function") {
    window.ResizeObserver = class {
      observe(): void {}
      unobserve(): void {}
      disconnect(): void {}
    };
  }
  if (typeof Element.prototype.scrollIntoView !== "function") {
    Element.prototype.scrollIntoView = () => undefined;
  }
  // axe-core probes canvas support; jsdom has none and only logs a warning.
  HTMLCanvasElement.prototype.getContext = (() => null) as typeof HTMLCanvasElement.prototype.getContext;

  beforeEach(() => {
    window.localStorage.clear();
    document.documentElement.removeAttribute("data-theme");
    document.documentElement.lang = "en";
  });

  afterEach(() => {
    cleanup();
  });
}
