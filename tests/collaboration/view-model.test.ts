import { describe, expect, it } from "vitest";

import {
  buildConversationDetailView,
  buildConversationListSections,
} from "../../src/collaboration/view-model.js";
import type {
  CollaborationConversation,
  CollaborationMessage,
  CollaborationParticipant,
  CollaborationTask,
} from "../../src/collaboration/types.js";

const participants: CollaborationParticipant[] = [
  { id: "user-1", kind: "user", displayName: "Yuqing" },
  { id: "agent-1", kind: "agent", displayName: "Otto" },
  { id: "agent-2", kind: "agent", displayName: "Planner" },
  { id: "system-1", kind: "system", displayName: "System" },
];

describe("buildConversationDetailView", () => {
  it("builds an internal agent dialogue view with ordered lanes and badges", () => {
    const conversation: CollaborationConversation = {
      id: "conv-agent",
      ref: {
        provider: "database",
        spaceKind: "agent",
        conversationKind: "thread",
        externalId: "conv-agent",
      },
      visibility: "internal",
      title: "Task 42 internal planning",
      participantIds: ["agent-1", "agent-2", "system-1"],
      taskId: "task-42",
      createdAt: "2026-03-29T10:00:00.000Z",
      updatedAt: "2026-03-29T10:05:00.000Z",
    };
    const task: CollaborationTask = {
      id: "task-42",
      title: "Design collaboration UI",
      status: "running",
      ownerAgentId: "agent-1",
      runtimeTarget: "codex",
      primaryConversationId: "conv-channel",
      internalConversationIds: ["conv-agent"],
      createdAt: "2026-03-29T09:58:00.000Z",
      updatedAt: "2026-03-29T10:05:00.000Z",
    };
    const messages: CollaborationMessage[] = [
      {
        id: "msg-1",
        conversationId: "conv-agent",
        taskId: "task-42",
        sender: { id: "agent-2", kind: "agent" },
        kind: "message",
        body: "I will own the planning slice.",
        createdAt: "2026-03-29T10:01:00.000Z",
        visibleToUser: false,
      },
      {
        id: "msg-2",
        conversationId: "conv-agent",
        taskId: "task-42",
        sender: { id: "system-1", kind: "system" },
        kind: "handoff",
        body: "Planner handed the task back to Otto.",
        createdAt: "2026-03-29T10:02:00.000Z",
        visibleToUser: false,
        handoff: {
          fromAgentId: "agent-2",
          toAgentId: "agent-1",
          reason: "Planning is complete.",
        },
      },
    ];
    const linkedConversations: CollaborationConversation[] = [
      {
        id: "conv-channel",
        ref: {
          provider: "database",
          spaceKind: "channel",
          conversationKind: "thread",
          externalId: "conv-channel",
        },
        visibility: "shared",
        title: "Design thread",
        participantIds: ["user-1", "agent-1"],
        taskId: "task-42",
        createdAt: "2026-03-29T09:59:00.000Z",
        updatedAt: "2026-03-29T10:05:00.000Z",
      },
    ];

    const view = buildConversationDetailView({
      conversation,
      participants,
      messages,
      task,
      linkedConversations,
    });

    expect(view.subtitle).toBe("Internal Agent Dialogue");
    expect(view.lanes.map((lane) => lane.label)).toEqual(["Otto", "Planner", "System"]);
    expect(view.timeline[1]?.badge).toBe("handoff agent-2 -> agent-1");
    expect(view.activeTask?.ownerAgentId).toBe("Otto");
    expect(view.linkedConversations[0]?.title).toBe("Design thread");
  });
});

describe("buildConversationListSections", () => {
  it("groups conversations by space kind and sorts each section by latest activity", () => {
    const conversations: CollaborationConversation[] = [
      {
        id: "conv-dm",
        ref: {
          provider: "database",
          spaceKind: "dm",
          conversationKind: "root",
          externalId: "conv-dm",
        },
        visibility: "private",
        title: "Yuqing + Otto",
        participantIds: ["user-1", "agent-1"],
        createdAt: "2026-03-29T09:00:00.000Z",
        updatedAt: "2026-03-29T09:10:00.000Z",
      },
      {
        id: "conv-channel",
        ref: {
          provider: "database",
          spaceKind: "channel",
          conversationKind: "thread",
          externalId: "conv-channel",
        },
        visibility: "shared",
        title: "Feature review thread",
        participantIds: ["user-1", "agent-1"],
        taskId: "task-42",
        createdAt: "2026-03-29T09:20:00.000Z",
        updatedAt: "2026-03-29T10:20:00.000Z",
      },
      {
        id: "conv-agent",
        ref: {
          provider: "database",
          spaceKind: "agent",
          conversationKind: "thread",
          externalId: "conv-agent",
        },
        visibility: "internal",
        title: "Workers on task 42",
        participantIds: ["agent-1", "agent-2"],
        taskId: "task-42",
        createdAt: "2026-03-29T09:30:00.000Z",
        updatedAt: "2026-03-29T10:30:00.000Z",
      },
    ];
    const messages: CollaborationMessage[] = [
      {
        id: "msg-dm",
        conversationId: "conv-dm",
        sender: { id: "user-1", kind: "user" },
        kind: "message",
        body: "Can you review this idea?",
        createdAt: "2026-03-29T09:10:00.000Z",
        visibleToUser: true,
      },
      {
        id: "msg-channel",
        conversationId: "conv-channel",
        taskId: "task-42",
        sender: { id: "agent-1", kind: "agent" },
        kind: "status",
        body: "The design draft is ready.",
        createdAt: "2026-03-29T10:20:00.000Z",
        visibleToUser: true,
        status: {
          status: "running",
          label: "In progress",
        },
      },
      {
        id: "msg-agent",
        conversationId: "conv-agent",
        taskId: "task-42",
        sender: { id: "agent-2", kind: "agent" },
        kind: "message",
        body: "I have finished the breakdown.",
        createdAt: "2026-03-29T10:30:00.000Z",
        visibleToUser: false,
      },
    ];
    const tasks: CollaborationTask[] = [
      {
        id: "task-42",
        title: "Design collaboration UI",
        status: "running",
        ownerAgentId: "agent-1",
        runtimeTarget: "codex",
        primaryConversationId: "conv-channel",
        internalConversationIds: ["conv-agent"],
        createdAt: "2026-03-29T09:20:00.000Z",
        updatedAt: "2026-03-29T10:30:00.000Z",
      },
    ];

    const sections = buildConversationListSections({
      conversations,
      participants,
      messages,
      tasks,
      unreadCounts: {
        "conv-channel": 2,
      },
    });

    expect(sections.map((section) => section.title)).toEqual([
      "Direct Messages",
      "Channel Conversations",
      "Agent Dialogues",
    ]);
    expect(sections[0]?.items[0]?.latestPreview).toBe("Can you review this idea?");
    expect(sections[1]?.items[0]?.taskStatus).toBe("running");
    expect(sections[1]?.items[0]?.unreadCount).toBe(2);
    expect(sections[2]?.items[0]?.latestPreview).toBe("I have finished the breakdown.");
  });
});
