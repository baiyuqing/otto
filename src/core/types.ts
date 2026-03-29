export type TurnOutcome = "success" | "partial" | "failed";

export interface UserMessage {
  id?: string;
  text: string;
  timestamp?: string;
}

export interface TurnMetadata {
  agentId: string;
  logicalSessionId: string;
  turnId: string;
  startedAt: string;
}

export interface TokenUsage {
  inputTokens: number;
  outputTokens: number;
  totalTokens?: number;
}

export interface RuntimeError {
  code: string;
  message: string;
  cause?: unknown;
}

export interface KernelDiagnostics {
  runtime: string;
  recalledCounts: {
    working: number;
    factual: number;
    experiential: number;
  };
  skills?: string[];
  usage?: TokenUsage;
}

export interface TurnSummary {
  summary: string;
  outcome: TurnOutcome;
  lessons: string[];
  relatedFiles: string[];
}

export class NotImplementedError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "NotImplementedError";
  }
}
