import { describe, expect, it } from "vitest";
import { conflictCopyName, formatOf, nameBytes } from "./fileNames.ts";

describe("file names", () => {
  it("derives the format from the last extension", () => {
    expect(formatOf("design.FIG")).toBe("fig");
    expect(formatOf("archive.tar.gz")).toBe("gz");
    expect(formatOf("README")).toBe("bin");
    expect(formatOf("odd.ex_t")).toBe("bin");
    expect(formatOf(".hidden")).toBe("hidden");
  });

  it("builds conflict copy names that keep the extension", () => {
    expect(conflictCopyName("design.fig", 1)).toBe("design (conflict copy).fig");
    expect(conflictCopyName("design.fig", 2)).toBe("design (conflict copy 2).fig");
    expect(conflictCopyName("notes", 1)).toBe("notes (conflict copy)");
  });

  it("keeps the copy name within 255 bytes, also for multi-byte characters", () => {
    const long = `${"ä".repeat(200)}.fig`;
    const copy = conflictCopyName(long, 3);
    expect(nameBytes(copy)).toBeLessThanOrEqual(255);
    expect(copy.endsWith(" (conflict copy 3).fig")).toBe(true);
  });
});
