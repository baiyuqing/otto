import { randomUUID } from "node:crypto";

import type { RuntimeSessionRef, RuntimeTarget } from "../runtime/types.js";

export interface LogicalSession {
  agentId: string;
  logicalSessionId: string;
  runtimeSession?: RuntimeSessionRef;
  startedAt: string;
  lastTurnAt: string;
  summary?: string;
}

export interface LoadOrCreateSessionInput {
  agentId: string;
  runtimeTarget: RuntimeTarget;
  workspacePath: string;
  sessionHint?: string;
}

export interface SessionManager {
  loadOrCreate(input: LoadOrCreateSessionInput): Promise<LogicalSession>;
  attachRuntimeSession(
    session: LogicalSession,
    runtimeSession: RuntimeSessionRef,
  ): Promise<LogicalSession>;
}

export class InMemorySessionManager implements SessionManager {
  private readonly sessions = new Map<string, LogicalSession>();

  async loadOrCreate(input: LoadOrCreateSessionInput): Promise<LogicalSession> {
    const now = new Date().toISOString();
    const logicalSessionId = input.sessionHint ?? randomUUID();
    const existing = this.sessions.get(logicalSessionId);

    if (existing) {
      const updated: LogicalSession = {
        ...existing,
        lastTurnAt: now,
      };

      this.sessions.set(logicalSessionId, updated);
      return updated;
    }

    const created: LogicalSession = {
      agentId: input.agentId,
      logicalSessionId,
      startedAt: now,
      lastTurnAt: now,
    };

    this.sessions.set(logicalSessionId, created);
    return created;
  }

  async attachRuntimeSession(
    session: LogicalSession,
    runtimeSession: RuntimeSessionRef,
  ): Promise<LogicalSession> {
    const updated: LogicalSession = {
      ...session,
      runtimeSession,
      lastTurnAt: new Date().toISOString(),
    };

    this.sessions.set(updated.logicalSessionId, updated);
    return updated;
  }
}
