import { Button } from "react-aria-components";
import { useI18n } from "../i18n/index.tsx";
import type { PagedList } from "../usePagedList.ts";
import { EmptyState } from "./EmptyState.tsx";
import { ErrorNotice } from "./ErrorNotice.tsx";

/**
 * Loading, error, empty and "load more" states of a paged list. The table
 * itself is drawn by the caller when `list.items` is not empty.
 */
export function ListStatus<T>({
  list,
  empty,
  loadingLabel,
}: {
  list: PagedList<T>;
  empty: string;
  loadingLabel: string;
}) {
  return (
    <div className="list-status">
      {list.status === "loading" && (
        <div className="list-status__loading">
          <div className="progress progress--indeterminate" aria-hidden="true"><progress /></div>
          <p role="status" className="muted">{loadingLabel}</p>
        </div>
      )}
      {list.status === "error" && (
        <ErrorNotice error={list.error} onRetry={list.reload} onReload={list.reload} />
      )}
      {list.status === "ready" && list.items.length === 0 && <EmptyState>{empty}</EmptyState>}
    </div>
  );
}

export function LoadMoreButton<T>({ list }: { list: PagedList<T> }) {
  const { t } = useI18n();
  if (!list.hasMore) return null;
  return (
    <div className="list-status__more">
      <Button className="button button--quiet" onPress={list.loadMore} isDisabled={list.loadingMore}>
        {list.loadingMore ? t("Loading…") : t("Load more")}
      </Button>
    </div>
  );
}
