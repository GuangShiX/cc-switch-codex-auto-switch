import { invoke } from "@tauri-apps/api/core";

export interface CodexAutoSwitchStatus {
  enabled: boolean;
  phase: string;
  message: string;
  operationId?: string | null;
  currentProviderId?: string | null;
  targetProviderId?: string | null;
  checkedAt?: number | null;
  candidateFailures: string[];
  canCancel: boolean;
}

/** These commands control the native background service; no monitoring runs in the renderer. */
export const codexAutoSwitchApi = {
  getStatus: (): Promise<CodexAutoSwitchStatus> =>
    invoke("get_codex_auto_switch_status"),
  setEnabled: (enabled: boolean): Promise<unknown> =>
    invoke("set_codex_auto_switch_enabled", { enabled }),
  cancel: (): Promise<unknown> => invoke("cancel_codex_auto_switch"),
};
