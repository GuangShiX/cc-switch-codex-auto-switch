import React from "react";
import { RefreshCw } from "lucide-react";
import { useTranslation } from "react-i18next";
import type { ProviderMeta } from "@/types";
import { useCodexOauthQuota } from "@/lib/query/subscription";
import { SubscriptionQuotaView } from "@/components/SubscriptionQuotaFooter";

interface CodexOauthQuotaFooterProps {
  meta?: ProviderMeta;
  inline?: boolean;
  /** 是否为当前激活的供应商 */
  isCurrent?: boolean;
  autoQueryInterval?: number;
}

/**
 * Codex OAuth (ChatGPT Plus/Pro 反代) 订阅额度 footer
 *
 * 复用 SubscriptionQuotaView 的全部渲染逻辑（5 状态 × inline/expanded）。
 * 数据源切换为 cc-switch 自管的 OAuth token 而非 Codex CLI 凭据。
 */
const CodexOauthQuotaFooter: React.FC<CodexOauthQuotaFooterProps> = ({
  meta,
  inline = false,
  isCurrent = false,
  autoQueryInterval = 5,
}) => {
  const { t } = useTranslation();
  const {
    data: quota,
    isFetching: loading,
    refetch,
  } = useCodexOauthQuota(meta, {
    // Every account loads its initial quota; only the current account polls.
    // Inactive accounts reuse cached/native results for thirty minutes,
    // including when cards are remounted during navigation.
    enabled: true,
    autoQuery: isCurrent && autoQueryInterval > 0,
    autoQueryIntervalMinutes: autoQueryInterval,
  });

  if (!isCurrent && !quota) {
    return (
      <button
        type="button"
        onClick={() => void refetch()}
        disabled={loading}
        className="inline-flex items-center gap-1.5 rounded p-1 text-xs text-muted-foreground hover:bg-muted disabled:opacity-50"
      >
        <RefreshCw size={12} className={loading ? "animate-spin" : ""} />
        {t("subscription.refresh")}
      </button>
    );
  }

  return (
    <div className={inline ? "inline-flex items-center gap-1" : undefined}>
      {quota?.success && quota.codexLimitPolicy === "weekly_only" ? (
        <span className="text-xs text-muted-foreground">
          {t("codexAutoSwitch.weeklyOnlyPro", "Pro · 无5小时窗口")}
        </span>
      ) : null}
      {quota?.success ? (
        <span className="text-xs text-muted-foreground">
          {t("codexAutoSwitch.quotaUsed", "已用")}
        </span>
      ) : null}
      <SubscriptionQuotaView
        quota={quota}
        loading={loading}
        refetch={refetch}
        appIdForExpiredHint="codex_oauth"
        inline={inline}
      />
    </div>
  );
};

export default CodexOauthQuotaFooter;
