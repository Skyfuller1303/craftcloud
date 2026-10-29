import type { ReactNode } from "react";

export type BadgeTone = "neutral" | "good" | "warn" | "bad" | "info";

export function Badge({ tone = "neutral", children }: { tone?: BadgeTone; children: ReactNode }) {
  return <span className={`badge badge--${tone}`}>{children}</span>;
}
