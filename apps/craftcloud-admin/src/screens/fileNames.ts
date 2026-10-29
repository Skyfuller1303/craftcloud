// Helpers for names of files the user uploads. The server validates names
// again (NFC, control characters, device names, 1 to 255 bytes); these only
// derive sensible defaults and catch the obvious cases early.

export const DEFAULT_APP = "generic";
export const APP_PATTERN = /^[a-z0-9-]{1,32}$/;
export const FORMAT_PATTERN = /^[a-z0-9.+-]{1,64}$/;
export const MAX_NAME_BYTES = 255;

export function nameBytes(name: string): number {
  return new TextEncoder().encode(name).length;
}

/** Lowercased last extension of `name` if it fits the format pattern, otherwise `bin`. */
export function formatOf(name: string): string {
  const match = /\.([A-Za-z0-9+-]{1,64})$/.exec(name);
  const candidate = match?.[1]?.toLowerCase();
  return candidate !== undefined && FORMAT_PATTERN.test(candidate) ? candidate : "bin";
}

/**
 * Name for the copy kept next to a file somebody else changed meanwhile:
 * `design.fig` becomes `design (conflict copy).fig`, then `design (conflict copy 2).fig`.
 * The stem is shortened so the result stays within 255 bytes.
 */
export function conflictCopyName(name: string, attempt: number): string {
  const dot = name.lastIndexOf(".");
  const hasExtension = dot > 0 && name.length - dot <= 65;
  const stem = hasExtension ? name.slice(0, dot) : name;
  const extension = hasExtension ? name.slice(dot) : "";
  const suffix = attempt <= 1 ? " (conflict copy)" : ` (conflict copy ${attempt})`;
  let shortened = [...stem];
  while (
    shortened.length > 1 &&
    nameBytes(`${shortened.join("")}${suffix}${extension}`) > MAX_NAME_BYTES
  ) {
    shortened = shortened.slice(0, -1);
  }
  return `${shortened.join("")}${suffix}${extension}`;
}
