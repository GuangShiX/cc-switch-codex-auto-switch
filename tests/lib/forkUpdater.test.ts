import { describe, expect, it, vi } from "vitest";

const getVersion = vi.hoisted(() => vi.fn());
const check = vi.hoisted(() => vi.fn());

vi.mock("@tauri-apps/api/app", () => ({ getVersion }));
vi.mock("@tauri-apps/plugin-updater", () => ({ check }));

import { checkForUpdate } from "@/lib/updater";

describe("fork update channel", () => {
  it("does not query or install upstream packages", async () => {
    await expect(checkForUpdate()).rejects.toThrow(
      "cc-switch-codex-auto-switch/releases",
    );
    expect(check).not.toHaveBeenCalled();
    expect(getVersion).not.toHaveBeenCalled();
  });
});
