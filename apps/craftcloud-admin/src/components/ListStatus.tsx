import { Button } from "react-aria-components";
import { useI18n } from "../i18n/index.tsx";
import type { PagedList } from "../usePagedList.ts";
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
  const { t } = useI18n();
  return (
    <>
      {list.status === "loading" && (
        <p role="status" className="muted">
          {loadingLabel}
        </p>
      )}
      {list.status === "error" && (
        <ErrorNotice error={list.error} onRetry={list.reload} onReload={list.reload} />
      )}
      {list.status === "ready" && list.items.length === 0 && <p className="muted">{empty}</p>}
      {list.hasMore && (
        <Button className="button" onPress={list.loadMore} isDisabled={list.loadingMore}>
          {list.loadingMore ? t("Loading…") : t("Load more")}
        </Button>
      )}
    </>
  );
}
