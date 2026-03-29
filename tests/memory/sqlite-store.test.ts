import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, describe, expect, it } from "vitest";

import { SqliteMemoryStore } from "../../src/memory/sqlite-store.js";

const tempDirs: string[] = [];

afterEach(async () => {
  await Promise.all(tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

describe("SqliteMemoryStore", () => {
  it("stores working memory, entries, and candidates", async () => {
    const dir = await mkdtemp(join(tmpdir(), "otto-memory-"));
    tempDirs.push(dir);

    const databasePath = join(dir, "memory.sqlite");
    const store = new SqliteMemoryStore(databasePath);

    try {
      await store.saveWorkingMemory({
        key: "task:task-42",
        scope: "task",
        objective: "Ship the memory foundation",
        plan: ["define types", "create sqlite store"],
        openLoops: ["wire retrieval into the kernel"],
        blockers: [],
        activeArtifacts: ["docs/memory-architecture.md"],
        ownerAgentId: "agent-manager",
        summary: "Memory foundation is in progress.",
        updatedAt: "2026-03-29T09:00:00.000Z",
      });

      await store.saveEntry({
        id: "fact-1",
        kind: "factual",
        scope: "project-shared",
        title: "Memory vocabulary",
        content: "Use working, factual, and experiential memory.",
        confidence: 0.98,
        sourceRefs: ["docs/memory-architecture.md"],
        tags: ["memory", "architecture"],
        createdAt: "2026-03-29T09:01:00.000Z",
        updatedAt: "2026-03-29T09:01:00.000Z",
      });

      await store.saveEntry({
        id: "exp-1",
        kind: "experiential",
        scope: "agent-private",
        title: "Task-first memory design",
        content: "Start with strong working memory before deep retrieval.",
        confidence: 0.88,
        sourceRefs: ["task-42"],
        tags: ["memory", "tactics"],
        createdAt: "2026-03-29T09:02:00.000Z",
        updatedAt: "2026-03-29T09:02:00.000Z",
      });

      await store.appendCandidate({
        id: "candidate-1",
        kind: "fact",
        scope: "project-shared",
        content: "The user considers memory critical for agent quality.",
        evidenceRefs: [
          {
            kind: "message",
            id: "msg-1",
          },
          {
            kind: "doc",
            id: "docs/memory-architecture.md",
          },
        ],
        proposedBy: "agent",
        createdAt: "2026-03-29T09:03:00.000Z",
      });

      const working = await store.getWorkingMemory("task:task-42");
      const factual = await store.listEntries({
        kinds: ["factual"],
        scopes: ["project-shared"],
      });
      const experiential = await store.listEntries({
        kinds: ["experiential"],
      });
      const candidates = await store.listCandidates({
        kinds: ["fact"],
      });

      expect(working?.plan).toEqual(["define types", "create sqlite store"]);
      expect(working?.ownerAgentId).toBe("agent-manager");
      expect(factual[0]?.title).toBe("Memory vocabulary");
      expect(factual[0]?.sourceRefs).toEqual(["docs/memory-architecture.md"]);
      expect(experiential[0]?.scope).toBe("agent-private");
      expect(candidates[0]?.evidenceRefs[0]?.kind).toBe("message");
    } finally {
      store.close();
    }
  });
});
