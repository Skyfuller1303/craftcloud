// @vitest-environment node
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

// jsdom has no layout or style engine, so axe cannot check colour contrast
// there. This test computes WCAG 2.2 contrast ratios straight from the tokens.

type Rgb = [number, number, number];

function parseColor(value: string): { rgb: Rgb; alpha: number } {
  const hex = /^#([0-9a-f]{6})$/i.exec(value);
  if (hex?.[1] !== undefined) {
    const n = parseInt(hex[1], 16);
    return { rgb: [(n >> 16) & 255, (n >> 8) & 255, n & 255], alpha: 1 };
  }
  const fn = /^rgb\((\d+) (\d+) (\d+) \/ (\d+)%\)$/.exec(value);
  if (fn !== null) {
    return { rgb: [Number(fn[1]), Number(fn[2]), Number(fn[3])], alpha: Number(fn[4]) / 100 };
  }
  throw new Error(`unsupported colour ${value}`);
}

function mixinTokens(source: string, name: "light" | "dark"): Record<string, string> {
  const start = source.indexOf(`@mixin ${name} {`);
  const end = source.indexOf("\n}", start);
  const tokens: Record<string, string> = {};
  for (const match of source.slice(start, end).matchAll(/--([a-z-]+):\s*([^;]+);/g)) {
    tokens[match[1] ?? ""] = (match[2] ?? "").trim();
  }
  return tokens;
}

function over(foreground: string, background: Rgb): Rgb {
  const { rgb, alpha } = parseColor(foreground);
  return rgb.map((c, i) => c * alpha + (background[i] ?? 0) * (1 - alpha)) as Rgb;
}

function luminance([r, g, b]: Rgb): number {
  const lin = (c: number) => {
    const s = c / 255;
    return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}

function contrast(a: Rgb, b: Rgb): number {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return ((hi ?? 0) + 0.05) / ((lo ?? 0) + 0.05);
}

const scss = readFileSync(new URL("./_tokens.scss", import.meta.url), "utf8");

describe.each(["light", "dark"] as const)("%s theme contrast", (name) => {
  const tokens = mixinTokens(scss, name);
  const solid = (key: string, onto: Rgb) => over(tokens[key] ?? "", onto);
  const surfaces = ["bg", "sunken"] as const;

  it.each(surfaces)("body, muted and status text reach 4.5:1 on %s", (surface) => {
    const background = solid(surface, [0, 0, 0]);
    for (const key of ["ink", "ink-strong", "muted", "accent-ink", "good", "warn", "bad"]) {
      const ratio = contrast(solid(key, background), background);
      expect(ratio, `${key} on ${surface} (${name}): ${ratio.toFixed(2)}`).toBeGreaterThanOrEqual(4.5);
    }
  });

  it("primary button text reaches 4.5:1", () => {
    const ratio = contrast(solid("invert-fg", [0, 0, 0]), solid("invert-bg", [0, 0, 0]));
    expect(ratio).toBeGreaterThanOrEqual(4.5);
  });

  it.each(surfaces)("control borders and the focus ring reach 3:1 on %s (WCAG 1.4.11)", (surface) => {
    const background = solid(surface, solid("bg", [0, 0, 0]));
    for (const key of ["control-border", "accent-ink"]) {
      const ratio = contrast(solid(key, background), background);
      expect(ratio, `${key} on ${surface} (${name}): ${ratio.toFixed(2)}`).toBeGreaterThanOrEqual(3);
    }
  });
});
