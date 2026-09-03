package server

import (
	"encoding/json"
	"errors"
	"testing"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/tool"
)

func TestToWire(t *testing.T) {
	cases := []struct {
		name  string
		event agent.Event
		want  string
	}{
		{
			name:  "agent_started",
			event: agent.Event{Type: agent.EventAgentStarted},
			want:  `{"type":"agent_started"}`,
		},
		{
			name:  "agent_finished",
			event: agent.Event{Type: agent.EventAgentFinished},
			want:  `{"type":"agent_finished"}`,
		},
		{
			name:  "text_delta",
			event: agent.Event{Type: agent.EventTextDelta, Text: "hello"},
			want:  `{"type":"text_delta","text":"hello"}`,
		},
		{
			name:  "tool_call_started valid args",
			event: agent.Event{Type: agent.EventToolCallStarted, ToolName: "read", ToolCallID: "call_1", ToolArgs: `{"path":"a.txt"}`},
			want:  `{"type":"tool_call_started","tool_name":"read","tool_call_id":"call_1","tool_args":{"path":"a.txt"}}`,
		},
		{
			name:  "tool_call_started invalid args become a JSON string",
			event: agent.Event{Type: agent.EventToolCallStarted, ToolName: "read", ToolCallID: "call_2", ToolArgs: "{oops"},
			want:  `{"type":"tool_call_started","tool_name":"read","tool_call_id":"call_2","tool_args":"{oops"}`,
		},
		{
			name:  "tool_call_started empty args are omitted",
			event: agent.Event{Type: agent.EventToolCallStarted, ToolName: "read", ToolCallID: "call_3", ToolArgs: ""},
			want:  `{"type":"tool_call_started","tool_name":"read","tool_call_id":"call_3"}`,
		},
		{
			name: "tool_call_finished uses Content not PersistedContent",
			event: agent.Event{
				Type: agent.EventToolCallFinished, ToolName: "read", ToolCallID: "call_1",
				ToolResult: tool.Result{Content: "file contents", PersistedContent: "TRUNCATED", IsError: false},
			},
			want: `{"type":"tool_call_finished","tool_name":"read","tool_call_id":"call_1","result":{"content":"file contents","is_error":false}}`,
		},
		{
			name: "tool_call_finished error result",
			event: agent.Event{
				Type: agent.EventToolCallFinished, ToolName: "bash", ToolCallID: "call_2",
				ToolResult: tool.Result{Content: "boom", IsError: true},
			},
			want: `{"type":"tool_call_finished","tool_name":"bash","tool_call_id":"call_2","result":{"content":"boom","is_error":true}}`,
		},
		{
			name:  "provider_usage with cached tokens",
			event: agent.Event{Type: agent.EventProviderUsage, Usage: model.Usage{InputTokens: 12, OutputTokens: 34, CachedInputTokens: 5}},
			want:  `{"type":"provider_usage","usage":{"input_tokens":12,"output_tokens":34,"cached_input_tokens":5}}`,
		},
		{
			name:  "provider_usage without cached tokens",
			event: agent.Event{Type: agent.EventProviderUsage, Usage: model.Usage{InputTokens: 12, OutputTokens: 34}},
			want:  `{"type":"provider_usage","usage":{"input_tokens":12,"output_tokens":34}}`,
		},
		{
			name:  "notification carries task id, text, and usage",
			event: agent.Event{Type: agent.EventNotification, TaskID: "t1", Text: "[task-notification] task t1 succeeded\nreport", Usage: model.Usage{InputTokens: 5, OutputTokens: 6}},
			want:  `{"type":"notification","task_id":"t1","text":"[task-notification] task t1 succeeded\nreport","usage":{"input_tokens":5,"output_tokens":6}}`,
		},
		{
			name: "compaction_started without usage",
			event: agent.Event{Type: agent.EventCompactionStarted, Compaction: &agent.CompactionEvent{
				Reason: agent.CompactionThreshold, TokensBefore: 1000, EstimatedTokensAfter: 400, Automatic: true,
			}},
			want: `{"type":"compaction_started","compaction":{"reason":"threshold","tokens_before":1000,"estimated_tokens_after":400,"automatic":true,"noop":false}}`,
		},
		{
			name: "compaction_completed with usage and checkpoint id",
			event: agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{
				CheckpointID: "cp1", Reason: agent.CompactionManual, TokensBefore: 900, EstimatedTokensAfter: 300,
				Usage: model.Usage{InputTokens: 10, OutputTokens: 20}, UsagePresent: true,
			}},
			want: `{"type":"compaction_completed","compaction":{"checkpoint_id":"cp1","reason":"manual","tokens_before":900,"estimated_tokens_after":300,"automatic":false,"usage":{"input_tokens":10,"output_tokens":20},"noop":false}}`,
		},
		{
			name: "compaction_completed noop without usage",
			event: agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{
				Reason: agent.CompactionOverflow, Automatic: true, Noop: true,
			}},
			want: `{"type":"compaction_completed","compaction":{"reason":"overflow","tokens_before":0,"estimated_tokens_after":0,"automatic":true,"noop":true}}`,
		},
		{
			name: "compaction_planned",
			event: agent.Event{Type: agent.EventCompactionPlanned, Plan: &agent.CompactionPlan{
				Reason: agent.CompactionThreshold, Automatic: true, TokensBefore: 1000, EstimatedTokensAfter: 400,
				SummarizedMessages: 5, RetainedMessages: 3, Mode: agent.CompactionModeStructured,
			}},
			want: `{"type":"compaction_planned","plan":{"reason":"threshold","automatic":true,"tokens_before":1000,"estimated_tokens_after":400,"summarized_messages":5,"retained_messages":3,"mode":"structured"}}`,
		},
		{
			name:  "compaction_warning carries the error text",
			event: agent.Event{Type: agent.EventCompactionWarning, Err: errors.New("boom")},
			want:  `{"type":"compaction_warning","error":"boom"}`,
		},
		{
			name:  "memory_warning carries the error text",
			event: agent.Event{Type: agent.EventMemoryWarning, Err: errors.New("memory down")},
			want:  `{"type":"memory_warning","error":"memory down"}`,
		},
		{
			name:  "agent_error carries the error text",
			event: agent.Event{Type: agent.EventAgentError, Err: errors.New("failed")},
			want:  `{"type":"agent_error","error":"failed"}`,
		},
		{
			name:  "nil Err yields no error key",
			event: agent.Event{Type: agent.EventAgentError},
			want:  `{"type":"agent_error"}`,
		},
	}

	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got, err := json.Marshal(toWire(c.event))
			if err != nil {
				t.Fatalf("Marshal: %v", err)
			}
			if string(got) != c.want {
				t.Fatalf("toWire mismatch:\n got:  %s\n want: %s", got, c.want)
			}
		})
	}
}

func TestToWireDoesNotAliasCompactionPointer(t *testing.T) {
	src := &agent.CompactionEvent{Reason: agent.CompactionManual, TokensBefore: 10}
	event := agent.Event{Type: agent.EventCompactionStarted, Compaction: src}
	wire := toWire(event)
	if wire.Compaction == nil {
		t.Fatal("expected non-nil wire compaction")
	}
	src.TokensBefore = 999
	if wire.Compaction.TokensBefore == 999 {
		t.Fatal("toWire must copy the compaction struct, not alias the pointer")
	}
}

func TestToWireDoesNotAliasPlanPointer(t *testing.T) {
	src := &agent.CompactionPlan{Reason: agent.CompactionManual, TokensBefore: 10}
	event := agent.Event{Type: agent.EventCompactionPlanned, Plan: src}
	wire := toWire(event)
	if wire.Plan == nil {
		t.Fatal("expected non-nil wire plan")
	}
	src.TokensBefore = 999
	if wire.Plan.TokensBefore == 999 {
		t.Fatal("toWire must copy the plan struct, not alias the pointer")
	}
}
