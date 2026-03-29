#!/usr/bin/env node

import {
  DefaultAgentKernel,
  HeuristicTurnSummarizer,
  StaticRuntimeShimProvider,
} from "./core/agent-kernel.js";
import { NullMemoryEngine } from "./memory/engine.js";
import { DefaultPromptAssembler } from "./prompt/assembler.js";
import { RuntimeRegistry } from "./runtime/adapter.js";
import { ClaudeCodeRuntimeAdapter } from "./runtime/claude-code-runtime.js";
import { CodexRuntimeAdapter } from "./runtime/codex-runtime.js";
import { DemoRuntimeAdapter } from "./runtime/demo-runtime.js";
import { CompositeRuntimeInventoryProvider, StaticRuntimeInventoryProvider } from "./runtime/inventory.js";
import {
  SlockDaemonAdapter,
  SlockRuntimeInventoryProvider,
  StaticSlockDaemonBridge,
} from "./runtime/slock-daemon.js";
import type { RuntimeAdapter, RuntimeInventoryProvider } from "./runtime/types.js";
import { InMemorySessionManager } from "./session/types.js";
import { BuiltinSkillRegistry } from "./skills/registry.js";
import { FileProjectContextStore, FileSoulStore } from "./storage/file-stores.js";
import { CliUsageError, formatUsage, parseCliArgs } from "./cli/args.js";

function printResult(result: {
  logicalSessionId: string;
  outputText: string;
  diagnostics: { runtime: string; skills?: string[] };
}): void {
  console.log(`runtime: ${result.diagnostics.runtime}`);
  console.log(`session: ${result.logicalSessionId}`);
  if (result.diagnostics.skills && result.diagnostics.skills.length > 0) {
    console.log(`skills: ${result.diagnostics.skills.join(", ")}`);
  }
  console.log("");
  console.log(result.outputText);
}

function printRuntimeList(runtimes: Awaited<ReturnType<RuntimeRegistry["list"]>>): void {
  for (const runtime of runtimes) {
    console.log(`${runtime.target}  [${runtime.transport}]  ${runtime.label}`);
  }
}

async function main(): Promise<void> {
  try {
    const options = parseCliArgs(process.argv.slice(2));
    const localAdapters: RuntimeAdapter[] = [
      new DemoRuntimeAdapter(),
      new CodexRuntimeAdapter(),
      new ClaudeCodeRuntimeAdapter(),
    ];
    const inventoryProviders: RuntimeInventoryProvider[] = [
      StaticRuntimeInventoryProvider.fromAdapters(localAdapters),
    ];
    const adapters: RuntimeAdapter[] = [...localAdapters];

    if (options.slockRuntimes.length > 0) {
      adapters.push(new SlockDaemonAdapter());
      inventoryProviders.push(
        new SlockRuntimeInventoryProvider(
          new StaticSlockDaemonBridge(
            options.slockRuntimes.map((family) => ({
              family: family as "claude" | "codex" | "gemini",
            })),
          ),
          {
            ...(options.slockServerUrl ? { serverUrl: options.slockServerUrl } : {}),
            ...(options.slockMachineLabel ? { machineLabel: options.slockMachineLabel } : {}),
          },
        ),
      );
    }

    const runtimeRegistry = new RuntimeRegistry(
      adapters,
      new CompositeRuntimeInventoryProvider(inventoryProviders),
    );

    if (options.listRuntimes) {
      printRuntimeList(await runtimeRegistry.list());
      return;
    }

    const kernel = new DefaultAgentKernel({
      runtimeRegistry,
      sessionManager: new InMemorySessionManager(),
      memoryEngine: new NullMemoryEngine(),
      promptAssembler: new DefaultPromptAssembler(),
      soulStore: new FileSoulStore(),
      projectContextStore: new FileProjectContextStore(),
      turnSummarizer: new HeuristicTurnSummarizer(),
      skillResolver: new BuiltinSkillRegistry(),
      runtimeShimProvider: new StaticRuntimeShimProvider({
        demo: {
          kind: "runtime",
          priority: 70,
          source: "demo",
          content: "Explain what the framework would do without executing external tools.",
        },
        codex: {
          kind: "runtime",
          priority: 70,
          source: "codex",
          content: "Prefer direct tool usage when the next action is clear.",
        },
        "claude-code": {
          kind: "runtime",
          priority: 70,
          source: "claude-code",
          content: "Preserve session continuity and make reasoning explicit when needed.",
        },
        claude: {
          kind: "runtime",
          priority: 70,
          source: "claude",
          content: "Use the daemon transport but preserve the same agent identity.",
        },
        gemini: {
          kind: "runtime",
          priority: 70,
          source: "gemini",
          content: "Use the daemon transport but preserve the same agent identity.",
        },
      }),
    });

    const result = await kernel.handleTurn({
      agentId: "otto",
      runtimeTarget: options.runtimeTarget,
      workspacePath: options.workspacePath,
      userMessage: { text: options.message ?? "" },
      ...(options.sessionHint ? { sessionHint: options.sessionHint } : {}),
    });

    if (options.json) {
      console.log(JSON.stringify(result, null, 2));
      return;
    }

    printResult(result);
  } catch (error) {
    if (error instanceof CliUsageError) {
      console.error(error.message);
      console.error("");
      console.error(formatUsage());
      process.exitCode = 1;
      return;
    }

    const message = error instanceof Error ? error.message : String(error);
    console.error(message);
    process.exitCode = 1;
  }
}

await main();
