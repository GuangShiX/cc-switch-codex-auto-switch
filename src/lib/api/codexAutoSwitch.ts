import { invoke } from "@tauri-apps/api/core";

export interface CodexAutoSwitchStatus {
  enabled: boolean;
  phase: string;
  message: string;
  operationId?: string | null;
  currentProviderId?: string | null;
  targetProviderId?: string | null;
  checkedAt?: number | null;
  waitUntil?: number | null;
  candidateFailures: string[];
  canCancel: boolean;
}

/** A bounded, native-recorded failure entry for later diagnosis. */
export interface CodexAutoSwitchFailure {
  at: number;
  phase: string;
  reason: string;
  currentProviderId?: string | null;
  targetProviderId?: string | null;
  operationId?: string | null;
  source?: string | null;
  stage?: string | null;
  candidateFailures?: string[];
}

/** These commands control the native background service; no monitoring runs in the renderer. */
export const codexAutoSwitchApi = {
  getStatus: (): Promise<CodexAutoSwitchStatus> =>
    invoke("get_codex_auto_switch_status"),
  getFailureHistory: (): Promise<CodexAutoSwitchFailure[]> =>
    invoke("get_codex_auto_switch_failure_history"),
  setEnabled: (enabled: boolean): Promise<unknown> =>
    invoke("set_codex_auto_switch_enabled", { enabled }),
  cancel: (): Promise<unknown> => invoke("cancel_codex_auto_switch"),
};
