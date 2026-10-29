import { describe, expect, it } from "vitest";
import { entryKey, formatMessage, parseTsv, placeholders } from "./tsv.ts";

describe("parseTsv", () => {
  it("reads context, source and translation separated by tabs", () => {
    const { entries, problems } = parseTsv("ctx\tSave\tSpeichern\n\tCancel\tAbbrechen\n");
    expect(problems).toEqual([]);
    expect(entries).toEqual([
      { context: "ctx", source: "Save", translation: "Speichern", line: 1 },
      { context: "", source: "Cancel", translation: "Abbrechen", line: 2 },
    ]);
  });

  it("skips comments and empty lines but keeps line numbers", () => {
    const { entries } = parseTsv("# header\n\n   \n\tA\tB\n#\tnot\tan entry\n\tC\tD");
    expect(entries.map((e) => [e.source, e.line])).toEqual([
      ["A", 4],
      ["C", 6],
    ]);
  });

  it("accepts CRLF, CR and a byte order mark", () => {
    const { entries, problems } = parseTsv("\uFEFF\tA\tB\r\n\tC\tD\r\tE\tF");
    expect(problems).toEqual([]);
    expect(entries.map((e) => e.source)).toEqual(["A", "C", "E"]);
  });

  it("decodes escapes in every column", () => {
    const { entries, problems } = parseTsv("a\\tb\tline1\\nline2\ttab\\there \\\\ end\\r");
    expect(problems).toEqual([]);
    expect(entries[0]).toMatchObject({
      context: "a\tb",
      source: "line1\nline2",
      translation: "tab\there \\ end\r",
    });
  });

  it("reports unknown escapes and a dangling backslash", () => {
    const { entries, problems } = parseTsv("\tbad \\x\tx\n\tend\\\tx\n\tgood\tok");
    expect(entries.map((e) => e.source)).toEqual(["good"]);
    expect(problems.map((p) => [p.line, p.message])).toEqual([
      [1, "unknown escape \\x"],
      [2, "dangling backslash at end of column"],
    ]);
  });

  it("reports missing columns instead of guessing", () => {
    const { entries, problems } = parseTsv("only one column\n\tsource only\n\tok\tfine");
    expect(entries.map((e) => e.source)).toEqual(["ok"]);
    expect(problems).toHaveLength(2);
    expect(problems[0]).toMatchObject({ line: 1 });
    expect(problems[0]?.message).toContain("found 1");
    expect(problems[1]?.message).toContain("found 2");
  });

  it("reports extra columns (an unescaped tab inside a column)", () => {
    const { entries, problems } = parseTsv("\tA\tB\tC");
    expect(entries).toEqual([]);
    expect(problems[0]?.message).toContain("found 4");
  });

  it("rejects an empty source column", () => {
    const { problems } = parseTsv("\t\tx");
    expect(problems).toEqual([{ line: 1, message: "empty source column" }]);
  });

  it("keeps an empty translation (untranslated) as an entry", () => {
    const { entries } = parseTsv("\tA\t");
    expect(entries).toEqual([{ context: "", source: "A", translation: "", line: 1 }]);
  });

  it("returns nothing for an empty file", () => {
    expect(parseTsv("")).toEqual({ entries: [], problems: [] });
  });
});

describe("placeholders and formatting", () => {
  it("lists sorted unique placeholder names", () => {
    expect(placeholders("{b} and {a} and {b}")).toEqual(["a", "b"]);
    expect(placeholders("no braces { } {1x} {}")).toEqual([]);
  });

  it("replaces known placeholders and leaves unknown ones visible", () => {
    expect(formatMessage("Hi {name}, {count} items {missing}", { name: "Ada", count: 3 })).toBe(
      "Hi Ada, 3 items {missing}",
    );
    expect(formatMessage("Hi {name}")).toBe("Hi {name}");
  });

  it("does not interpret replacement patterns inside values", () => {
    expect(formatMessage("A {x}", { x: "$& $1" })).toBe("A $& $1");
  });

  it("keys differ by context", () => {
    expect(entryKey("a", "S")).not.toBe(entryKey("", "S"));
  });
});
