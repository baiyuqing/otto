import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, describe, expect, it } from "vitest";

import { createDemoCollaborationSnapshot } from "../../src/collaboration/demo-data.js";
import { SqliteCollaborationStore } from "../../src/collaboration/sqlite-store.js";

const tempDirs: string[] = [];

afterEach(async () => {
  await Promise.all(tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

describe("SqliteCollaborationStore", () => {
  it("stores and recalls the demo collaboration snapshot", async () => {
    const dir = await mkdtemp(join(tmpdir(), "otto-collab-"));
    tempDirs.push(dir);

    const databasePath = join(dir, "collaboration.sqlite");
    const store = new SqliteCollaborationStore(databasePath);

    try {
      await store.replaceSnapshot(createDemoCollaborationSnapshot());

      const conversations = await store.listConversations();
      const tasks = await store.listTasks();
      const messages = await store.listMessages({
        conversationId: "channel-design-thread",
      });
      const unreadCounts = await store.listUnreadCounts?.();
      const participants = await store.listParticipants(["user-yuqing", "agent-otto"]);
      const snapshot = await store.readSnapshot();

      expect(conversations.map((conversation) => conversation.id)).toContain("agent-task-42");
      expect(tasks.find((task) => task.id === "task-42")?.internalConversationIds).toEqual(["agent-task-42"]);
      expect(messages.at(-1)?.kind).toBe("result");
      expect(unreadCounts?.["channel-design-thread"]).toBe(2);
      expect(participants.map((participant) => participant.displayName)).toEqual(["Yuqing", "Otto"]);
      expect(snapshot.messages).toHaveLength(9);
    } finally {
      store.close();
    }
  });

  it("filters conversations and tasks by collaboration metadata", async () => {
    const store = new SqliteCollaborationStore(":memory:");

    try {
      await store.replaceSnapshot(createDemoCollaborationSnapshot());

      const internalConversations = await store.listConversations({
        spaceKinds: ["agent"],
      });
      const approvalTasks = await store.listTasks({
        status: ["waiting-approval"],
      });
      const internalTask = await store.getTask("task-42");

      expect(internalConversations).toHaveLength(1);
      expect(internalConversations[0]?.visibility).toBe("internal");
      expect(approvalTasks[0]?.id).toBe("task-17");
      expect(internalTask?.runtimeTarget).toBe("codex");
    } finally {
      store.close();
    }
  });
});
