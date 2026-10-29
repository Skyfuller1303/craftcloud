// The only data this app stores in the browser: the UI language and the theme
// choice. Never tokens, session data or user data. Storage can be unavailable
// (private mode, blocked cookies), so every access is wrapped.

export function storageGet(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}

export function storageSet(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    // Preference is simply not remembered.
  }
}

export type ThemeChoice = "system" | "light" | "dark";
export const THEME_CHOICES: readonly ThemeChoice[] = ["system", "light", "dark"];
const THEME_KEY = "craftcloud.admin.theme";

export function readTheme(): ThemeChoice {
  const stored = storageGet(THEME_KEY);
  return THEME_CHOICES.find((choice) => choice === stored) ?? "system";
}

/** Applies the choice to `<html data-theme>`; no attribute means "follow the system". */
export function applyTheme(choice: ThemeChoice): void {
  const root = document.documentElement;
  if (choice === "system") {
    root.removeAttribute("data-theme");
  } else {
    root.setAttribute("data-theme", choice);
  }
}

export function saveTheme(choice: ThemeChoice): void {
  storageSet(THEME_KEY, choice);
  applyTheme(choice);
}
