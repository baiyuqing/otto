import { StubRuntimeAdapter } from "./adapter.js";

export class CodexRuntimeAdapter extends StubRuntimeAdapter {
  readonly adapterId = "local:codex";

  readonly capabilities = {
    canResumeSession: true,
    canInterrupt: true,
    supportsToolStreaming: true,
    supportsStructuredOutput: false,
  } as const;

  readonly descriptor = {
    target: "codex",
    adapterId: this.adapterId,
    family: "codex",
    label: "Local Codex",
    transport: "local",
    capabilities: this.capabilities,
  } as const;
}
