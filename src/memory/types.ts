import type { TokenUsage, TurnSummary } from "../core/types.js";
import type { AssembledPrompt } from "../prompt/layers.js";
import type { RuntimeEvent, RuntimeFamily, RuntimeTarget } from "../runtime/types.js";

export type WorkingMemoryScope = "session" | "task";

export type MemoryScope =
  | WorkingMemoryScope
  | "agent-private"
  | "user-private"
  | "project-shared"
  | "team-shared"
  | "published";

export type MemoryEntryKind = "factual" | "experiential";

export type MemoryCandidateKind = "working" | "fact" | "experience";

export type MemoryEvidenceKind = "message" | "activity" | "artifact" | "doc" | "task" | "review" | "output";

export interface WorkingMemoryState {
  key: string;
  scope: WorkingMemoryScope;
  objective: string;
  plan: string[];
  openLoops: string[];
  blockers: string[];
  activeArtifacts: string[];
  ownerAgentId?: string;
  summary: string;
  updatedAt: string;
}

export interface MemoryEvidenceRef {
  kind: MemoryEvidenceKind;
  id: string;
  detail?: string;
}

export interface MemoryEntry {
  id: string;
  kind: MemoryEntryKind;
  scope: Exclude<MemoryScope, WorkingMemoryScope>;
  title: string;
  content: string;
  confidence: number;
  sourceRefs: string[];
  tags: string[];
  createdAt: string;
  updatedAt: string;
  lastVerifiedAt?: string;
  supersedes?: string;
}

export interface MemoryCandidate {
  id: string;
  kind: MemoryCandidateKind;
  scope: MemoryScope;
  content: string;
  evidenceRefs: MemoryEvidenceRef[];
  proposedBy: "policy" | "agent";
  createdAt: string;
}

export interface MemoryRetrievalQuery {
  agentId: string;
  task: string;
  workspacePath: string;
  limit: number;
}

export interface MemoryEntryQuery {
  kinds?: MemoryEntryKind[];
  scopes?: MemoryEntry["scope"][];
  tags?: string[];
  limit?: number;
}

export interface MemoryCandidateQuery {
  kinds?: MemoryCandidateKind[];
  scopes?: MemoryScope[];
  limit?: number;
}

export interface MemoryRecall {
  working: WorkingMemoryState | null;
  factual: MemoryEntry[];
  experiential: MemoryEntry[];
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
  workingMemoryUpdated: boolean;
  factsWritten: number;
  experiencesWritten: number;
  candidateSoulDeltas: number;
}

export interface MemoryEngine {
  recall(query: MemoryRetrievalQuery): Promise<MemoryRecall>;
  writeTurn(input: TurnWritebackInput): Promise<WritebackReport>;
}

export interface MemoryStore {
  getWorkingMemory(key: string): Promise<WorkingMemoryState | null>;
  saveWorkingMemory(state: WorkingMemoryState): Promise<void>;
  listEntries(query?: MemoryEntryQuery): Promise<MemoryEntry[]>;
  saveEntry(entry: MemoryEntry): Promise<void>;
  listCandidates(query?: MemoryCandidateQuery): Promise<MemoryCandidate[]>;
  appendCandidate(candidate: MemoryCandidate): Promise<void>;
}
