import { NotImplementedError } from "../core/types.js";
import { StubRuntimeAdapter } from "./adapter.js";
import type {
  ExecuteTurnInput,
  RuntimeCapabilities,
  RuntimeDescriptor,
  RuntimeEvent,
  RuntimeInventoryProvider,
  RuntimeSessionRef,
} from "./types.js";

export type SlockRuntimeFamily = "claude" | "codex" | "gemini";

export interface SlockDaemonConfig {
  serverUrl?: string;
  machineLabel?: string;
}

export interface SlockDiscoveredRuntime {
  family: SlockRuntimeFamily;
  label?: string;
  capabilities?: Partial<RuntimeCapabilities>;
}

export interface SlockDaemonBridge {
  discoverRuntimes(): Promise<SlockDiscoveredRuntime[]>;
}

const defaultSlockCapabilities: RuntimeCapabilities = {
  canResumeSession: true,
  canInterrupt: true,
  supportsToolStreaming: true,
  supportsStructuredOutput: false,
};

export class StaticSlockDaemonBridge implements SlockDaemonBridge {
  constructor(private readonly runtimes: SlockDiscoveredRuntime[]) {}

  async discoverRuntimes(): Promise<SlockDiscoveredRuntime[]> {
    return [...this.runtimes];
  }
}

export class SlockRuntimeInventoryProvider implements RuntimeInventoryProvider {
  constructor(
    private readonly bridge: SlockDaemonBridge,
    private readonly config: SlockDaemonConfig = {},
  ) {}

  async list(): Promise<RuntimeDescriptor[]> {
    const runtimes = await this.bridge.discoverRuntimes();

    return runtimes.map((runtime) => ({
      target: `slock:${runtime.family}`,
      adapterId: "slock",
      family: runtime.family,
      label: runtime.label ?? `Slock ${runtime.family}`,
      transport: "daemon",
      capabilities: {
        ...defaultSlockCapabilities,
        ...runtime.capabilities,
      },
      metadata: {
        ...(this.config.serverUrl ? { serverUrl: this.config.serverUrl } : {}),
        ...(this.config.machineLabel ? { machineLabel: this.config.machineLabel } : {}),
      },
    }));
  }
}

export class SlockDaemonAdapter extends StubRuntimeAdapter {
  readonly adapterId = "slock";

  readonly capabilities = defaultSlockCapabilities;

  async *executeTurn(input: ExecuteTurnInput): AsyncIterable<RuntimeEvent> {
    throw new NotImplementedError(
      `Slock daemon transport for "${input.descriptor.target}" is not implemented yet.`,
    );
  }

  async interrupt(_session: RuntimeSessionRef): Promise<void> {}
}
