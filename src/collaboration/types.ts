export type CollaborationProvider = "database" | "slack";
export type ConversationSpaceKind = "dm" | "channel" | "agent";
export type ConversationKind = "root" | "thread";
export type ConversationVisibility = "private" | "shared" | "internal";
export type ParticipantKind = "user" | "agent" | "system";
export type CollaborationMessageKind = "message" | "status" | "approval" | "result" | "handoff";
export type CollaborationTaskStatus =
  | "queued"
  | "running"
  | "blocked"
  | "waiting-approval"
  | "completed"
  | "failed"
  | "cancelled";
export type ApprovalStatus = "pending" | "approved" | "rejected";

export interface ConversationRef {
  provider: CollaborationProvider;
  spaceKind: ConversationSpaceKind;
  conversationKind: ConversationKind;
  externalId: string;
  parentExternalId?: string;
}

export interface CollaborationParticipant {
  id: string;
  kind: ParticipantKind;
  displayName: string;
  runtimeTarget?: string;
}

export interface CollaborationConversation {
  id: string;
  ref: ConversationRef;
  visibility: ConversationVisibility;
  title: string;
  participantIds: string[];
  projectId?: string;
  taskId?: string;
  createdAt: string;
  updatedAt: string;
}

export interface MessageSenderRef {
  id: string;
  kind: ParticipantKind;
}

export interface AgentHandoffPayload {
  fromAgentId: string;
  toAgentId: string;
  reason: string;
}

export interface MessageStatusPayload {
  status: CollaborationTaskStatus;
  label: string;
  detail?: string;
}

export interface MessageApprovalPayload {
  requestId: string;
  label: string;
  status: ApprovalStatus;
}

export interface CollaborationMessage {
  id: string;
  conversationId: string;
  taskId?: string;
  sender: MessageSenderRef;
  kind: CollaborationMessageKind;
  body: string;
  createdAt: string;
  replyToMessageId?: string;
  visibleToUser: boolean;
  handoff?: AgentHandoffPayload;
  status?: MessageStatusPayload;
  approval?: MessageApprovalPayload;
}

export interface CollaborationTask {
  id: string;
  title: string;
  status: CollaborationTaskStatus;
  ownerAgentId: string;
  runtimeTarget: string;
  primaryConversationId: string;
  internalConversationIds: string[];
  createdAt: string;
  updatedAt: string;
}
