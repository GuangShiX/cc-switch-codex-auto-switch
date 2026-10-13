import { StrictMode, type PropsWithChildren } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useUsageSessionSync } from "@/hooks/useUsageSessionSync";
import { useUsageSummaryByApp } from "@/lib/query/usage";
import type { SessionSyncResult } from "@/types/usage";

const api = vi.hoisted(() => ({
  syncSessionUsage: vi.fn(),
  getUsageSummaryByApp: vi.fn(),
}));
vi.mock("@/lib/api/usage", () => ({ usageApi: api }));
const nativeWindow = vi.hoisted(() => ({
  isVisible: vi.fn(),
  isMinimized: vi.fn(),
}));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => nativeWindow,
}));

const FIVE_MINUTES = 300_000;
const result: SessionSyncResult = {
  imported: 12,
  skipped: 0,
  filesScanned: 3,
  suspectedDuplicates: 0,
  deferredFiles: 0,
  errors: [],
};
let importedTokens = 0;
const clients: QueryClient[] = [];

function createClient() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: Infinity } },
  });
  clients.push(client);
  return client;
}

function wrapper(client: QueryClient, strict = false) {
  return function Wrapper({ children }: PropsWithChildren) {
    const content = (
      <QueryClientProvider client={client}>{children}</QueryClientProvider>
    );
    return strict ? <StrictMode>{content}</StrictMode> : content;
  };
}

function Probe({ background = false }: { background?: boolean }) {
  const sync = useUsageSessionSync(background);
  const summary = useUsageSummaryByApp({ preset: "today" }, undefined, {
    refetchInterval: false,
  });
  return (
    <div>
      <span data-testid="tokens">
        {summary.data?.[0]?.summary.realTotalTokens ?? 0}
      </span>
      <span data-testid="sync-status">
        {sync.isSyncing ? "syncing" : sync.error ? "failed" : "ready"}
      </span>
      <button onClick={() => void sync.refresh().catch(() => {})}>
        Refresh
      </button>
    </div>
  );
}

async function settle() {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(0);
  });
  await act(async () => {
    await vi.advanceTimersByTimeAsync(0);
  });
}

async function advance(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
  await settle();
}

function visibility(state: DocumentVisibilityState) {
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: state,
  });
  act(() => document.dispatchEvent(new Event("visibilitychange")));
}

describe("usage page session imports", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-10-13T08:00:00Z"));
    visibility("visible");
    importedTokens = 0;
    api.syncSessionUsage.mockReset();
    api.getUsageSummaryByApp.mockReset();
    nativeWindow.isVisible.mockReset().mockResolvedValue(true);
    nativeWindow.isMinimized.mockReset().mockResolvedValue(false);
    api.syncSessionUsage.mockImplementation(async () => {
      importedTokens += 42;
      return result;
    });
    api.getUsageSummaryByApp.mockImplementation(async () => [
      {
        appType: "codex",
        summary: { realTotalTokens: importedTokens },
      },
    ]);
  });

  afterEach(() => {
    clients.splice(0).forEach((client) => client.clear());
    vi.useRealTimers();
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      value: "visible",
    });
  });

  it("imports on entry and refreshes database totals without enabling background scans", async () => {
    render(<Probe />, { wrapper: wrapper(createClient()) });
    await settle();
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(1);
    expect(screen.getByTestId("tokens")).toHaveTextContent("42");
    expect(screen.getByTestId("sync-status")).toHaveTextContent("ready");
  });

  it("deduplicates StrictMode, an in-flight remount and a recent completed remount", async () => {
    let finish!: (value: SessionSyncResult) => void;
    api.syncSessionUsage.mockImplementation(
      () =>
        new Promise<SessionSyncResult>((resolve) => {
          finish = resolve;
        }),
    );
    const client = createClient();
    const first = render(<Probe />, { wrapper: wrapper(client, true) });
    await settle();
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(1);
    first.unmount();
    const second = render(<Probe />, { wrapper: wrapper(client, true) });
    await settle();
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(1);
    await act(async () => {
      finish(result);
    });
    await settle();
    second.unmount();
    render(<Probe />, { wrapper: wrapper(client) });
    await settle();
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(1);
  });

  it("limits page imports to five minutes even when database queries refresh faster", async () => {
    render(<Probe />, { wrapper: wrapper(createClient()) });
    await settle();
    await advance(FIVE_MINUTES - 1);
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(1);
    await advance(1);
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(2);
    await advance(1);
    expect(screen.getByTestId("tokens")).toHaveTextContent("84");
  });

  it("starts the five-minute cooldown after a long import finishes", async () => {
    let finish!: (value: SessionSyncResult) => void;
    api.syncSessionUsage.mockImplementationOnce(
      () =>
        new Promise<SessionSyncResult>((resolve) => {
          finish = resolve;
        }),
    );
    const client = createClient();
    const first = render(<Probe />, { wrapper: wrapper(client) });
    await advance(3 * 60_000);
    first.unmount();
    render(<Probe />, { wrapper: wrapper(client) });
    await advance(3 * 60_000);
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(1);
    await act(async () => {
      finish(result);
    });
    await settle();
    await advance(FIVE_MINUTES - 1);
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(1);
    await advance(2);
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(2);
  });

  it("does not scan hidden pages or run queued catch-up scans when visible again", async () => {
    render(<Probe />, { wrapper: wrapper(createClient()) });
    await settle();
    await advance(60_000);
    visibility("hidden");
    await advance(20 * 60_000);
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(1);
    visibility("visible");
    await settle();
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(1);
    await advance(FIVE_MINUTES);
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(2);
  });

  it("does not start an import when initially hidden", async () => {
    visibility("hidden");
    render(<Probe />, { wrapper: wrapper(createClient()) });
    await advance(20 * 60_000);
    expect(api.syncSessionUsage).not.toHaveBeenCalled();
    visibility("visible");
    await settle();
    expect(api.syncSessionUsage).not.toHaveBeenCalled();
    await advance(FIVE_MINUTES);
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(1);
  });

  it("does not scan a native tray window even when document visibility remains visible", async () => {
    nativeWindow.isVisible.mockResolvedValue(false);
    render(<Probe />, { wrapper: wrapper(createClient()) });
    await settle();
    await advance(20 * 60_000);
    expect(api.syncSessionUsage).not.toHaveBeenCalled();
    expect(nativeWindow.isVisible).toHaveBeenCalledTimes(5);
    expect(nativeWindow.isMinimized).toHaveBeenCalledTimes(5);
    nativeWindow.isVisible.mockResolvedValue(true);
    await advance(FIVE_MINUTES);
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(1);
  });

  it("does not scan a minimized native window", async () => {
    nativeWindow.isMinimized.mockResolvedValue(true);
    render(<Probe />, { wrapper: wrapper(createClient()) });
    await settle();
    await advance(FIVE_MINUTES);
    expect(api.syncSessionUsage).not.toHaveBeenCalled();
    expect(nativeWindow.isVisible).toHaveBeenCalledTimes(2);
  });

  it("fails closed when native state cannot be read while leaving explicit refresh usable", async () => {
    nativeWindow.isVisible.mockRejectedValue(new Error("Window unavailable"));
    render(<Probe />, { wrapper: wrapper(createClient()) });
    await settle();
    expect(api.syncSessionUsage).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await settle();
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(1);
    expect(screen.getByTestId("tokens")).toHaveTextContent("42");
  });

  it("does not import after leaving the page during a pending native visibility check", async () => {
    let finish!: (visible: boolean) => void;
    nativeWindow.isVisible.mockImplementationOnce(
      () =>
        new Promise<boolean>((resolve) => {
          finish = resolve;
        }),
    );
    const view = render(<Probe />, { wrapper: wrapper(createClient()) });
    view.unmount();
    await act(async () => {
      finish(true);
    });
    await settle();
    expect(api.syncSessionUsage).not.toHaveBeenCalled();
  });

  it("does not replay a pending window check after the document hides and returns", async () => {
    let finish!: (visible: boolean) => void;
    nativeWindow.isVisible.mockImplementationOnce(
      () =>
        new Promise<boolean>((resolve) => {
          finish = resolve;
        }),
    );
    render(<Probe />, { wrapper: wrapper(createClient()) });
    visibility("hidden");
    visibility("visible");
    await act(async () => {
      finish(true);
    });
    await settle();
    expect(api.syncSessionUsage).not.toHaveBeenCalled();
    await advance(FIVE_MINUTES);
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(1);
  });

  it("deduplicates asynchronous native checks across StrictMode effect lifecycles", async () => {
    let finish!: (visible: boolean) => void;
    const pending = new Promise<boolean>((resolve) => {
      finish = resolve;
    });
    nativeWindow.isVisible.mockReturnValue(pending);
    render(<Probe />, { wrapper: wrapper(createClient(), true) });
    expect(api.syncSessionUsage).not.toHaveBeenCalled();
    await act(async () => {
      finish(true);
    });
    await settle();
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(1);
  });

  it("stops timers on leaving the page and imports once on a later visit", async () => {
    const client = createClient();
    const first = render(<Probe />, { wrapper: wrapper(client) });
    await settle();
    first.unmount();
    await advance(20 * 60_000);
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(1);
    render(<Probe />, { wrapper: wrapper(client) });
    await settle();
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(2);
  });

  it("does not add page polling when native background sync is enabled; manual refresh remains available", async () => {
    render(<Probe background />, { wrapper: wrapper(createClient()) });
    await settle();
    await advance(20 * 60_000);
    act(() => window.dispatchEvent(new Event("focus")));
    act(() => window.dispatchEvent(new Event("online")));
    await settle();
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await settle();
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(2);
    expect(screen.getByTestId("tokens")).toHaveTextContent("84");
  });

  it("allows explicit refresh inside the automatic cooldown and shares concurrent requests", async () => {
    render(<Probe />, { wrapper: wrapper(createClient()) });
    await settle();
    let finish!: (value: SessionSyncResult) => void;
    api.syncSessionUsage.mockImplementationOnce(
      () =>
        new Promise<SessionSyncResult>((resolve) => {
          finish = resolve;
        }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(2);
    await act(async () => {
      finish(result);
    });
    await settle();
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(2);
  });

  it("exposes failures, avoids a remount retry loop and allows manual recovery", async () => {
    importedTokens = 12_500;
    api.syncSessionUsage.mockRejectedValueOnce(
      new Error("Cannot read session files"),
    );
    const client = createClient();
    const first = render(<Probe />, { wrapper: wrapper(client) });
    await settle();
    expect(screen.getByTestId("sync-status")).toHaveTextContent("failed");
    expect(screen.getByTestId("tokens")).toHaveTextContent("12500");
    first.unmount();
    render(<Probe />, { wrapper: wrapper(client) });
    await settle();
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await settle();
    expect(api.syncSessionUsage).toHaveBeenCalledTimes(2);
    expect(screen.getByTestId("tokens")).toHaveTextContent("12542");
    expect(screen.getByTestId("sync-status")).toHaveTextContent("ready");
  });
});
