import type { ReactNode } from "react";

export type IconName = "chevron-down" | "close" | "check" | "external" | "copy" | "status";

const PATHS: Record<IconName, ReactNode> = {
  "chevron-down": <path d="M4 6l4 4 4-4" />,
  close: <path d="M3.5 3.5l9 9M12.5 3.5l-9 9" />,
  check: <path d="M3 8.5l3 3 7-7" />,
  external: <path d="M6 3h7v7M13 3L5 11" />,
  copy: <><path d="M5.5 5.5h7v7h-7z" /><path d="M3.5 10.5v-7h7" /></>,
  status: <rect x="5" y="5" width="6" height="6" fill="currentColor" stroke="none" />,
};

export function Icon({ name, size = 16 }: { name: IconName; size?: 12 | 16 }) {
  return (
    <svg
      className={`icon icon--${name}`}
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.5}
      strokeLinecap="square"
      aria-hidden="true"
      focusable="false"
    >
      {PATHS[name]}
    </svg>
  );
}
