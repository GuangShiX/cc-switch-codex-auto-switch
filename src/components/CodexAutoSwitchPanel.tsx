import { useTranslation } from "react-i18next";
import { AlertTriangle, ArrowRight, Loader2, Repeat2 } from "lucide-react";
import type { Provider } from "@/types";
import { useCodexAutoSwitch } from "@/lib/query/codexAutoSwitch";
import { extractErrorMessage } from "@/utils/errorUtils";
import { Switch } from "@/components/ui/switch";
import { Button } from "@/components/ui/button";

const PHASE_LABELS: Record<string, string> = {
  waiting: "等待可用账号",
  monitoring: "正在后台监测",
  checking: "正在查询当前账号额度",
  selecting: "正在按列表顺序查询候选账号",
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

export function CodexAutoSwitchPanel({
  providers = {},
}: {
  providers?: Record<string, Provider>;
}) {
  const { t, i18n } = useTranslation();
  const { status, control, isPending } = useCodexAutoSwitch();
  const data = status.data;
  const controlsDisabled = !data || status.isError || isPending;
  const needsAttention =
    data && ["blocked", "failed", "waiting"].includes(data.phase);
  const checkedAt = data?.checkedAt
    ? new Date(data.checkedAt < 1e12 ? data.checkedAt * 1000 : data.checkedAt)
    : null;
  const validCheckedAt = checkedAt && Number.isFinite(checkedAt.getTime());
  const providerName = (id: string) => providers[id]?.name || id;
  const error = control.error || status.error;

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
          "5 小时剩余额度严格低于 5%，或周剩余额度为 0% 时触发。5 小时恰好 5%、周剩余 1% 仍可使用。按 Codex 账号列表从上到下查询，跳过当前账号，使用第一个可用账号。",
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
          "切号时正常关闭并重开 Codex 桌面，再恢复本次暂停的原任务。用户手动停止、等待审批和已完成的任务不会自动继续。关闭自动开关不影响手动启用账号。",
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
      {!status.isError && data?.candidateFailures?.length ? (
        <div className="space-y-1 text-xs">
          <p className="font-medium">
            {t("codexAutoSwitch.candidateFailures", "账号不可用原因")}
          </p>
          <ul className="list-disc space-y-1 pl-4 text-muted-foreground">
            {data.candidateFailures.map((failure, index) => (
              <li key={`${index}:${failure}`} className="break-words">
                {failure}
              </li>
            ))}
          </ul>
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
