import { Component, Suspense } from "react";
import type { ErrorInfo, ReactNode } from "react";
import { Button } from "react-aria-components";
import { useI18n } from "../i18n/index.tsx";
import { Alert } from "./forms.tsx";

/** Accessible loading state shown while a lazily loaded screen is fetched. */
export function ScreenLoading() {
  const { t } = useI18n();
  return (
    <div className="list-status__loading">
      <div className="progress progress--indeterminate" aria-hidden="true"><progress /></div>
      <p role="status" className="muted">{t("Loading…")}</p>
    </div>
  );
}

function LoadFailed() {
  const { t } = useI18n();
  return (
    <div className="stack">
      <Alert>{t("Something went wrong. Try again.")}</Alert>
      <div>
        <Button className="button" onPress={() => window.location.reload()}>{t("Reload")}</Button>
      </div>
    </div>
  );
}

/**
 * Catches a failed chunk download (for example while offline). A rejected
 * lazy import stays rejected, so the only recovery is reloading the page.
 */
class LoadBoundary extends Component<{ children: ReactNode }, { failed: boolean }> {
  override state = { failed: false };

  static getDerivedStateFromError(): { failed: boolean } {
    return { failed: true };
  }

  override componentDidCatch(_error: Error, _info: ErrorInfo): void {
    // The error carries no request data; the notice below is the whole report.
  }

  override render(): ReactNode {
    return this.state.failed ? <LoadFailed /> : this.props.children;
  }
}

/** Suspense with the shared loading state plus a boundary for failed chunk loads. */
export function ScreenBoundary({ children }: { children: ReactNode }) {
  return (
    <LoadBoundary>
      <Suspense fallback={<ScreenLoading />}>{children}</Suspense>
    </LoadBoundary>
  );
}
