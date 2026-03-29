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
      pinned: [],
      episodic: [],
      semantic: [],
      relationship: [],
    };
  }

  async writeTurn(_input: TurnWritebackInput): Promise<WritebackReport> {
    return {
      episodesWritten: 0,
      semanticsWritten: 0,
      relationshipsUpdated: 0,
      candidateSoulDeltas: 0,
    };
  }
}
