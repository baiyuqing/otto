import { describe, expect, it } from "vitest";

import { createDemoCompanySnapshot } from "../../src/company/demo-company.js";

describe("createDemoCompanySnapshot", () => {
  it("creates a 3-agent company snapshot with public and internal conversations", () => {
    const result = createDemoCompanySnapshot(
      "Build a settings page, review it, and report the result.",
    );

    expect(result.company.agents.map((agent) => agent.role)).toEqual([
      "manager",
      "builder",
      "reviewer",
    ]);
    expect(result.snapshot.conversations.map((conversation) => conversation.ref.spaceKind)).toEqual([
      "dm",
      "channel",
      "agent",
    ]);
    expect(result.snapshot.tasks[0]?.internalConversationIds).toEqual(["agent-task-company-1"]);
    expect(
      result.snapshot.messages.some(
        (message) => message.conversationId === "agent-task-company-1" && message.sender.id === "agent-builder",
      ),
    ).toBe(true);
    expect(
      result.snapshot.activities.some(
        (activity) => activity.kind === "shell.command_started" && activity.actor.id === "agent-builder",
      ),
    ).toBe(true);
  });
});
