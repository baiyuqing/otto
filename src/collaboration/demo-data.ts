import type {
  CollaborationConversation,
  CollaborationMessage,
  CollaborationParticipant,
  CollaborationTask,
} from "./types.js";

export interface CollaborationSnapshot {
  participants: CollaborationParticipant[];
  conversations: CollaborationConversation[];
  messages: CollaborationMessage[];
  tasks: CollaborationTask[];
  unreadCounts: Record<string, number>;
}

export function createDemoCollaborationSnapshot(): CollaborationSnapshot {
  const participants: CollaborationParticipant[] = [
    { id: "user-yuqing", kind: "user", displayName: "Yuqing" },
    { id: "agent-otto", kind: "agent", displayName: "Otto", runtimeTarget: "codex" },
    { id: "agent-planner", kind: "agent", displayName: "Planner", runtimeTarget: "claude-code" },
    { id: "system", kind: "system", displayName: "System" },
  ];

  const conversations: CollaborationConversation[] = [
    {
      id: "dm-yuqing",
      ref: {
        provider: "database",
        spaceKind: "dm",
        conversationKind: "root",
        externalId: "dm-yuqing",
      },
      visibility: "private",
      title: "Yuqing + Otto",
      participantIds: ["user-yuqing", "agent-otto"],
      createdAt: "2026-03-29T05:01:00.000Z",
      updatedAt: "2026-03-29T05:08:00.000Z",
    },
    {
      id: "channel-design-thread",
      ref: {
        provider: "database",
        spaceKind: "channel",
        conversationKind: "thread",
        externalId: "channel-design-thread",
        parentExternalId: "channel-design",
      },
      visibility: "shared",
      title: "# design-thread",
      participantIds: ["user-yuqing", "agent-otto", "system"],
      taskId: "task-42",
      projectId: "project-otto",
      createdAt: "2026-03-29T05:10:00.000Z",
      updatedAt: "2026-03-29T05:22:00.000Z",
    },
    {
      id: "channel-roadmap",
      ref: {
        provider: "database",
        spaceKind: "channel",
        conversationKind: "thread",
        externalId: "channel-roadmap",
        parentExternalId: "channel-roadmap-root",
      },
      visibility: "shared",
      title: "# roadmap",
      participantIds: ["user-yuqing", "agent-otto", "system"],
      taskId: "task-17",
      projectId: "project-otto",
      createdAt: "2026-03-29T04:20:00.000Z",
      updatedAt: "2026-03-29T04:44:00.000Z",
    },
    {
      id: "agent-task-42",
      ref: {
        provider: "database",
        spaceKind: "agent",
        conversationKind: "thread",
        externalId: "agent-task-42",
      },
      visibility: "internal",
      title: "task-42 internal",
      participantIds: ["agent-otto", "agent-planner", "system"],
      taskId: "task-42",
      projectId: "project-otto",
      createdAt: "2026-03-29T05:11:00.000Z",
      updatedAt: "2026-03-29T05:21:00.000Z",
    },
  ];

  const tasks: CollaborationTask[] = [
    {
      id: "task-42",
      title: "Design collaboration UI",
      status: "running",
      ownerAgentId: "agent-otto",
      runtimeTarget: "codex",
      primaryConversationId: "channel-design-thread",
      internalConversationIds: ["agent-task-42"],
      createdAt: "2026-03-29T05:10:00.000Z",
      updatedAt: "2026-03-29T05:22:00.000Z",
    },
    {
      id: "task-17",
      title: "Publish roadmap summary",
      status: "waiting-approval",
      ownerAgentId: "agent-otto",
      runtimeTarget: "claude-code",
      primaryConversationId: "channel-roadmap",
      internalConversationIds: [],
      createdAt: "2026-03-29T04:20:00.000Z",
      updatedAt: "2026-03-29T04:44:00.000Z",
    },
  ];

  const messages: CollaborationMessage[] = [
    {
      id: "dm-1",
      conversationId: "dm-yuqing",
      sender: { id: "user-yuqing", kind: "user" },
      kind: "message",
      body: "Can you review this collaboration idea before we put it into the team channel?",
      createdAt: "2026-03-29T05:01:00.000Z",
      visibleToUser: true,
    },
    {
      id: "dm-2",
      conversationId: "dm-yuqing",
      sender: { id: "agent-otto", kind: "agent" },
      kind: "result",
      body: "Yes. I would keep DM for private planning, channel threads for public execution, and internal agent dialogue separate.",
      createdAt: "2026-03-29T05:08:00.000Z",
      visibleToUser: true,
    },
    {
      id: "thread-1",
      conversationId: "channel-design-thread",
      taskId: "task-42",
      sender: { id: "user-yuqing", kind: "user" },
      kind: "message",
      body: "Can you simplify the collaboration UI so it feels closer to Slack?",
      createdAt: "2026-03-29T05:10:00.000Z",
      visibleToUser: true,
    },
    {
      id: "thread-2",
      conversationId: "channel-design-thread",
      taskId: "task-42",
      sender: { id: "agent-otto", kind: "agent" },
      kind: "status",
      body: "I will reduce it to a familiar shell: conversation list, main thread, and a task sidebar.",
      createdAt: "2026-03-29T05:14:00.000Z",
      visibleToUser: true,
      status: {
        status: "running",
        label: "In progress",
        detail: "Preparing a simpler Slack-like layout.",
      },
    },
    {
      id: "thread-3",
      conversationId: "channel-design-thread",
      taskId: "task-42",
      sender: { id: "agent-otto", kind: "agent" },
      kind: "result",
      body: "Public thread stays simple. Internal worker dialogue moves into a linked side panel instead of the main timeline.",
      createdAt: "2026-03-29T05:22:00.000Z",
      visibleToUser: true,
    },
    {
      id: "roadmap-1",
      conversationId: "channel-roadmap",
      taskId: "task-17",
      sender: { id: "agent-otto", kind: "agent" },
      kind: "approval",
      body: "Roadmap summary is ready. Waiting for approval to publish.",
      createdAt: "2026-03-29T04:44:00.000Z",
      visibleToUser: true,
      approval: {
        requestId: "approval-roadmap",
        label: "Publish summary",
        status: "pending",
      },
    },
    {
      id: "agent-1",
      conversationId: "agent-task-42",
      taskId: "task-42",
      sender: { id: "agent-planner", kind: "agent" },
      kind: "message",
      body: "I can keep the worker discussion in an internal thread instead of a multi-lane control panel.",
      createdAt: "2026-03-29T05:12:00.000Z",
      visibleToUser: false,
    },
    {
      id: "agent-2",
      conversationId: "agent-task-42",
      taskId: "task-42",
      sender: { id: "system", kind: "system" },
      kind: "handoff",
      body: "Planner handed the UI recommendation back to Otto.",
      createdAt: "2026-03-29T05:18:00.000Z",
      visibleToUser: false,
      handoff: {
        fromAgentId: "agent-planner",
        toAgentId: "agent-otto",
        reason: "The public interaction model is decided.",
      },
    },
    {
      id: "agent-3",
      conversationId: "agent-task-42",
      taskId: "task-42",
      sender: { id: "agent-otto", kind: "agent" },
      kind: "result",
      body: "I will keep the public thread readable and show internal dialogue as a linked task detail.",
      createdAt: "2026-03-29T05:21:00.000Z",
      visibleToUser: false,
    },
  ];

  return {
    participants,
    conversations,
    messages,
    tasks,
    unreadCounts: {
      "dm-yuqing": 3,
      "channel-design-thread": 2,
      "agent-task-42": 1,
    },
  };
}
