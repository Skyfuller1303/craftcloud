import { createContext, useCallback, useContext, useEffect, useMemo, useState } from "react";
import type { ReactNode } from "react";
import { storageGet, storageSet } from "../preferences.ts";
import { entryKey, formatMessage, parseTsv } from "./tsv.ts";

export const DEFAULT_LANGUAGE = "en";
const LANGUAGE_KEY = "craftcloud.admin.language";
/** Source text of the entry that names a language in its own language. */
export const LANGUAGE_NAME_KEY = "Language name";

export type Params = Record<string, string | number>;
export type Translate = (source: string, params?: Params) => string;
export type TranslateInContext = (context: string, source: string, params?: Params) => string;

export interface Catalog {
  code: string;
  messages: Map<string, string>;
}

/** Builds catalogues from `{ "<lang>.tsv path": raw text }`. A new language is a new file. */
export function loadCatalogs(files: Record<string, string>): Map<string, Catalog> {
  const catalogs = new Map<string, Catalog>();
  for (const [path, raw] of Object.entries(files)) {
    const match = /([A-Za-z]{2,3}(?:-[A-Za-z0-9]+)*)\.tsv$/.exec(path);
    if (match?.[1] === undefined) {
      continue;
    }
    const code = match[1].toLowerCase();
    const messages = new Map<string, string>();
    // Entries with problems are skipped here; `npm run i18n:check` makes them fail the build.
    for (const entry of parseTsv(raw).entries) {
      if (entry.translation !== "") {
        messages.set(entryKey(entry.context, entry.source), entry.translation);
      }
    }
    catalogs.set(code, { code, messages });
  }
  return catalogs;
}

const bundled = import.meta.glob("../locales/*.tsv", {
  query: "?raw",
  import: "default",
  eager: true,
}) as Record<string, string>;

export const CATALOGS = loadCatalogs(bundled);

/** Picks the first browser language that has a catalogue (`de-AT` matches `de`). */
export function negotiateLanguage(
  preferred: readonly string[],
  available: Iterable<string>,
): string {
  const have = new Set(available);
  for (const tag of preferred) {
    const lower = tag.toLowerCase();
    if (have.has(lower)) {
      return lower;
    }
    const base = lower.split("-")[0] ?? "";
    if (have.has(base)) {
      return base;
    }
  }
  return DEFAULT_LANGUAGE;
}

export function languageName(catalogs: Map<string, Catalog>, code: string): string {
  return catalogs.get(code)?.messages.get(entryKey("", LANGUAGE_NAME_KEY)) ?? code;
}

function initialLanguage(catalogs: Map<string, Catalog>): string {
  const stored = storageGet(LANGUAGE_KEY);
  if (stored !== null && catalogs.has(stored)) {
    return stored;
  }
  const preferred = typeof navigator === "undefined" ? [] : (navigator.languages ?? []);
  return negotiateLanguage(preferred.length > 0 ? preferred : [navigator.language], catalogs.keys());
}

export function createTranslator(catalog: Catalog | undefined): {
  t: Translate;
  tc: TranslateInContext;
} {
  const tc: TranslateInContext = (context, source, params) =>
    formatMessage(catalog?.messages.get(entryKey(context, source)) ?? source, params);
  return { t: (source, params) => tc("", source, params), tc };
}

interface I18nValue {
  language: string;
  languages: { code: string; name: string }[];
  setLanguage: (code: string) => void;
  t: Translate;
  tc: TranslateInContext;
}

const I18nContext = createContext<I18nValue | null>(null);

export function I18nProvider({
  children,
  catalogs = CATALOGS,
  initial,
}: {
  children: ReactNode;
  catalogs?: Map<string, Catalog>;
  initial?: string;
}) {
  const [language, setLanguageState] = useState(() => initial ?? initialLanguage(catalogs));

  useEffect(() => {
    document.documentElement.lang = language;
  }, [language]);

  const setLanguage = useCallback(
    (code: string) => {
      if (catalogs.has(code)) {
        setLanguageState(code);
        storageSet(LANGUAGE_KEY, code);
      }
    },
    [catalogs],
  );

  const value = useMemo<I18nValue>(() => {
    const { t, tc } = createTranslator(catalogs.get(language));
    const languages = [...catalogs.keys()]
      .sort()
      .map((code) => ({ code, name: languageName(catalogs, code) }));
    return { language, languages, setLanguage, t, tc };
  }, [catalogs, language, setLanguage]);

  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>;
}

export function useI18n(): I18nValue {
  const value = useContext(I18nContext);
  if (value === null) {
    throw new Error("useI18n needs an I18nProvider");
  }
  return value;
}
