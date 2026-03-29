import type { RuntimeError, TokenUsage, TurnMetadata } from "../core/types.js";
import type { AssembledPrompt } from "../prompt/layers.js";

export type RuntimeAdapterId = string;
export type RuntimeTarget = string;
export type RuntimeTransport = "local" | "daemon";
export type RuntimeFamily = "codex" | "claude-code" | "claude" | "demo" | "gemini";

export interface RuntimeCapabilities {
  canResumeSession: boolean;
  canInterrupt: boolean;
  supportsToolStreaming: boolean;
  supportsStructuredOutput: boolean;
}

export interface RuntimeDescriptor {
  target: RuntimeTarget;
  adapterId: RuntimeAdapterId;
  family: RuntimeFamily;
  label: string;
  transport: RuntimeTransport;
  capabilities: RuntimeCapabilities;
  metadata?: Record<string, string>;
}

export interface RuntimeSessionRef {
  target: RuntimeTarget;
  adapterId: RuntimeAdapterId;
  runtime: RuntimeFamily;
  sessionId: string;
  workspacePath: string;
}

export interface CreateSessionInput {
  descriptor: RuntimeDescriptor;
  agentId: string;
  workspacePath: string;
  logicalSessionId: string;
}

export interface ResumeSessionInput extends CreateSessionInput {
  runtimeSessionId: string;
}

export interface ExecuteTurnInput {
  descriptor: RuntimeDescriptor;
  session: RuntimeSessionRef;
  prompt: AssembledPrompt;
  metadata: TurnMetadata;
}

export type RuntimeEvent =
  | { type: "assistant_message_delta"; text: string }
  | { type: "assistant_message"; text: string }
  | { type: "tool_call_started"; toolName: string; callId: string; input: unknown }
  | { type: "tool_call_finished"; toolName: string; callId: string; output: unknown }
  | { type: "warning"; message: string }
  | { type: "completed"; usage?: TokenUsage }
  | { type: "failed"; error: RuntimeError };

export interface RuntimeAdapter {
  readonly adapterId: RuntimeAdapterId;
  readonly capabilities: RuntimeCapabilities;
  readonly descriptor?: RuntimeDescriptor;

  createSession(input: CreateSessionInput): Promise<RuntimeSessionRef>;
  resumeSession(input: ResumeSessionInput): Promise<RuntimeSessionRef>;
  executeTurn(input: ExecuteTurnInput): AsyncIterable<RuntimeEvent>;
  interrupt(session: RuntimeSessionRef): Promise<void>;
}

export interface RuntimeInventoryProvider {
  list(): Promise<RuntimeDescriptor[]>;
}
