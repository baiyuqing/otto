import { mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { DatabaseSync } from "node:sqlite";

import type { CollaborationSnapshot } from "./demo-data.js";
import type {
  CollaborationActivityEvent,
  CollaborationConversation,
  CollaborationMessage,
  CollaborationParticipant,
  CollaborationTask,
  MessageApprovalPayload,
  MessageStatusPayload,
  AgentHandoffPayload,
} from "./types.js";
import type {
  CollaborationActivityQuery,
  CollaborationConversationQuery,
  CollaborationMessageQuery,
  CollaborationStore,
  CollaborationTaskQuery,
} from "./store.js";

type SqliteScalar = string | number | bigint | Uint8Array | null;

interface ParticipantRow {
  id: string;
  kind: CollaborationParticipant["kind"];
  display_name: string;
  runtime_target: string | null;
}

interface ConversationRow {
  id: string;
  provider: CollaborationConversation["ref"]["provider"];
  space_kind: CollaborationConversation["ref"]["spaceKind"];
  conversation_kind: CollaborationConversation["ref"]["conversationKind"];
  external_id: string;
  parent_external_id: string | null;
  visibility: CollaborationConversation["visibility"];
  title: string;
  participant_ids_json: string;
  project_id: string | null;
  task_id: string | null;
  created_at: string;
  updated_at: string;
}

interface TaskRow {
  id: string;
  title: string;
  status: CollaborationTask["status"];
  owner_agent_id: string;
  runtime_target: string;
  primary_conversation_id: string;
  internal_conversation_ids_json: string;
  created_at: string;
  updated_at: string;
}

interface MessageRow {
  id: string;
  conversation_id: string;
  task_id: string | null;
  sender_id: string;
  sender_kind: CollaborationMessage["sender"]["kind"];
  kind: CollaborationMessage["kind"];
  body: string;
  created_at: string;
  reply_to_message_id: string | null;
  visible_to_user: number;
  handoff_json: string | null;
  status_json: string | null;
  approval_json: string | null;
}

interface ActivityRow {
  id: string;
  task_id: string | null;
  conversation_id: string | null;
  actor_id: string;
  actor_kind: CollaborationActivityEvent["actor"]["kind"];
  kind: CollaborationActivityEvent["kind"];
  visibility: CollaborationActivityEvent["visibility"];
  status: CollaborationActivityEvent["status"];
  title: string;
  detail: string | null;
  payload_json: string | null;
  created_at: string;
  ended_at: string | null;
  parent_event_id: string | null;
}

function ensureDatabaseDirectory(path: string): void {
  if (path === ":memory:") {
    return;
  }

  mkdirSync(dirname(resolve(path)), { recursive: true });
}

function parseJsonColumn<T>(value: string | null): T | undefined {
  if (!value) {
    return undefined;
  }

  return JSON.parse(value) as T;
}

function serializeJson(value: unknown): string | null {
  if (value === undefined) {
    return null;
  }

  return JSON.stringify(value);
}

function createPlaceholders(length: number): string {
  return Array.from({ length }, () => "?").join(", ");
}

function decodeConversation(row: ConversationRow): CollaborationConversation {
  return {
    id: row.id,
    ref: {
      provider: row.provider,
      spaceKind: row.space_kind,
      conversationKind: row.conversation_kind,
      externalId: row.external_id,
      ...(row.parent_external_id ? { parentExternalId: row.parent_external_id } : {}),
    },
    visibility: row.visibility,
    title: row.title,
    participantIds: JSON.parse(row.participant_ids_json) as string[],
    ...(row.project_id ? { projectId: row.project_id } : {}),
    ...(row.task_id ? { taskId: row.task_id } : {}),
    createdAt: row.created_at,
    updatedAt: row.updated_at,
  };
}

function decodeParticipant(row: ParticipantRow): CollaborationParticipant {
  return {
    id: row.id,
    kind: row.kind,
    displayName: row.display_name,
    ...(row.runtime_target ? { runtimeTarget: row.runtime_target } : {}),
  };
}

function decodeTask(row: TaskRow): CollaborationTask {
  return {
    id: row.id,
    title: row.title,
    status: row.status,
    ownerAgentId: row.owner_agent_id,
    runtimeTarget: row.runtime_target,
    primaryConversationId: row.primary_conversation_id,
    internalConversationIds: JSON.parse(row.internal_conversation_ids_json) as string[],
    createdAt: row.created_at,
    updatedAt: row.updated_at,
  };
}

function decodeMessage(row: MessageRow): CollaborationMessage {
  return {
    id: row.id,
    conversationId: row.conversation_id,
    ...(row.task_id ? { taskId: row.task_id } : {}),
    sender: {
      id: row.sender_id,
      kind: row.sender_kind,
    },
    kind: row.kind,
    body: row.body,
    createdAt: row.created_at,
    ...(row.reply_to_message_id ? { replyToMessageId: row.reply_to_message_id } : {}),
    visibleToUser: row.visible_to_user === 1,
    ...(parseJsonColumn<AgentHandoffPayload>(row.handoff_json)
      ? { handoff: parseJsonColumn<AgentHandoffPayload>(row.handoff_json)! }
      : {}),
    ...(parseJsonColumn<MessageStatusPayload>(row.status_json)
      ? { status: parseJsonColumn<MessageStatusPayload>(row.status_json)! }
      : {}),
    ...(parseJsonColumn<MessageApprovalPayload>(row.approval_json)
      ? { approval: parseJsonColumn<MessageApprovalPayload>(row.approval_json)! }
      : {}),
  };
}

function decodeActivity(row: ActivityRow): CollaborationActivityEvent {
  return {
    id: row.id,
    ...(row.task_id ? { taskId: row.task_id } : {}),
    ...(row.conversation_id ? { conversationId: row.conversation_id } : {}),
    actor: {
      id: row.actor_id,
      kind: row.actor_kind,
    },
    kind: row.kind,
    visibility: row.visibility,
    status: row.status,
    title: row.title,
    ...(row.detail ? { detail: row.detail } : {}),
    ...(parseJsonColumn<Record<string, boolean | number | string | null>>(row.payload_json)
      ? { payload: parseJsonColumn<Record<string, boolean | number | string | null>>(row.payload_json)! }
      : {}),
    createdAt: row.created_at,
    ...(row.ended_at ? { endedAt: row.ended_at } : {}),
    ...(row.parent_event_id ? { parentEventId: row.parent_event_id } : {}),
  };
}

export interface SqliteCollaborationStoreOptions {
  initialize?: boolean;
}

export class SqliteCollaborationStore implements CollaborationStore {
  private readonly database: DatabaseSync;

  constructor(
    private readonly databasePath: string,
    options: SqliteCollaborationStoreOptions = {},
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
      CREATE TABLE IF NOT EXISTS collaboration_participants (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        display_name TEXT NOT NULL,
        runtime_target TEXT
      );

      CREATE TABLE IF NOT EXISTS collaboration_conversations (
        id TEXT PRIMARY KEY,
        provider TEXT NOT NULL,
        space_kind TEXT NOT NULL,
        conversation_kind TEXT NOT NULL,
        external_id TEXT NOT NULL,
        parent_external_id TEXT,
        visibility TEXT NOT NULL,
        title TEXT NOT NULL,
        participant_ids_json TEXT NOT NULL,
        project_id TEXT,
        task_id TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
      );

      CREATE TABLE IF NOT EXISTS collaboration_tasks (
        id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        status TEXT NOT NULL,
        owner_agent_id TEXT NOT NULL,
        runtime_target TEXT NOT NULL,
        primary_conversation_id TEXT NOT NULL,
        internal_conversation_ids_json TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
      );

      CREATE TABLE IF NOT EXISTS collaboration_messages (
        id TEXT PRIMARY KEY,
        conversation_id TEXT NOT NULL,
        task_id TEXT,
        sender_id TEXT NOT NULL,
        sender_kind TEXT NOT NULL,
        kind TEXT NOT NULL,
        body TEXT NOT NULL,
        created_at TEXT NOT NULL,
        reply_to_message_id TEXT,
        visible_to_user INTEGER NOT NULL,
        handoff_json TEXT,
        status_json TEXT,
        approval_json TEXT
      );

      CREATE TABLE IF NOT EXISTS collaboration_activity_events (
        id TEXT PRIMARY KEY,
        task_id TEXT,
        conversation_id TEXT,
        actor_id TEXT NOT NULL,
        actor_kind TEXT NOT NULL,
        kind TEXT NOT NULL,
        visibility TEXT NOT NULL,
        status TEXT NOT NULL,
        title TEXT NOT NULL,
        detail TEXT,
        payload_json TEXT,
        created_at TEXT NOT NULL,
        ended_at TEXT,
        parent_event_id TEXT
      );

      CREATE TABLE IF NOT EXISTS collaboration_unread_counts (
        conversation_id TEXT PRIMARY KEY,
        unread_count INTEGER NOT NULL
      );
    `);
  }

  async listConversations(query: CollaborationConversationQuery = {}): Promise<CollaborationConversation[]> {
    const conditions: string[] = [];
    const params: SqliteScalar[] = [];

    if (query.spaceKinds && query.spaceKinds.length > 0) {
      conditions.push(`space_kind IN (${createPlaceholders(query.spaceKinds.length)})`);
      params.push(...query.spaceKinds);
    }

    if (query.visibility && query.visibility.length > 0) {
      conditions.push(`visibility IN (${createPlaceholders(query.visibility.length)})`);
      params.push(...query.visibility);
    }

    if (query.taskId) {
      conditions.push("task_id = ?");
      params.push(query.taskId);
    }

    const whereClause = conditions.length > 0 ? `WHERE ${conditions.join(" AND ")}` : "";
    const sql = `
      SELECT *
      FROM collaboration_conversations
      ${whereClause}
      ORDER BY updated_at DESC
      LIMIT ?
    `;
    params.push(query.limit ?? 100);

    const rows = this.database.prepare(sql).all(...params) as unknown as ConversationRow[];
    return rows.map(decodeConversation);
  }

  async getConversation(conversationId: string): Promise<CollaborationConversation | null> {
    const row = this.database
      .prepare("SELECT * FROM collaboration_conversations WHERE id = ?")
      .get(conversationId) as unknown as ConversationRow | undefined;

    return row ? decodeConversation(row) : null;
  }

  async listMessages(query: CollaborationMessageQuery): Promise<CollaborationMessage[]> {
    const conditions = ["conversation_id = ?"];
    const params: SqliteScalar[] = [query.conversationId];

    if (query.before) {
      conditions.push("created_at < ?");
      params.push(query.before);
    }

    if (query.after) {
      conditions.push("created_at > ?");
      params.push(query.after);
    }

    const sql = `
      SELECT *
      FROM collaboration_messages
      WHERE ${conditions.join(" AND ")}
      ORDER BY created_at ASC
      LIMIT ?
    `;
    params.push(query.limit ?? 200);

    const rows = this.database.prepare(sql).all(...params) as unknown as MessageRow[];
    return rows.map(decodeMessage);
  }

  async listActivityEvents(query: CollaborationActivityQuery = {}): Promise<CollaborationActivityEvent[]> {
    const conditions: string[] = [];
    const params: SqliteScalar[] = [];

    if (query.taskId) {
      conditions.push("task_id = ?");
      params.push(query.taskId);
    }

    if (query.conversationId) {
      conditions.push("conversation_id = ?");
      params.push(query.conversationId);
    }

    if (query.visibility && query.visibility.length > 0) {
      conditions.push(`visibility IN (${createPlaceholders(query.visibility.length)})`);
      params.push(...query.visibility);
    }

    if (query.kinds && query.kinds.length > 0) {
      conditions.push(`kind IN (${createPlaceholders(query.kinds.length)})`);
      params.push(...query.kinds);
    }

    const whereClause = conditions.length > 0 ? `WHERE ${conditions.join(" AND ")}` : "";
    const sql = `
      SELECT *
      FROM collaboration_activity_events
      ${whereClause}
      ORDER BY created_at DESC
      LIMIT ?
    `;
    params.push(query.limit ?? 100);

    const rows = this.database.prepare(sql).all(...params) as unknown as ActivityRow[];
    return rows.map(decodeActivity);
  }

  async listParticipants(participantIds: string[]): Promise<CollaborationParticipant[]> {
    if (participantIds.length === 0) {
      return [];
    }

    const rows = this.database
      .prepare(`
        SELECT *
        FROM collaboration_participants
        WHERE id IN (${createPlaceholders(participantIds.length)})
      `)
      .all(...participantIds) as unknown as ParticipantRow[];
    const byId = new Map(rows.map((row) => [row.id, decodeParticipant(row)]));

    return participantIds
      .map((participantId) => byId.get(participantId))
      .filter((participant): participant is CollaborationParticipant => participant !== undefined);
  }

  async listTasks(query: CollaborationTaskQuery = {}): Promise<CollaborationTask[]> {
    const conditions: string[] = [];
    const params: SqliteScalar[] = [];

    if (query.status && query.status.length > 0) {
      conditions.push(`status IN (${createPlaceholders(query.status.length)})`);
      params.push(...query.status);
    }

    if (query.ownerAgentId) {
      conditions.push("owner_agent_id = ?");
      params.push(query.ownerAgentId);
    }

    const whereClause = conditions.length > 0 ? `WHERE ${conditions.join(" AND ")}` : "";
    const sql = `
      SELECT *
      FROM collaboration_tasks
      ${whereClause}
      ORDER BY updated_at DESC
      LIMIT ?
    `;
    params.push(query.limit ?? 100);

    const rows = this.database.prepare(sql).all(...params) as unknown as TaskRow[];
    return rows.map(decodeTask);
  }

  async getTask(taskId: string): Promise<CollaborationTask | null> {
    const row = this.database
      .prepare("SELECT * FROM collaboration_tasks WHERE id = ?")
      .get(taskId) as unknown as TaskRow | undefined;

    return row ? decodeTask(row) : null;
  }

  async listUnreadCounts(conversationIds: string[] = []): Promise<Record<string, number>> {
    const rows = conversationIds.length > 0
      ? (this.database
          .prepare(`
            SELECT conversation_id, unread_count
            FROM collaboration_unread_counts
            WHERE conversation_id IN (${createPlaceholders(conversationIds.length)})
          `)
          .all(...conversationIds) as Array<{ conversation_id: string; unread_count: number }>)
      : (this.database
          .prepare("SELECT conversation_id, unread_count FROM collaboration_unread_counts")
          .all() as Array<{ conversation_id: string; unread_count: number }>);

    return Object.fromEntries(rows.map((row) => [row.conversation_id, row.unread_count]));
  }

  async readSnapshot(): Promise<CollaborationSnapshot> {
    const activities = (this.database
      .prepare("SELECT * FROM collaboration_activity_events ORDER BY created_at DESC")
      .all() as unknown as ActivityRow[]).map(decodeActivity);
    const participants = (this.database
      .prepare("SELECT * FROM collaboration_participants ORDER BY id ASC")
      .all() as unknown as ParticipantRow[]).map(decodeParticipant);
    const conversations = (this.database
      .prepare("SELECT * FROM collaboration_conversations ORDER BY updated_at DESC")
      .all() as unknown as ConversationRow[]).map(decodeConversation);
    const tasks = (this.database
      .prepare("SELECT * FROM collaboration_tasks ORDER BY updated_at DESC")
      .all() as unknown as TaskRow[]).map(decodeTask);
    const messages = (this.database
      .prepare("SELECT * FROM collaboration_messages ORDER BY created_at ASC")
      .all() as unknown as MessageRow[]).map(decodeMessage);
    const unreadCounts = await this.listUnreadCounts();

    return {
      activities,
      participants,
      conversations,
      tasks,
      messages,
      unreadCounts,
    };
  }

  async replaceSnapshot(snapshot: CollaborationSnapshot): Promise<void> {
    const insertParticipant = this.database.prepare(`
      INSERT INTO collaboration_participants (id, kind, display_name, runtime_target)
      VALUES (?, ?, ?, ?)
    `);
    const insertConversation = this.database.prepare(`
      INSERT INTO collaboration_conversations (
        id,
        provider,
        space_kind,
        conversation_kind,
        external_id,
        parent_external_id,
        visibility,
        title,
        participant_ids_json,
        project_id,
        task_id,
        created_at,
        updated_at
      ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
    `);
    const insertTask = this.database.prepare(`
      INSERT INTO collaboration_tasks (
        id,
        title,
        status,
        owner_agent_id,
        runtime_target,
        primary_conversation_id,
        internal_conversation_ids_json,
        created_at,
        updated_at
      ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
    `);
    const insertMessage = this.database.prepare(`
      INSERT INTO collaboration_messages (
        id,
        conversation_id,
        task_id,
        sender_id,
        sender_kind,
        kind,
        body,
        created_at,
        reply_to_message_id,
        visible_to_user,
        handoff_json,
        status_json,
        approval_json
      ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
    `);
    const insertActivity = this.database.prepare(`
      INSERT INTO collaboration_activity_events (
        id,
        task_id,
        conversation_id,
        actor_id,
        actor_kind,
        kind,
        visibility,
        status,
        title,
        detail,
        payload_json,
        created_at,
        ended_at,
        parent_event_id
      ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
    `);
    const insertUnreadCount = this.database.prepare(`
      INSERT INTO collaboration_unread_counts (conversation_id, unread_count)
      VALUES (?, ?)
    `);

    this.database.exec("BEGIN");

    try {
      this.database.exec(`
        DELETE FROM collaboration_activity_events;
        DELETE FROM collaboration_messages;
        DELETE FROM collaboration_unread_counts;
        DELETE FROM collaboration_tasks;
        DELETE FROM collaboration_conversations;
        DELETE FROM collaboration_participants;
      `);

      for (const participant of snapshot.participants) {
        insertParticipant.run(
          participant.id,
          participant.kind,
          participant.displayName,
          participant.runtimeTarget ?? null,
        );
      }

      for (const conversation of snapshot.conversations) {
        insertConversation.run(
          conversation.id,
          conversation.ref.provider,
          conversation.ref.spaceKind,
          conversation.ref.conversationKind,
          conversation.ref.externalId,
          conversation.ref.parentExternalId ?? null,
          conversation.visibility,
          conversation.title,
          JSON.stringify(conversation.participantIds),
          conversation.projectId ?? null,
          conversation.taskId ?? null,
          conversation.createdAt,
          conversation.updatedAt,
        );
      }

      for (const task of snapshot.tasks) {
        insertTask.run(
          task.id,
          task.title,
          task.status,
          task.ownerAgentId,
          task.runtimeTarget,
          task.primaryConversationId,
          JSON.stringify(task.internalConversationIds),
          task.createdAt,
          task.updatedAt,
        );
      }

      for (const message of snapshot.messages) {
        insertMessage.run(
          message.id,
          message.conversationId,
          message.taskId ?? null,
          message.sender.id,
          message.sender.kind,
          message.kind,
          message.body,
          message.createdAt,
          message.replyToMessageId ?? null,
          message.visibleToUser ? 1 : 0,
          serializeJson(message.handoff),
          serializeJson(message.status),
          serializeJson(message.approval),
        );
      }

      for (const activity of snapshot.activities) {
        insertActivity.run(
          activity.id,
          activity.taskId ?? null,
          activity.conversationId ?? null,
          activity.actor.id,
          activity.actor.kind,
          activity.kind,
          activity.visibility,
          activity.status,
          activity.title,
          activity.detail ?? null,
          serializeJson(activity.payload),
          activity.createdAt,
          activity.endedAt ?? null,
          activity.parentEventId ?? null,
        );
      }

      for (const [conversationId, unreadCount] of Object.entries(snapshot.unreadCounts)) {
        insertUnreadCount.run(conversationId, unreadCount);
      }

      this.database.exec("COMMIT");
    } catch (error) {
      this.database.exec("ROLLBACK");
      throw error;
    }
  }
}
