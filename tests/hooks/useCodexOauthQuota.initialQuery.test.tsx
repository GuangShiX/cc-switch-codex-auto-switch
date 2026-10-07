import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import type { PropsWithChildren } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { subscriptionApi } from "@/lib/api/subscription";
import { useCodexOauthQuota } from "@/lib/query/subscription";
import type { ProviderMeta } from "@/types";
import type { SubscriptionQuota } from "@/types/subscription";

vi.mock("@/lib/api/subscription", () => ({
  subscriptionApi: { getCodexOauthQuota: vi.fn() },
}));

const getQuota = vi.mocked(subscriptionApi.getCodexOauthQuota);
const clients: QueryClient[] = [];
const initialQueryOptions = {
  enabled: true,
  autoQuery: false,
  autoQueryIntervalMinutes: 5,
};

function managedMeta(accountId: string): ProviderMeta {
  return {
    authBinding: {
      source: "managed_account",
      authProvider: "codex_oauth",
      accountId,
    },
  };
}

function quota(): SubscriptionQuota {
  return {
    tool: "codex_oauth",
    credentialStatus: "valid",
    credentialMessage: null,
    success: true,
    tiers: [{ name: "five_hour", utilization: 20, resetsAt: null }],
    extraUsage: null,
    error: null,
    queriedAt: Date.now(),
  };
}

function setup() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: Infinity } },
  });
  clients.push(client);
  const wrapper = ({ children }: PropsWithChildren) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return { client, wrapper };
}

beforeEach(() => {
  getQuota.mockReset();
  getQuota.mockImplementation(async () => quota());
});

afterEach(() => {
  clients.splice(0).forEach((client) => client.clear());
  vi.useRealTimers();
});

describe("managed Codex quota initial queries", () => {
  it("loads each inactive managed account once using its own binding", async () => {
    const { wrapper } = setup();
    const { result } = renderHook(
      () => ({
        first: useCodexOauthQuota(
          managedMeta("account-a"),
          initialQueryOptions,
        ),
        second: useCodexOauthQuota(
          managedMeta("account-b"),
          initialQueryOptions,
        ),
      }),
      { wrapper },
    );

    await waitFor(() => {
      expect(result.current.first.data?.success).toBe(true);
      expect(result.current.second.data?.success).toBe(true);
    });
    expect(getQuota.mock.calls).toEqual([["account-a"], ["account-b"]]);
  });

  it("shares one initial request between cards bound to the same account", async () => {
    const { wrapper } = setup();
    const { result } = renderHook(
      () => ({
        first: useCodexOauthQuota(
          managedMeta("shared-account"),
          initialQueryOptions,
        ),
        second: useCodexOauthQuota(
          managedMeta("shared-account"),
          initialQueryOptions,
        ),
      }),
      { wrapper },
    );

    await waitFor(() => {
      expect(result.current.first.data?.success).toBe(true);
      expect(result.current.second.data?.success).toBe(true);
    });
    expect(getQuota).toHaveBeenCalledTimes(1);
    expect(getQuota).toHaveBeenCalledWith("shared-account");
    expect(result.current.first.data).toBe(result.current.second.data);
  });

  it("reuses fresh quota when an inactive card is remounted", async () => {
    const { wrapper } = setup();
    const first = renderHook(
      () => useCodexOauthQuota(managedMeta("account-a"), initialQueryOptions),
      { wrapper },
    );
    await waitFor(() => expect(first.result.current.data?.success).toBe(true));
    const cached = first.result.current.data;
    first.unmount();

    const second = renderHook(
      () => useCodexOauthQuota(managedMeta("account-a"), initialQueryOptions),
      { wrapper },
    );
    expect(second.result.current.data).toBe(cached);
    expect(getQuota).toHaveBeenCalledTimes(1);
  });

  it("does not poll inactive accounts after their initial query", async () => {
    const { wrapper } = setup();
    const { result } = renderHook(
      () => useCodexOauthQuota(managedMeta("account-a"), initialQueryOptions),
      { wrapper },
    );
    await waitFor(() => expect(result.current.data?.success).toBe(true));

    vi.useFakeTimers();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(20 * 60 * 1000);
    });
    expect(getQuota).toHaveBeenCalledTimes(1);
  });

  it("still performs the initial query when periodic refresh is disabled", async () => {
    const { wrapper } = setup();
    const { result } = renderHook(
      () =>
        useCodexOauthQuota(managedMeta("account-a"), {
          ...initialQueryOptions,
          autoQueryIntervalMinutes: 0,
        }),
      { wrapper },
    );

    await waitFor(() => expect(result.current.data?.success).toBe(true));
    expect(getQuota).toHaveBeenCalledTimes(1);
    expect(getQuota).toHaveBeenCalledWith("account-a");
  });
});
