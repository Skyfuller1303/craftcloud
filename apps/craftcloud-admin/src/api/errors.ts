import type { Translate } from "../i18n/index.tsx";
import { ApiError, NETWORK_ERROR_CODE, ABORTED_ERROR_CODE, isApiError } from "./client.ts";

/**
 * Human-readable text for a failed call. The server message is never shown
 * verbatim (it is English only and may change); the text is chosen by
 * machine-readable code first and by status second. Callers pass `overrides`
 * for codes with a screen-specific meaning (for example INVALID_CREDENTIALS on
 * the login form).
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
  return `${codeMessage(t, error) ?? baseMessage(t, error)}${reference}`;
}

/** Error kinds the UI treats alike; see {@link classifyError}. */
export type ErrorKind =
  | "session" // 401: the session ended, the app returns to the login form
  | "forbidden" // 403
  | "missing" // 404, 410
  | "conflict" // 409
  | "precondition" // 412 (somebody else changed the item)
  | "too-large" // 413, 507
  | "invalid" // 400, 422, 428
  | "rate-limited" // 429
  | "unavailable" // 502, 503, 504
  | "offline" // no HTTP answer
  | "cancelled"
  | "server"; // 500 and everything else

export function classifyError(error: unknown): ErrorKind {
  if (!isApiError(error)) {
    return "server";
  }
  if (error.code === ABORTED_ERROR_CODE) {
    return "cancelled";
  }
  if (error.code === NETWORK_ERROR_CODE || error.status === 0) {
    return "offline";
  }
  switch (error.status) {
    case 401:
      return "session";
    case 403:
      return "forbidden";
    case 404:
    case 410:
      return "missing";
    case 409:
      return "conflict";
    case 412:
      return "precondition";
    case 413:
    case 507:
      return "too-large";
    case 400:
    case 422:
    case 428:
      return "invalid";
    case 429:
      return "rate-limited";
    case 502:
    case 503:
    case 504:
      return "unavailable";
    default:
      return "server";
  }
}

function codeMessage(t: Translate, error: ApiError): string | undefined {
  switch (error.code) {
    case "QUOTA_EXCEEDED":
      return t("The storage quota is used up. Free space or ask an administrator for more.");
    case "PAYLOAD_TOO_LARGE":
      return t("This file is larger than the server accepts.");
    case "INSUFFICIENT_STORAGE":
      return t("The server has no space left to store this file.");
    case "STORAGE_UNAVAILABLE":
      return t("File storage is not available on this server.");
    case "PROJECT_NAME_TAKEN":
      return t("A project with this name already exists.");
    case "FILE_NAME_TAKEN":
      return t("A file with this name already exists in this project.");
    case "PROJECT_DELETED":
      return t("The project is in the trash. Restore it first.");
    case "FILE_DELETED":
      return t("The file is in the trash. Restore it first.");
    case "RETENTION_EXPIRED":
      return t("The retention period has passed; this item can no longer be restored.");
    case "LAST_OWNER":
      return t("An organization needs at least one active owner.");
    case "ALREADY_MEMBER":
      return t("This person is already a member.");
    case "INVITATION_PENDING":
      return t("This address already has an open invitation.");
    case "INVITATION_LIMIT":
      return t("There are too many open invitations. Revoke some first.");
    case "UPLOAD_LIMIT":
      return t("Too many uploads are open at the moment. Finish or cancel some first.");
    case "UPLOAD_EXPIRED":
      return t("The upload took too long and expired. Start it again.");
    case "UPLOAD_CLOSED":
    case "UPLOAD_IN_PROGRESS":
      return t("This upload cannot continue. Start it again.");
    case "CHECKSUM_MISMATCH":
    case "LENGTH_MISMATCH":
      return t("The file changed while it was being uploaded. Start the upload again.");
    case "REQUEST_TIMEOUT":
      return t("The upload stalled and was stopped. Check the connection and start it again.");
    case "UPLOAD_INTERRUPTED":
      return t("The upload was interrupted. Start it again.");
    case "BLOB_CORRUPT":
      return t("The stored content failed verification. Ask an administrator.");
    case "VERSION_CONFLICT":
      return t("Somebody else changed this item in the meantime. Reload and try again.");
    case "INVALID_RESET_TOKEN":
      return t("This reset link is not valid any more. Ask for a new one.");
    case "TWO_FACTOR_SETUP_PENDING":
      return t("Another session is already setting up two-factor authentication. Finish it there or wait a few minutes.");
    default:
      return undefined;
  }
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
    case 408:
      return t("The request took too long. Try again.");
    case 409:
      return t("This conflicts with the current state. Reload and try again.");
    case 410:
      return t("This item is gone.");
    case 412:
      return t("Somebody else changed this item in the meantime. Reload and try again.");
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
    case 507:
      return t("The server has no space left to store this file.");
    default:
      return t("Something went wrong. Try again.");
  }
}
