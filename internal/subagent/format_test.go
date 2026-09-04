package subagent

import (
	"encoding/json"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/model"
)

func TestTaskLineFormatsColumns(t *testing.T) {
	now := time.Now()
	started := now.Add(-42 * time.Second)
	task := agent.Task{
		ID:        "t1",
		Agent:     "explorer",
		Status:    agent.TaskRunning,
		StartedAt: started,
		ToolCalls: 4,
		LastTool:  `grep "session"`,
		Prompt:    "find where sessions are written",
	}
	got := TaskLine(task, now)
	want := "t1   explorer   running      42s   4 tools  grep \"session\"           find where sessions are written"
	if got != want {
		t.Fatalf("TaskLine() = %q, want %q", got, want)
	}
}

func TestTaskLineDefaultAgentAndDescriptionLabel(t *testing.T) {
	now := time.Now()
	task := agent.Task{
		ID:          "t2",
		Status:      agent.TaskQueued,
		Description: "review the diff",
	}
	got := TaskLine(task, now)
	if !strings.Contains(got, "(default)") {
		t.Fatalf("TaskLine() = %q, want it to contain %q", got, "(default)")
	}
	if !strings.HasSuffix(got, "review the diff") {
		t.Fatalf("TaskLine() = %q, want suffix %q", got, "review the diff")
	}
}

func TestTaskStepsRendersToolCallsAndAssistantTextOnly(t *testing.T) {
	history := []model.Message{
		{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "review please"}}},
		{Role: model.RoleAssistant, Blocks: []model.Block{
			{Type: model.BlockToolCall, ToolName: "read", Arguments: json.RawMessage(`{"path":"main.go"}`)},
		}},
		{Role: model.RoleTool, Blocks: []model.Block{{Type: model.BlockToolResult, Text: "package main"}}},
		{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "looks fine"}}},
	}
	got := TaskSteps(history)
	want := "  → read {\"path\":\"main.go\"}\n  assistant: looks fine\n"
	if got != want {
		t.Fatalf("TaskSteps() = %q, want %q", got, want)
	}
}
