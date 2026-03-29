import type {
  CollaborationActivityEvent,
  CollaborationConversation,
  CollaborationMessage,
  CollaborationParticipant,
  CollaborationTask,
  ConversationSpaceKind,
  ConversationVisibility,
  ParticipantKind,
} from "./types.js";

export interface ConversationLinkSummary {
  conversationId: string;
  title: string;
  spaceKind: ConversationSpaceKind;
  visibility: ConversationVisibility;
}

export interface ConversationLane {
  participantId: string;
  label: string;
  kind: ParticipantKind;
}

export interface ConversationTimelineItem {
  id: string;
  laneParticipantId: string;
  kind: CollaborationMessage["kind"];
  body: string;
  createdAt: string;
  badge?: string;
}

export interface ConversationTaskSummary {
  id: string;
  title: string;
  status: CollaborationTask["status"];
  ownerAgentId: string;
  runtimeTarget: string;
}

export interface ActivityFeedItem {
  id: string;
  actorLabel: string;
  actorKind: ParticipantKind;
  kind: CollaborationActivityEvent["kind"];
  status: CollaborationActivityEvent["status"];
  title: string;
  detail?: string;
  visibility: CollaborationActivityEvent["visibility"];
  createdAt: string;
}

export interface ConversationDetailView {
  conversationId: string;
  title: string;
  subtitle: string;
  visibility: ConversationVisibility;
  lanes: ConversationLane[];
  timeline: ConversationTimelineItem[];
  activityFeed: ActivityFeedItem[];
  linkedConversations: ConversationLinkSummary[];
  activeTask?: ConversationTaskSummary;
}

export interface ConversationListItem {
  conversationId: string;
  title: string;
  subtitle: string;
  spaceKind: ConversationSpaceKind;
  visibility: ConversationVisibility;
  participantLabels: string[];
  latestMessageAt: string;
  latestPreview: string;
  unreadCount: number;
  taskStatus?: CollaborationTask["status"];
}

export interface ConversationListSection {
  kind: ConversationSpaceKind;
  title: string;
  items: ConversationListItem[];
}

export interface BuildConversationDetailInput {
  conversation: CollaborationConversation;
  participants: CollaborationParticipant[];
  messages: CollaborationMessage[];
  activities?: CollaborationActivityEvent[];
  task?: CollaborationTask;
  linkedConversations?: CollaborationConversation[];
}

export interface BuildConversationListInput {
  conversations: CollaborationConversation[];
  participants: CollaborationParticipant[];
  messages: CollaborationMessage[];
  tasks?: CollaborationTask[];
  unreadCounts?: Record<string, number>;
}

const participantOrder: Record<ParticipantKind, number> = {
  user: 0,
  agent: 1,
  system: 2,
};

const sectionTitles: Record<ConversationSpaceKind, string> = {
  dm: "Direct Messages",
  channel: "Channel Conversations",
  agent: "Agent Dialogues",
};

function conversationSubtitle(conversation: CollaborationConversation): string {
  if (conversation.ref.spaceKind === "dm") {
    return "Private DM";
  }

  if (conversation.ref.spaceKind === "agent") {
    return "Internal Agent Dialogue";
  }

  return conversation.ref.conversationKind === "thread" ? "Channel Thread" : "Channel";
}

function getParticipantLabel(participants: Map<string, CollaborationParticipant>, participantId: string): string {
  return participants.get(participantId)?.displayName ?? participantId;
}

function getMessageBadge(message: CollaborationMessage): string | undefined {
  if (message.kind === "handoff" && message.handoff) {
    return `handoff ${message.handoff.fromAgentId} -> ${message.handoff.toAgentId}`;
  }

  if (message.kind === "approval" && message.approval) {
    return `${message.approval.status} approval`;
  }

  if (message.kind === "status" && message.status) {
    return message.status.status;
  }

  return undefined;
}

function compareByTimestampAsc(left: { createdAt: string }, right: { createdAt: string }): number {
  return left.createdAt.localeCompare(right.createdAt);
}

function compareByTimestampDesc(left: { latestMessageAt: string }, right: { latestMessageAt: string }): number {
  return right.latestMessageAt.localeCompare(left.latestMessageAt);
}

export function buildConversationDetailView(input: BuildConversationDetailInput): ConversationDetailView {
  const participantMap = new Map(input.participants.map((participant) => [participant.id, participant]));
  const lanes = input.participants
    .filter((participant) => input.conversation.participantIds.includes(participant.id))
    .slice()
    .sort((left, right) => {
      const byKind = participantOrder[left.kind] - participantOrder[right.kind];
      if (byKind !== 0) {
        return byKind;
      }

      return left.displayName.localeCompare(right.displayName);
    })
    .map((participant) => ({
      participantId: participant.id,
      label: participant.displayName,
      kind: participant.kind,
    }));

  const timeline = input.messages
    .slice()
    .sort(compareByTimestampAsc)
    .map((message) => {
      const item: ConversationTimelineItem = {
        id: message.id,
        laneParticipantId: message.sender.id,
        kind: message.kind,
        body: message.body,
        createdAt: message.createdAt,
      };
      const badge = getMessageBadge(message);
      if (badge) {
        item.badge = badge;
      }

      return item;
    });

  const activityFeed = (input.activities ?? [])
    .slice()
    .sort((left, right) => right.createdAt.localeCompare(left.createdAt))
    .map((activity) => {
      const item: ActivityFeedItem = {
        id: activity.id,
        actorLabel: getParticipantLabel(participantMap, activity.actor.id),
        actorKind: activity.actor.kind,
        kind: activity.kind,
        status: activity.status,
        title: activity.title,
        visibility: activity.visibility,
        createdAt: activity.createdAt,
      };
      if (activity.detail) {
        item.detail = activity.detail;
      }

      return item;
    });

  const linkedConversations = (input.linkedConversations ?? []).map((conversation) => ({
    conversationId: conversation.id,
    title: conversation.title,
    spaceKind: conversation.ref.spaceKind,
    visibility: conversation.visibility,
  }));

  const view: ConversationDetailView = {
    conversationId: input.conversation.id,
    title: input.conversation.title,
    subtitle: conversationSubtitle(input.conversation),
    visibility: input.conversation.visibility,
    lanes,
    timeline,
    activityFeed,
    linkedConversations,
  };
  if (input.task) {
    view.activeTask = {
      id: input.task.id,
      title: input.task.title,
      status: input.task.status,
      ownerAgentId: getParticipantLabel(participantMap, input.task.ownerAgentId),
      runtimeTarget: input.task.runtimeTarget,
    };
  }

  return view;
}

export function buildConversationListSections(input: BuildConversationListInput): ConversationListSection[] {
  const participantMap = new Map(input.participants.map((participant) => [participant.id, participant]));
  const taskByConversationId = new Map(
    (input.tasks ?? []).map((task) => [task.primaryConversationId, task]),
  );
  const messagesByConversationId = new Map<string, CollaborationMessage[]>();

  for (const message of input.messages) {
    const bucket = messagesByConversationId.get(message.conversationId) ?? [];
    bucket.push(message);
    messagesByConversationId.set(message.conversationId, bucket);
  }

  const itemsBySection = new Map<ConversationSpaceKind, ConversationListItem[]>();

  for (const conversation of input.conversations) {
    const messages = (messagesByConversationId.get(conversation.id) ?? []).slice().sort(compareByTimestampAsc);
    const latestMessage = messages[messages.length - 1];
    const participantLabels = conversation.participantIds.map((participantId) =>
      getParticipantLabel(participantMap, participantId),
    );
    const task = taskByConversationId.get(conversation.id);
    const item: ConversationListItem = {
      conversationId: conversation.id,
      title: conversation.title,
      subtitle: conversationSubtitle(conversation),
      spaceKind: conversation.ref.spaceKind,
      visibility: conversation.visibility,
      participantLabels,
      latestMessageAt: latestMessage?.createdAt ?? conversation.updatedAt,
      latestPreview: latestMessage?.body ?? "",
      unreadCount: input.unreadCounts?.[conversation.id] ?? 0,
    };
    if (task) {
      item.taskStatus = task.status;
    }
    const sectionItems = itemsBySection.get(conversation.ref.spaceKind) ?? [];
    sectionItems.push(item);
    itemsBySection.set(conversation.ref.spaceKind, sectionItems);
  }

  return (["dm", "channel", "agent"] as const).map((kind) => ({
    kind,
    title: sectionTitles[kind],
    items: (itemsBySection.get(kind) ?? []).sort(compareByTimestampDesc),
  }));
}
