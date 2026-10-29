import { useEffect, useRef } from "react";

export interface ToastMessage { id: number; message: string; }

function Toast({ toast, onDismiss }: { toast: ToastMessage; onDismiss: (id: number) => void }) {
  const timer = useRef<number | undefined>(undefined);
  const startTimer = () => {
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => onDismiss(toast.id), 4500);
  };
  useEffect(() => {
    startTimer();
    return () => window.clearTimeout(timer.current);
  }, [toast.id]);
  return (
    <div
      className="toast"
      onClick={() => onDismiss(toast.id)}
      onPointerEnter={() => window.clearTimeout(timer.current)}
      onPointerLeave={startTimer}
    >
      <span aria-hidden="true" className="toast__mark" />
      <span>{toast.message}</span>
    </div>
  );
}

export function Toaster({ toasts, onDismiss }: { toasts: ToastMessage[]; onDismiss: (id: number) => void }) {
  if (toasts.length === 0) return null;
  return (
    <div className="toasts" aria-hidden="true">
      {toasts.slice(-4).map((toast) => <Toast key={toast.id} toast={toast} onDismiss={onDismiss} />)}
    </div>
  );
}
