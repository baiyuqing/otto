import { describe, expect, it } from "vitest";

import {
  SlockRuntimeInventoryProvider,
  StaticSlockDaemonBridge,
} from "../../src/runtime/slock-daemon.js";

describe("SlockRuntimeInventoryProvider", () => {
  it("maps discovered daemon runtimes into framework runtime targets", async () => {
    const provider = new SlockRuntimeInventoryProvider(
      new StaticSlockDaemonBridge([
        { family: "claude" },
        { family: "codex" },
        { family: "gemini" },
      ]),
      { serverUrl: "https://api.slock.ai", machineLabel: "Yuqing" },
    );

    const runtimes = await provider.list();

    expect(runtimes.map((runtime) => runtime.target)).toEqual([
      "slock:claude",
      "slock:codex",
      "slock:gemini",
    ]);
    expect(runtimes[1]?.adapterId).toBe("slock");
    expect(runtimes[1]?.metadata?.serverUrl).toBe("https://api.slock.ai");
  });
});
