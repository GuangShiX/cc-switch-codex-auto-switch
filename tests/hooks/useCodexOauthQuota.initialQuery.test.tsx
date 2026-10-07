import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import type { PropsWithChildren } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { subscriptionApi } from "@/lib/api/subscription";
import {
  useCodexOauthQuota,
  useCodexOauthQuotaByAccountId,
} from "@/lib/query/subscription";
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
    defaultOptions: { queries: { retry: false } },
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

  it("keeps inactive quota across navigation after five minutes", async () => {
    vi.useFakeTimers();
    const { client, wrapper } = setup();
    const first = renderHook(
      () => useCodexOauthQuota(managedMeta("account-a"), initialQueryOptions),
      { wrapper },
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(first.result.current.data?.success).toBe(true);
    const cached = first.result.current.data;
    first.unmount();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(5 * 60 * 1000 + 1);
    });
    expect(client.getQueryData(["codex_oauth", "quota", "account-a"])).toBe(
      cached,
    );
    const second = renderHook(
      () => useCodexOauthQuota(managedMeta("account-a"), initialQueryOptions),
      { wrapper },
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(second.result.current.data).toBe(cached);
    expect(getQuota).toHaveBeenCalledTimes(1);
  });

  it("refreshes an inactive account on reopening after thirty minutes", async () => {
    vi.useFakeTimers();
    const { client, wrapper } = setup();
    const first = renderHook(
      () => useCodexOauthQuota(managedMeta("account-a"), initialQueryOptions),
      { wrapper },
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(first.result.current.data?.success).toBe(true);
    const cached = first.result.current.data;
    first.unmount();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(30 * 60 * 1000 + 1);
    });
    // Retain the previous value for display while the fresh query runs.
    expect(client.getQueryData(["codex_oauth", "quota", "account-a"])).toBe(
      cached,
    );
    const second = renderHook(
      () => useCodexOauthQuota(managedMeta("account-a"), initialQueryOptions),
      { wrapper },
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(second.result.current.data?.success).toBe(true);
    expect(getQuota).toHaveBeenCalledTimes(2);
  });

  it("shares the longer cache when the auth center opens", async () => {
    vi.useFakeTimers();
    const { wrapper } = setup();
    const card = renderHook(
      () => useCodexOauthQuota(managedMeta("account-a"), initialQueryOptions),
      { wrapper },
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(card.result.current.data?.success).toBe(true);
    card.unmount();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10 * 60 * 1000);
    });

    const center = renderHook(
      () => useCodexOauthQuotaByAccountId("account-a", { autoQuery: false }),
      { wrapper },
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(center.result.current.data?.success).toBe(true);
    expect(getQuota).toHaveBeenCalledTimes(1);
  });

  it("allows manual refresh while inactive quota is still fresh", async () => {
    vi.useFakeTimers();
    const { wrapper } = setup();
    const { result } = renderHook(
      () => useCodexOauthQuota(managedMeta("account-a"), initialQueryOptions),
      { wrapper },
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(result.current.data?.success).toBe(true);

    await act(async () => {
      await result.current.refetch();
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(getQuota).toHaveBeenCalledTimes(2);
    expect(getQuota.mock.calls).toEqual([["account-a"], ["account-a"]]);
  });

  it("reuses a more recent native result instead of querying on remount", async () => {
    vi.useFakeTimers();
    const { client, wrapper } = setup();
    const first = renderHook(
      () => useCodexOauthQuota(managedMeta("account-a"), initialQueryOptions),
      { wrapper },
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    first.unmount();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10 * 60 * 1000);
    });
    const nativeResult = quota();
    act(() => {
      client.setQueryData(["codex_oauth", "quota", "account-a"], nativeResult);
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(20 * 60 * 1000 + 1);
    });

    const second = renderHook(
      () => useCodexOauthQuota(managedMeta("account-a"), initialQueryOptions),
      { wrapper },
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(second.result.current.data).toEqual(nativeResult);
    expect(getQuota).toHaveBeenCalledTimes(1);
  });

  it("keeps active polling at five minutes without polling other accounts", async () => {
    vi.useFakeTimers();
    const { wrapper } = setup();
    const { result } = renderHook(
      () => ({
        active: useCodexOauthQuota(managedMeta("active-account"), {
          enabled: true,
          autoQuery: true,
          autoQueryIntervalMinutes: 5,
        }),
        inactive: useCodexOauthQuota(
          managedMeta("inactive-account"),
          initialQueryOptions,
        ),
      }),
      { wrapper },
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(result.current.active.data?.success).toBe(true);
    expect(result.current.inactive.data?.success).toBe(true);
    expect(getQuota.mock.calls).toEqual([
      ["active-account"],
      ["inactive-account"],
    ]);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5 * 60 * 1000);
    });
    expect(getQuota.mock.calls).toEqual([
      ["active-account"],
      ["inactive-account"],
      ["active-account"],
    ]);
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
