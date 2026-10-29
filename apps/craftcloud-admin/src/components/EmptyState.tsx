import type { ReactNode } from "react";

export function EmptyState({ children, action }: { children: ReactNode; action?: ReactNode }) {
  return (
    <div className="empty">
      <p className="empty__text muted">{children}</p>
      {action !== undefined && <div className="empty__action">{action}</div>}
    </div>
  );
}
