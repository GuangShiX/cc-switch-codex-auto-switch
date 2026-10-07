import { useTranslation } from "react-i18next";
import { AlertTriangle, ArrowRight, Loader2, Repeat2 } from "lucide-react";
import type { Provider } from "@/types";
import { useCodexAutoSwitch } from "@/lib/query/codexAutoSwitch";
import type { CodexAutoSwitchFailure } from "@/lib/api/codexAutoSwitch";
import { extractErrorMessage } from "@/utils/errorUtils";
import { Switch } from "@/components/ui/switch";
import { Button } from "@/components/ui/button";

const PHASE_LABELS: Record<string, string> = {
  waiting: "等待可用账号",
  "waiting-for-reset": "等待 5 小时额度重置",
  monitoring: "正在后台监测",
  checking: "正在查询当前账号额度",
  selecting: "正在选择 5 小时剩余额度最多的账号",
  preflight: "正在核对账号和桌面任务",
  pausing: "正在安全暂停任务",
  closing: "正在关闭 Codex 桌面",
  enabling: "正在启用目标账号",
  starting: "正在重开 Codex 桌面",
  verifying: "正在核实桌面账号",
  resuming: "正在恢复本次暂停的原任务",
  "waiting-for-chat": "正在等待原聊天加载",
  "restoring-settings": "正在恢复原模型和权限",
  completed: "已完成",
  cancelled: "已取消",
  blocked: "需要处理",
  failed: "执行失败",
  disabled: "自动换号已关闭",
};

const SOURCE_LABELS: Record<string, string> = {
  manual: "手动启用",
  automatic: "自动检测",
  "reset-wait": "等待重置",
};

export function CodexAutoSwitchPanel({
  providers = {},
}: {
  providers?: Record<string, Provider>;
}) {
  const { t, i18n } = useTranslation();
  const { status, failureHistory, control, isPending } = useCodexAutoSwitch();
  const data = status.data;
  const controlsDisabled = !data || status.isError || isPending;
  const needsAttention =
    data &&
    ["blocked", "failed", "waiting", "waiting-for-reset"].includes(data.phase);
  const checkedAt = data?.checkedAt
    ? new Date(data.checkedAt < 1e12 ? data.checkedAt * 1000 : data.checkedAt)
    : null;
  const validCheckedAt = checkedAt && Number.isFinite(checkedAt.getTime());
  const providerName = (id: string) => providers[id]?.name || id;
  const candidateReason = (reason: string) => {
    const separator = reason.indexOf("：");
    if (separator < 0) return reason;
    const id = reason.slice(0, separator);
    return `${providerName(id)}：${reason.slice(separator + 1)}`;
  };
  const error = control.error || status.error;
  const recentFailures = [...(failureHistory.data ?? [])]
    .sort((left, right) => right.at - left.at)
    .slice(0, 5);
  const formatFailureTime = (failure: CodexAutoSwitchFailure) => {
    const date = new Date(failure.at < 1e12 ? failure.at * 1000 : failure.at);
    return Number.isFinite(date.getTime())
      ? date.toLocaleString(i18n.language)
      : String(failure.at);
  };
  const failureDateTime = (failure: CodexAutoSwitchFailure) => {
    const date = new Date(failure.at < 1e12 ? failure.at * 1000 : failure.at);
    return Number.isFinite(date.getTime()) ? date.toISOString() : undefined;
  };

  return (
    <section
      aria-label={t("codexAutoSwitch.title", "Codex 桌面自动换号")}
      className="space-y-3 rounded-xl border border-border bg-card/50 p-4"
    >
      <div className="flex items-center justify-between gap-4">
        <div className="flex items-center gap-2">
          <Repeat2 className="h-4 w-4 text-primary" />
          <h3 className="text-sm font-medium">
            {t("codexAutoSwitch.title", "Codex 桌面自动换号")}
          </h3>
        </div>
        <Switch
          aria-label={t("codexAutoSwitch.enable", "自动换号开关")}
          checked={data?.enabled ?? false}
          disabled={controlsDisabled}
          onCheckedChange={(enabled) => control.mutate({ enabled })}
        />
      </div>

      <p className="text-xs leading-relaxed text-muted-foreground">
        {t(
          "codexAutoSwitch.thresholds",
          "当前账号 5 小时剩余额度严格低于 5%，或周剩余额度为 0% 时触发。候选账号 5 小时剩余必须大于 5%，周剩余必须大于 0%。优先选择 5 小时剩余额度最多的账号，额度相同时按列表顺序。",
        )}
      </p>
      <p className="text-xs text-muted-foreground">
        {t(
          "codexAutoSwitch.background",
          "CC Switch 每 5 分钟在后台独立查询额度，最小化或隐藏到托盘后仍会运行。锁屏时跳过本次切换，解锁后重新检查。",
        )}
      </p>
      <p className="text-xs leading-relaxed text-muted-foreground">
        {t(
          "codexAutoSwitch.lifecycle",
          "切号时正常关闭并重开 Codex 桌面，再恢复本次暂停或明确因额度耗尽停止的原任务。用户手动停止、等待审批和已完成的任务不会自动继续。关闭自动开关不影响手动启用账号。",
        )}
      </p>
      <p className="text-xs leading-relaxed text-muted-foreground">
        {t(
          "codexAutoSwitch.waitPolicy",
          "没有可用账号时，切至周额度仍可用且 5 小时窗口最早重置的账号，保存本次原任务等待。重置后重新查询，确认额度可用才继续，等待期间不反复重启。",
        )}
      </p>

      <div className="flex flex-wrap items-center justify-between gap-2">
        <div
          role="status"
          aria-live="polite"
          className="min-w-0 flex-1 space-y-1"
        >
          <div className="flex items-center gap-2 text-sm font-medium">
            {needsAttention ? (
              <AlertTriangle className="h-4 w-4 shrink-0 text-amber-500" />
            ) : null}
            {status.isLoading ? (
              <Loader2 className="h-4 w-4 shrink-0 animate-spin" />
            ) : null}
            <span>
              {status.isError
                ? t("codexAutoSwitch.statusUnavailable", "无法读取后台状态")
                : data
                  ? t(`codexAutoSwitch.phases.${data.phase}`, {
                      defaultValue: PHASE_LABELS[data.phase] || data.phase,
                    })
                  : t("codexAutoSwitch.loading", "正在读取后台状态")}
            </span>
          </div>
          {data?.message && !status.isError ? (
            <p className="break-words text-xs text-muted-foreground">
              {data.message}
            </p>
          ) : null}
        </div>
        {data?.canCancel ? (
          <Button
            type="button"
            variant="outline"
            size="sm"
            disabled={controlsDisabled}
            onClick={() => control.mutate({ cancel: true })}
          >
            {t("codexAutoSwitch.cancel", "取消本次切换")}
          </Button>
        ) : null}
      </div>

      {data?.currentProviderId && !status.isError ? (
        <div className="flex flex-wrap items-center gap-2 break-all text-xs text-muted-foreground">
          <span>
            {t("codexAutoSwitch.checkedAccount", "本次检查账号")}:{" "}
            {providerName(data.currentProviderId)}
          </span>
          {data.targetProviderId ? (
            <>
              <ArrowRight className="h-3 w-3" />
              <span>
                {t("codexAutoSwitch.target", "目标账号")}:{" "}
                {providerName(data.targetProviderId)}
              </span>
            </>
          ) : null}
        </div>
      ) : null}
      {validCheckedAt ? (
        <p className="text-xs text-muted-foreground">
          {t("codexAutoSwitch.lastChecked", "最近额度检查")}:{" "}
          {checkedAt.toLocaleString(i18n.language)}
        </p>
      ) : null}
      {data?.waitUntil ? (
        <p className="text-xs text-muted-foreground">
          {t("codexAutoSwitch.waitUntil", "最早 5 小时额度重置")}:{" "}
          {new Date(data.waitUntil).toLocaleString(i18n.language)}
        </p>
      ) : null}
      {!status.isError && data?.candidateFailures?.length ? (
        <div className="space-y-1 text-xs">
          <p className="font-medium">
            {t("codexAutoSwitch.candidateFailures", "账号不可用原因")}
          </p>
          <ul className="list-disc space-y-1 pl-4 text-muted-foreground">
            {data.candidateFailures.map((failure, index) => (
              <li key={`${index}:${failure}`} className="break-words">
                {candidateReason(failure)}
              </li>
            ))}
          </ul>
        </div>
      ) : null}
      {!failureHistory.isError && recentFailures.length ? (
        <div className="space-y-1 text-xs">
          <p className="font-medium">
            {t("codexAutoSwitch.failureHistory", "最近切换失败记录")}
          </p>
          <ol className="space-y-1 text-muted-foreground">
            {recentFailures.map((failure, index) => (
              <li
                key={`${failure.at}:${failure.phase}:${index}`}
                className="rounded-md border border-border/60 px-2 py-1.5"
              >
                <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5">
                  <time dateTime={failureDateTime(failure)}>
                    {formatFailureTime(failure)}
                  </time>
                  <span aria-hidden="true">·</span>
                  <span>
                    {t(
                      `codexAutoSwitch.phases.${failure.stage || failure.phase}`,
                      {
                        defaultValue:
                          PHASE_LABELS[failure.stage || failure.phase] ||
                          failure.stage ||
                          failure.phase,
                      },
                    )}
                  </span>
                  {failure.source ? (
                    <span>
                      {t(`codexAutoSwitch.sources.${failure.source}`, {
                        defaultValue:
                          SOURCE_LABELS[failure.source] || failure.source,
                      })}
                    </span>
                  ) : null}
                  {failure.currentProviderId ? (
                    <span>
                      {t("codexAutoSwitch.checkedAccount", "本次检查账号")}:{" "}
                      {providerName(failure.currentProviderId)}
                    </span>
                  ) : null}
                  {failure.targetProviderId ? (
                    <span>
                      {t("codexAutoSwitch.target", "目标账号")}:{" "}
                      {providerName(failure.targetProviderId)}
                    </span>
                  ) : null}
                </div>
                <p className="break-words">{failure.reason}</p>
                {failure.candidateFailures?.length ? (
                  <ul className="list-disc pl-4">
                    {failure.candidateFailures.map((reason, reasonIndex) => (
                      <li key={reasonIndex}>{candidateReason(reason)}</li>
                    ))}
                  </ul>
                ) : null}
              </li>
            ))}
          </ol>
        </div>
      ) : null}
      {failureHistory.isError ? (
        <div role="alert" className="space-y-1 text-xs text-destructive">
          <p>
            {t("codexAutoSwitch.historyUnavailable", "切换失败记录读取失败")}:{" "}
            {extractErrorMessage(failureHistory.error)}
          </p>
          <Button
            type="button"
            variant="outline"
            size="sm"
            disabled={failureHistory.isFetching}
            onClick={() => void failureHistory.refetch()}
          >
            {t("codexAutoSwitch.refreshHistory", "重新读取失败记录")}
          </Button>
        </div>
      ) : null}
      {error ? (
        <div role="alert" className="space-y-2 text-xs text-destructive">
          <p className="break-words">{extractErrorMessage(error)}</p>
          {status.isError ? (
            <Button
              type="button"
              variant="outline"
              size="sm"
              disabled={status.isFetching || isPending}
              onClick={() => {
                control.reset();
                void status.refetch();
              }}
            >
              {t("codexAutoSwitch.refreshStatus", "重新读取状态")}
            </Button>
          ) : null}
        </div>
      ) : null}
    </section>
  );
}
