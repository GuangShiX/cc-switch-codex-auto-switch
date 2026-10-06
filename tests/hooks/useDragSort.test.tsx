import type { ReactNode } from "react";
import { renderHook, act } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { describe, expect, it, vi, beforeEach, afterAll } from "vitest";
import type { Provider } from "@/types";
import { useDragSort } from "@/hooks/useDragSort";

const updateSortOrderMock = vi.fn();
const toastSuccessMock = vi.fn();
const toastErrorMock = vi.fn();
const consoleErrorSpy = vi.spyOn(console, "error").mockImplementation(() => {});

vi.mock("sonner", () => ({
  toast: {
    success: (...args: unknown[]) => toastSuccessMock(...args),
    error: (...args: unknown[]) => toastErrorMock(...args),
  },
}));

vi.mock("@/lib/api", () => ({
  providersApi: {
    updateSortOrder: (...args: unknown[]) => updateSortOrderMock(...args),
  },
}));

interface WrapperProps {
  children: ReactNode;
}

function createWrapper() {
  const queryClient = new QueryClient();

  const wrapper = ({ children }: WrapperProps) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );

  return { wrapper, queryClient };
}

const mockProviders: Record<string, Provider> = {
  a: {
    id: "a",
    name: "AAA",
    settingsConfig: {},
    sortIndex: 1,
    createdAt: 5,
  },
  b: {
    id: "b",
    name: "BBB",
    settingsConfig: {},
    sortIndex: 0,
    createdAt: 10,
  },
  c: {
    id: "c",
    name: "CCC",
    settingsConfig: {},
    createdAt: 1,
  },
};

describe("useDragSort", () => {
  beforeEach(() => {
    updateSortOrderMock.mockReset();
    toastSuccessMock.mockReset();
    toastErrorMock.mockReset();
    consoleErrorSpy.mockClear();
  });

  afterAll(() => {
    consoleErrorSpy.mockRestore();
  });

  it("keeps distinct sortIndex values in the same visible order", () => {
    const { wrapper } = createWrapper();

    const { result } = renderHook(() => useDragSort(mockProviders, "claude"), {
      wrapper,
    });

    expect(result.current.sortedProviders.map((item) => item.id)).toEqual([
      "b",
      "a",
      "c",
    ]);
  });

  it("preserves Claude name tie-breaking while applying database ID order only to Codex", () => {
    const providers: Record<string, Provider> = {
      z: { id: "z", name: "AAA", settingsConfig: {}, createdAt: 10 },
      a: { id: "a", name: "ZZZ", settingsConfig: {}, createdAt: 10 },
    };
    const { wrapper } = createWrapper();
    const { result, rerender } = renderHook(
      ({ appId }: { appId: "claude" | "codex" }) =>
        useDragSort(providers, appId),
      { wrapper, initialProps: { appId: "claude" } },
    );
    expect(
      result.current.sortedProviders.map((provider) => provider.id),
    ).toEqual(["z", "a"]);
    rerender({ appId: "codex" });
    expect(
      result.current.sortedProviders.map((provider) => provider.id),
    ).toEqual(["a", "z"]);
  });

  it("orders tied sortIndex values by creation time and binary ID, never account name", () => {
    const providers = Object.fromEntries(
      [
        { id: "z", name: "AAA", sortIndex: 2, createdAt: 10 },
        { id: "a", name: "ZZZ", sortIndex: 2, createdAt: 10 },
        { id: "older", name: "ZZZ", sortIndex: 2, createdAt: 1 },
      ].map((provider) => [provider.id, { ...provider, settingsConfig: {} }]),
    );
    const { wrapper } = createWrapper();
    const { result } = renderHook(() => useDragSort(providers, "codex"), {
      wrapper,
    });
    expect(
      result.current.sortedProviders.map((provider) => provider.id),
    ).toEqual(["older", "a", "z"]);
  });

  it("matches SQLite NULL-first creation times while retaining explicit zero", () => {
    const providers: Record<string, Provider> = {
      positive: {
        id: "positive",
        name: "A",
        settingsConfig: {},
        sortIndex: 2,
        createdAt: 10,
      },
      zero: {
        id: "zero",
        name: "B",
        settingsConfig: {},
        sortIndex: 2,
        createdAt: 0,
      },
      negative: {
        id: "negative",
        name: "C",
        settingsConfig: {},
        sortIndex: 2,
        createdAt: -1,
      },
      missing: { id: "missing", name: "Z", settingsConfig: {}, sortIndex: 2 },
    };
    const { wrapper } = createWrapper();
    const { result } = renderHook(() => useDragSort(providers, "codex"), {
      wrapper,
    });
    expect(
      result.current.sortedProviders.map((provider) => provider.id),
    ).toEqual(["missing", "negative", "zero", "positive"]);
  });

  it("matches the database sortIndex fallback and UTF-8 binary ID ordering", () => {
    const providers: Record<string, Provider> = Object.fromEntries(
      [
        { id: "😀", name: "AAA", sortIndex: 999999, createdAt: 1 },
        { id: "", name: "BBB", sortIndex: 999999, createdAt: 1 },
        { id: "a", name: "ZZZ", createdAt: 1 },
        { id: "Z", name: "ZZZ", sortIndex: 999999, createdAt: 1 },
        { id: "last", name: "AAA", sortIndex: 1000000, createdAt: 0 },
      ].map((provider) => [provider.id, { ...provider, settingsConfig: {} }]),
    );
    const { wrapper } = createWrapper();
    const { result } = renderHook(() => useDragSort(providers, "codex"), {
      wrapper,
    });
    expect(
      result.current.sortedProviders.map((provider) => provider.id),
    ).toEqual(["Z", "a", "", "😀", "last"]);
  });

  it("should call API and invalidate query cache after successful drag", async () => {
    updateSortOrderMock.mockResolvedValue(true);
    const { wrapper, queryClient } = createWrapper();
    const invalidateSpy = vi.spyOn(queryClient, "invalidateQueries");

    const { result } = renderHook(() => useDragSort(mockProviders, "claude"), {
      wrapper,
    });

    await act(async () => {
      await result.current.handleDragEnd({
        active: { id: "b" },
        over: { id: "a" },
      } as any);
    });

    expect(updateSortOrderMock).toHaveBeenCalledTimes(1);
    expect(updateSortOrderMock).toHaveBeenCalledWith(
      [
        { id: "a", sortIndex: 0 },
        { id: "b", sortIndex: 1 },
        { id: "c", sortIndex: 2 },
      ],
      "claude",
    );
    expect(invalidateSpy).toHaveBeenCalledWith({
      queryKey: ["providers", "claude"],
    });
    expect(toastSuccessMock).toHaveBeenCalledTimes(1);
    expect(toastErrorMock).not.toHaveBeenCalled();
  });

  it("should show error toast when drag operation fails", async () => {
    updateSortOrderMock.mockRejectedValue(new Error("network"));
    const { wrapper } = createWrapper();

    const { result } = renderHook(() => useDragSort(mockProviders, "claude"), {
      wrapper,
    });

    await act(async () => {
      await result.current.handleDragEnd({
        active: { id: "b" },
        over: { id: "a" },
      } as any);
    });

    expect(toastErrorMock).toHaveBeenCalledTimes(1);
    expect(toastSuccessMock).not.toHaveBeenCalled();
    expect(consoleErrorSpy).toHaveBeenCalled();
  });

  it("should not trigger API call when there is no valid target", async () => {
    const { wrapper } = createWrapper();

    const { result } = renderHook(() => useDragSort(mockProviders, "claude"), {
      wrapper,
    });

    await act(async () => {
      await result.current.handleDragEnd({
        active: { id: "b" },
        over: null,
      } as any);
    });

    expect(updateSortOrderMock).not.toHaveBeenCalled();
  });
});
