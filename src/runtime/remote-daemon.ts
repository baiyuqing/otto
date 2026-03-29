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

export type RemoteRuntimeFamily = "claude" | "codex" | "gemini";

export interface RemoteDaemonConfig {
  serverUrl?: string;
  machineLabel?: string;
}

export interface RemoteDiscoveredRuntime {
  family: RemoteRuntimeFamily;
  label?: string;
  capabilities?: Partial<RuntimeCapabilities>;
}

export interface RemoteDaemonBridge {
  discoverRuntimes(): Promise<RemoteDiscoveredRuntime[]>;
}

const defaultRemoteCapabilities: RuntimeCapabilities = {
  canResumeSession: true,
  canInterrupt: true,
  supportsToolStreaming: true,
  supportsStructuredOutput: false,
};

export class StaticRemoteDaemonBridge implements RemoteDaemonBridge {
  constructor(private readonly runtimes: RemoteDiscoveredRuntime[]) {}

  async discoverRuntimes(): Promise<RemoteDiscoveredRuntime[]> {
    return [...this.runtimes];
  }
}

export class RemoteRuntimeInventoryProvider implements RuntimeInventoryProvider {
  constructor(
    private readonly bridge: RemoteDaemonBridge,
    private readonly config: RemoteDaemonConfig = {},
  ) {}

  async list(): Promise<RuntimeDescriptor[]> {
    const runtimes = await this.bridge.discoverRuntimes();

    return runtimes.map((runtime) => ({
      target: `remote:${runtime.family}`,
      adapterId: "remote",
      family: runtime.family,
      label: runtime.label ?? `Remote ${runtime.family}`,
      transport: "daemon",
      capabilities: {
        ...defaultRemoteCapabilities,
        ...runtime.capabilities,
      },
      metadata: {
        ...(this.config.serverUrl ? { serverUrl: this.config.serverUrl } : {}),
        ...(this.config.machineLabel ? { machineLabel: this.config.machineLabel } : {}),
      },
    }));
  }
}

export class RemoteDaemonAdapter extends StubRuntimeAdapter {
  readonly adapterId = "remote";

  readonly capabilities = defaultRemoteCapabilities;

  async *executeTurn(input: ExecuteTurnInput): AsyncIterable<RuntimeEvent> {
    throw new NotImplementedError(
      `Remote daemon transport for "${input.descriptor.target}" is not implemented yet.`,
    );
  }

  async interrupt(_session: RuntimeSessionRef): Promise<void> {}
}
