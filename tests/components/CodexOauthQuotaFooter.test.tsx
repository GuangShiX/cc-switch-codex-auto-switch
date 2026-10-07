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
  it("loads inactive managed cards initially without enabling their polling", () => {
    quotaHook.mockReturnValue({
      data: undefined,
      isFetching: false,
      refetch: vi.fn(),
    });

    render(<CodexOauthQuotaFooter isCurrent={false} />);

    expect(quotaHook).toHaveBeenCalledWith(undefined, {
      enabled: true,
      autoQuery: false,
      autoQueryIntervalMinutes: 5,
    });
  });

  it("keeps the active card's existing five minute query", () => {
    quotaHook.mockReturnValue({
      data: undefined,
      isFetching: false,
      refetch: vi.fn(),
    });

    render(<CodexOauthQuotaFooter isCurrent autoQueryInterval={5} />);

    expect(quotaHook).toHaveBeenCalledWith(undefined, {
      enabled: true,
      autoQuery: true,
      autoQueryIntervalMinutes: 5,
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
