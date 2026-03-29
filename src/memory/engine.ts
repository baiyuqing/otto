import type {
  MemoryEngine,
  MemoryRecall,
  MemoryRetrievalQuery,
  TurnWritebackInput,
  WritebackReport,
} from "./types.js";

export class NullMemoryEngine implements MemoryEngine {
  async recall(_query: MemoryRetrievalQuery): Promise<MemoryRecall> {
    return {
      working: null,
      factual: [],
      experiential: [],
    };
  }

  async writeTurn(_input: TurnWritebackInput): Promise<WritebackReport> {
    return {
      workingMemoryUpdated: false,
      factsWritten: 0,
      experiencesWritten: 0,
      candidateSoulDeltas: 0,
    };
  }
}
