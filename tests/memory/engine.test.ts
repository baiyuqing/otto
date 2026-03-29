import { describe, expect, it } from "vitest";

import { NullMemoryEngine } from "../../src/memory/engine.js";

describe("NullMemoryEngine", () => {
  it("returns the new empty recall and writeback shapes", async () => {
    const engine = new NullMemoryEngine();

    const recall = await engine.recall({
      agentId: "otto",
      task: "design memory",
      workspacePath: "/tmp/workspace",
      limit: 8,
    });

    expect(recall).toEqual({
      working: null,
      factual: [],
      experiential: [],
    });

    const writeback = await engine.writeTurn({
      agentId: "otto",
      workspacePath: "/tmp/workspace",
      logicalSessionId: "session-1",
      runtimeTarget: "codex",
      runtimeFamily: "codex",
      runtimeSessionId: "runtime-1",
      prompt: {
        system: "system",
        user: "user",
        layers: [],
      },
      outputText: "done",
      transcript: [],
      summary: {
        summary: "done",
        outcome: "success",
        lessons: [],
        relatedFiles: [],
      },
    });

    expect(writeback).toEqual({
      workingMemoryUpdated: false,
      factsWritten: 0,
      experiencesWritten: 0,
      candidateSoulDeltas: 0,
    });
  });
});
