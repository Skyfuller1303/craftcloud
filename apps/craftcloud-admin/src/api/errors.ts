import type { Translate } from "../i18n/index.tsx";
import { ApiError, NETWORK_ERROR_CODE, isApiError } from "./client.ts";

/**
 * Human-readable text for a failed call. The server message is never shown
 * verbatim (it is English only and may change); the text is chosen by status
 * and machine-readable code. Callers pass `overrides` for codes with a
 * screen-specific meaning (for example INVALID_CREDENTIALS on the login form).
 */
export function describeError(
  t: Translate,
  error: unknown,
  overrides: Record<string, string> = {},
): string {
  if (!isApiError(error)) {
    return t("Something went wrong. Try again.");
  }
  const override = overrides[error.code];
  if (override !== undefined) {
    return override;
  }
  const reference =
    error.requestId === null ? "" : ` ${t("Reference: {id}", { id: error.requestId })}`;
  return `${baseMessage(t, error)}${reference}`;
}

function baseMessage(t: Translate, error: ApiError): string {
  if (error.code === NETWORK_ERROR_CODE) {
    return t("The server cannot be reached. Check the connection and try again.");
  }
  switch (error.status) {
    case 401:
      return t("Your session has ended. Log in again.");
    case 403:
      return error.code === "CSRF_FAILED"
        ? t("The request was rejected for security reasons. Reload the page and try again.")
        : t("You do not have permission to do this.");
    case 404:
      return t("The requested item was not found.");
    case 409:
      return t("This conflicts with the current state. Reload and try again.");
    case 413:
      return t("The request is too large.");
    case 422:
      return t("Some values are not valid. Check the marked fields.");
    case 429:
      return error.retryAfterSeconds === undefined
        ? t("Too many attempts. Wait a moment and try again.")
        : t("Too many attempts. Try again in {seconds} seconds.", {
            seconds: error.retryAfterSeconds,
          });
    case 500:
      return t("The server reported an internal error. Try again later.");
    case 503:
      return t("The service is temporarily unavailable. Try again later.");
    default:
      return t("Something went wrong. Try again.");
  }
}
