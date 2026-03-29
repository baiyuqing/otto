import { NotImplementedError } from "../core/types.js";
import { findRuntimeDescriptor, StaticRuntimeInventoryProvider } from "./inventory.js";
import type { RuntimeAdapter, RuntimeDescriptor, RuntimeInventoryProvider, RuntimeSessionRef } from "./types.js";
import type {
  CreateSessionInput,
  ExecuteTurnInput,
  ResumeSessionInput,
  RuntimeCapabilities,
  RuntimeEvent,
  RuntimeTarget,
} from "./types.js";

export class RuntimeRegistry {
  private readonly adapters = new Map<string, RuntimeAdapter>();
  private readonly inventory: RuntimeInventoryProvider;

  constructor(adapters: Iterable<RuntimeAdapter>, inventory?: RuntimeInventoryProvider) {
    const adapterList = [...adapters];

    this.inventory = inventory ?? StaticRuntimeInventoryProvider.fromAdapters(adapterList);

    for (const adapter of adapterList) {
      this.adapters.set(adapter.adapterId, adapter);
    }
  }

  async list(): Promise<RuntimeDescriptor[]> {
    return this.inventory.list();
  }

  async resolve(target: RuntimeTarget): Promise<{ adapter: RuntimeAdapter; descriptor: RuntimeDescriptor }> {
    const runtimes = await this.inventory.list();
    const descriptor = findRuntimeDescriptor(runtimes, target);

    if (!descriptor) {
      throw new Error(`Runtime target "${target}" is not available.`);
    }

    const adapter = this.adapters.get(descriptor.adapterId);

    if (!adapter) {
      throw new Error(`Runtime adapter "${descriptor.adapterId}" is not registered.`);
    }

    return { adapter, descriptor };
  }
}

export abstract class StubRuntimeAdapter implements RuntimeAdapter {
  abstract readonly adapterId: string;
  abstract readonly capabilities: RuntimeCapabilities;
  readonly descriptor?: RuntimeDescriptor;

  async createSession(input: CreateSessionInput): Promise<RuntimeSessionRef> {
    return {
      target: input.descriptor.target,
      adapterId: input.descriptor.adapterId,
      runtime: input.descriptor.family,
      sessionId: `${input.descriptor.target}:${input.logicalSessionId}`,
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

  async *executeTurn(_input: ExecuteTurnInput): AsyncIterable<RuntimeEvent> {
    throw new NotImplementedError(`${this.adapterId} executeTurn is not implemented yet.`);
  }

  async interrupt(_session: RuntimeSessionRef): Promise<void> {
    throw new NotImplementedError(`${this.adapterId} interrupt is not implemented yet.`);
  }
}
