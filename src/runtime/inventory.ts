import type {
  RuntimeAdapter,
  RuntimeDescriptor,
  RuntimeInventoryProvider,
  RuntimeTarget,
} from "./types.js";

export class StaticRuntimeInventoryProvider implements RuntimeInventoryProvider {
  constructor(private readonly runtimes: RuntimeDescriptor[]) {}

  static fromAdapters(adapters: Iterable<RuntimeAdapter>): StaticRuntimeInventoryProvider {
    const runtimes: RuntimeDescriptor[] = [];

    for (const adapter of adapters) {
      if (adapter.descriptor) {
        runtimes.push(adapter.descriptor);
      }
    }

    return new StaticRuntimeInventoryProvider(runtimes);
  }

  async list(): Promise<RuntimeDescriptor[]> {
    return [...this.runtimes];
  }
}

export class CompositeRuntimeInventoryProvider implements RuntimeInventoryProvider {
  constructor(private readonly providers: RuntimeInventoryProvider[]) {}

  async list(): Promise<RuntimeDescriptor[]> {
    const merged = new Map<string, RuntimeDescriptor>();

    for (const provider of this.providers) {
      for (const runtime of await provider.list()) {
        merged.set(runtime.target, runtime);
      }
    }

    return [...merged.values()];
  }
}

export function findRuntimeDescriptor(
  runtimes: RuntimeDescriptor[],
  target: RuntimeTarget,
): RuntimeDescriptor | undefined {
  return runtimes.find((runtime) => runtime.target === target);
}
