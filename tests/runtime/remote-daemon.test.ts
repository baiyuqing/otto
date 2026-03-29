import { describe, expect, it } from "vitest";

import { RemoteRuntimeInventoryProvider, StaticRemoteDaemonBridge } from "../../src/runtime/remote-daemon.js";

describe("RemoteRuntimeInventoryProvider", () => {
  it("maps discovered daemon runtimes into framework runtime targets", async () => {
    const provider = new RemoteRuntimeInventoryProvider(
      new StaticRemoteDaemonBridge([
        { family: "claude" },
        { family: "codex" },
        { family: "gemini" },
      ]),
      { serverUrl: "https://runtime.example.com", machineLabel: "Yuqing" },
    );

    const runtimes = await provider.list();

    expect(runtimes.map((runtime) => runtime.target)).toEqual([
      "remote:claude",
      "remote:codex",
      "remote:gemini",
    ]);
    expect(runtimes[1]?.adapterId).toBe("remote");
    expect(runtimes[1]?.metadata?.serverUrl).toBe("https://runtime.example.com");
  });
});
