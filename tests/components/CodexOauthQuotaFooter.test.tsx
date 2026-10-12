import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import CodexOauthQuotaFooter from "@/components/CodexOauthQuotaFooter";

const quotaHook = vi.hoisted(() => vi.fn());
const viewProps = vi.hoisted(() => vi.fn());

vi.mock("@/lib/query/subscription", () => ({
  useCodexOauthQuota: quotaHook,
}));

vi.mock("@/components/SubscriptionQuotaFooter", () => ({
  SubscriptionQuotaView: (props: unknown) => {
    viewProps(props);
    return null;
  },
}));

describe("Codex OAuth quota card", () => {
  it("labels explicitly weekly-only Pro without inventing a five-hour quota", () => {
    const quota = {
      success: true,
      codexLimitPolicy: "weekly_only",
      tiers: [{ name: "seven_day", utilization: 18, resetsAt: null }],
    };
    quotaHook.mockReturnValue({
      data: quota,
      isFetching: false,
      refetch: vi.fn(),
    });

    render(<CodexOauthQuotaFooter isCurrent />);

    expect(screen.getByText("Pro · 无5小时窗口")).toBeInTheDocument();
    expect(viewProps).toHaveBeenCalledWith(expect.objectContaining({ quota }));
    expect(quota.tiers).toHaveLength(1);
  });

  it("does not infer the Pro label from a missing five-hour tier", () => {
    quotaHook.mockReturnValue({
      data: { success: true, tiers: [{ name: "seven_day", utilization: 18 }] },
      isFetching: false,
      refetch: vi.fn(),
    });
    render(<CodexOauthQuotaFooter isCurrent />);
    expect(screen.queryByText("Pro · 无5小时窗口")).not.toBeInTheDocument();
  });

  it("displays inactive managed cards without automatic queries", () => {
    quotaHook.mockReturnValue({
      data: undefined,
      isFetching: false,
      refetch: vi.fn(),
    });

    render(<CodexOauthQuotaFooter isCurrent={false} />);

    expect(quotaHook).toHaveBeenCalledWith(undefined, {
      enabled: false,
      autoQuery: false,
    });
  });

  it("leaves current-account monitoring to the native service", () => {
    quotaHook.mockReturnValue({
      data: undefined,
      isFetching: false,
      refetch: vi.fn(),
    });

    render(<CodexOauthQuotaFooter isCurrent autoQueryInterval={5} />);

    expect(quotaHook).toHaveBeenCalledWith(undefined, {
      enabled: false,
      autoQuery: false,
    });
  });

  it("queries only the selected inactive card when its refresh is clicked", () => {
    const firstRefetch = vi.fn();
    const secondRefetch = vi.fn();
    quotaHook
      .mockReturnValueOnce({
        data: undefined,
        isFetching: false,
        refetch: firstRefetch,
      })
      .mockReturnValueOnce({
        data: undefined,
        isFetching: false,
        refetch: secondRefetch,
      });

    render(
      <>
        <CodexOauthQuotaFooter isCurrent={false} />
        <CodexOauthQuotaFooter isCurrent={false} />
      </>,
    );

    expect(firstRefetch).not.toHaveBeenCalled();
    expect(secondRefetch).not.toHaveBeenCalled();
    fireEvent.click(screen.getAllByRole("button")[0]);
    expect(firstRefetch).toHaveBeenCalledTimes(1);
    expect(secondRefetch).not.toHaveBeenCalled();
  });
});
