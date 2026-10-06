import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import type { PropsWithChildren } from "react";
import { describe, expect, it } from "vitest";
import { useUsageCacheBridge } from "@/hooks/useUsageCacheBridge";
import type { SubscriptionQuota } from "@/types/subscription";
import { emitTauriEvent } from "../msw/tauriMocks";

const quota = (used: number, queriedAt: number): SubscriptionQuota => ({
  tool: "codex_oauth",
  credentialStatus: "valid",
  credentialMessage: null,
  success: true,
  tiers: [{ name: "five_hour", utilization: used, resetsAt: null }],
  extraUsage: null,
  error: null,
  queriedAt,
});

function setup() {
  const client = new QueryClient();
  const wrapper = ({ children }: PropsWithChildren) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  const hook = renderHook(() => useUsageCacheBridge(), { wrapper });
  return { client, ...hook };
}

describe("native managed-account quota cache bridge", () => {
  it("updates the checked inactive account without refreshing other accounts", async () => {
    const { client } = setup();
    const untouched = quota(40, 10);
    client.setQueryData(["codex_oauth", "quota", "account-b"], untouched);
    const checked = quota(3, 20);
    act(() =>
      emitTauriEvent("codex-oauth-quota-updated", {
        accountId: "account-a",
        quota: checked,
      }),
    );
    await waitFor(() =>
      expect(
        client.getQueryData(["codex_oauth", "quota", "account-a"]),
      ).toEqual(checked),
    );
    expect(client.getQueryData(["codex_oauth", "quota", "account-b"])).toEqual(
      untouched,
    );
    expect(client.isFetching()).toBe(0);
  });

  it("does not replace a newer sample with a delayed native result", () => {
    const { client } = setup();
    const fresh = quota(70, 30);
    client.setQueryData(["codex_oauth", "quota", "account-a"], fresh);
    act(() =>
      emitTauriEvent("codex-oauth-quota-updated", {
        accountId: "account-a",
        quota: quota(5, 20),
      }),
    );
    expect(client.getQueryData(["codex_oauth", "quota", "account-a"])).toEqual(
      fresh,
    );
  });

  it("shows a rejected authorization instead of an old usable quota", () => {
    const { client } = setup();
    const fresh = quota(70, 30);
    client.setQueryData(["codex_oauth", "quota", "account-a"], fresh);
    const rejected = {
      ...quota(0, 40),
      success: false,
      credentialStatus: "expired" as const,
      error: "synthetic authorization rejection",
    };
    act(() =>
      emitTauriEvent("codex-oauth-quota-updated", {
        accountId: "account-a",
        quota: rejected,
      }),
    );
    expect(client.getQueryData(["codex_oauth", "quota", "account-a"])).toEqual(
      rejected,
    );
  });
});
