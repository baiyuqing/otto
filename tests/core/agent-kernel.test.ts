import { describe, expect, it } from "vitest";

import {
  DefaultAgentKernel,
  DefaultProjectContextStore,
  HeuristicTurnSummarizer,
  StaticRuntimeShimProvider,
} from "../../src/core/agent-kernel.js";
import { NullMemoryEngine } from "../../src/memory/engine.js";
import { NullSoulStore } from "../../src/persona/soul.js";
import { DefaultPromptAssembler } from "../../src/prompt/assembler.js";
import { RuntimeRegistry } from "../../src/runtime/adapter.js";
import type {
  CreateSessionInput,
  ExecuteTurnInput,
  ResumeSessionInput,
  RuntimeAdapter,
  RuntimeDescriptor,
  RuntimeCapabilities,
  RuntimeEvent,
  RuntimeSessionRef,
} from "../../src/runtime/types.js";
import { InMemorySessionManager } from "../../src/session/types.js";

class FakeRuntimeAdapter implements RuntimeAdapter {
  readonly adapterId = "fake:codex";
  readonly capabilities: RuntimeCapabilities = {
    canResumeSession: true,
    canInterrupt: true,
    supportsToolStreaming: true,
    supportsStructuredOutput: false,
  };
  readonly descriptor: RuntimeDescriptor = {
    target: "codex",
    adapterId: this.adapterId,
    family: "codex",
    label: "Fake Codex",
    transport: "local",
    capabilities: this.capabilities,
  };

  lastPrompt?: ExecuteTurnInput["prompt"];

  async createSession(input: CreateSessionInput): Promise<RuntimeSessionRef> {
    return {
      target: input.descriptor.target,
      adapterId: input.descriptor.adapterId,
      runtime: input.descriptor.family,
      sessionId: `fake:${input.logicalSessionId}`,
      workspacePath: input.workspacePath,
    };
  }

  async resumeSession(input: ResumeSessionInput): Promise<RuntimeSessionRef> {
    return {
      target: input.descriptor.target,
      adapterId: input.descriptor.adapterId,
      runtime: input.descriptor.family,
      sessionId: input.runtimeSessionId,
      workspacePath: input.workspacePath,
    };
  }

  async *executeTurn(input: ExecuteTurnInput): AsyncIterable<RuntimeEvent> {
    this.lastPrompt = input.prompt;
    yield { type: "assistant_message_delta", text: "memory" };
    yield { type: "assistant_message_delta", text: " keeps identity" };
    yield {
      type: "completed",
      usage: {
        inputTokens: 100,
        outputTokens: 20,
        totalTokens: 120,
      },
    };
  }

  async interrupt(): Promise<void> {}
}

describe("DefaultAgentKernel", () => {
  it("assembles prompt layers, executes the runtime, and writes memory", async () => {
    const runtime = new FakeRuntimeAdapter();
    const kernel = new DefaultAgentKernel({
      runtimeRegistry: new RuntimeRegistry([runtime]),
      sessionManager: new InMemorySessionManager(),
      memoryEngine: new NullMemoryEngine(),
      promptAssembler: new DefaultPromptAssembler(),
      soulStore: new NullSoulStore(),
      projectContextStore: new DefaultProjectContextStore(),
      turnSummarizer: new HeuristicTurnSummarizer(),
      runtimeShimProvider: new StaticRuntimeShimProvider({
        codex: {
          kind: "runtime",
          priority: 70,
          source: "codex",
          content: "Prefer direct tool calls when the plan is clear.",
        },
      }),
    });

    const result = await kernel.handleTurn({
      agentId: "otto",
      runtimeTarget: "codex",
      workspacePath: "/tmp/workspace",
      userMessage: {
        text: "Design a TypeScript agent kernel.",
      },
    });

    expect(result.outputText).toBe("memory keeps identity");
    expect(result.session.sessionId).toContain("fake:");
    expect(result.writeback.episodesWritten).toBe(0);
    expect(result.diagnostics.runtime).toBe("codex");
    expect(runtime.lastPrompt?.system).toContain("framework");
    expect(runtime.lastPrompt?.system).toContain("codex");
  });
});
