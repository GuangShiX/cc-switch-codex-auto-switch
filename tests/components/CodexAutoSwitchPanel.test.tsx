import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { CodexAutoSwitchPanel } from "@/components/CodexAutoSwitchPanel";
import type { CodexAutoSwitchStatus } from "@/lib/api/codexAutoSwitch";
import { codexAutoSwitchKeys } from "@/lib/query/codexAutoSwitch";

const api = vi.hoisted(() => ({
  getStatus: vi.fn(),
  setEnabled: vi.fn(),
  cancel: vi.fn(),
}));

vi.mock("@/lib/api/codexAutoSwitch", () => ({ codexAutoSwitchApi: api }));

function status(
  overrides: Partial<CodexAutoSwitchStatus> = {},
): CodexAutoSwitchStatus {
  return {
    enabled: false,
    phase: "disabled",
    message: "后台自动换号已关闭",
    candidateFailures: [],
    canCancel: false,
    ...overrides,
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((complete) => {
    resolve = complete;
  });
  return { promise, resolve };
}

function renderPanel(copies = 1) {
  const client = new QueryClient({
    defaultOptions: {
      queries: { retry: false, gcTime: 0 },
      mutations: { retry: false },
    },
  });
  const view = render(
    <QueryClientProvider client={client}>
      {Array.from({ length: copies }, (_, index) => (
        <CodexAutoSwitchPanel
          key={index}
          providers={{
            first: { id: "first", name: "当前账号 A", settingsConfig: {} },
            second: { id: "second", name: "候选账号 B", settingsConfig: {} },
          }}
        />
      ))}
    </QueryClientProvider>,
  );
  return { ...view, client };
}

beforeEach(() => {
  api.getStatus.mockReset().mockResolvedValue(status());
  api.setEnabled.mockReset().mockResolvedValue(undefined);
  api.cancel.mockReset().mockResolvedValue(undefined);
});

describe("CodexAutoSwitchPanel", () => {
  it("loads native status without starting monitoring or changing an account", async () => {
    renderPanel();
    await screen.findByText("后台自动换号已关闭");
    expect(
      screen.getByRole("switch", { name: "自动换号开关" }),
    ).not.toBeChecked();
    expect(screen.getByText(/严格低于 5%/)).toHaveTextContent(
      "候选账号 5 小时剩余必须大于 5%",
    );
    expect(screen.getByText(/严格低于 5%/)).toHaveTextContent(
      "优先选择 5 小时剩余额度最多的账号，额度相同时按列表顺序",
    );
    expect(screen.getByText(/CC Switch 每 5 分钟/)).toHaveTextContent(
      "锁屏时跳过本次切换，解锁后重新检查",
    );
    expect(screen.getByText(/切号时正常关闭并重开/)).toHaveTextContent(
      "关闭自动开关不影响手动启用账号",
    );
    expect(api.getStatus).toHaveBeenCalledTimes(1);
    expect(api.setEnabled).not.toHaveBeenCalled();
    expect(api.cancel).not.toHaveBeenCalled();
  });

  it("keeps controls disabled until the native status has been read", async () => {
    const pendingStatus = deferred<CodexAutoSwitchStatus>();
    api.getStatus.mockReturnValue(pendingStatus.promise);
    renderPanel();
    const toggle = screen.getByRole("switch", { name: "自动换号开关" });
    expect(toggle).toBeDisabled();
    await userEvent.click(toggle);
    expect(api.setEnabled).not.toHaveBeenCalled();
    await act(async () => pendingStatus.resolve(status()));
    await waitFor(() => expect(toggle).toBeEnabled());
  });

  it("shows the specific blocked reason, account names, failures and last check", async () => {
    api.getStatus.mockResolvedValue(
      status({
        enabled: true,
        phase: "blocked",
        message: "当前桌面未提供安全暂停与恢复接口，尚未关闭或换号",
        currentProviderId: "first",
        targetProviderId: "second",
        checkedAt: 1791000000000,
        candidateFailures: [
          "账号 C：周额度 0%",
          "账号 D：额度查询失败 HTTP 401",
        ],
      }),
    );
    renderPanel();
    await screen.findByText("当前桌面未提供安全暂停与恢复接口，尚未关闭或换号");
    expect(screen.getByRole("status")).toHaveTextContent("需要处理");
    expect(screen.getByText("本次检查账号: 当前账号 A")).toBeVisible();
    expect(screen.getByText("目标账号: 候选账号 B")).toBeVisible();
    expect(screen.getByText("账号 C：周额度 0%")).toBeVisible();
    expect(screen.getByText("账号 D：额度查询失败 HTTP 401")).toBeVisible();
    expect(screen.getByText(/最近额度检查/)).toBeVisible();
    expect(
      screen.queryByRole("button", { name: "取消本次切换" }),
    ).not.toBeInTheDocument();
    expect(api.setEnabled).not.toHaveBeenCalled();
  });

  it("labels a retained cancelled snapshot as the checked account, not the current login", async () => {
    api.getStatus.mockResolvedValue(
      status({
        enabled: true,
        phase: "cancelled",
        message: "用户已选择其他供应商，旧计划失效",
        currentProviderId: "first",
      }),
    );
    renderPanel();
    await screen.findByText("用户已选择其他供应商，旧计划失效");
    expect(screen.getByText("本次检查账号: 当前账号 A")).toBeVisible();
    expect(screen.queryByText("当前账号: 当前账号 A")).not.toBeInTheDocument();
    expect(screen.getByText(/切号时正常关闭并重开/)).toBeVisible();
    expect(api.setEnabled).not.toHaveBeenCalled();
    expect(api.cancel).not.toHaveBeenCalled();
  });

  it("does not infer a successful enable from the command reply and reads status again", async () => {
    const confirmation = deferred<CodexAutoSwitchStatus>();
    api.getStatus
      .mockResolvedValueOnce(status())
      .mockReturnValue(confirmation.promise);
    api.setEnabled.mockResolvedValue(true);
    renderPanel();
    await screen.findByText("后台自动换号已关闭");
    const toggle = screen.getByRole("switch", { name: "自动换号开关" });
    await userEvent.click(toggle);
    await waitFor(() => expect(api.setEnabled).toHaveBeenCalledWith(true));
    expect(toggle).not.toBeChecked();
    expect(toggle).toBeDisabled();
    await act(async () =>
      confirmation.resolve(
        status({
          enabled: true,
          phase: "monitoring",
          message: "正在检查当前账号",
        }),
      ),
    );
    await screen.findByText("正在检查当前账号");
    await waitFor(() => expect(toggle).toBeChecked());
    expect(toggle).toBeEnabled();
    expect(api.setEnabled).toHaveBeenCalledTimes(1);
  });

  it("disables all controls in every mounted panel while cancellation is pending", async () => {
    let current = status({
      enabled: true,
      phase: "preflight",
      canCancel: true,
      operationId: "operation-one",
    });
    api.getStatus.mockImplementation(async () => current);
    const cancellation = deferred<unknown>();
    api.cancel.mockReturnValue(cancellation.promise);
    renderPanel(2);
    await waitFor(() =>
      expect(
        screen.getAllByRole("button", { name: "取消本次切换" }),
      ).toHaveLength(2),
    );
    const cancelButtons = screen.getAllByRole("button", {
      name: "取消本次切换",
    });
    await userEvent.click(cancelButtons[0]);
    await waitFor(() => {
      cancelButtons.forEach((button) => expect(button).toBeDisabled());
      screen
        .getAllByRole("switch")
        .forEach((toggle) => expect(toggle).toBeDisabled());
    });
    await userEvent.click(cancelButtons[1]);
    expect(api.cancel).toHaveBeenCalledTimes(1);
    current = status({
      enabled: true,
      phase: "cancelled",
      message: "本次计划已失效，用户选择已保留",
    });
    await act(async () => cancellation.resolve(undefined));
    await waitFor(() =>
      expect(
        screen.queryAllByRole("button", { name: "取消本次切换" }),
      ).toHaveLength(0),
    );
    expect(screen.getAllByText("本次计划已失效，用户选择已保留")).toHaveLength(
      2,
    );
    expect(api.setEnabled).not.toHaveBeenCalled();
  });

  it("reconciles status after an uncertain control failure and preserves the exact error", async () => {
    api.getStatus.mockResolvedValueOnce(status()).mockResolvedValue(
      status({
        enabled: true,
        phase: "monitoring",
        message: "后台已确认自动换号开启",
      }),
    );
    api.setEnabled.mockRejectedValue(new Error("控制回复中断，结果待核对"));
    renderPanel();
    await screen.findByText("后台自动换号已关闭");
    await userEvent.click(screen.getByRole("switch"));
    await screen.findByText("后台已确认自动换号开启");
    expect(screen.getByRole("alert")).toHaveTextContent(
      "控制回复中断，结果待核对",
    );
    await waitFor(() => expect(screen.getByRole("switch")).toBeChecked());
    expect(api.setEnabled).toHaveBeenCalledTimes(1);
  });

  it("blocks controls when status is unknown and lets the user retry the read only", async () => {
    api.getStatus
      .mockRejectedValueOnce(new Error("后台状态读取失败：连接中断"))
      .mockResolvedValue(status());
    renderPanel();
    await screen.findByText("无法读取后台状态");
    expect(screen.getByRole("switch")).toBeDisabled();
    expect(screen.getByRole("alert")).toHaveTextContent(
      "后台状态读取失败：连接中断",
    );
    await userEvent.click(screen.getByRole("button", { name: "重新读取状态" }));
    await screen.findByText("后台自动换号已关闭");
    expect(screen.getByRole("switch")).toBeEnabled();
    expect(api.setEnabled).not.toHaveBeenCalled();
    expect(api.cancel).not.toHaveBeenCalled();
  });

  it("re-reads changed native state instead of retaining a stale completed claim", async () => {
    let current = status({
      enabled: true,
      phase: "completed",
      message: "本次切换流程完成",
    });
    api.getStatus.mockImplementation(async () => current);
    const { client } = renderPanel();
    await screen.findByText("本次切换流程完成");
    current = status({
      enabled: true,
      phase: "waiting",
      message: "所有账号暂不可用，等待额度恢复",
      candidateFailures: ["账号 B：5 小时剩余 4%"],
    });
    await act(async () => {
      await client.invalidateQueries({ queryKey: codexAutoSwitchKeys.status });
    });
    await screen.findByText("所有账号暂不可用，等待额度恢复");
    expect(screen.queryByText("本次切换流程完成")).not.toBeInTheDocument();
    expect(api.setEnabled).not.toHaveBeenCalled();
    expect(api.cancel).not.toHaveBeenCalled();
  });
});
