import { randomUUID } from "node:crypto";

import type {
  KernelDiagnostics,
  TokenUsage,
  TurnMetadata,
  TurnSummary,
  UserMessage,
} from "./types.js";
import type { MemoryEngine, MemoryRecall, WritebackReport } from "../memory/types.js";
import type { PromptAssembler, PromptLayer } from "../prompt/layers.js";
import { renderSoulPrompt, type SoulProfile, type SoulStore } from "../persona/soul.js";
import type {
  RuntimeDescriptor,
  RuntimeEvent,
  RuntimeSessionRef,
  RuntimeTarget,
} from "../runtime/types.js";
import { RuntimeRegistry } from "../runtime/adapter.js";
import type { SessionManager } from "../session/types.js";
import type { SkillDefinition, SkillResolver } from "../skills/types.js";

export interface ProjectContext {
  instructions: string[];
  files: string[];
}

export interface ProjectContextStore {
  load(agentId: string, workspacePath: string): Promise<ProjectContext>;
}

export interface RuntimeShimProvider {
  getLayer(target: RuntimeTarget, descriptor: RuntimeDescriptor): PromptLayer | null;
}

export interface SummarizeTurnInput {
  userMessage: UserMessage;
  outputText: string;
  recall: MemoryRecall;
}

export interface TurnSummarizer {
  summarize(input: SummarizeTurnInput): Promise<TurnSummary>;
}

export interface KernelTurnInput {
  agentId: string;
  runtimeTarget: RuntimeTarget;
  workspacePath: string;
  userMessage: UserMessage;
  sessionHint?: string;
}

export interface KernelTurnResult {
  logicalSessionId: string;
  session: RuntimeSessionRef;
  outputText: string;
  writeback: WritebackReport;
  diagnostics: KernelDiagnostics;
}

export interface AgentKernel {
  handleTurn(input: KernelTurnInput): Promise<KernelTurnResult>;
}

export interface AgentKernelOptions {
  runtimeRegistry: RuntimeRegistry;
  sessionManager: SessionManager;
  memoryEngine: MemoryEngine;
  promptAssembler: PromptAssembler;
  soulStore: SoulStore;
  projectContextStore: ProjectContextStore;
  turnSummarizer: TurnSummarizer;
  runtimeShimProvider?: RuntimeShimProvider;
  skillResolver?: SkillResolver;
  baseInstructions?: string[];
}

function createBaseLayer(baseInstructions: string[]): PromptLayer | null {
  if (baseInstructions.length === 0) {
    return null;
  }

  return {
    kind: "base",
    priority: 10,
    source: "framework",
    content: baseInstructions.map((line) => `- ${line}`).join("\n"),
  };
}

function createSoulLayer(profile: SoulProfile | null): PromptLayer | null {
  if (!profile) {
    return null;
  }

  return {
    kind: "soul",
    priority: 20,
    source: "SOUL.md",
    content: renderSoulPrompt(profile),
  };
}

function createProjectLayer(project: ProjectContext): PromptLayer | null {
  if (project.instructions.length === 0) {
    return null;
  }

  return {
    kind: "project",
    priority: 30,
    source: "AGENT.md",
    content: project.instructions.map((line) => `- ${line}`).join("\n"),
  };
}

function createSkillLayer(skills: SkillDefinition[]): PromptLayer | null {
  if (skills.length === 0) {
    return null;
  }

  return {
    kind: "skill",
    priority: 35,
    source: "builtin-skills",
    content: skills
      .map((skill) =>
        [
          `### ${skill.title} (${skill.id})`,
          ...skill.instructions.map((instruction) => `- ${instruction}`),
        ].join("\n"),
      )
      .join("\n\n"),
  };
}

function createTaskLayer(userMessage: UserMessage): PromptLayer {
  return {
    kind: "task",
    priority: 40,
    source: "user",
    content: userMessage.text,
  };
}

function renderMemoryRecall(recall: MemoryRecall): string {
  const parts: string[] = [];

  if (recall.working) {
    const workingLines = [
      `- Objective: ${recall.working.objective}`,
      ...(recall.working.summary.length > 0 ? [`- Summary: ${recall.working.summary}`] : []),
      ...(recall.working.ownerAgentId ? [`- Owner: ${recall.working.ownerAgentId}`] : []),
      ...(recall.working.plan.length > 0 ? [`- Plan: ${recall.working.plan.join(" | ")}`] : []),
      ...(recall.working.openLoops.length > 0
        ? [`- Open loops: ${recall.working.openLoops.join(" | ")}`]
        : []),
      ...(recall.working.blockers.length > 0 ? [`- Blockers: ${recall.working.blockers.join(" | ")}`] : []),
      ...(recall.working.activeArtifacts.length > 0
        ? [`- Active artifacts: ${recall.working.activeArtifacts.join(" | ")}`]
        : []),
    ];

    parts.push("### Working\n" + workingLines.join("\n"));
  }

  if (recall.factual.length > 0) {
    parts.push(
      "### Factual\n" +
        recall.factual
          .map((entry) => `- ${entry.title}: ${entry.content}`)
          .join("\n"),
    );
  }

  if (recall.experiential.length > 0) {
    parts.push(
      "### Experiential\n" +
        recall.experiential
          .map((entry) => `- ${entry.title}: ${entry.content}`)
          .join("\n"),
    );
  }

  return parts.join("\n\n");
}

function createMemoryLayer(recall: MemoryRecall): PromptLayer | null {
  const content = renderMemoryRecall(recall);

  if (content.length === 0) {
    return null;
  }

  return {
    kind: "memory",
    priority: 50,
    source: "memory-engine",
    content,
  };
}

async function collectRuntimeOutput(
  events: AsyncIterable<RuntimeEvent>,
): Promise<{ outputText: string; transcript: RuntimeEvent[]; usage?: TokenUsage }> {
  const transcript: RuntimeEvent[] = [];
  let outputText = "";
  let usage: TokenUsage | undefined;
  let sawFullMessage = false;

  for await (const event of events) {
    transcript.push(event);

    switch (event.type) {
      case "assistant_message_delta":
        if (!sawFullMessage) {
          outputText += event.text;
        }
        break;
      case "assistant_message":
        outputText = event.text;
        sawFullMessage = true;
        break;
      case "completed":
        usage = event.usage;
        break;
      case "failed":
        throw new Error(`[${event.error.code}] ${event.error.message}`);
      default:
        break;
    }
  }

  return usage ? { outputText, transcript, usage } : { outputText, transcript };
}

export class DefaultProjectContextStore implements ProjectContextStore {
  async load(): Promise<ProjectContext> {
    return { instructions: [], files: [] };
  }
}

export class StaticRuntimeShimProvider implements RuntimeShimProvider {
  constructor(private readonly layers: Record<string, PromptLayer>) {}

  getLayer(target: RuntimeTarget, descriptor: RuntimeDescriptor): PromptLayer | null {
    return this.layers[target] ?? this.layers[descriptor.family] ?? null;
  }
}

export class HeuristicTurnSummarizer implements TurnSummarizer {
  async summarize(input: SummarizeTurnInput): Promise<TurnSummary> {
    const summary = `${input.userMessage.text.trim()} -> ${input.outputText.trim()}`.slice(0, 240);

    return {
      summary,
      outcome: input.outputText.trim().length > 0 ? "success" : "partial",
      lessons: [],
      relatedFiles: [],
    };
  }
}

export class DefaultAgentKernel implements AgentKernel {
  private readonly baseInstructions: string[];

  constructor(private readonly options: AgentKernelOptions) {
    this.baseInstructions = options.baseInstructions ?? [
      "Prefer correct, verifiable actions over confident guessing.",
      "Use memory as support, not as a substitute for current evidence.",
      "Keep the same identity across runtime switches.",
    ];
  }

  async handleTurn(input: KernelTurnInput): Promise<KernelTurnResult> {
    const { adapter, descriptor } = await this.options.runtimeRegistry.resolve(input.runtimeTarget);
    const logicalSession = await this.options.sessionManager.loadOrCreate({
      agentId: input.agentId,
      runtimeTarget: input.runtimeTarget,
      workspacePath: input.workspacePath,
      ...(input.sessionHint ? { sessionHint: input.sessionHint } : {}),
    });

    const runtimeSession =
      logicalSession.runtimeSession?.target === descriptor.target && adapter.capabilities.canResumeSession
        ? await adapter.resumeSession({
            descriptor,
            agentId: input.agentId,
            workspacePath: input.workspacePath,
            logicalSessionId: logicalSession.logicalSessionId,
            runtimeSessionId: logicalSession.runtimeSession.sessionId,
          })
        : await adapter.createSession({
            descriptor,
            agentId: input.agentId,
            workspacePath: input.workspacePath,
            logicalSessionId: logicalSession.logicalSessionId,
          });

    const activeSession = await this.options.sessionManager.attachRuntimeSession(
      logicalSession,
      runtimeSession,
    );

    const recall = await this.options.memoryEngine.recall({
      agentId: input.agentId,
      task: input.userMessage.text,
      workspacePath: input.workspacePath,
      limit: 12,
    });

    const soul = await this.options.soulStore.load(input.agentId, input.workspacePath);
    const project = await this.options.projectContextStore.load(input.agentId, input.workspacePath);
    const skills = this.options.skillResolver
      ? await this.options.skillResolver.resolve(input.workspacePath)
      : [];
    const runtimeLayer = this.options.runtimeShimProvider?.getLayer(input.runtimeTarget, descriptor) ?? null;

    const layers = [
      createBaseLayer(this.baseInstructions),
      createSoulLayer(soul),
      createProjectLayer(project),
      createSkillLayer(skills),
      createTaskLayer(input.userMessage),
      createMemoryLayer(recall),
      runtimeLayer,
    ].filter((layer): layer is PromptLayer => layer !== null);

    const prompt = await this.options.promptAssembler.assemble({
      userMessage: input.userMessage.text,
      layers,
    });

    const metadata: TurnMetadata = {
      agentId: input.agentId,
      logicalSessionId: activeSession.logicalSessionId,
      turnId: randomUUID(),
      startedAt: new Date().toISOString(),
    };

    const { outputText, transcript, usage } = await collectRuntimeOutput(
      adapter.executeTurn({
        descriptor,
        session: runtimeSession,
        prompt,
        metadata,
      }),
    );

    const summary = await this.options.turnSummarizer.summarize({
      userMessage: input.userMessage,
      outputText,
      recall,
    });

    const writeback = await this.options.memoryEngine.writeTurn({
      agentId: input.agentId,
      workspacePath: input.workspacePath,
      logicalSessionId: activeSession.logicalSessionId,
      runtimeTarget: descriptor.target,
      runtimeFamily: descriptor.family,
      runtimeSessionId: runtimeSession.sessionId,
      prompt,
      outputText,
      transcript,
      summary,
      ...(usage ? { usage } : {}),
    });

    return {
      logicalSessionId: activeSession.logicalSessionId,
      session: runtimeSession,
      outputText,
      writeback,
      diagnostics: {
        runtime: descriptor.target,
        recalledCounts: {
          working: recall.working ? 1 : 0,
          factual: recall.factual.length,
          experiential: recall.experiential.length,
        },
        ...(skills.length > 0 ? { skills: skills.map((skill) => skill.id) } : {}),
        ...(usage ? { usage } : {}),
      },
    };
  }
}
