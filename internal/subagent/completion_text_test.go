package subagent

import (
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/tool"
)

// Test 12: CompletionText table test. Cases are built directly from
// hand-constructed agent.Task values; no Runner involved.
func TestCompletionText(t *testing.T) {
	base := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	longResult := strings.Repeat("a", 5000)

	cases := []struct {
		name           string
		task           agent.Task
		maxOutputBytes int
		want           string
	}{
		{
			name: "succeeded",
			task: agent.Task{
				ID: "t1", Status: agent.TaskSucceeded,
				StartedAt: base, FinishedAt: base.Add(42 * time.Second),
				ToolCalls: 7,
				Usage:     model.Usage{InputTokens: 10000, OutputTokens: 2310},
				Result:    "the full report",
			},
			maxOutputBytes: 16384,
			want:           "[task-notification] task t1 (default) succeeded · 42s · 7 tool calls · 12,310 tokens\nthe full report",
		},
		{
			name: "failed",
			task: agent.Task{
				ID: "t1", Status: agent.TaskFailed,
				StartedAt: base, FinishedAt: base.Add(12 * time.Second),
				ToolCalls: 3,
				Error:     "provider exploded",
			},
			maxOutputBytes: 16384,
			want:           "[task-notification] task t1 (default) failed · 12s · 3 tool calls\nprovider exploded",
		},
		{
			name: "canceled",
			task: agent.Task{
				ID: "t1", Status: agent.TaskCanceled,
				StartedAt: base, FinishedAt: base.Add(12 * time.Second),
				ToolCalls: 3,
			},
			maxOutputBytes: 16384,
			want:           "[task-notification] task t1 (default) canceled · 12s · 3 tool calls",
		},
		{
			name: "canceled while queued has zero duration and zero tool calls",
			task: agent.Task{
				ID: "t3", Status: agent.TaskCanceled,
				StartedAt: time.Time{}, FinishedAt: base,
				ToolCalls: 0,
			},
			maxOutputBytes: 16384,
			want:           "[task-notification] task t3 (default) canceled · 0s · 0 tool calls",
		},
		{
			name: "singular tool call and singular-looking token count stay unpluralized/pluralized per spec",
			task: agent.Task{
				ID: "t4", Status: agent.TaskSucceeded,
				StartedAt: base, FinishedAt: base.Add(1 * time.Second),
				ToolCalls: 1,
				Usage:     model.Usage{InputTokens: 1},
				Result:    "ok",
			},
			maxOutputBytes: 16384,
			want:           "[task-notification] task t4 (default) succeeded · 1s · 1 tool call · 1 tokens\nok",
		},
		{
			name: "duration over a minute renders as Go's Duration.String",
			task: agent.Task{
				ID: "t5", Status: agent.TaskSucceeded,
				StartedAt: base, FinishedAt: base.Add(65 * time.Second),
				ToolCalls: 2,
				Result:    "done",
			},
			maxOutputBytes: 16384,
			want:           "[task-notification] task t5 (default) succeeded · 1m5s · 2 tool calls · 0 tokens\ndone",
		},
		{
			name: "succeeded with model",
			task: agent.Task{
				ID: "t1", Status: agent.TaskSucceeded,
				StartedAt: base, FinishedAt: base.Add(42 * time.Second),
				ToolCalls: 7,
				Model:     "gpt-4o-mini",
				Usage:     model.Usage{InputTokens: 10000, OutputTokens: 2310},
				Result:    "the full report",
			},
			maxOutputBytes: 16384,
			want:           "[task-notification] task t1 (default) succeeded · gpt-4o-mini · 42s · 7 tool calls · 12,310 tokens\nthe full report",
		},
		{
			name: "failed with model",
			task: agent.Task{
				ID: "t1", Status: agent.TaskFailed,
				StartedAt: base, FinishedAt: base.Add(12 * time.Second),
				ToolCalls: 3,
				Model:     "gpt-4o-mini",
				Error:     "provider exploded",
			},
			maxOutputBytes: 16384,
			want:           "[task-notification] task t1 (default) failed · gpt-4o-mini · 12s · 3 tool calls\nprovider exploded",
		},
		{
			name: "canceled with model",
			task: agent.Task{
				ID: "t1", Status: agent.TaskCanceled,
				StartedAt: base, FinishedAt: base.Add(12 * time.Second),
				ToolCalls: 3,
				Model:     "gpt-4o-mini",
			},
			maxOutputBytes: 16384,
			want:           "[task-notification] task t1 (default) canceled · gpt-4o-mini · 12s · 3 tool calls",
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := CompletionText(tc.task, tc.maxOutputBytes); got != tc.want {
				t.Fatalf("CompletionText() = %q, want %q", got, tc.want)
			}
		})
	}

	// A result longer than maxOutputBytes is capped the same way
	// tool.CappedTextResult caps it directly; CompletionText only composes
	// the header on top.
	t.Run("succeeded result is capped by maxOutputBytes", func(t *testing.T) {
		task := agent.Task{
			ID: "t6", Status: agent.TaskSucceeded,
			StartedAt: base, FinishedAt: base.Add(3 * time.Second),
			ToolCalls: 0,
			Result:    longResult,
		}
		got := CompletionText(task, 100)
		wantHeader := "[task-notification] task t6 (default) succeeded · 3s · 0 tool calls · 0 tokens"
		wantBody := tool.CappedTextResult(longResult, 100).Content
		if want := wantHeader + "\n" + wantBody; got != want {
			t.Fatalf("CompletionText() = %q, want %q", got, want)
		}
		if !strings.Contains(got, "truncated") {
			t.Fatal("expected the capped result to mention truncation")
		}
	})
}
