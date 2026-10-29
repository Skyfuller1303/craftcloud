import { useEffect, useRef } from "react";
import { Button } from "react-aria-components";
import { isApiError } from "../api/client.ts";
import { classifyError, describeError } from "../api/errors.ts";
import { useCountdown } from "../hooks.ts";
import { useI18n } from "../i18n/index.tsx";
import { useAnnounce } from "./Announcer.tsx";
import { Alert } from "./forms.tsx";

/**
 * The one place that turns a failed request into text and recovery actions
 * (CC-P03-04):
 *
 * - 401: the app already returned to the login form (`onUnauthenticated`); the
 *   message is only a fallback.
 * - 403, 404, 409, 410, 412: the data on screen is probably stale, so the
 *   action is "Reload".
 * - 413, 422, 400, 428 and local cancellations: the input has to change; there
 *   is nothing to retry.
 * - 429: "Try again" stays disabled while the `Retry-After` countdown runs.
 * - 5xx and "no answer": "Try again".
 *
 * The message is a `role="alert"` element; the buttons sit next to it so that
 * a changing countdown label does not make a screen reader repeat the alert.
 */
export function ErrorNotice({
  error,
  onRetry,
  onReload,
  overrides = {},
}: {
  error: unknown;
  /** Repeats the failed action. */
  onRetry?: (() => void) | undefined;
  /** Re-reads the data shown (offered for stale-data errors). */
  onReload?: (() => void) | undefined;
  overrides?: Record<string, string>;
}) {
  const { t } = useI18n();
  const announce = useAnnounce();
  const kind = classifyError(error);
  const seconds = isApiError(error) && kind === "rate-limited" ? error.retryAfterSeconds : undefined;
  const left = useCountdown(seconds, error);
  const counting = seconds !== undefined && left > 0;
  const wasCounting = useRef(false);

  useEffect(() => {
    if (wasCounting.current && !counting) {
      announce(t("You can try again now."));
    }
    wasCounting.current = counting;
  }, [counting, announce, t]);

  if (kind === "cancelled") {
    return null;
  }

  const text =
    kind === "rate-limited" && isApiError(error) && overrides[error.code] === undefined
      ? t("Too many requests. Wait a moment before trying again.")
      : describeError(t, error, overrides);

  const staleData = kind === "forbidden" || kind === "missing" || kind === "conflict" || kind === "precondition";
  const canRetry = kind === "rate-limited" || kind === "unavailable" || kind === "offline" || kind === "server";

  return (
    <div className="error-notice">
      <Alert>{text}</Alert>
      {(staleData || canRetry) && (
        <div className="error-notice__actions">
          {staleData && onReload !== undefined && (
            <Button className="button" onPress={onReload}>
              {t("Reload")}
            </Button>
          )}
          {canRetry && onRetry !== undefined && (
            <Button className="button" onPress={onRetry} isDisabled={counting}>
              {counting ? t("Try again in {seconds} s", { seconds: left }) : t("Try again")}
            </Button>
          )}
        </div>
      )}
    </div>
  );
}
