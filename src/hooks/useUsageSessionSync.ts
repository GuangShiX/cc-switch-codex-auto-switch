import { useCallback, useEffect } from "react";
import {
  useQuery,
  useQueryClient,
  type QueryClient,
} from "@tanstack/react-query";
import { usageApi } from "@/lib/api/usage";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { usageKeys } from "@/lib/query/usage";
import type { SessionSyncResult } from "@/types/usage";

const SESSION_SYNC_INTERVAL_MS = 5 * 60 * 1000;
// Keep the import outside `usageKeys.all`: refreshing database views must not
// recursively trigger another file scan. Each client owns its cooldown/cache.
const SESSION_SYNC_KEY = ["usage-session-import"] as const;
const lastCompletions = new WeakMap<QueryClient, number>();

/** Import on entry, then at most every five minutes while this page is visible. */
export function useUsageSessionSync(backgroundSyncEnabled: boolean) {
  const queryClient = useQueryClient();
  const importUsage = useCallback(async () => {
    try {
      const result = await usageApi.syncSessionUsage();
      await queryClient.invalidateQueries({ queryKey: usageKeys.all });
      return result;
    } finally {
      // A first import can take minutes. Its completion, including failure,
      // starts the cooldown; unmounting never cancels/requeues that import.
      lastCompletions.set(queryClient, Date.now());
    }
  }, [queryClient]);
  const query = useQuery<SessionSyncResult>({
    queryKey: SESSION_SYNC_KEY,
    queryFn: importUsage,
    enabled: false,
    retry: false,
    gcTime: Infinity,
    refetchOnMount: false,
    refetchOnWindowFocus: false,
    refetchOnReconnect: false,
  });

  const sync = useCallback(() => {
    // fetchQuery shares an in-flight import across StrictMode and remounts.
    return queryClient.fetchQuery({
      queryKey: SESSION_SYNC_KEY,
      queryFn: importUsage,
      staleTime: 0,
      retry: false,
      gcTime: Infinity,
    });
  }, [importUsage, queryClient]);

  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    let disposed = false;
    let visibilityRevision = 0;
    const isVisible = () => document.visibilityState !== "hidden";
    const isFetching = () =>
      queryClient.getQueryState(SESSION_SYNC_KEY)?.fetchStatus === "fetching";
    const canSync = () => {
      const lastCompletion = lastCompletions.get(queryClient);
      return (
        !disposed &&
        isVisible() &&
        !isFetching() &&
        (lastCompletion === undefined ||
          Date.now() - lastCompletion >= SESSION_SYNC_INTERVAL_MS)
      );
    };
    const maybeSync = async () => {
      if (!canSync()) return;
      const revision = visibilityRevision;
      try {
        // WebView2 can report document.visible while Tauri is in the tray.
        // Read native state only on entry and the five-minute deadline.
        const window = getCurrentWindow();
        const [visible, minimized] = await Promise.all([
          window.isVisible(),
          window.isMinimized(),
        ]);
        if (
          !visible ||
          minimized ||
          revision !== visibilityRevision ||
          !canSync()
        )
          return;
      } catch {
        // An unknown window state must not start a file scan. Explicit manual
        // refresh remains available and bypasses this automatic-only gate.
        return;
      }
      // Recheck after native reads: another page instance/manual action can
      // have started an import while the window state was being resolved.
      void sync().catch(() => {});
    };
    const schedule = (delay = SESSION_SYNC_INTERVAL_MS) => {
      clearTimeout(timer);
      if (!backgroundSyncEnabled && isVisible() && !isFetching()) {
        timer = setTimeout(() => {
          void maybeSync();
          schedule();
        }, delay);
      }
    };
    const onVisibilityChange = () => {
      visibilityRevision += 1;
      // Returning from the tray starts a new interval; no overdue scans queue.
      schedule();
    };

    void maybeSync();
    const lastCompletion = lastCompletions.get(queryClient);
    schedule(
      lastCompletion === undefined
        ? SESSION_SYNC_INTERVAL_MS
        : Math.max(1, SESSION_SYNC_INTERVAL_MS - (Date.now() - lastCompletion)),
    );
    document.addEventListener("visibilitychange", onVisibilityChange);
    return () => {
      disposed = true;
      clearTimeout(timer);
      document.removeEventListener("visibilitychange", onVisibilityChange);
    };
  }, [
    backgroundSyncEnabled,
    queryClient,
    sync,
    query.isFetching,
    query.dataUpdatedAt,
    query.errorUpdatedAt,
  ]);

  return {
    isSyncing: query.isFetching,
    result: query.data,
    error: query.error,
    refresh: sync,
  };
}
