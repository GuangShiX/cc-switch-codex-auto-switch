import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import type { ComponentProps } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { UsageDashboard } from "@/components/usage/UsageDashboard";

const useProviderStatsMock = vi.hoisted(() => vi.fn());
const useModelStatsMock = vi.hoisted(() => vi.fn());
const usageHeroMock = vi.hoisted(() => vi.fn());
const syncSessionUsageMock = vi.hoisted(() => vi.fn());
const toastMock = vi.hoisted(() => ({
  success: vi.fn(),
  warning: vi.fn(),
  error: vi.fn(),
}));

vi.mock("@/lib/api/usage", () => ({
  usageApi: { syncSessionUsage: syncSessionUsageMock },
}));
vi.mock("sonner", () => ({ toast: toastMock }));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    isVisible: async () => true,
    isMinimized: async () => false,
  }),
}));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, fallback?: string | Record<string, unknown>) =>
      typeof fallback === "string"
        ? fallback
        : fallback
          ? `${key}: ${JSON.stringify(fallback)}`
          : key,
    i18n: {
      resolvedLanguage: "en",
      language: "en",
    },
  }),
}));

vi.mock("framer-motion", () => ({
  motion: {
    div: ({ children, ...props }: any) => <div {...props}>{children}</div>,
  },
}));

vi.mock("@/hooks/useUsageEventBridge", () => ({
  useUsageEventBridge: () => {},
}));

vi.mock("@/lib/query/usage", async () => {
  const actual =
    await vi.importActual<typeof import("@/lib/query/usage")>(
      "@/lib/query/usage",
    );
  return {
    ...actual,
    useProviderStats: (...args: unknown[]) => useProviderStatsMock(...args),
    useModelStats: (...args: unknown[]) => useModelStatsMock(...args),
  };
});

vi.mock("@/components/usage/UsageHero", () => ({
  UsageHero: (props: unknown) => {
    usageHeroMock(props);
    return <div data-testid="usage-hero" />;
  },
}));

vi.mock("@/components/usage/UsageTrendChart", () => ({
  UsageTrendChart: () => <div data-testid="usage-trend" />,
}));

vi.mock("@/components/usage/RequestLogTable", () => ({
  RequestLogTable: () => <div data-testid="request-log-table" />,
}));

vi.mock("@/components/usage/ProviderStatsTable", () => ({
  ProviderStatsTable: () => <div data-testid="provider-stats-table" />,
}));

vi.mock("@/components/usage/ModelStatsTable", () => ({
  ModelStatsTable: () => <div data-testid="model-stats-table" />,
}));

vi.mock("@/components/usage/PricingConfigPanel", () => ({
  PricingConfigPanel: () => <div data-testid="pricing-config-panel" />,
}));

vi.mock("@/components/usage/UsageDateRangePicker", () => ({
  UsageDateRangePicker: () => <button type="button">date-range</button>,
}));

vi.mock("@/components/ui/select", () => ({
  Select: ({ value, onValueChange, children }: any) => (
    <div data-testid={`select-${value}`}>
      {children}
      <button type="button" onClick={() => onValueChange?.("5000")}>
        choose-5000
      </button>
    </div>
  ),
  SelectTrigger: ({ children, ...props }: any) => (
    <button type="button" {...props}>
      {children}
    </button>
  ),
  SelectValue: () => null,
  SelectContent: ({ children }: any) => <div>{children}</div>,
  SelectItem: ({ children, ...props }: any) => <div {...props}>{children}</div>,
}));

const renderDashboard = (props: ComponentProps<typeof UsageDashboard> = {}) => {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: { retry: false },
    },
  });
  return render(
    <QueryClientProvider client={queryClient}>
      <UsageDashboard {...props} />
    </QueryClientProvider>,
  );
};

describe("UsageDashboard", () => {
  beforeEach(() => {
    useProviderStatsMock.mockReset();
    useModelStatsMock.mockReset();
    usageHeroMock.mockReset();
    syncSessionUsageMock.mockReset();
    syncSessionUsageMock.mockResolvedValue({
      imported: 12,
      skipped: 0,
      filesScanned: 3,
      suspectedDuplicates: 0,
      deferredFiles: 0,
      errors: [],
    });
    useProviderStatsMock.mockReturnValue({ data: [] });
    useModelStatsMock.mockReturnValue({ data: [] });
  });

  it("uses the saved refresh interval when mounted", () => {
    renderDashboard({ refreshIntervalMs: 5000 });

    expect(screen.getByTestId("select-5000")).toBeInTheDocument();
  });

  it("filters usage queries to Pi", async () => {
    renderDashboard();

    fireEvent.click(screen.getByRole("button", { name: "usage.appFilter.pi" }));

    await waitFor(() =>
      expect(useProviderStatsMock).toHaveBeenLastCalledWith(
        expect.anything(),
        { appType: "pi" },
        expect.anything(),
      ),
    );
    expect(useModelStatsMock).toHaveBeenLastCalledWith(
      expect.anything(),
      { appType: "pi", providerName: undefined },
      expect.anything(),
    );
    await waitFor(() =>
      expect(usageHeroMock).toHaveBeenLastCalledWith(
        expect.objectContaining({ appType: "pi" }),
      ),
    );
  });

  it("persists refresh interval changes", async () => {
    const onRefreshIntervalChange = vi.fn().mockResolvedValue(true);
    renderDashboard({ onRefreshIntervalChange });

    fireEvent.click(
      within(screen.getByTestId("select-30000")).getByRole("button", {
        name: "choose-5000",
      }),
    );

    await waitFor(() =>
      expect(onRefreshIntervalChange).toHaveBeenCalledWith(5000),
    );
    expect(screen.getByTestId("select-5000")).toBeInTheDocument();
  });

  it("rolls back optimistic interval changes when persistence fails", async () => {
    const onRefreshIntervalChange = vi.fn().mockResolvedValue(false);
    renderDashboard({ onRefreshIntervalChange });

    fireEvent.click(
      within(screen.getByTestId("select-30000")).getByRole("button", {
        name: "choose-5000",
      }),
    );

    await waitFor(() =>
      expect(onRefreshIntervalChange).toHaveBeenCalledWith(5000),
    );
    await waitFor(() =>
      expect(screen.getByTestId("select-30000")).toBeInTheDocument(),
    );
  });

  it("imports once on entry and offers a top refresh action with background scanning enabled", async () => {
    const onSessionAutoSyncEnabledChange = vi.fn();
    renderDashboard({
      sessionAutoSyncEnabled: true,
      onSessionAutoSyncEnabledChange,
    });
    await waitFor(() =>
      expect(
        screen.getByRole("button", { name: "usage.sessionSync.refreshStats" }),
      ).toBeEnabled(),
    );
    expect(syncSessionUsageMock).toHaveBeenCalledTimes(1);
    fireEvent.click(
      screen.getByRole("button", { name: "usage.sessionSync.refreshStats" }),
    );
    await waitFor(() => expect(syncSessionUsageMock).toHaveBeenCalledTimes(2));
    expect(onSessionAutoSyncEnabledChange).not.toHaveBeenCalled();
  });

  it("marks imports as loading while retaining the existing database view", async () => {
    let finish!: (value: unknown) => void;
    syncSessionUsageMock.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          finish = resolve;
        }),
    );
    renderDashboard();
    await waitFor(() =>
      expect(screen.getByRole("status")).toHaveTextContent(
        "usage.sessionSync.syncing",
      ),
    );
    expect(screen.getByTestId("usage-hero")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "usage.sessionSync.refreshStats" }),
    ).toBeDisabled();
    finish({
      imported: 1,
      filesScanned: 1,
      errors: [],
      deferredFiles: 0,
      suspectedDuplicates: 0,
    });
    await waitFor(() =>
      expect(screen.getByTestId("usage-hero")).toBeInTheDocument(),
    );
  });

  it("reports failed imports inline and recovers with manual refresh", async () => {
    syncSessionUsageMock.mockRejectedValueOnce(new Error("Import unavailable"));
    renderDashboard();
    await waitFor(() =>
      expect(screen.getByRole("alert")).toHaveTextContent("Import unavailable"),
    );
    expect(screen.getByTestId("usage-hero")).toBeInTheDocument();
    fireEvent.click(
      screen.getByRole("button", { name: "usage.sessionSync.refreshStats" }),
    );
    await waitFor(() =>
      expect(screen.queryByRole("alert")).not.toBeInTheDocument(),
    );
  });

  it("shows incomplete results for deferred or suspected duplicate files", async () => {
    syncSessionUsageMock.mockResolvedValue({
      imported: 5,
      filesScanned: 3,
      errors: [],
      deferredFiles: 2,
      suspectedDuplicates: 1,
    });
    renderDashboard();
    await waitFor(() =>
      expect(screen.getByRole("status")).toHaveTextContent(
        "usage.sessionSync.incomplete",
      ),
    );
    expect(screen.getByRole("status")).toHaveTextContent('"deferred":2');
    expect(screen.getByRole("status")).toHaveTextContent('"suspected":1');
    fireEvent.click(
      screen.getByRole("button", { name: "usage.sessionSync.refreshStats" }),
    );
    await waitFor(() => expect(toastMock.warning).toHaveBeenCalled());
  });
});
