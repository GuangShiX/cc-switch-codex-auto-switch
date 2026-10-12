import {
  QueryClient,
  QueryClientProvider,
  focusManager,
  onlineManager,
} from "@tanstack/react-query";
import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import CodexOauthQuotaFooter from "@/components/CodexOauthQuotaFooter";
import CodexOauthAccountQuota from "@/components/CodexOauthAccountQuota";
import { useUsageCacheBridge } from "@/hooks/useUsageCacheBridge";
import { subscriptionApi } from "@/lib/api/subscription";
import type { ProviderMeta } from "@/types";
import type { SubscriptionQuota } from "@/types/subscription";
import { emitTauriEvent } from "../msw/tauriMocks";

vi.mock("@/lib/api/subscription", () => ({
  subscriptionApi: { getCodexOauthQuota: vi.fn() },
}));

vi.mock("@/components/SubscriptionQuotaFooter", () => ({
  SubscriptionQuotaView: ({
    quota,
    refetch,
  }: {
    quota: SubscriptionQuota;
    refetch: () => unknown;
  }) => (
    <div>
      <span>{quota.tiers[0]?.utilization}</span>
      <button onClick={() => void refetch()}>refresh</button>
    </div>
  ),
}));

const getQuota = vi.mocked(subscriptionApi.getCodexOauthQuota);
const clients: QueryClient[] = [];

function meta(accountId: string): ProviderMeta {
  return {
    authBinding: {
      source: "managed_account",
      authProvider: "codex_oauth",
      accountId,
    },
  };
}

function quota(used = 20): SubscriptionQuota {
  return {
    tool: "codex_oauth",
    credentialStatus: "valid",
    credentialMessage: null,
    success: true,
    tiers: [{ name: "five_hour", utilization: used, resetsAt: null }],
    extraUsage: null,
    error: null,
    queriedAt: Date.now(),
  };
}

function AccountViews({ show = true }: { show?: boolean }) {
  useUsageCacheBridge();
  return show ? (
    <>
      <section data-testid="current">
        <CodexOauthQuotaFooter meta={meta("current")} isCurrent />
      </section>
      <section data-testid="other">
        <CodexOauthQuotaFooter meta={meta("other")} />
      </section>
      <section data-testid="center">
        <CodexOauthAccountQuota accountId="other" />
      </section>
    </>
  ) : null;
}

function setup() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  clients.push(client);
  const view = (show = true) => (
    <QueryClientProvider client={client}>
      <AccountViews show={show} />
    </QueryClientProvider>
  );
  return { client, view, ...render(view()) };
}

beforeEach(() => {
  getQuota.mockReset();
  getQuota.mockImplementation(async () => quota());
});

afterEach(() => {
  clients.splice(0).forEach((client) => client.clear());
  focusManager.setFocused(undefined);
  onlineManager.setOnline(true);
  vi.useRealTimers();
});

describe("managed Codex quota views use native results and manual queries", () => {
  it("does not query on mounting, focus, reconnect, elapsed time or remounting", async () => {
    vi.useFakeTimers();
    const { client, view, rerender } = setup();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
      focusManager.setFocused(false);
      focusManager.setFocused(true);
      onlineManager.setOnline(false);
      onlineManager.setOnline(true);
      await vi.advanceTimersByTimeAsync(35 * 60 * 1000);
    });
    rerender(view(false));
    rerender(view());
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(getQuota).not.toHaveBeenCalled();
    expect(client.isFetching()).toBe(0);
    expect(screen.getAllByRole("button")).toHaveLength(3);
  });

  it("shows native results in cards and the auth center without extra requests", async () => {
    const { client } = setup();
    act(() => {
      emitTauriEvent("codex-oauth-quota-updated", {
        accountId: "other",
        quota: quota(43),
      });
    });
    await waitFor(() => {
      expect(
        within(screen.getByTestId("other")).getByText("43"),
      ).toBeInTheDocument();
      expect(
        within(screen.getByTestId("center")).getByText("43"),
      ).toBeInTheDocument();
    });
    expect(
      within(screen.getByTestId("current")).queryByText("43"),
    ).not.toBeInTheDocument();
    expect(getQuota).not.toHaveBeenCalled();
    expect(client.isFetching()).toBe(0);
  });

  it("allows explicit current-card and auth-center refresh without automatic queries", async () => {
    setup();
    fireEvent.click(within(screen.getByTestId("current")).getByRole("button"));
    await waitFor(() => {
      expect(
        within(screen.getByTestId("current")).getByText("20"),
      ).toBeInTheDocument();
    });
    expect(getQuota.mock.calls).toEqual([["current"]]);

    fireEvent.click(within(screen.getByTestId("center")).getByRole("button"));
    await waitFor(() => {
      expect(
        within(screen.getByTestId("other")).getByText("20"),
      ).toBeInTheDocument();
      expect(
        within(screen.getByTestId("center")).getByText("20"),
      ).toBeInTheDocument();
    });
    expect(getQuota.mock.calls).toEqual([["current"], ["other"]]);
  });
});
