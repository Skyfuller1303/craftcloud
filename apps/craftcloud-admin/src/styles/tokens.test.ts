// @vitest-environment node
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

// jsdom has no layout or style engine, so axe cannot check colour contrast
// there. This test computes WCAG 2.2 contrast ratios straight from the tokens.

type Rgb = [number, number, number];
type Color = { rgb: Rgb; alpha: number };

function parseColor(value: string): Color {
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

function over(foreground: Color, background: Rgb): Rgb {
  return foreground.rgb.map((c, i) => c * foreground.alpha + (background[i] ?? 0) * (1 - foreground.alpha)) as Rgb;
}

function mix(color: Rgb, background: Rgb, percent: number): Rgb {
  return color.map((c, i) => c * percent + (background[i] ?? 0) * (1 - percent)) as Rgb;
}

function luminance([r, g, b]: Rgb): number {
  const lin = (c: number) => {
    const s = c / 255;
    return s <= 0.04045 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
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
  const color = (key: string) => parseColor(tokens[key] ?? "");
  const solid = (key: string, onto: Rgb = [0, 0, 0]) => over(color(key), onto);
  const bg = solid("bg");
  const sunken = solid("sunken");
  const textPair = (foreground: string, background: Rgb, label: string) => {
    const ratio = contrast(solid(foreground, background), background);
    expect(ratio, `${foreground} on ${label} (${name}): ${ratio.toFixed(2)}`).toBeGreaterThanOrEqual(4.5);
  };

  it("body, heading, and muted text reach 4.5:1 on their documented surfaces", () => {
    for (const key of ["ink", "ink-strong", "muted"]) textPair(key, bg, "bg");
    for (const key of ["ink", "muted"]) textPair(key, sunken, "sunken");
  });

  it("primary and danger button text reach 4.5:1", () => {
    expect(contrast(solid("invert-fg"), solid("invert-bg"))).toBeGreaterThanOrEqual(4.5);
    const ratio = contrast(solid("danger-fg"), solid("bad"));
    expect(ratio, `danger-fg on bad (${name}): ${ratio.toFixed(2)}`).toBeGreaterThanOrEqual(4.5);
  });

  it("danger button hover and pressed text reach 4.5:1", () => {
    for (const background of [bg, sunken]) {
      const danger = solid("bad", background);
      for (const strength of [0.85, 0.9]) {
        const state = mix(danger, background, strength);
        expect(contrast(solid("danger-fg", state), state)).toBeGreaterThanOrEqual(4.5);
      }
    }
  });

  it("badge tones reach 4.5:1 on sunken", () => {
    for (const key of ["good", "warn", "bad", "accent-ink", "muted"]) textPair(key, sunken, "sunken");
  });

  it("links reach 4.5:1 on the page", () => textPair("accent-ink", bg, "bg"));

  it.each(["accent", "good", "warn", "bad"] as const)("ink reaches 4.5:1 on the %s banner tint", (tone) => {
    const strength = Number((tokens["tint-strength"] ?? "0%").replace("%", "")) / 100;
    const tinted = mix(color(tone).rgb, bg, strength);
    textPair("ink", tinted, `${tone} tint`);
  });

  it("control border reaches 3:1 on page and sunken surfaces", () => {
    for (const [label, surface] of [["bg", bg], ["sunken", sunken]] as const) {
      const border = solid("control-border", surface);
      expect(contrast(border, surface), `control-border on ${label} (${name})`).toBeGreaterThanOrEqual(3);
    }
  });

  it("focus ring reaches 3:1 on light, sunken, and inverted surfaces", () => {
    for (const surface of [bg, sunken, solid("invert-bg")]) {
      const ring = solid("focus-ink", surface);
      expect(contrast(ring, surface), `focus-ink (${name})`).toBeGreaterThanOrEqual(3);
    }
  });

  it("progress fill reaches 3:1 against its track", () => {
    const track = solid("progress-track", bg);
    const fill = solid("progress-fill", track);
    expect(contrast(fill, track), `progress-fill on track (${name})`).toBeGreaterThanOrEqual(3);
  });
});
