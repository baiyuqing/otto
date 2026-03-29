import { mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { DatabaseSync } from "node:sqlite";

import type {
  MemoryCandidate,
  MemoryCandidateQuery,
  MemoryEntry,
  MemoryEntryQuery,
  MemoryStore,
  WorkingMemoryState,
} from "./types.js";

type SqliteScalar = string | number | bigint | Uint8Array | null;

interface WorkingMemoryRow {
  key: string;
  scope: WorkingMemoryState["scope"];
  objective: string;
  plan_json: string;
  open_loops_json: string;
  blockers_json: string;
  active_artifacts_json: string;
  owner_agent_id: string | null;
  summary: string;
  updated_at: string;
}

interface MemoryEntryRow {
  id: string;
  kind: MemoryEntry["kind"];
  scope: MemoryEntry["scope"];
  title: string;
  content: string;
  confidence: number;
  source_refs_json: string;
  tags_json: string;
  created_at: string;
  updated_at: string;
  last_verified_at: string | null;
  supersedes: string | null;
}

interface MemoryCandidateRow {
  id: string;
  kind: MemoryCandidate["kind"];
  scope: MemoryCandidate["scope"];
  content: string;
  evidence_refs_json: string;
  proposed_by: MemoryCandidate["proposedBy"];
  created_at: string;
}

function ensureDatabaseDirectory(path: string): void {
  if (path === ":memory:") {
    return;
  }

  mkdirSync(dirname(resolve(path)), { recursive: true });
}

function serializeJson(value: unknown): string {
  return JSON.stringify(value);
}

function createPlaceholders(length: number): string {
  return Array.from({ length }, () => "?").join(", ");
}

function decodeWorkingMemory(row: WorkingMemoryRow): WorkingMemoryState {
  return {
    key: row.key,
    scope: row.scope,
    objective: row.objective,
    plan: JSON.parse(row.plan_json) as string[],
    openLoops: JSON.parse(row.open_loops_json) as string[],
    blockers: JSON.parse(row.blockers_json) as string[],
    activeArtifacts: JSON.parse(row.active_artifacts_json) as string[],
    ...(row.owner_agent_id ? { ownerAgentId: row.owner_agent_id } : {}),
    summary: row.summary,
    updatedAt: row.updated_at,
  };
}

function decodeEntry(row: MemoryEntryRow): MemoryEntry {
  return {
    id: row.id,
    kind: row.kind,
    scope: row.scope,
    title: row.title,
    content: row.content,
    confidence: row.confidence,
    sourceRefs: JSON.parse(row.source_refs_json) as string[],
    tags: JSON.parse(row.tags_json) as string[],
    createdAt: row.created_at,
    updatedAt: row.updated_at,
    ...(row.last_verified_at ? { lastVerifiedAt: row.last_verified_at } : {}),
    ...(row.supersedes ? { supersedes: row.supersedes } : {}),
  };
}

function decodeCandidate(row: MemoryCandidateRow): MemoryCandidate {
  return {
    id: row.id,
    kind: row.kind,
    scope: row.scope,
    content: row.content,
    evidenceRefs: JSON.parse(row.evidence_refs_json) as MemoryCandidate["evidenceRefs"],
    proposedBy: row.proposed_by,
    createdAt: row.created_at,
  };
}

export interface SqliteMemoryStoreOptions {
  initialize?: boolean;
}

export class SqliteMemoryStore implements MemoryStore {
  private readonly database: DatabaseSync;

  constructor(
    private readonly databasePath: string,
    options: SqliteMemoryStoreOptions = {},
  ) {
    ensureDatabaseDirectory(databasePath);
    this.database = new DatabaseSync(databasePath);
    this.database.exec("PRAGMA foreign_keys = ON;");

    if (options.initialize !== false) {
      this.initialize();
    }
  }

  close(): void {
    this.database.close();
  }

  initialize(): void {
    this.database.exec(`
      CREATE TABLE IF NOT EXISTS working_memory_states (
        key TEXT PRIMARY KEY,
        scope TEXT NOT NULL,
        objective TEXT NOT NULL,
        plan_json TEXT NOT NULL,
        open_loops_json TEXT NOT NULL,
        blockers_json TEXT NOT NULL,
        active_artifacts_json TEXT NOT NULL,
        owner_agent_id TEXT,
        summary TEXT NOT NULL,
        updated_at TEXT NOT NULL
      );

      CREATE TABLE IF NOT EXISTS memory_entries (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        scope TEXT NOT NULL,
        title TEXT NOT NULL,
        content TEXT NOT NULL,
        confidence REAL NOT NULL,
        source_refs_json TEXT NOT NULL,
        tags_json TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        last_verified_at TEXT,
        supersedes TEXT
      );

      CREATE TABLE IF NOT EXISTS memory_candidates (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        scope TEXT NOT NULL,
        content TEXT NOT NULL,
        evidence_refs_json TEXT NOT NULL,
        proposed_by TEXT NOT NULL,
        created_at TEXT NOT NULL
      );
    `);
  }

  async getWorkingMemory(key: string): Promise<WorkingMemoryState | null> {
    const row = this.database
      .prepare("SELECT * FROM working_memory_states WHERE key = ? LIMIT 1")
      .get(key) as WorkingMemoryRow | undefined;

    return row ? decodeWorkingMemory(row) : null;
  }

  async saveWorkingMemory(state: WorkingMemoryState): Promise<void> {
    this.database
      .prepare(`
        INSERT INTO working_memory_states (
          key,
          scope,
          objective,
          plan_json,
          open_loops_json,
          blockers_json,
          active_artifacts_json,
          owner_agent_id,
          summary,
          updated_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(key) DO UPDATE SET
          scope = excluded.scope,
          objective = excluded.objective,
          plan_json = excluded.plan_json,
          open_loops_json = excluded.open_loops_json,
          blockers_json = excluded.blockers_json,
          active_artifacts_json = excluded.active_artifacts_json,
          owner_agent_id = excluded.owner_agent_id,
          summary = excluded.summary,
          updated_at = excluded.updated_at
      `)
      .run(
        state.key,
        state.scope,
        state.objective,
        serializeJson(state.plan),
        serializeJson(state.openLoops),
        serializeJson(state.blockers),
        serializeJson(state.activeArtifacts),
        state.ownerAgentId ?? null,
        state.summary,
        state.updatedAt,
      );
  }

  async listEntries(query: MemoryEntryQuery = {}): Promise<MemoryEntry[]> {
    const conditions: string[] = [];
    const params: SqliteScalar[] = [];

    if (query.kinds && query.kinds.length > 0) {
      conditions.push(`kind IN (${createPlaceholders(query.kinds.length)})`);
      params.push(...query.kinds);
    }

    if (query.scopes && query.scopes.length > 0) {
      conditions.push(`scope IN (${createPlaceholders(query.scopes.length)})`);
      params.push(...query.scopes);
    }

    const whereClause = conditions.length > 0 ? `WHERE ${conditions.join(" AND ")}` : "";
    const limit = query.limit ?? 100;
    const rows = this.database
      .prepare(`
        SELECT *
        FROM memory_entries
        ${whereClause}
        ORDER BY updated_at DESC, id ASC
        LIMIT ?
      `)
      .all(...params, limit) as unknown as MemoryEntryRow[];

    const entries = rows.map(decodeEntry);

    if (!query.tags || query.tags.length === 0) {
      return entries;
    }

    return entries.filter((entry) => query.tags!.every((tag) => entry.tags.includes(tag)));
  }

  async saveEntry(entry: MemoryEntry): Promise<void> {
    this.database
      .prepare(`
        INSERT INTO memory_entries (
          id,
          kind,
          scope,
          title,
          content,
          confidence,
          source_refs_json,
          tags_json,
          created_at,
          updated_at,
          last_verified_at,
          supersedes
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(id) DO UPDATE SET
          kind = excluded.kind,
          scope = excluded.scope,
          title = excluded.title,
          content = excluded.content,
          confidence = excluded.confidence,
          source_refs_json = excluded.source_refs_json,
          tags_json = excluded.tags_json,
          created_at = excluded.created_at,
          updated_at = excluded.updated_at,
          last_verified_at = excluded.last_verified_at,
          supersedes = excluded.supersedes
      `)
      .run(
        entry.id,
        entry.kind,
        entry.scope,
        entry.title,
        entry.content,
        entry.confidence,
        serializeJson(entry.sourceRefs),
        serializeJson(entry.tags),
        entry.createdAt,
        entry.updatedAt,
        entry.lastVerifiedAt ?? null,
        entry.supersedes ?? null,
      );
  }

  async listCandidates(query: MemoryCandidateQuery = {}): Promise<MemoryCandidate[]> {
    const conditions: string[] = [];
    const params: SqliteScalar[] = [];

    if (query.kinds && query.kinds.length > 0) {
      conditions.push(`kind IN (${createPlaceholders(query.kinds.length)})`);
      params.push(...query.kinds);
    }

    if (query.scopes && query.scopes.length > 0) {
      conditions.push(`scope IN (${createPlaceholders(query.scopes.length)})`);
      params.push(...query.scopes);
    }

    const whereClause = conditions.length > 0 ? `WHERE ${conditions.join(" AND ")}` : "";
    const limit = query.limit ?? 100;
    const rows = this.database
      .prepare(`
        SELECT *
        FROM memory_candidates
        ${whereClause}
        ORDER BY created_at DESC, id ASC
        LIMIT ?
      `)
      .all(...params, limit) as unknown as MemoryCandidateRow[];

    return rows.map(decodeCandidate);
  }

  async appendCandidate(candidate: MemoryCandidate): Promise<void> {
    this.database
      .prepare(`
        INSERT OR REPLACE INTO memory_candidates (
          id,
          kind,
          scope,
          content,
          evidence_refs_json,
          proposed_by,
          created_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?)
      `)
      .run(
        candidate.id,
        candidate.kind,
        candidate.scope,
        candidate.content,
        serializeJson(candidate.evidenceRefs),
        candidate.proposedBy,
        candidate.createdAt,
      );
  }
}
