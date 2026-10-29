// Parser for the translation catalogues (ADR-0015): one `<lang>.tsv` per
// language, three tab-separated columns `context <TAB> source <TAB> translation`.
// The English source text is the key; the context column may be empty and only
// disambiguates identical source texts. Lines starting with `#` are comments,
// empty lines are ignored.
//
// Escapes inside a column: `\t` (tab), `\n` (line feed), `\r` (carriage
// return), `\\` (backslash). Placeholders are written `{name}` and must appear
// in the translation exactly as in the source. This file avoids TypeScript-only
// runtime syntax so that scripts/i18n-check.mjs can import it directly.

export interface TsvEntry {
  context: string;
  source: string;
  translation: string;
  /** 1-based line number in the file. */
  line: number;
}

export interface TsvProblem {
  line: number;
  message: string;
}

export interface TsvParseResult {
  entries: TsvEntry[];
  problems: TsvProblem[];
}

const ESCAPES: Record<string, string> = { t: "\t", n: "\n", r: "\r", "\\": "\\" };

/** Returns the unescaped text, or `null` plus a message for an unknown or dangling escape. */
function unescapeColumn(raw: string): { text: string; problem: string | null } {
  let text = "";
  let problem: string | null = null;
  for (let i = 0; i < raw.length; i += 1) {
    const ch = raw.charAt(i);
    if (ch !== "\\") {
      text += ch;
      continue;
    }
    const next = raw.charAt(i + 1);
    const replacement = next === "" ? undefined : ESCAPES[next];
    if (replacement === undefined) {
      problem ??= next === "" ? "dangling backslash at end of column" : `unknown escape \\${next}`;
      text += ch;
    } else {
      text += replacement;
      i += 1;
    }
  }
  return { text, problem };
}

export function parseTsv(input: string): TsvParseResult {
  const entries: TsvEntry[] = [];
  const problems: TsvProblem[] = [];
  const text = input.startsWith("﻿") ? input.slice(1) : input;
  const lines = text.split(/\r\n|\n|\r/);
  lines.forEach((rawLine, index) => {
    const line = index + 1;
    if (rawLine.trim() === "" || rawLine.startsWith("#")) {
      return;
    }
    const columns = rawLine.split("\t");
    if (columns.length < 3) {
      problems.push({
        line,
        message: `expected 3 tab-separated columns (context, source, translation), found ${columns.length}`,
      });
      return;
    }
    if (columns.length > 3) {
      problems.push({
        line,
        message: `expected 3 columns, found ${columns.length}; write a tab inside a column as \\t`,
      });
      return;
    }
    const [rawContext = "", rawSource = "", rawTranslation = ""] = columns;
    const context = unescapeColumn(rawContext);
    const source = unescapeColumn(rawSource);
    const translation = unescapeColumn(rawTranslation);
    const problem = context.problem ?? source.problem ?? translation.problem;
    if (problem !== null) {
      problems.push({ line, message: problem });
      return;
    }
    if (source.text === "") {
      problems.push({ line, message: "empty source column" });
      return;
    }
    entries.push({
      context: context.text,
      source: source.text,
      translation: translation.text,
      line,
    });
  });
  return { entries, problems };
}

/** Lookup key of a catalogue entry. */
export function entryKey(context: string, source: string): string {
  return `${context}\u0000${source}`;
}

/** The `{name}` placeholders of a text, sorted and de-duplicated. */
export function placeholders(text: string): string[] {
  const found = new Set<string>();
  for (const match of text.matchAll(/\{([A-Za-z_][A-Za-z0-9_]*)\}/g)) {
    found.add(match[1] ?? "");
  }
  return [...found].sort();
}

/** Replaces `{name}` placeholders; unknown placeholders stay visible as written. */
export function formatMessage(template: string, params?: Record<string, string | number>): string {
  if (params === undefined) {
    return template;
  }
  return template.replace(/\{([A-Za-z_][A-Za-z0-9_]*)\}/g, (whole, name: string) => {
    const value = params[name];
    return value === undefined ? whole : String(value);
  });
}
