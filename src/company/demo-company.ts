import type { CollaborationSnapshot } from "../collaboration/demo-data.js";
import type {
  CollaborationActivityEvent,
  CollaborationConversation,
  CollaborationMessage,
  CollaborationParticipant,
  CollaborationTask,
} from "../collaboration/types.js";
import type {
  CompanyAgentProfile,
  CompanyDefinition,
  CompanySimulationResult,
  CompanyTaskRequest,
} from "./types.js";

function companyAgents(): CompanyAgentProfile[] {
  return [
    {
      id: "agent-manager",
      displayName: "Mara (Manager)",
      role: "manager",
      runtimeTarget: "claude-code",
      responsibilities: [
        "Own the public thread",
        "Turn requests into tasks",
        "Summarize builder and reviewer output",
      ],
    },
    {
      id: "agent-builder",
      displayName: "Jules (Builder)",
      role: "builder",
      runtimeTarget: "codex",
      responsibilities: [
        "Implement the requested change",
        "Run build and targeted checks",
        "Report concrete progress back to the manager",
      ],
    },
    {
      id: "agent-reviewer",
      displayName: "Nia (Reviewer)",
      role: "reviewer",
      runtimeTarget: "codex",
      responsibilities: [
        "Review the builder output",
        "Check risks and regressions",
        "Recommend merge or follow-up work",
      ],
    },
  ];
}

export function createDemoCompanyDefinition(): CompanyDefinition {
  return {
    id: "company-otto-labs",
    name: "Otto Labs",
    projectId: "project-otto-labs",
    userId: "user-founder",
    userDisplayName: "Yuqing",
    agents: companyAgents(),
  };
}

function toParticipants(company: CompanyDefinition): CollaborationParticipant[] {
  return [
    { id: company.userId, kind: "user", displayName: company.userDisplayName },
    ...company.agents.map((agent) => ({
      id: agent.id,
      kind: "agent" as const,
      displayName: agent.displayName,
      runtimeTarget: agent.runtimeTarget,
    })),
    { id: "system", kind: "system" as const, displayName: "System" },
  ];
}

function taskConversations(company: CompanyDefinition, task: CompanyTaskRequest): CollaborationConversation[] {
  return [
    {
      id: "dm-founder",
      ref: {
        provider: "database",
        spaceKind: "dm",
        conversationKind: "root",
        externalId: "dm-founder",
      },
      visibility: "private",
      title: `${company.userDisplayName} + ${company.name}`,
      participantIds: [company.userId, "agent-manager"],
      createdAt: task.createdAt,
      updatedAt: "2026-03-29T06:07:00.000Z",
    },
    {
      id: `channel-${task.id}`,
      ref: {
        provider: "database",
        spaceKind: "channel",
        conversationKind: "thread",
        externalId: `channel-${task.id}`,
        parentExternalId: "company-updates",
      },
      visibility: "shared",
      title: "# company-updates",
      participantIds: [company.userId, "agent-manager", "system"],
      projectId: company.projectId,
      taskId: task.id,
      createdAt: "2026-03-29T06:10:00.000Z",
      updatedAt: "2026-03-29T06:28:00.000Z",
    },
    {
      id: `agent-${task.id}`,
      ref: {
        provider: "database",
        spaceKind: "agent",
        conversationKind: "thread",
        externalId: `agent-${task.id}`,
      },
      visibility: "internal",
      title: `${task.id} internal company room`,
      participantIds: ["agent-manager", "agent-builder", "agent-reviewer", "system"],
      projectId: company.projectId,
      taskId: task.id,
      createdAt: "2026-03-29T06:11:00.000Z",
      updatedAt: "2026-03-29T06:27:00.000Z",
    },
  ];
}

function taskRecord(task: CompanyTaskRequest): CollaborationTask[] {
  return [
    {
      id: task.id,
      title: task.title,
      status: "running",
      ownerAgentId: "agent-manager",
      runtimeTarget: "company",
      primaryConversationId: `channel-${task.id}`,
      internalConversationIds: [`agent-${task.id}`],
      createdAt: "2026-03-29T06:10:00.000Z",
      updatedAt: "2026-03-29T06:28:00.000Z",
    },
  ];
}

function taskMessages(company: CompanyDefinition, task: CompanyTaskRequest): CollaborationMessage[] {
  return [
    {
      id: "company-dm-1",
      conversationId: "dm-founder",
      sender: { id: company.userId, kind: "user" },
      kind: "message",
      body: task.prompt,
      createdAt: task.createdAt,
      visibleToUser: true,
    },
    {
      id: "company-dm-2",
      conversationId: "dm-founder",
      sender: { id: "agent-manager", kind: "agent" },
      kind: "result",
      body: "I have opened a task in the company queue. Jules will build it, Nia will review it, and I will keep the public thread clean.",
      createdAt: "2026-03-29T06:07:00.000Z",
      visibleToUser: true,
    },
    {
      id: "company-thread-1",
      conversationId: `channel-${task.id}`,
      taskId: task.id,
      sender: { id: company.userId, kind: "user" },
      kind: "message",
      body: task.prompt,
      createdAt: "2026-03-29T06:10:00.000Z",
      visibleToUser: true,
    },
    {
      id: "company-thread-2",
      conversationId: `channel-${task.id}`,
      taskId: task.id,
      sender: { id: "agent-manager", kind: "agent" },
      kind: "status",
      body: "Task opened. Builder is implementing the slice now, and reviewer is queued behind it.",
      createdAt: "2026-03-29T06:12:00.000Z",
      visibleToUser: true,
      status: {
        status: "running",
        label: "In progress",
        detail: "Manager posted the company status update.",
      },
    },
    {
      id: "company-thread-3",
      conversationId: `channel-${task.id}`,
      taskId: task.id,
      sender: { id: "agent-manager", kind: "agent" },
      kind: "result",
      body: "Builder finished the first pass, reviewer cleared the slice, and I am preparing the public summary for merge or follow-up.",
      createdAt: "2026-03-29T06:28:00.000Z",
      visibleToUser: true,
    },
    {
      id: "company-internal-1",
      conversationId: `agent-${task.id}`,
      taskId: task.id,
      sender: { id: "agent-manager", kind: "agent" },
      kind: "message",
      body: "Jules, own implementation. Nia, review after build. I will keep the external thread concise.",
      createdAt: "2026-03-29T06:11:00.000Z",
      visibleToUser: false,
    },
    {
      id: "company-internal-2",
      conversationId: `agent-${task.id}`,
      taskId: task.id,
      sender: { id: "agent-builder", kind: "agent" },
      kind: "message",
      body: "I am taking the build slice. I will run targeted checks before handoff.",
      createdAt: "2026-03-29T06:13:00.000Z",
      visibleToUser: false,
    },
    {
      id: "company-internal-3",
      conversationId: `agent-${task.id}`,
      taskId: task.id,
      sender: { id: "agent-reviewer", kind: "agent" },
      kind: "message",
      body: "I am watching for regressions and will review once the build output lands.",
      createdAt: "2026-03-29T06:14:00.000Z",
      visibleToUser: false,
    },
    {
      id: "company-internal-4",
      conversationId: `agent-${task.id}`,
      taskId: task.id,
      sender: { id: "agent-builder", kind: "agent" },
      kind: "result",
      body: "Implementation is done. Build and targeted tests passed. Handing this to review.",
      createdAt: "2026-03-29T06:22:00.000Z",
      visibleToUser: false,
    },
    {
      id: "company-internal-5",
      conversationId: `agent-${task.id}`,
      taskId: task.id,
      sender: { id: "agent-reviewer", kind: "agent" },
      kind: "handoff",
      body: "Review complete. No blocker found. Returning this to Mara for public sign-off.",
      createdAt: "2026-03-29T06:26:00.000Z",
      visibleToUser: false,
      handoff: {
        fromAgentId: "agent-reviewer",
        toAgentId: "agent-manager",
        reason: "Review is clear.",
      },
    },
  ];
}

function taskActivities(task: CompanyTaskRequest): CollaborationActivityEvent[] {
  return [
    {
      id: "company-activity-1",
      taskId: task.id,
      conversationId: `channel-${task.id}`,
      actor: { id: "agent-manager", kind: "agent" },
      kind: "task.status_updated",
      visibility: "shared",
      status: "completed",
      title: "Task opened by manager",
      detail: "Mara accepted the request and opened the company task.",
      createdAt: "2026-03-29T06:10:20.000Z",
    },
    {
      id: "company-activity-2",
      taskId: task.id,
      conversationId: `agent-${task.id}`,
      actor: { id: "agent-manager", kind: "agent" },
      kind: "message.sent",
      visibility: "internal",
      status: "completed",
      title: "Manager assigned builder and reviewer",
      detail: "The internal company room received the role handoff.",
      createdAt: "2026-03-29T06:11:00.000Z",
    },
    {
      id: "company-activity-3",
      taskId: task.id,
      conversationId: `agent-${task.id}`,
      actor: { id: "agent-builder", kind: "agent" },
      kind: "shell.command_started",
      visibility: "internal",
      status: "started",
      title: "Builder started implementation",
      detail: "Jules is editing code and preparing targeted checks.",
      createdAt: "2026-03-29T06:13:10.000Z",
      payload: {
        role: "builder",
      },
    },
    {
      id: "company-activity-4",
      taskId: task.id,
      conversationId: `agent-${task.id}`,
      actor: { id: "agent-builder", kind: "agent" },
      kind: "shell.command_finished",
      visibility: "internal",
      status: "completed",
      title: "Builder finished build and checks",
      detail: "Implementation and targeted verification passed.",
      createdAt: "2026-03-29T06:22:00.000Z",
      parentEventId: "company-activity-3",
    },
    {
      id: "company-activity-5",
      taskId: task.id,
      conversationId: `agent-${task.id}`,
      actor: { id: "agent-reviewer", kind: "agent" },
      kind: "agent.output",
      visibility: "internal",
      status: "completed",
      title: "Reviewer finished the pass",
      detail: "Nia found no blocker and recommended sign-off.",
      createdAt: "2026-03-29T06:26:00.000Z",
    },
    {
      id: "company-activity-6",
      taskId: task.id,
      conversationId: `channel-${task.id}`,
      actor: { id: "agent-manager", kind: "agent" },
      kind: "message.sent",
      visibility: "shared",
      status: "completed",
      title: "Manager posted the public summary",
      detail: "The public thread now has the concise company update.",
      createdAt: "2026-03-29T06:28:00.000Z",
    },
  ];
}

export function createDemoCompanySnapshot(taskPrompt: string): CompanySimulationResult {
  const company = createDemoCompanyDefinition();
  const task: CompanyTaskRequest = {
    id: "task-company-1",
    title: "Company delivery task",
    prompt: taskPrompt,
    createdAt: "2026-03-29T06:05:00.000Z",
  };

  const snapshot: CollaborationSnapshot = {
    activities: taskActivities(task),
    participants: toParticipants(company),
    conversations: taskConversations(company, task),
    tasks: taskRecord(task),
    messages: taskMessages(company, task),
    unreadCounts: {
      "dm-founder": 1,
      [`channel-${task.id}`]: 2,
      [`agent-${task.id}`]: 4,
    },
  };

  return {
    company,
    snapshot,
  };
}
