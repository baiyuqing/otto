import { describe, expect, it } from "vitest";

import { DefaultPromptAssembler } from "../../src/prompt/assembler.js";

describe("DefaultPromptAssembler", () => {
  it("sorts layers by priority and strips empty content", async () => {
    const assembler = new DefaultPromptAssembler();

    const prompt = await assembler.assemble({
      userMessage: "  implement memory writeback  ",
      layers: [
        { kind: "runtime", priority: 70, source: "runtime", content: "Runtime hint" },
        { kind: "base", priority: 10, source: "framework", content: "Base policy" },
        { kind: "memory", priority: 50, source: "memory-engine", content: "" },
        { kind: "soul", priority: 20, source: "SOUL.md", content: "Stay pragmatic" },
      ],
    });

    expect(prompt.user).toBe("implement memory writeback");
    expect(prompt.layers.map((layer) => layer.kind)).toEqual(["base", "soul", "runtime"]);
    expect(prompt.system).toContain("## BASE (framework)");
    expect(prompt.system).toContain("## SOUL (SOUL.md)");
    expect(prompt.system).toContain("## RUNTIME (runtime)");
  });
});
