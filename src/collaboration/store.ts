import type {
  CollaborationConversation,
  CollaborationMessage,
  CollaborationParticipant,
  CollaborationTask,
  ConversationSpaceKind,
  ConversationVisibility,
} from "./types.js";

export interface CollaborationConversationQuery {
  spaceKinds?: ConversationSpaceKind[];
  visibility?: ConversationVisibility[];
  taskId?: string;
  limit?: number;
}

export interface CollaborationMessageQuery {
  conversationId: string;
  before?: string;
  after?: string;
  limit?: number;
}

export interface CollaborationTaskQuery {
  status?: CollaborationTask["status"][];
  ownerAgentId?: string;
  limit?: number;
}

export interface CollaborationStore {
  listConversations(query?: CollaborationConversationQuery): Promise<CollaborationConversation[]>;
  getConversation(conversationId: string): Promise<CollaborationConversation | null>;
  listMessages(query: CollaborationMessageQuery): Promise<CollaborationMessage[]>;
  listParticipants(participantIds: string[]): Promise<CollaborationParticipant[]>;
  listTasks(query?: CollaborationTaskQuery): Promise<CollaborationTask[]>;
  getTask(taskId: string): Promise<CollaborationTask | null>;
  listUnreadCounts?(conversationIds?: string[]): Promise<Record<string, number>>;
}
