import { useEffect, useState } from "react";
import type { ReactNode } from "react";
import { Button, Dialog, Heading, Modal, ModalOverlay } from "react-aria-components";
import { useI18n } from "../i18n/index.tsx";
import { ErrorNotice } from "./ErrorNotice.tsx";

/**
 * Modal dialog (React Aria): focus moves into it, Tab stays inside, Escape and
 * a click outside close it unless `busy`, and focus returns to the control that
 * opened it. Render it only while it is open.
 *
 * When the opening control is gone afterwards (for example the row was removed),
 * focus would fall to the page body; it then moves to the main region instead.
 * If focus lands on the body although the opener still exists, it goes back to the opener.
 */
export function AppModal({
  title,
  onClose,
  busy = false,
  size = "md",
  children,
}: {
  title: string;
  onClose: () => void;
  busy?: boolean;
  size?: "sm" | "md" | "lg";
  children: ReactNode;
}) {
  // The control that had focus when the dialog opened (read during the first
  // render, before React Aria moves focus into the dialog).
  const [opener] = useState(() => document.activeElement);
  useEffect(
    () => () => {
      window.setTimeout(() => {
        const active = document.activeElement;
        if (active === null || active === document.body) {
          // React Aria's own restore can lose a race with the unmount; prefer the
          // opener while it is still on the page, the main region otherwise.
          if (opener instanceof HTMLElement && opener.isConnected) {
            opener.focus();
          } else {
            document.getElementById("main")?.focus();
          }
        }
      }, 80);
    },
    [opener],
  );
  return (
    <ModalOverlay
      className="overlay"
      isOpen
      isDismissable={!busy}
      isKeyboardDismissDisabled={busy}
      onOpenChange={(open) => {
        if (!open && !busy) {
          onClose();
        }
      }}
    >
      <Modal className={`modal modal--${size}`}>
        <Dialog className="modal__dialog">
          <Heading slot="title" className="modal__title">
            {title}
          </Heading>
          {children}
        </Dialog>
      </Modal>
    </ModalOverlay>
  );
}

/** Confirmation with a consequence the user must accept; the error stays inside the dialog. */
export function ConfirmDialog({
  title,
  message,
  confirmLabel,
  busy,
  error,
  danger = false,
  onConfirm,
  onClose,
  onReload,
}: {
  title: string;
  message: string;
  confirmLabel: string;
  busy: boolean;
  error: unknown;
  danger?: boolean;
  onConfirm: () => void;
  onClose: () => void;
  onReload?: () => void;
}) {
  const { t } = useI18n();
  return (
    <AppModal title={title} onClose={onClose} busy={busy} size="sm">
      <p>{message}</p>
      {error !== null && <ErrorNotice error={error} onRetry={onConfirm} onReload={onReload} />}
      <div className="form__actions">
        <Button
          className="button"
          onPress={onClose}
          isDisabled={busy}
        >
          {t("Cancel")}
        </Button>
        <Button
          className={`button ${danger ? "button--danger" : "button--primary"}`}
          onPress={onConfirm}
          isDisabled={busy}
        >
          {confirmLabel}
        </Button>
      </div>
    </AppModal>
  );
}
