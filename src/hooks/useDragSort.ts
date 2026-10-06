import { useCallback, useMemo } from "react";
import {
  KeyboardSensor,
  PointerSensor,
  useSensor,
  useSensors,
  type DragEndEvent,
} from "@dnd-kit/core";
import { arrayMove, sortableKeyboardCoordinates } from "@dnd-kit/sortable";
import { useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { useTranslation } from "react-i18next";
import type { Provider } from "@/types";
import { providersApi, type AppId } from "@/lib/api";
import { isProxyAppId } from "@/config/appConfig";

// SQLite's default BINARY collation compares UTF-8 IDs. Comparing Unicode
// code points gives the same order, including IDs outside the BMP.
const compareProviderIds = (a: string, b: string): number => {
  const left = Array.from(a);
  const right = Array.from(b);
  for (let index = 0; index < Math.min(left.length, right.length); index++) {
    const difference =
      left[index].codePointAt(0)! - right[index].codePointAt(0)!;
    if (difference !== 0) return difference;
  }
  return left.length - right.length;
};

export function useDragSort(providers: Record<string, Provider>, appId: AppId) {
  const queryClient = useQueryClient();
  const { t, i18n } = useTranslation();

  const sortedProviders = useMemo(() => {
    if (appId !== "codex") {
      const locale =
        i18n.language === "zh"
          ? "zh-CN"
          : i18n.language === "zh-TW"
            ? "zh-TW"
            : "en-US";
      return Object.values(providers).sort((a, b) => {
        if (a.sortIndex !== undefined && b.sortIndex !== undefined) {
          return a.sortIndex - b.sortIndex;
        }
        if (a.sortIndex !== undefined) return -1;
        if (b.sortIndex !== undefined) return 1;

        const timeA = a.createdAt ?? 0;
        const timeB = b.createdAt ?? 0;
        if (timeA && timeB && timeA !== timeB) {
          return timeA - timeB;
        }

        return a.name.localeCompare(b.name, locale);
      });
    }
    return Object.values(providers).sort((a, b) => {
      // Match get_all_providers exactly, so the visible account order is the
      // same order used by the independent native Codex monitor.
      const indexA = a.sortIndex ?? 999999;
      const indexB = b.sortIndex ?? 999999;
      if (indexA !== indexB) {
        return indexA - indexB;
      }

      // SQLite sorts NULL first. An explicit 0 is a timestamp, not missing.
      const timeA = a.createdAt;
      const timeB = b.createdAt;
      if (timeA == null && timeB != null) return -1;
      if (timeB == null && timeA != null) return 1;
      if (timeA != null && timeB != null && timeA !== timeB) {
        return timeA - timeB;
      }

      return compareProviderIds(a.id, b.id);
    });
  }, [providers, appId, i18n.language]);

  const sensors = useSensors(
    useSensor(PointerSensor, {
      activationConstraint: { distance: 8 },
    }),
    useSensor(KeyboardSensor, {
      coordinateGetter: sortableKeyboardCoordinates,
    }),
  );

  const handleDragEnd = useCallback(
    async (event: DragEndEvent) => {
      const { active, over } = event;
      if (!over || active.id === over.id) {
        return;
      }

      const oldIndex = sortedProviders.findIndex(
        (provider) => provider.id === active.id,
      );
      const newIndex = sortedProviders.findIndex(
        (provider) => provider.id === over.id,
      );

      if (oldIndex === -1 || newIndex === -1) {
        return;
      }

      const reordered = arrayMove(sortedProviders, oldIndex, newIndex);
      const updates = reordered.map((provider, index) => ({
        id: provider.id,
        sortIndex: index,
      }));

      try {
        await providersApi.updateSortOrder(updates, appId);
        await queryClient.invalidateQueries({
          queryKey: ["providers", appId],
        });

        // Routing apps derive failover order from sort_index.
        if (isProxyAppId(appId)) {
          await queryClient.invalidateQueries({
            queryKey: ["failoverQueue", appId],
          });
        }

        // 更新托盘菜单以反映新的排序（失败不影响主操作）
        try {
          await providersApi.updateTrayMenu();
        } catch (trayError) {
          console.error("Failed to update tray menu after sort", trayError);
          // 托盘菜单更新失败不影响排序成功
        }

        toast.success(
          t("provider.sortUpdated", {
            defaultValue: "排序已更新",
          }),
          { closeButton: true },
        );
      } catch (error) {
        console.error("Failed to update provider sort order", error);
        toast.error(
          t("provider.sortUpdateFailed", {
            defaultValue: "排序更新失败",
          }),
        );
      }
    },
    [sortedProviders, appId, queryClient, t],
  );

  return {
    sortedProviders,
    sensors,
    handleDragEnd,
  };
}
