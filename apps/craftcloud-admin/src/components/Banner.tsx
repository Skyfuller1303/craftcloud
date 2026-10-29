import type { ReactNode } from "react";
import { useI18n } from "../i18n/index.tsx";
import { Icon } from "./Icon.tsx";

export type BannerTone = "neutral" | "bad" | "warn" | "good" | "info";

export function Banner({
  tone = "bad",
  title,
  children,
  actions,
  onDismiss,
  role,
}: {
  tone?: BannerTone;
  title?: string;
  children: ReactNode;
  actions?: ReactNode;
  onDismiss?: () => void;
  role?: "alert" | "status";
}) {
  const { t } = useI18n();
  return (
    <div className={`banner banner--${tone}`} role={role ?? (tone === "bad" ? "alert" : "status")}>
      <div className="banner__content">
        {title !== undefined && <strong className="banner__title">{title}</strong>}
        {children}
      </div>
      {actions !== undefined && <div className="banner__actions">{actions}</div>}
      {onDismiss !== undefined && (
        <button type="button" className="icon-button icon-button--sm" aria-label={t("Close")} onClick={onDismiss}>
          <Icon name="close" size={12} />
        </button>
      )}
    </div>
  );
}
