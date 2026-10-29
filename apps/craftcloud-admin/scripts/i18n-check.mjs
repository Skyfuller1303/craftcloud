// `npm run i18n:check`: fails when the translation catalogues are inconsistent
// (ADR-0015). Checked:
//   - every src/locales/<lang>.tsv parses (3 columns, valid escapes);
//   - no duplicate (context, source) keys inside one file;
//   - en.tsv lists exactly the strings the code asks for with t("...") /
//     tc("context", "...") (missing = used but not listed, extra = listed but unused);
//   - every other language has the same keys as en.tsv (missing / extra);
//   - every translation is non-empty and uses the same {placeholders} as its source;
//   - every catalogue names its language (entry "Language name").

import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { entryKey, parseTsv, placeholders } from "../src/i18n/tsv.ts";

const root = fileURLToPath(new URL("..", import.meta.url));
const defaultDirs = { root, localesDir: join(root, "src", "locales"), srcDir: join(root, "src") };
const LANGUAGE_NAME = "Language name";
const REFERENCE = "en";

function walk(dir, testDir) {
  const out = [];
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) {
      out.push(...walk(path, testDir));
    } else if (/\.(ts|tsx)$/.test(name) && !/\.test\.(ts|tsx)$/.test(name) && !path.startsWith(testDir)) {
      out.push(path);
    }
  }
  return out;
}

const STRING = String.raw`"((?:[^"\\\n]|\\.)*)"`;
const T_CALL = new RegExp(String.raw`(?<![A-Za-z0-9_.])t\(\s*${STRING}`, "g");
const TC_CALL = new RegExp(String.raw`(?<![A-Za-z0-9_.])tc\(\s*${STRING}\s*,\s*${STRING}`, "g");

function unescapeLiteral(text) {
  return JSON.parse(`"${text}"`);
}

/** Strings the code asks for: Map(key -> {context, source, where}). */
export function usedKeys({ root, srcDir } = defaultDirs) {
  const used = new Map();
  for (const file of walk(srcDir, join(srcDir, "test"))) {
    const text = readFileSync(file, "utf8");
    for (const match of text.matchAll(T_CALL)) {
      const source = unescapeLiteral(match[1]);
      used.set(entryKey("", source), { context: "", source, where: relative(root, file) });
    }
    for (const match of text.matchAll(TC_CALL)) {
      const context = unescapeLiteral(match[1]);
      const source = unescapeLiteral(match[2]);
      used.set(entryKey(context, source), { context, source, where: relative(root, file) });
    }
  }
  return used;
}

export function check(dirs = defaultDirs) {
  const { localesDir } = dirs;
  const problems = [];
  const files = readdirSync(localesDir).filter((name) => name.endsWith(".tsv")).sort();
  if (!files.includes(`${REFERENCE}.tsv`)) {
    return [`src/locales/${REFERENCE}.tsv is missing`];
  }
  const catalogs = new Map();
  for (const name of files) {
    const code = name.replace(/\.tsv$/, "");
    const { entries, problems: parseProblems } = parseTsv(readFileSync(join(localesDir, name), "utf8"));
    for (const p of parseProblems) {
      problems.push(`${name}:${p.line}: ${p.message}`);
    }
    const byKey = new Map();
    for (const entry of entries) {
      const key = entryKey(entry.context, entry.source);
      if (byKey.has(key)) {
        problems.push(`${name}:${entry.line}: duplicate entry ${JSON.stringify(entry.source)} (first at line ${byKey.get(key).line})`);
      } else {
        byKey.set(key, entry);
      }
    }
    catalogs.set(code, byKey);
    if (!byKey.has(entryKey("", LANGUAGE_NAME))) {
      problems.push(`${name}: missing entry ${JSON.stringify(LANGUAGE_NAME)} (the language's own name)`);
    }
  }

  const reference = catalogs.get(REFERENCE);
  const used = usedKeys(dirs);
  used.set(entryKey("", LANGUAGE_NAME), { context: "", source: LANGUAGE_NAME, where: "i18n/index.tsx" });
  for (const [key, use] of used) {
    if (!reference.has(key)) {
      problems.push(`${REFERENCE}.tsv: missing entry for ${JSON.stringify(use.source)} used in ${use.where}`);
    }
  }
  for (const [key, entry] of reference) {
    if (!used.has(key)) {
      problems.push(`${REFERENCE}.tsv:${entry.line}: extra entry ${JSON.stringify(entry.source)} is not used by the code`);
    }
  }

  for (const [code, entries] of catalogs) {
    for (const [key, entry] of entries) {
      if (code !== REFERENCE && !reference.has(key)) {
        problems.push(`${code}.tsv:${entry.line}: extra entry ${JSON.stringify(entry.source)} (not in ${REFERENCE}.tsv)`);
      }
      if (entry.translation.trim() === "") {
        problems.push(`${code}.tsv:${entry.line}: empty translation for ${JSON.stringify(entry.source)}`);
        continue;
      }
      const want = placeholders(entry.source).join(",");
      const have = placeholders(entry.translation).join(",");
      if (want !== have) {
        problems.push(`${code}.tsv:${entry.line}: placeholders {${have}} differ from source {${want}} in ${JSON.stringify(entry.source)}`);
      }
    }
    if (code !== REFERENCE) {
      for (const [key, entry] of reference) {
        if (!entries.has(key)) {
          problems.push(`${code}.tsv: missing translation for ${JSON.stringify(entry.source)}`);
        }
      }
    }
  }
  return problems;
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const problems = check();
  if (problems.length > 0) {
    for (const problem of problems) {
      console.error(problem);
    }
    console.error(`i18n:check FAILED (${problems.length} problem${problems.length === 1 ? "" : "s"})`);
    process.exit(1);
  }
  const files = readdirSync(defaultDirs.localesDir).filter((name) => name.endsWith(".tsv"));
  console.log(`i18n:check ok (${files.length} catalogues, ${usedKeys().size + 1} strings)`);
}
