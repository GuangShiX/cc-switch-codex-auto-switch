import { describe, expect, it, vi } from "vitest";
import { http, HttpResponse } from "msw";
import { codexAutoSwitchApi } from "@/lib/api/codexAutoSwitch";
import { server } from "../msw/server";

describe("Codex auto switch native command contract", () => {
  it("reads status and sends only explicit control commands with the expected arguments", async () => {
    const calls = vi.fn();
    const nativeStatus = {
      enabled: false,
      phase: "disabled",
      message: "自动换号已关闭",
      candidateFailures: [],
      canCancel: false,
    };
    for (const command of [
      "get_codex_auto_switch_status",
      "set_codex_auto_switch_enabled",
      "cancel_codex_auto_switch",
    ]) {
      server.use(
        http.post(`http://tauri.local/${command}`, async ({ request }) => {
          calls(command, await request.json());
          return HttpResponse.json(
            command === "get_codex_auto_switch_status" ? nativeStatus : null,
          );
        }),
      );
    }

    expect(await codexAutoSwitchApi.getStatus()).toEqual(nativeStatus);
    await codexAutoSwitchApi.setEnabled(true);
    await codexAutoSwitchApi.setEnabled(false);
    await codexAutoSwitchApi.cancel();

    expect(calls.mock.calls).toEqual([
      ["get_codex_auto_switch_status", {}],
      ["set_codex_auto_switch_enabled", { enabled: true }],
      ["set_codex_auto_switch_enabled", { enabled: false }],
      ["cancel_codex_auto_switch", {}],
    ]);
  });
});
