import type { TokenUsage, TurnSummary } from "../core/types.js";
import type { AssembledPrompt } from "../prompt/layers.js";
import type { RuntimeEvent, RuntimeFamily, RuntimeTarget } from "../runtime/types.js";

export interface WorkingMemoryEntry {
  turnId: string;
  content: string;
  expiresAt?: string;
}

export interface EpisodicMemory {
  id: string;
  timestamp: string;
  taskSummary: string;
  outcome: TurnSummary["outcome"];
  lessons: string[];
  relatedFiles: string[];
}

export interface SemanticMemory {
  id: string;
  topic: string;
  fact: string;
  confidence: number;
  sources: string[];
}

export interface RelationshipMemory {
  id: string;
  userId: string;
  preference: string;
  evidence: string;
  confidence: number;
}

export interface MemoryRetrievalQuery {
  agentId: string;
  task: string;
  workspacePath: string;
  limit: number;
}

export interface MemoryRecall {
  pinned: string[];
  episodic: EpisodicMemory[];
  semantic: SemanticMemory[];
  relationship: RelationshipMemory[];
}

export interface TurnWritebackInput {
  agentId: string;
  workspacePath: string;
  logicalSessionId: string;
  runtimeTarget: RuntimeTarget;
  runtimeFamily: RuntimeFamily;
  runtimeSessionId: string;
  prompt: AssembledPrompt;
  outputText: string;
  transcript: RuntimeEvent[];
  summary: TurnSummary;
  usage?: TokenUsage;
}

export interface WritebackReport {
  episodesWritten: number;
  semanticsWritten: number;
  relationshipsUpdated: number;
  candidateSoulDeltas: number;
}

export interface MemoryEngine {
  recall(query: MemoryRetrievalQuery): Promise<MemoryRecall>;
  writeTurn(input: TurnWritebackInput): Promise<WritebackReport>;
}
