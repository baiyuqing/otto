import { StubRuntimeAdapter } from "./adapter.js";

export class ClaudeCodeRuntimeAdapter extends StubRuntimeAdapter {
  readonly adapterId = "local:claude-code";

  readonly capabilities = {
    canResumeSession: true,
    canInterrupt: true,
    supportsToolStreaming: true,
    supportsStructuredOutput: false,
  } as const;

  readonly descriptor = {
    target: "claude-code",
    adapterId: this.adapterId,
    family: "claude-code",
    label: "Local Claude Code",
    transport: "local",
    capabilities: this.capabilities,
  } as const;
}
