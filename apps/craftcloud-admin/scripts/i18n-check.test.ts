// @vitest-environment node
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { check } from "./i18n-check.mjs";

const created: string[] = [];

function project(files: { en: string; de?: string; code: string }) {
  const root = mkdtempSync(join(tmpdir(), "cc-i18n-"));
  created.push(root);
  const srcDir = join(root, "src");
  const localesDir = join(srcDir, "locales");
  mkdirSync(localesDir, { recursive: true });
  writeFileSync(join(srcDir, "app.tsx"), files.code);
  writeFileSync(join(localesDir, "en.tsv"), files.en);
  if (files.de !== undefined) {
    writeFileSync(join(localesDir, "de.tsv"), files.de);
  }
  return { root, srcDir, localesDir };
}

afterEach(() => {
  for (const dir of created.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
});

const NAME = "\tLanguage name\tEnglish\n";
const DE_NAME = "\tLanguage name\tDeutsch\n";
const CODE = 't("Save"); t("Hello {name}", {name}); tc("menu", "Open");';
const EN = `${NAME}\tSave\tSave\n\tHello {name}\tHello {name}\nmenu\tOpen\tOpen\n`;
const DE = `${DE_NAME}\tSave\tSpeichern\n\tHello {name}\tHallo {name}\nmenu\tOpen\tÖffnen\n`;

describe("i18n check", () => {
  it("passes for consistent catalogues", () => {
    expect(check(project({ en: EN, de: DE, code: CODE }))).toEqual([]);
  });

  it("reports a missing and an extra entry in the translation", () => {
    const de = `${DE_NAME}\tSave\tSpeichern\n\tHello {name}\tHallo {name}\n\tOld text\tAlter Text\n`;
    const problems = check(project({ en: EN, de, code: CODE }));
    expect(problems.some((p) => p.includes('de.tsv: missing translation for "Open"'))).toBe(true);
    expect(problems.some((p) => p.includes('de.tsv:4: extra entry "Old text"'))).toBe(true);
  });

  it("reports texts used in code but absent from en.tsv, and unused en.tsv entries", () => {
    const problems = check(project({ en: `${NAME}\tSave\tSave\n\tUnused\tUnused\n`, code: 't("Save"); t("New text");' }));
    expect(problems.some((p) => p.includes('missing entry for "New text"'))).toBe(true);
    expect(problems.some((p) => p.includes('extra entry "Unused" is not used'))).toBe(true);
  });

  it("reports empty translations and changed placeholders", () => {
    const de = `${DE_NAME}\tSave\t\n\tHello {name}\tHallo {nom}\nmenu\tOpen\tÖffnen\n`;
    const problems = check(project({ en: EN, de, code: CODE }));
    expect(problems.some((p) => p.includes("empty translation"))).toBe(true);
    expect(problems.some((p) => p.includes("placeholders {nom} differ from source {name}"))).toBe(true);
  });

  it("reports parse problems, duplicates and a missing language name", () => {
    const de = "\tSave\n\tHello {name}\tHallo {name}\n\tHello {name}\tNochmal {name}\nmenu\tOpen\tÖffnen\n";
    const problems = check(project({ en: EN, de, code: CODE }));
    expect(problems.some((p) => p.startsWith("de.tsv:1: expected 3 tab-separated columns"))).toBe(true);
    expect(problems.some((p) => p.includes("duplicate entry"))).toBe(true);
    expect(problems.some((p) => p.includes('missing entry "Language name"'))).toBe(true);
  });

  it("ignores test files when collecting used texts", () => {
    const dirs = project({ en: `${NAME}`, code: "" });
    writeFileSync(join(dirs.srcDir, "app.test.tsx"), 't("only in a test");');
    expect(check(dirs)).toEqual([]);
  });
});
