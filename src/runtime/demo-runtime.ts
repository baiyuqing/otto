import { StubRuntimeAdapter } from "./adapter.js";
import type { ExecuteTurnInput, RuntimeEvent, RuntimeSessionRef } from "./types.js";

function estimateTokens(text: string): number {
  return Math.max(1, Math.ceil(text.length / 4));
}

export class DemoRuntimeAdapter extends StubRuntimeAdapter {
  readonly adapterId = "local:demo";

  readonly capabilities = {
    canResumeSession: true,
    canInterrupt: false,
    supportsToolStreaming: false,
    supportsStructuredOutput: false,
  } as const;

  readonly descriptor = {
    target: "demo",
    adapterId: this.adapterId,
    family: "demo",
    label: "Local Demo Runtime",
    transport: "local",
    capabilities: this.capabilities,
  } as const;

  async *executeTurn(input: ExecuteTurnInput): AsyncIterable<RuntimeEvent> {
    const layerKinds = input.prompt.layers.map((layer) => layer.kind).join(", ");
    const text = [
      "Demo runtime response.",
      "",
      `Task: ${input.prompt.user}`,
      `Runtime target: ${input.descriptor.target}`,
      `Prompt layers: ${layerKinds || "none"}`,
      `Workspace: ${input.session.workspacePath}`,
      "",
      "This is a smoke-test runtime. Replace DemoRuntimeAdapter with CodexRuntimeAdapter or ClaudeCodeRuntimeAdapter to execute real work.",
    ].join("\n");

    yield { type: "assistant_message", text };
    yield {
      type: "completed",
      usage: {
        inputTokens: estimateTokens(input.prompt.system + input.prompt.user),
        outputTokens: estimateTokens(text),
        totalTokens: estimateTokens(input.prompt.system + input.prompt.user + text),
      },
    };
  }

  async interrupt(_session: RuntimeSessionRef): Promise<void> {}
}
