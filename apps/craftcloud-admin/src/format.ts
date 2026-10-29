// Locale-aware display of dates and sizes. Both take the UI language so the
// output follows the language switch rather than the browser setting.

export function formatDateTime(language: string, iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) {
    return iso;
  }
  try {
    return new Intl.DateTimeFormat(language, { dateStyle: "medium", timeStyle: "short" }).format(date);
  } catch {
    return date.toISOString();
  }
}

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB"] as const;

export function formatBytes(language: string, bytes: number): string {
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const digits = unit === 0 || value >= 100 ? 0 : 1;
  let number: string;
  try {
    number = new Intl.NumberFormat(language, { maximumFractionDigits: digits }).format(value);
  } catch {
    number = value.toFixed(digits);
  }
  return `${number} ${UNITS[unit]}`;
}
