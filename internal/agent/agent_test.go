package agent

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/sandbox"
	"github.com/baiyuqing/otto/internal/sandbox/direct"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
)

func TestRunExecutesToolAndReturnsToProvider(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{
			response: provider.Response{
				Message: model.Message{FinishReason: model.FinishToolCalls, Usage: &model.Usage{InputTokens: 11, OutputTokens: 7}, Role: model.RoleAssistant, Blocks: []model.Block{
					{Type: model.BlockText, Text: "checking"},
					{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{"value":"hello"}`)},
				}},
			},
		},
		{
			response: provider.Response{
				Message: model.Message{FinishReason: model.FinishStop, Usage: &model.Usage{InputTokens: 3, OutputTokens: 2}, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}},
			},
		},
	}}
	registry, err := tool.NewRegistry(echoTool{})
	if err != nil {
		t.Fatal(err)
	}
	memory := session.NewMemory(testHeader(t))
	runner := New(fakeProvider, registry, memory, Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	var events []Event
	if err := runner.Run(context.Background(), "inspect", func(event Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}

	messages := memory.Messages()
	if len(messages) != 4 || messages[0].Role != model.RoleUser || messages[1].Role != model.RoleAssistant || messages[2].Role != model.RoleTool || messages[3].Role != model.RoleAssistant {
		t.Fatalf("unexpected message sequence: %#v", messages)
	}
	if messages[1].FinishReason != model.FinishToolCalls || messages[1].Usage == nil || *messages[1].Usage != (model.Usage{InputTokens: 11, OutputTokens: 7}) {
		t.Fatalf("first assistant message missing finish reason or usage: %#v", messages[1])
	}
	if messages[3].FinishReason != model.FinishStop || messages[3].Usage == nil || *messages[3].Usage != (model.Usage{InputTokens: 3, OutputTokens: 2}) {
		t.Fatalf("second assistant message missing finish reason or usage: %#v", messages[3])
	}
	if len(fakeProvider.requests) != 2 {
		t.Fatalf("provider calls = %d, want %d", len(fakeProvider.requests), 2)
	}
	if got := fakeProvider.requests[0].Messages[0].Text(); got != "inspect" {
		t.Fatalf("first request user text = %q, want %q", got, "inspect")
	}
	if got := fakeProvider.requests[1].Messages[2].Blocks[0].Text; got != "hello" {
		t.Fatalf("second request tool result = %q, want %q", got, "hello")
	}
	if got := fakeProvider.requests[0].Tools; len(got) != 1 || got[0].Name != "echo" {
		t.Fatalf("first request tools = %#v, want echo", got)
	}
	if got := eventTypes(events); got[0] != EventAgentStarted || got[len(got)-1] != EventAgentFinished {
		t.Fatalf("event flow = %v, want started...finished", got)
	}
}

func TestRunForwardsTextDeltas(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{{
		response: provider.Response{
			Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{
				{Type: model.BlockText, Text: "hello"},
				{Type: model.BlockText, Text: " world"},
			}},
		},
	}}}
	registry, err := tool.NewRegistry()
	if err != nil {
		t.Fatal(err)
	}
	runner := New(fakeProvider, registry, session.NewMemory(testHeader(t)), Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	var deltas []string
	if err := runner.Run(context.Background(), "inspect", func(event Event) {
		if event.Type == EventTextDelta {
			deltas = append(deltas, event.Text)
		}
	}); err != nil {
		t.Fatal(err)
	}
	if got, want := strings.Join(deltas, ""), "hello world"; got != want {
		t.Fatalf("text deltas = %q, want %q", got, want)
	}
}

func TestRunEmitsProviderAPICallMetricsEvent(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{{
		response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}},
	}}}
	registry, err := tool.NewRegistry()
	if err != nil {
		t.Fatal(err)
	}
	runner := New(fakeProvider, registry, session.NewMemory(testHeader(t)), Options{Provider: "openai-compatible", Model: "test-model", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	var events []Event
	if err := runner.Run(context.Background(), "inspect", func(event Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}

	var got *Event
	for i := range events {
		if events[i].Type == EventProviderAPICall {
			got = &events[i]
			break
		}
	}
	if got == nil {
		t.Fatalf("missing %s event in %#v", EventProviderAPICall, eventTypes(events))
	}
	if got.ProviderName != "openai-compatible" || got.Model != "test-model" || got.APIStatus != "ok" {
		t.Fatalf("provider API event = %#v", *got)
	}
	if got.APIDuration < 0 {
		t.Fatalf("provider API duration = %s, want nonnegative", got.APIDuration)
	}
}

func TestProviderAPICallStatus(t *testing.T) {
	runner := &Agent{options: Options{Provider: "chatgpt", Model: "gpt-5"}}

	for _, test := range []struct {
		name   string
		ctx    context.Context
		err    error
		status string
	}{
		{name: "ok", ctx: context.Background(), status: "ok"},
		{name: "error", ctx: context.Background(), err: errors.New("boom"), status: "error"},
		{name: "canceled error", ctx: context.Background(), err: context.Canceled, status: "canceled"},
		{name: "canceled context", ctx: canceledContext(), status: "canceled"},
	} {
		t.Run(test.name, func(t *testing.T) {
			var got Event
			runner.emitProviderAPICall(func(event Event) { got = event }, test.ctx, time.Second, test.err)
			if got.Type != EventProviderAPICall || got.APIStatus != test.status {
				t.Fatalf("event = %#v, want status %q", got, test.status)
			}
			if got.ProviderName != "chatgpt" || got.Model != "gpt-5" || got.APIDuration != time.Second {
				t.Fatalf("event metadata = %#v", got)
			}
		})
	}
}

func canceledContext() context.Context {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	return ctx
}

func TestRunPreservesProviderUsagePresence(t *testing.T) {
	for _, test := range []struct {
		name  string
		usage *model.Usage
	}{
		{name: "missing"},
		{name: "explicit zero", usage: &model.Usage{}},
	} {
		t.Run(test.name, func(t *testing.T) {
			fakeProvider := &scriptedProvider{scripts: []providerScript{{response: provider.Response{Message: model.Message{
				Role: model.RoleAssistant, FinishReason: model.FinishStop,
				Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}, Usage: test.usage,
			}}}}}
			memory := session.NewMemory(testHeader(t))
			runner := New(fakeProvider, nil, memory, Options{Model: "test", Now: fixedClock, NewID: fixedIDs()})
			var usageEvent Event
			if err := runner.Run(context.Background(), "inspect", func(event Event) {
				if event.Type == EventProviderUsage {
					usageEvent = event
				}
			}); err != nil {
				t.Fatal(err)
			}
			got := memory.Messages()[1].Usage
			if (got == nil) != (test.usage == nil) || got != nil && *got != *test.usage {
				t.Fatalf("stored usage = %#v, want %#v", got, test.usage)
			}
			if usageEvent.UsagePresent != (test.usage != nil) {
				t.Fatalf("usage event presence = %v, want %v", usageEvent.UsagePresent, test.usage != nil)
			}
		})
	}
}

func TestRunRejectsInvalidProviderUsage(t *testing.T) {
	for _, usage := range []model.Usage{
		{InputTokens: -1},
		{InputTokens: 1, CachedInputTokens: 2},
	} {
		t.Run(fmt.Sprintf("%+v", usage), func(t *testing.T) {
			memory := session.NewMemory(testHeader(t))
			runner := New(&scriptedProvider{scripts: []providerScript{{response: provider.Response{Message: model.Message{
				Role: model.RoleAssistant, FinishReason: model.FinishStop,
				Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}, Usage: &usage,
			}}}}}, nil, memory, Options{Model: "test", Now: fixedClock, NewID: fixedIDs()})
			if err := runner.Run(context.Background(), "inspect", nil); err == nil || !strings.Contains(err.Error(), "token") {
				t.Fatalf("Run() error = %v, want usage validation error", err)
			}
			if got := memory.Messages(); len(got) != 1 || got[0].Role != model.RoleUser {
				t.Fatalf("unexpected persisted messages: %#v", got)
			}
		})
	}
}

func TestRunRejectsNonAssistantProviderRole(t *testing.T) {
	memory := session.NewMemory(testHeader(t))
	runner := New(&scriptedProvider{scripts: []providerScript{{response: provider.Response{Message: model.Message{
		Role: model.RoleUser, FinishReason: model.FinishStop,
		Blocks: []model.Block{{Type: model.BlockText, Text: "wrong role"}},
	}}}}}, nil, memory, Options{Model: "test", Now: fixedClock, NewID: fixedIDs()})
	if err := runner.Run(context.Background(), "inspect", nil); err == nil || !strings.Contains(err.Error(), "assistant role") {
		t.Fatalf("Run() error = %v, want assistant-role validation error", err)
	}
}

func TestRunSendsThinkingToProvider(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{{
		response: provider.Response{
			Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "ok"}}},
		},
	}}}
	runner := New(fakeProvider, nil, session.NewMemory(testHeader(t)), Options{Model: "test", Thinking: "high", Now: fixedClock, NewID: fixedIDs()})
	if err := runner.Run(context.Background(), "inspect", nil); err != nil {
		t.Fatal(err)
	}
	if len(fakeProvider.requests) != 1 || fakeProvider.requests[0].Thinking != "high" {
		t.Fatalf("provider requests = %#v, want Thinking high", fakeProvider.requests)
	}
}

func TestRunEmitsToolCallEvents(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{
			response: provider.Response{
				Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{"value":"hello"}`)}}},
			},
		},
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}}},
	}}
	registry, err := tool.NewRegistry(echoTool{})
	if err != nil {
		t.Fatal(err)
	}
	runner := New(fakeProvider, registry, session.NewMemory(testHeader(t)), Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	var got []Event
	if err := runner.Run(context.Background(), "inspect", func(event Event) {
		if event.Type == EventToolCallStarted || event.Type == EventToolCallFinished {
			got = append(got, event)
		}
	}); err != nil {
		t.Fatal(err)
	}
	if len(got) != 2 {
		t.Fatalf("tool events = %#v, want start+finish", got)
	}
	if got[0].Type != EventToolCallStarted || got[0].ToolName != "echo" || got[0].ToolCallID != "call-1" {
		t.Fatalf("start event = %#v", got[0])
	}
	if got[1].Type != EventToolCallFinished || got[1].ToolResult.Content != "hello" || got[1].ToolResult.IsError {
		t.Fatalf("finish event = %#v", got[1])
	}
}

func TestRunPersistsUnknownToolResults(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{
			response: provider.Response{
				Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "missing", Arguments: json.RawMessage(`{"value":"hello"}`)}}},
			},
		},
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}}},
	}}
	registry, err := tool.NewRegistry()
	if err != nil {
		t.Fatal(err)
	}
	memory := session.NewMemory(testHeader(t))
	runner := New(fakeProvider, registry, memory, Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	if err := runner.Run(context.Background(), "inspect", nil); err != nil {
		t.Fatal(err)
	}
	result := memory.Messages()[2].Blocks[0]
	if !result.IsError || result.Text != "unknown tool: missing" || result.ToolName != "missing" {
		t.Fatalf("unexpected tool result: %#v", result)
	}
	if got := fakeProvider.requests[1].Messages[2].Blocks[0]; got.Text != "unknown tool: missing" || !got.IsError {
		t.Fatalf("unexpected tool result in provider request: %#v", got)
	}
}

func TestRunRejectsInvalidProviderToolArguments(t *testing.T) {
	recorder := &recordingTool{name: "echo"}
	recorder.execute = func(_ context.Context, arguments json.RawMessage) tool.Result {
		recorder.calls = append(recorder.calls, string(arguments))
		var args struct {
			Value string `json:"value"`
		}
		if err := json.Unmarshal(arguments, &args); err != nil {
			return tool.Result{Content: err.Error(), IsError: true}
		}
		return tool.Result{Content: args.Value}
	}
	registry, err := toolpkgNewRegistry(recorder)
	if err != nil {
		t.Fatal(err)
	}
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{
			response: provider.Response{
				Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{`)}}},
			},
		},
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}}},
	}}
	memory := session.NewMemory(testHeader(t))
	runner := New(fakeProvider, registry, memory, Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	err = runner.Run(context.Background(), "inspect", nil)
	if err == nil || !strings.Contains(err.Error(), "tool-call block is malformed") {
		t.Fatalf("Run() error = %v, want malformed tool-call error", err)
	}
	if got := len(recorder.calls); got != 0 {
		t.Fatalf("tool calls = %d, want no call", got)
	}
	if got := memory.Messages(); len(got) != 1 || got[0].Role != model.RoleUser {
		t.Fatalf("unexpected persisted messages: %#v", got)
	}
}

func TestRunExecutesMultipleToolCallsSequentially(t *testing.T) {
	recorder := &recordingTool{name: "echo"}
	recorder.execute = func(_ context.Context, arguments json.RawMessage) tool.Result {
		var args struct {
			Value string `json:"value"`
		}
		if err := json.Unmarshal(arguments, &args); err != nil {
			return tool.Result{Content: err.Error(), IsError: true}
		}
		recorder.calls = append(recorder.calls, args.Value)
		return tool.Result{Content: args.Value}
	}
	registry, err := toolpkgNewRegistry(recorder)
	if err != nil {
		t.Fatal(err)
	}
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{
			response: provider.Response{
				Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{
					{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{"value":"one"}`)},
					{Type: model.BlockToolCall, ToolCallID: "call-2", ToolName: "echo", Arguments: json.RawMessage(`{"value":"two"}`)},
				}},
			},
		},
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}}},
	}}
	memory := session.NewMemory(testHeader(t))
	runner := New(fakeProvider, registry, memory, Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	if err := runner.Run(context.Background(), "inspect", nil); err != nil {
		t.Fatal(err)
	}
	if got := strings.Join(recorder.calls, ","); got != "one,two" {
		t.Fatalf("tool call order = %q, want %q", got, "one,two")
	}
	if got := memory.Messages(); len(got) != 5 || got[2].Blocks[0].Text != "one" || got[3].Blocks[0].Text != "two" {
		t.Fatalf("unexpected messages: %#v", got)
	}
}

func TestRunMaxTurnsOptionIsIgnored(t *testing.T) {
	const turns = 55 // exceeds the former default cap of 50
	scripts := make([]providerScript, 0, turns+1)
	for i := 0; i < turns; i++ {
		scripts = append(scripts, providerScript{
			response: provider.Response{
				Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{
					{Type: model.BlockToolCall, ToolCallID: fmt.Sprintf("call-%d", i), ToolName: "echo", Arguments: json.RawMessage(`{"value":"x"}`)},
				}},
			},
		})
	}
	scripts = append(scripts, providerScript{
		response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}},
	})
	fakeProvider := &scriptedProvider{scripts: scripts}
	registry, err := tool.NewRegistry(echoTool{})
	if err != nil {
		t.Fatal(err)
	}
	runner := New(fakeProvider, registry, session.NewMemory(testHeader(t)), Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	if err := runner.Run(context.Background(), "inspect", nil); err != nil {
		t.Fatalf("Run() error = %v, want success with max turns ignored", err)
	}
	if got, want := len(fakeProvider.requests), turns+1; got != want {
		t.Fatalf("provider calls = %d, want %d", got, want)
	}
}

func TestRunContinuesWhileProviderRequestsTools(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{
			response: provider.Response{
				Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{"value":"hello"}`)}}},
			},
		},
		{
			response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}},
		},
	}}
	registry, err := tool.NewRegistry(echoTool{})
	if err != nil {
		t.Fatal(err)
	}
	memory := session.NewMemory(testHeader(t))
	runner := New(fakeProvider, registry, memory, Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	var events []Event
	err = runner.Run(context.Background(), "inspect", func(event Event) { events = append(events, event) })
	if err != nil {
		t.Fatalf("Run() error = %v, want success while tools are requested", err)
	}
	if len(fakeProvider.requests) != 2 {
		t.Fatalf("provider calls = %d, want 2 (one tool turn then a final turn)", len(fakeProvider.requests))
	}
	if got := memory.Messages(); len(got) != 4 {
		t.Fatalf("messages = %#v, want user+assistant+tool+assistant", got)
	}
	if got := eventTypes(events); got[0] != EventAgentStarted || got[len(got)-1] != EventAgentFinished {
		t.Fatalf("event flow = %v, want started...finished", got)
	}
}

func TestRunReturnsProviderFailure(t *testing.T) {
	providerErr := errors.New("provider failed")
	fakeProvider := &scriptedProvider{scripts: []providerScript{{err: providerErr}}}
	registry, err := tool.NewRegistry()
	if err != nil {
		t.Fatal(err)
	}
	memory := session.NewMemory(testHeader(t))
	runner := New(fakeProvider, registry, memory, Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	var gotErr error
	err = runner.Run(context.Background(), "inspect", func(event Event) {
		if event.Type == EventAgentError {
			gotErr = event.Err
		}
	})
	if !errors.Is(err, providerErr) {
		t.Fatalf("Run() error = %v, want %v", err, providerErr)
	}
	if !errors.Is(gotErr, providerErr) {
		t.Fatalf("error event = %v, want %v", gotErr, providerErr)
	}
	if got := memory.Messages(); len(got) != 1 || got[0].Role != model.RoleUser {
		t.Fatalf("unexpected persisted messages: %#v", got)
	}
}

func TestRunPersistsCanceledToolResultBeforeNextPrompt(t *testing.T) {
	started := make(chan struct{})
	cancelingTool := &recordingTool{name: "wait"}
	cancelingTool.execute = func(ctx context.Context, _ json.RawMessage) tool.Result {
		close(started)
		<-ctx.Done()
		return tool.Result{Content: "tool execution canceled", IsError: true}
	}
	registry, err := toolpkgNewRegistry(cancelingTool)
	if err != nil {
		t.Fatal(err)
	}
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{
			response: provider.Response{
				Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "wait", Arguments: json.RawMessage(`{}`)}}},
			},
		},
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "recovered"}}}}},
	}}
	memory := session.NewMemory(testHeader(t))
	runner := New(fakeProvider, registry, memory, Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	ctx, cancel := context.WithCancel(context.Background())
	errCh := make(chan error, 1)
	go func() { errCh <- runner.Run(ctx, "first prompt", nil) }()
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("tool did not start")
	}
	cancel()
	if err := <-errCh; !errors.Is(err, context.Canceled) {
		t.Fatalf("canceled Run() error = %v, want %v", err, context.Canceled)
	}

	messages := memory.Messages()
	if len(messages) != 3 || messages[1].Role != model.RoleAssistant || messages[2].Role != model.RoleTool {
		t.Fatalf("messages after cancellation = %#v, want user+assistant+tool", messages)
	}
	result := messages[2].Blocks[0]
	if result.ToolCallID != "call-1" || result.ToolName != "wait" || result.Text != "tool execution canceled" || !result.IsError {
		t.Fatalf("persisted cancellation result = %#v", result)
	}

	if err := runner.Run(context.Background(), "next prompt", nil); err != nil {
		t.Fatalf("next Run() = %v", err)
	}
	if len(fakeProvider.requests) != 2 {
		t.Fatalf("provider requests = %d, want canceled turn plus next prompt", len(fakeProvider.requests))
	}
	nextMessages := fakeProvider.requests[1].Messages
	if len(nextMessages) != 4 || nextMessages[2].Role != model.RoleTool || nextMessages[2].Blocks[0].ToolCallID != "call-1" || nextMessages[3].Text() != "next prompt" {
		t.Fatalf("next provider messages = %#v, want matched tool result before next user", nextMessages)
	}
}

func TestRunDoesNotExecuteLaterToolCallsAfterCancellation(t *testing.T) {
	started := make(chan struct{})
	waitTool := &recordingTool{name: "wait"}
	waitTool.execute = func(ctx context.Context, _ json.RawMessage) tool.Result {
		close(started)
		<-ctx.Done()
		return tool.Result{Content: "first canceled", IsError: true}
	}
	laterTool := &recordingTool{name: "later"}
	laterTool.execute = func(_ context.Context, _ json.RawMessage) tool.Result {
		laterTool.calls = append(laterTool.calls, "executed")
		return tool.Result{Content: "unexpected execution"}
	}
	registry, err := toolpkgNewRegistry(waitTool, laterTool)
	if err != nil {
		t.Fatal(err)
	}
	fakeProvider := &scriptedProvider{scripts: []providerScript{{
		response: provider.Response{
			Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{
				{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "wait", Arguments: json.RawMessage(`{}`)},
				{Type: model.BlockToolCall, ToolCallID: "call-2", ToolName: "later", Arguments: json.RawMessage(`{}`)},
			}},
		},
	}}}
	memory := session.NewMemory(testHeader(t))
	runner := New(fakeProvider, registry, memory, Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	ctx, cancel := context.WithCancel(context.Background())
	errCh := make(chan error, 1)
	go func() { errCh <- runner.Run(ctx, "prompt", nil) }()
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("first tool did not start")
	}
	cancel()
	if err := <-errCh; !errors.Is(err, context.Canceled) {
		t.Fatalf("Run() error = %v, want %v", err, context.Canceled)
	}
	if len(laterTool.calls) != 0 {
		t.Fatalf("later tool calls = %v, want no execution after cancellation", laterTool.calls)
	}
	messages := memory.Messages()
	if len(messages) != 4 || messages[2].Blocks[0].ToolCallID != "call-1" || messages[3].Blocks[0].ToolCallID != "call-2" {
		t.Fatalf("messages = %#v, want durable results for both tool calls", messages)
	}
	laterResult := messages[3].Blocks[0]
	if !laterResult.IsError || !strings.Contains(laterResult.Text, "context canceled") {
		t.Fatalf("later cancellation result = %#v", laterResult)
	}
}

func TestRunPreservesProviderCancellation(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{{err: context.Canceled}}}
	registry, err := tool.NewRegistry()
	if err != nil {
		t.Fatal(err)
	}
	runner := New(fakeProvider, registry, session.NewMemory(testHeader(t)), Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	var gotErr error
	err = runner.Run(context.Background(), "inspect", func(event Event) {
		if event.Type == EventAgentError {
			gotErr = event.Err
		}
	})
	if err != context.Canceled {
		t.Fatalf("Run() error = %v, want %v", err, context.Canceled)
	}
	if gotErr != context.Canceled {
		t.Fatalf("error event = %v, want %v", gotErr, context.Canceled)
	}
}

func TestRunBashCapCannotSynthesizeSecretAcrossAgentProviderAndPiSession(t *testing.T) {
	workspaceRoot := t.TempDir()
	workspace, err := tool.NewWorkspace(workspaceRoot)
	if err != nil {
		t.Fatal(err)
	}
	var printableASCII strings.Builder
	for value := byte(0x20); value <= 0x7e; value++ {
		printableASCII.WriteByte(value)
	}
	secrets := []string{"TOKEN", "prefix�", printableASCII.String()}
	executor := &fixedOutputExecutor{stdout: []byte("prefixTOKEN")}
	bash, err := tool.NewBashTool(
		workspace,
		executor,
		"/bin/sh",
		[]string{},
		time.Second,
		len("prefix")+1,
		secrets,
	)
	if err != nil {
		t.Fatal(err)
	}
	registry, err := tool.NewRegistry(bash)
	if err != nil {
		t.Fatal(err)
	}
	store, err := session.Create(t.TempDir(), session.Header{Version: 1, ID: "utf8-session", Workspace: workspaceRoot, Provider: "openai-compatible", Model: "test", CreatedAt: fixedClock()})
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()

	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "bash", Arguments: json.RawMessage(`{"command":"ignored"}`)}}}}},
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}}},
	}}
	runner := New(fakeProvider, registry, store, Options{Model: "test", Now: fixedClock, NewID: fixedIDs()}, NewRedactor(secrets))
	var events []Event
	if err := runner.Run(context.Background(), "check", func(event Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}
	persisted, err := os.ReadFile(store.Path())
	if err != nil {
		t.Fatal(err)
	}
	boundaries := map[string]any{
		"events":             events,
		"provider follow-up": fakeProvider.requests[1],
		"session messages":   store.Messages(),
	}
	for name, value := range boundaries {
		encoded, marshalErr := json.Marshal(value)
		if marshalErr != nil {
			t.Fatal(marshalErr)
		}
		if !utf8.Valid(encoded) || strings.Contains(string(encoded), "prefix�") {
			t.Fatalf("%s synthesized a configured secret: %s", name, encoded)
		}
	}
	if !utf8.Valid(persisted) || strings.Contains(string(persisted), "prefix�") || strings.Contains(string(persisted), `prefix\ufffd`) {
		t.Fatalf("Pi JSONL synthesized a configured secret: %q", persisted)
	}
}

func TestRunCanonicalizesInvalidConfiguredSecretsAcrossEventsFollowUpErrorsAndPi(t *testing.T) {
	invalidFF := string(append([]byte("decoded-prefix"), 0xff))
	invalidOverlong := string(append([]byte("overlong-prefix"), 0xc0, 0xaf))
	canonical := []string{"decoded-prefix�", "overlong-prefix��"}

	streamBytes := append([]byte("stream "), []byte(invalidFF)...)
	streamBytes = append(streamBytes, []byte(" | ")...)
	streamBytes = append(streamBytes, []byte(invalidOverlong)...)
	stream := make([]provider.StreamEvent, 0, len(streamBytes))
	for _, value := range streamBytes {
		stream = append(stream, provider.StreamEvent{Type: provider.StreamTextDelta, Text: string([]byte{value})})
	}

	recorder := &recordingTool{name: "echo"}
	recorder.execute = func(context.Context, json.RawMessage) tool.Result {
		return tool.Result{Content: "tool " + strings.Join(canonical, " | ")}
	}
	registry, err := toolpkgNewRegistry(recorder)
	if err != nil {
		t.Fatal(err)
	}
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{
			stream: stream,
			response: provider.Response{
				Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{
					{Type: model.BlockText, Text: "assistant " + strings.Join(canonical, " | ")},
					{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{}`)},
				}},
			},
		},
		{err: errors.New("provider " + strings.Join(canonical, " | "))},
	}}
	store, err := session.Create(t.TempDir(), session.Header{Version: 1, ID: "invalid-configured-secret", Workspace: t.TempDir(), Provider: "openai-compatible", Model: "test", CreatedAt: fixedClock()})
	if err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	runner := New(fakeProvider, registry, store, Options{Model: "test", Now: fixedClock, NewID: fixedIDs()}, NewRedactor([]string{invalidFF, invalidOverlong}))
	var events []Event
	runErr := runner.Run(context.Background(), "inspect", func(event Event) { events = append(events, event) })
	if runErr == nil {
		_ = store.Close()
		t.Fatal("Run() unexpectedly succeeded")
	}
	messages := store.Messages()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	persisted, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if len(fakeProvider.requests) != 2 {
		t.Fatalf("provider requests = %d, want follow-up request", len(fakeProvider.requests))
	}

	boundaries := map[string]any{
		"events":             events,
		"provider follow-up": fakeProvider.requests[1],
		"messages":           messages,
		"error":              runErr.Error(),
	}
	for name, value := range boundaries {
		encoded, marshalErr := json.Marshal(value)
		if marshalErr != nil {
			t.Fatal(marshalErr)
		}
		if !utf8.Valid(encoded) {
			t.Fatalf("%s returned invalid UTF-8: %q", name, encoded)
		}
		for _, secret := range canonical {
			if strings.Contains(string(encoded), secret) {
				t.Fatalf("%s retained canonical configured value %q: %s", name, secret, encoded)
			}
		}
	}
	for _, secret := range canonical {
		if strings.Contains(string(persisted), secret) || strings.Contains(string(persisted), strings.ReplaceAll(secret, "�", `\ufffd`)) {
			t.Fatalf("Pi JSONL retained canonical configured value %q: %q", secret, persisted)
		}
	}
}

func TestRunExpandsJSONStringEscapeSecretsAcrossEventsFollowUpAndRawPi(t *testing.T) {
	tests := []struct {
		raw     string
		decoded string
	}{
		{raw: `less\u003cthan`, decoded: "less<than"},
		{raw: `\\u003c`, decoded: `\u003c`},
		{raw: `greater\u003Ethan`, decoded: "greater>than"},
		{raw: `amp\u0026ersand`, decoded: "amp&ersand"},
		{raw: `quote\"value`, decoded: `quote"value`},
		{raw: `back\\slash`, decoded: `back\slash`},
		{raw: `slash\/value`, decoded: "slash/value"},
		{raw: `line\nbreak`, decoded: "line\nbreak"},
		{raw: `separator\u2028value`, decoded: "separator\u2028value"},
		{raw: `paragraph\u2029value`, decoded: "paragraph\u2029value"},
	}
	rawSecrets := make([]string, len(tests))
	decodedSecrets := make([]string, len(tests))
	for index, test := range tests {
		rawSecrets[index] = test.raw
		decodedSecrets[index] = test.decoded
	}
	dynamic := strings.Join(decodedSecrets, " | ")

	recorder := &recordingTool{name: "echo"}
	recorder.execute = func(context.Context, json.RawMessage) tool.Result {
		return tool.Result{Content: "tool " + dynamic}
	}
	registry, err := toolpkgNewRegistry(recorder)
	if err != nil {
		t.Fatal(err)
	}
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{
			stream: []provider.StreamEvent{{Type: provider.StreamTextDelta, Text: "event " + dynamic}},
			response: provider.Response{
				Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{
					{Type: model.BlockText, Text: "assistant " + dynamic},
					{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{}`)},
				}},
			},
		},
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}}},
	}}
	store, err := session.Create(t.TempDir(), session.Header{Version: 1, ID: "json-escape-session", Workspace: t.TempDir(), Provider: "openai-compatible", Model: "test", CreatedAt: fixedClock()})
	if err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	runner := New(fakeProvider, registry, store, Options{Model: "test", Now: fixedClock, NewID: fixedIDs()}, NewRedactor(rawSecrets))
	var events []Event
	if err := runner.Run(context.Background(), "user "+dynamic, func(event Event) { events = append(events, event) }); err != nil {
		_ = store.Close()
		t.Fatal(err)
	}
	messages := store.Messages()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	persisted, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if len(fakeProvider.requests) != 2 {
		t.Fatalf("provider requests = %d, want follow-up", len(fakeProvider.requests))
	}

	for name, value := range map[string]any{
		"events":             events,
		"provider first":     fakeProvider.requests[0],
		"provider follow-up": fakeProvider.requests[1],
		"messages":           messages,
	} {
		encoded, marshalErr := json.Marshal(value)
		if marshalErr != nil {
			t.Fatal(marshalErr)
		}
		assertNoJSONStringEscapeSecrets(t, name, string(encoded), tests)
	}
	assertNoJSONStringEscapeSecrets(t, "raw Pi JSONL", string(persisted), tests)
}

func assertNoJSONStringEscapeSecrets(t *testing.T, boundary, content string, secrets []struct {
	raw     string
	decoded string
}) {
	t.Helper()
	for _, secret := range secrets {
		if strings.Contains(content, secret.raw) || strings.Contains(content, secret.decoded) {
			t.Fatalf("%s retained raw/decoded JSON escape secret (%q, %q): %q", boundary, secret.raw, secret.decoded, content)
		}
	}
}

func TestRunUnrepresentableRedactorCallsNoProviderToolOrSessionBoundary(t *testing.T) {
	const (
		deleted     = "<DELETE-ME>"
		synthesized = "leftright"
	)
	allRunes := allNonControlUnicodeRunes(t)
	longPreferred := strings.Repeat(redactionMarker, 1<<20)
	redactor := NewRedactor([]string{allRunes, longPreferred, deleted, synthesized})
	if redactor.marker != "" || redactor.AllowsDynamicContent() {
		t.Fatalf("exhaustive marker fallback = marker %q allows-dynamic %t, want suppression", redactor.marker, redactor.AllowsDynamicContent())
	}

	recorder := &recordingTool{name: "echo"}
	recorder.execute = func(context.Context, json.RawMessage) tool.Result {
		return tool.Result{Content: "left" + deleted + "right"}
	}
	registry, err := toolpkgNewRegistry(recorder)
	if err != nil {
		t.Fatal(err)
	}
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{
			stream: []provider.StreamEvent{
				{Type: provider.StreamTextDelta, Text: "left"},
				{Type: provider.StreamTextDelta, Text: deleted},
				{Type: provider.StreamTextDelta, Text: "right"},
			},
			response: provider.Response{
				Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{
					{Type: model.BlockText, Text: "left" + deleted + "right"},
					{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{}`)},
				}},
			},
		},
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}}},
	}}
	store, err := session.Create(t.TempDir(), session.Header{Version: 1, ID: "deletion-session", Workspace: t.TempDir(), Provider: "openai-compatible", Model: "test", CreatedAt: fixedClock()})
	if err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	runner := New(fakeProvider, registry, store, Options{Model: "test", Now: fixedClock, NewID: fixedIDs()}, redactor)
	var events []Event
	if err := runner.Run(context.Background(), "inspect deletion", func(event Event) { events = append(events, event) }); err != nil {
		_ = store.Close()
		t.Fatal(err)
	}
	messages := store.Messages()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	persisted, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if len(fakeProvider.requests) != 0 || len(recorder.calls) != 0 || len(messages) != 0 {
		t.Fatalf("suppressed run crossed a dynamic boundary: requests=%d tools=%d messages=%#v", len(fakeProvider.requests), len(recorder.calls), messages)
	}
	if len(events) != 2 || events[0].Type != EventAgentStarted || events[1].Type != EventAgentFinished {
		t.Fatalf("suppressed lifecycle events = %#v", events)
	}
	encodedEvents, err := json.Marshal(events)
	if err != nil {
		t.Fatal(err)
	}
	for name, content := range map[string]string{"events": string(encodedEvents), "Pi JSONL": string(persisted)} {
		if len(content) > 64<<10 {
			t.Fatalf("%s expanded to %d bytes in suppression mode", name, len(content))
		}
		for _, credential := range []string{deleted, synthesized, longPreferred} {
			if strings.Contains(content, credential) {
				t.Fatalf("%s retained configured credential %q", name, credential)
			}
		}
	}
}

func TestRunIncompleteRedactionSnapshotSuppressesRequestsEventsAndPi(t *testing.T) {
	const omitted = "omitted-513th-environment-value"
	recorder := &recordingTool{name: "echo"}
	registry, err := toolpkgNewRegistry(recorder)
	if err != nil {
		t.Fatal(err)
	}
	fakeProvider := &scriptedProvider{scripts: []providerScript{{
		stream: []provider.StreamEvent{{Type: provider.StreamTextDelta, Text: "provider " + omitted}},
		response: provider.Response{
			Message: model.Message{FinishReason: model.FinishToolCalls, Usage: &model.Usage{InputTokens: 424_242, OutputTokens: 313_131, CachedInputTokens: 212_121}, ID: "response-" + omitted, Role: model.RoleAssistant, Blocks: []model.Block{
				{Type: model.BlockText, Text: "provider " + omitted},
				{Type: model.BlockToolCall, ToolName: "echo", ToolCallID: "call-" + omitted, Arguments: json.RawMessage(`{"value":"` + omitted + `"}`)},
			}},
		},
	}}}
	store, err := session.Create(t.TempDir(), session.Header{Version: 1, ID: "incomplete-redaction", Workspace: t.TempDir(), Provider: "openai-compatible", Model: "safe", CreatedAt: fixedClock()})
	if err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	runner := New(fakeProvider, registry, store, Options{
		Model: omitted, SystemPrompt: "safe fixed system prompt", Thinking: omitted,
		Now: fixedClock, NewID: fixedIDs(),
	}, NewRedactorWithCompleteness(nil, false))
	var events []Event
	if err := runner.Run(context.Background(), "user "+omitted, func(event Event) { events = append(events, event) }); err != nil {
		_ = store.Close()
		t.Fatal(err)
	}
	messages := store.Messages()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	persisted, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if len(fakeProvider.requests) != 0 {
		t.Fatalf("provider requests = %d, want 0", len(fakeProvider.requests))
	}
	if len(messages) != 0 || len(recorder.calls) != 0 {
		t.Fatalf("incomplete redaction mutated session or executed tool: messages=%#v calls=%#v", messages, recorder.calls)
	}
	if len(events) != 2 || events[0].Type != EventAgentStarted || events[1].Type != EventAgentFinished {
		t.Fatalf("events = %#v, want fixed lifecycle only", events)
	}
	for name, value := range map[string]any{
		"events":   events,
		"messages": messages,
	} {
		encoded, marshalErr := json.Marshal(value)
		if marshalErr != nil {
			t.Fatal(marshalErr)
		}
		if strings.Contains(string(encoded), omitted) {
			t.Fatalf("%s retained omitted environment value: %s", name, encoded)
		}
	}
	if strings.Contains(string(persisted), omitted) {
		t.Fatalf("Pi JSONL retained omitted environment value: %q", persisted)
	}
}

func TestRunIncompleteRedactionPreservesCancellationWithoutProviderOrSessionMutation(t *testing.T) {
	memory := session.NewMemory(testHeader(t))
	fakeProvider := &scriptedProvider{scripts: []providerScript{{response: provider.Response{
		Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant},
	}}}}
	runner := New(fakeProvider, nil, memory, Options{Model: "test", Now: fixedClock, NewID: fixedIDs()}, NewRedactorWithCompleteness(nil, false))
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	var events []Event
	err := runner.Run(ctx, "safe prompt", func(event Event) { events = append(events, event) })
	if !errors.Is(err, context.Canceled) || len(fakeProvider.requests) != 0 || len(memory.Messages()) != 0 {
		t.Fatalf("Run() error=%v provider calls=%d messages=%#v", err, len(fakeProvider.requests), memory.Messages())
	}
	if len(events) != 2 || events[0].Type != EventAgentStarted || events[1].Type != EventAgentError {
		t.Fatalf("events=%#v", events)
	}
}

func TestRunIncompleteRedactionDoesNotObserveProviderError(t *testing.T) {
	const omitted = "omitted-provider-error-value"
	memory := session.NewMemory(testHeader(t))
	fakeProvider := &scriptedProvider{scripts: []providerScript{{err: errors.New("provider exposed " + omitted)}}}
	runner := New(fakeProvider, nil, memory, Options{Model: omitted, Now: fixedClock, NewID: fixedIDs()}, NewRedactorWithCompleteness(nil, false))

	var events []Event
	err := runner.Run(context.Background(), "user "+omitted, func(event Event) { events = append(events, event) })
	if err != nil || len(fakeProvider.requests) != 0 || len(memory.Messages()) != 0 {
		t.Fatalf("Run() error=%v provider calls=%d messages=%#v", err, len(fakeProvider.requests), memory.Messages())
	}
	if len(events) != 2 || events[0].Type != EventAgentStarted || events[1].Type != EventAgentFinished {
		t.Fatalf("events=%#v", events)
	}
	encoded, marshalErr := json.Marshal(events)
	if marshalErr != nil {
		t.Fatal(marshalErr)
	}
	if strings.Contains(string(encoded), omitted) {
		t.Fatalf("events retained omitted value: %s", encoded)
	}
}

func TestRunPersistsPlaceholderButShowsFullContentToProviderWithinTheSameTurn(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "recorder", Arguments: json.RawMessage(`{}`)}}}}},
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}}},
	}}
	recorder := &recordingTool{name: "recorder", execute: func(context.Context, json.RawMessage) tool.Result {
		persisted := "3 records (ids omitted)"
		return tool.Result{Content: "3 records: id1, id2, id3", PersistedContent: &persisted}
	}}
	registry, err := tool.NewRegistry(recorder)
	if err != nil {
		t.Fatal(err)
	}
	memory := session.NewMemory(testHeader(t))
	runner := New(fakeProvider, registry, memory, Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	var finished Event
	if err := runner.Run(context.Background(), "inspect", func(event Event) {
		if event.Type == EventToolCallFinished {
			finished = event
		}
	}); err != nil {
		t.Fatal(err)
	}

	if finished.ToolResult.Content != "3 records: id1, id2, id3" {
		t.Fatalf("live event content = %q, want full content", finished.ToolResult.Content)
	}
	messages := memory.Messages()
	if got := messages[2].Blocks[0].Text; got != "3 records (ids omitted)" {
		t.Fatalf("persisted tool result = %q, want placeholder", got)
	}
	// The provider request built after the tool ran, within this same turn,
	// must see the full content the model needs to act on (e.g. to build a
	// valid forget/remember follow-up call) — only the durable session and
	// resumed history get the placeholder.
	if got := fakeProvider.requests[1].Messages[2].Blocks[0].Text; got != "3 records: id1, id2, id3" {
		t.Fatalf("provider history tool result = %q, want full content", got)
	}
}

func TestRunToolResultOverlayDoesNotPersistAcrossTurns(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "recorder", Arguments: json.RawMessage(`{}`)}}}}},
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}}},
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "ok"}}}}},
	}}
	recorder := &recordingTool{name: "recorder", execute: func(context.Context, json.RawMessage) tool.Result {
		persisted := "3 records (ids omitted)"
		return tool.Result{Content: "3 records: id1, id2, id3", PersistedContent: &persisted}
	}}
	registry, err := tool.NewRegistry(recorder)
	if err != nil {
		t.Fatal(err)
	}
	memory := session.NewMemory(testHeader(t))
	runner := New(fakeProvider, registry, memory, Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	if err := runner.Run(context.Background(), "inspect", func(Event) {}); err != nil {
		t.Fatal(err)
	}
	if err := runner.Run(context.Background(), "follow up", func(Event) {}); err != nil {
		t.Fatal(err)
	}

	// The second turn's request replays the first turn's tool result out of
	// session history; runDispatchState is fresh per Run() call, so the
	// overlay from turn one must not leak in and the model sees the
	// placeholder, same as what was actually persisted.
	if got := fakeProvider.requests[2].Messages[2].Blocks[0].Text; got != "3 records (ids omitted)" {
		t.Fatalf("second-turn provider history tool result = %q, want placeholder (overlay must not survive across turns)", got)
	}
}

func TestRunPreservesEmptyPersistedOverrideAndSameTurnOverlay(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "recorder", Arguments: json.RawMessage(`{}`)}}}}},
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}}},
	}}
	recorder := &recordingTool{name: "recorder", execute: func(context.Context, json.RawMessage) tool.Result {
		persisted := ""
		return tool.Result{Content: "full tool result", PersistedContent: &persisted}
	}}
	registry, err := tool.NewRegistry(recorder)
	if err != nil {
		t.Fatal(err)
	}
	root := t.TempDir()
	store, err := session.Create(root, testHeader(t))
	if err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	runner := New(fakeProvider, registry, store, Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})
	if err := runner.Run(context.Background(), "inspect", nil); err != nil {
		t.Fatal(err)
	}
	if got := store.Messages()[2].Blocks[0].Text; got != "" {
		t.Fatalf("persisted tool result = %q, want explicit empty override", got)
	}
	if got := fakeProvider.requests[1].Messages[2].Blocks[0].Text; got != "full tool result" {
		t.Fatalf("same-turn provider history = %q, want full content", got)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	reopened, _, err := session.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	if got := reopened.Messages()[2].Blocks[0].Text; got != "" {
		t.Fatalf("reopened tool result = %q, want explicit empty override", got)
	}
}

func TestRunRedactsCredentialFromToolEventPersistenceAndProviderHistory(t *testing.T) {
	credential := fmt.Sprintf("credential-%d", time.Now().UnixNano())
	workspaceRoot := t.TempDir()
	midpoint := len(credential) / 2
	if err := os.WriteFile(filepath.Join(workspaceRoot, ".part-1"), []byte(credential[:midpoint]), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(workspaceRoot, ".part-2"), []byte(credential[midpoint:]), 0o600); err != nil {
		t.Fatal(err)
	}
	workspace, err := tool.NewWorkspace(workspaceRoot)
	if err != nil {
		t.Fatal(err)
	}
	driver := direct.New()
	executor, err := sandbox.NewExecutor(driver, sandbox.Policy{
		Filesystem: sandbox.FilesystemUnconfined,
		Network:    sandbox.NetworkAllow,
	}, workspaceRoot)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if err := executor.Close(); err != nil {
			t.Errorf("Executor.Close() error = %v", err)
		}
	})
	bash, err := tool.NewBashTool(
		workspace,
		executor,
		"/bin/sh",
		[]string{"HOME=", "PATH=/usr/bin:/bin", "LC_ALL=C"},
		time.Second,
		51200,
		[]string{credential},
	)
	if err != nil {
		t.Fatal(err)
	}
	registry, err := tool.NewRegistry(bash)
	if err != nil {
		t.Fatal(err)
	}
	store, err := session.Create(t.TempDir(), session.Header{Version: 1, ID: "secret-session", Workspace: workspaceRoot, Provider: "openai-compatible", Model: "test", CreatedAt: fixedClock()})
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()

	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "bash", Arguments: json.RawMessage(`{"command":"cat .part-1 .part-2"}`)}}}}},
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}}},
	}}
	runner := New(fakeProvider, registry, store, Options{Model: "test", Now: fixedClock, NewID: fixedIDs()})
	var toolEvent Event
	if err := runner.Run(context.Background(), "check", func(event Event) {
		if event.Type == EventToolCallFinished {
			toolEvent = event
		}
	}); err != nil {
		t.Fatal(err)
	}

	persisted, err := os.ReadFile(store.Path())
	if err != nil {
		t.Fatal(err)
	}
	for location, content := range map[string]string{
		"tool event": toolEvent.ToolResult.Content,
		"session":    string(persisted),
		"next request": func() string {
			encoded, marshalErr := json.Marshal(fakeProvider.requests[1])
			if marshalErr != nil {
				t.Fatal(marshalErr)
			}
			return string(encoded)
		}(),
	} {
		hasMarker := strings.Contains(content, "[REDACTED]") || strings.Contains(content, "stdout:\n"+redactionMarker+"\n") ||
			strings.Contains(content, `stdout:\n`+redactionMarker+`\n`)
		if strings.Contains(content, credential) || !hasMarker {
			t.Fatalf("%s did not safely redact credential: %q", location, content)
		}
	}
}

func TestRunRedactsProviderTextArgumentsAndToolResultsAtAgentBoundary(t *testing.T) {
	credential := fmt.Sprintf("agent-boundary-secret-%d", time.Now().UnixNano())
	recorder := &recordingTool{name: "echo"}
	recorder.execute = func(_ context.Context, arguments json.RawMessage) tool.Result {
		recorder.calls = append(recorder.calls, string(arguments))
		persisted := "persisted " + credential
		return tool.Result{Content: "tool returned " + credential, PersistedContent: &persisted}
	}
	registry, err := toolpkgNewRegistry(recorder)
	if err != nil {
		t.Fatal(err)
	}
	arguments := json.RawMessage(fmt.Sprintf(`{%q:"echo","value":%q,"nested":{%q:%q},"duplicates":{"safe":"first","safe":"attacker-exact","a":"first","\u0061":"attacker-alias","secret-\ud800":"first","secret-\ud801":"attacker-surrogate"},"collision":{%q:"first",%q:"attacker-redacted"}}`, credential, credential, "prefix-"+credential, "Bearer "+credential, credential, redactionMarker))
	stream := []provider.StreamEvent{{Type: provider.StreamTextDelta, Text: "text "}}
	for _, character := range credential {
		stream = append(stream, provider.StreamEvent{Type: provider.StreamTextDelta, Text: string(character)})
	}
	stream = append(stream, provider.StreamEvent{Type: provider.StreamTextDelta, Text: " done"})
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{
			stream: stream,
			response: provider.Response{
				Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{
					{Type: model.BlockText, Text: "text " + credential + " done"},
					{Type: model.BlockToolCall, ToolCallID: "call-" + credential, ToolName: "echo", Arguments: arguments},
				}},
			},
		},
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "finished"}}}}},
	}}
	memory := session.NewMemory(testHeader(t))
	redactor := NewRedactor([]string{credential})
	runner := New(fakeProvider, registry, memory, Options{Model: "test", Now: fixedClock, NewID: fixedIDs()}, redactor)

	var events []Event
	if err := runner.Run(context.Background(), "inspect", func(event Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}
	if len(recorder.calls) != 1 || strings.Contains(recorder.calls[0], credential) || strings.Contains(recorder.calls[0], "attacker-") || !strings.Contains(recorder.calls[0], redactor.marker) || !json.Valid([]byte(recorder.calls[0])) {
		t.Fatalf("executed arguments = %#v", recorder.calls)
	}
	for index, event := range events {
		encoded, err := json.Marshal(event)
		if err != nil {
			t.Fatal(err)
		}
		if strings.Contains(string(encoded), credential) || strings.Contains(string(encoded), "attacker-") {
			t.Fatalf("event %d leaked credential or colliding value: %s", index, encoded)
		}
	}
	for location, value := range map[string]any{
		"messages":       memory.Messages(),
		"follow-up":      fakeProvider.requests[1],
		"tool execution": recorder.calls,
	} {
		encoded, err := json.Marshal(value)
		if err != nil {
			t.Fatal(err)
		}
		if strings.Contains(string(encoded), credential) || strings.Contains(string(encoded), "attacker-") || !strings.Contains(string(encoded), redactor.marker) {
			t.Fatalf("%s was not redacted or retained a colliding value: %s", location, encoded)
		}
	}
}

func TestRunClassifiesFatalPersistenceAtEveryDurableBoundary(t *testing.T) {
	for _, test := range []struct {
		name          string
		failRole      model.Role
		wantBoundary  string
		wantProviders int
		wantTools     int
	}{
		{name: "user append", failRole: model.RoleUser, wantBoundary: "persist user message", wantProviders: 0, wantTools: 0},
		{name: "assistant append", failRole: model.RoleAssistant, wantBoundary: "persist assistant message", wantProviders: 1, wantTools: 0},
		{name: "tool append", failRole: model.RoleTool, wantBoundary: "persist tool result", wantProviders: 1, wantTools: 1},
	} {
		t.Run(test.name, func(t *testing.T) {
			persistErr := fmt.Errorf("%w: injected session failure", session.ErrFatalPersistence)
			memory := &interceptSession{base: session.NewMemory(testHeader(t))}
			memory.fail = func(_ int, message model.Message) error {
				if message.Role == test.failRole {
					return persistErr
				}
				return nil
			}
			recorder := &recordingTool{name: "echo"}
			recorder.execute = func(context.Context, json.RawMessage) tool.Result {
				recorder.calls = append(recorder.calls, "called")
				return tool.Result{Content: "hello"}
			}
			registry, err := toolpkgNewRegistry(recorder)
			if err != nil {
				t.Fatal(err)
			}
			fakeProvider := &scriptedProvider{scripts: []providerScript{
				{response: provider.Response{Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{"value":"hello"}`)}}}}},
				{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "must not run"}}}}},
			}}
			runner := New(fakeProvider, registry, memory, Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

			runErr := runner.Run(context.Background(), "inspect", nil)
			if !errors.Is(runErr, session.ErrFatalPersistence) || !strings.Contains(runErr.Error(), test.wantBoundary) {
				t.Fatalf("Run() error = %v, want fatal persistence at %q", runErr, test.wantBoundary)
			}
			if len(fakeProvider.requests) != test.wantProviders {
				t.Fatalf("provider calls = %d, want %d", len(fakeProvider.requests), test.wantProviders)
			}
			if len(recorder.calls) != test.wantTools {
				t.Fatalf("tool calls = %d, want %d", len(recorder.calls), test.wantTools)
			}
		})
	}
}

func TestRunStopsWhenUserMessagePersistenceFails(t *testing.T) {
	persistErr := errors.New("persist user")
	memory := &interceptSession{base: session.NewMemory(testHeader(t))}
	memory.fail = func(count int, _ model.Message) error {
		if count == 1 {
			return persistErr
		}
		return nil
	}
	fakeProvider := &scriptedProvider{}
	registry, err := tool.NewRegistry()
	if err != nil {
		t.Fatal(err)
	}
	runner := New(fakeProvider, registry, memory, Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	var got []EventType
	err = runner.Run(context.Background(), "inspect", func(event Event) { got = append(got, event.Type) })
	if !errors.Is(err, persistErr) {
		t.Fatalf("Run() error = %v, want %v", err, persistErr)
	}
	if len(fakeProvider.requests) != 0 {
		t.Fatalf("provider calls = %d, want 0", len(fakeProvider.requests))
	}
	if len(memory.Messages()) != 0 {
		t.Fatalf("unexpected persisted messages: %#v", memory.Messages())
	}
	if len(got) < 2 || got[0] != EventAgentStarted || got[len(got)-1] != EventAgentError {
		t.Fatalf("events = %v, want started...error", got)
	}
}

func TestRunPersistsAssistantBeforeExecutingTools(t *testing.T) {
	persistErr := errors.New("persist assistant")
	memory := &interceptSession{base: session.NewMemory(testHeader(t))}
	memory.fail = func(_ int, message model.Message) error {
		if message.Role == model.RoleAssistant {
			return persistErr
		}
		return nil
	}
	recorder := &recordingTool{name: "echo"}
	recorder.execute = func(context.Context, json.RawMessage) tool.Result {
		recorder.calls = append(recorder.calls, "called")
		return tool.Result{Content: "hello"}
	}
	registry, err := toolpkgNewRegistry(recorder)
	if err != nil {
		t.Fatal(err)
	}
	fakeProvider := &scriptedProvider{scripts: []providerScript{{
		response: provider.Response{
			Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{"value":"hello"}`)}}},
		},
	}}}
	runner := New(fakeProvider, registry, memory, Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	err = runner.Run(context.Background(), "inspect", nil)
	if !errors.Is(err, persistErr) {
		t.Fatalf("Run() error = %v, want %v", err, persistErr)
	}
	if len(recorder.calls) != 0 {
		t.Fatalf("tool executed before assistant persistence: %v", recorder.calls)
	}
	if got := memory.Messages(); len(got) != 1 || got[0].Role != model.RoleUser {
		t.Fatalf("unexpected persisted messages: %#v", got)
	}
}

func TestRunPersistsToolResultsBeforeNextProviderCall(t *testing.T) {
	persistErr := errors.New("persist tool")
	memory := &interceptSession{base: session.NewMemory(testHeader(t))}
	memory.fail = func(_ int, message model.Message) error {
		if message.Role == model.RoleTool {
			return persistErr
		}
		return nil
	}
	recorder := &recordingTool{name: "echo"}
	recorder.execute = func(context.Context, json.RawMessage) tool.Result {
		recorder.calls = append(recorder.calls, "called")
		return tool.Result{Content: "hello"}
	}
	registry, err := toolpkgNewRegistry(recorder)
	if err != nil {
		t.Fatal(err)
	}
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{
			response: provider.Response{
				Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{"value":"hello"}`)}}},
			},
		},
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}}},
	}}
	runner := New(fakeProvider, registry, memory, Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	err = runner.Run(context.Background(), "inspect", nil)
	if !errors.Is(err, persistErr) {
		t.Fatalf("Run() error = %v, want %v", err, persistErr)
	}
	if len(recorder.calls) != 1 {
		t.Fatalf("tool calls = %v, want 1 call", recorder.calls)
	}
	if len(fakeProvider.requests) != 1 {
		t.Fatalf("provider calls = %d, want 1", len(fakeProvider.requests))
	}
	if got := memory.Messages(); len(got) != 2 || got[1].Role != model.RoleAssistant {
		t.Fatalf("unexpected persisted messages: %#v", got)
	}
}

func TestRunRejectsEmptyUserText(t *testing.T) {
	registry, err := tool.NewRegistry()
	if err != nil {
		t.Fatal(err)
	}
	fakeProvider := &scriptedProvider{}
	runner := New(fakeProvider, registry, session.NewMemory(testHeader(t)), Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	if err := runner.Run(context.Background(), "   ", nil); err == nil {
		t.Fatal("expected empty user text to fail")
	}
	if len(fakeProvider.requests) != 0 {
		t.Fatalf("provider calls = %d, want 0", len(fakeProvider.requests))
	}
}

func TestRunEmitsErrorForEmptyUserText(t *testing.T) {
	registry, err := tool.NewRegistry()
	if err != nil {
		t.Fatal(err)
	}
	fakeProvider := &scriptedProvider{}
	runner := New(fakeProvider, registry, session.NewMemory(testHeader(t)), Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	var gotErr error
	var events []Event
	err = runner.Run(context.Background(), "   ", func(event Event) {
		events = append(events, event)
		if event.Type == EventAgentError {
			gotErr = event.Err
		}
	})
	if err != ErrEmptyUserText {
		t.Fatalf("Run() error = %v, want %v", err, ErrEmptyUserText)
	}
	if gotErr != ErrEmptyUserText {
		t.Fatalf("error event = %v, want %v", gotErr, ErrEmptyUserText)
	}
	if len(fakeProvider.requests) != 0 {
		t.Fatalf("provider calls = %d, want 0", len(fakeProvider.requests))
	}
	if got := eventTypes(events); len(got) != 1 || got[0] != EventAgentError {
		t.Fatalf("events = %v, want only error", got)
	}
}

type fixedOutputExecutor struct {
	stdout []byte
	stderr []byte
}

func (e *fixedOutputExecutor) Execute(_ context.Context, _ sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
	if _, err := streams.Stdout.Write(e.stdout); err != nil {
		return sandbox.ExitStatus{}, err
	}
	if _, err := streams.Stderr.Write(e.stderr); err != nil {
		return sandbox.ExitStatus{}, err
	}
	return sandbox.ExitStatus{Code: 0}, nil
}

type providerScript struct {
	response provider.Response
	err      error
	stream   []provider.StreamEvent
}

type scriptedProvider struct {
	scripts  []providerScript
	requests []provider.Request
}

func (p *scriptedProvider) Complete(_ context.Context, request provider.Request, emit func(provider.StreamEvent)) (provider.Response, error) {
	p.requests = append(p.requests, testCloneRequest(request))
	if len(p.scripts) == 0 {
		return provider.Response{}, errors.New("no scripted response")
	}
	script := p.scripts[0]
	p.scripts = p.scripts[1:]
	stream := script.stream
	if len(stream) == 0 {
		for _, block := range script.response.Message.Blocks {
			if block.Type == model.BlockText && block.Text != "" {
				stream = append(stream, provider.StreamEvent{Type: provider.StreamTextDelta, Text: block.Text})
			}
		}
	}
	for _, event := range stream {
		emit(event)
	}
	if script.err != nil {
		return provider.Response{}, script.err
	}
	return testCloneResponse(script.response), nil
}

type echoTool struct{}

func (echoTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{Name: "echo", Parameters: map[string]any{"type": "object", "required": []string{"value"}}}
}

func (echoTool) Execute(_ context.Context, arguments json.RawMessage) tool.Result {
	var args struct {
		Value string `json:"value"`
	}
	if err := json.Unmarshal(arguments, &args); err != nil {
		return tool.Result{Content: err.Error(), IsError: true}
	}
	return tool.Result{Content: args.Value}
}

type recordingTool struct {
	name    string
	calls   []string
	execute func(context.Context, json.RawMessage) tool.Result
}

func (t *recordingTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{Name: t.name, Parameters: map[string]any{"type": "object"}}
}

func (t *recordingTool) Execute(ctx context.Context, arguments json.RawMessage) tool.Result {
	return t.execute(ctx, arguments)
}

type interceptSession struct {
	base  session.Session
	count int
	fail  func(int, model.Message) error
}

func (s *interceptSession) Header() session.Header { return s.base.Header() }

func (s *interceptSession) Messages() []model.Message { return s.base.Messages() }

func (s *interceptSession) Append(ctx context.Context, message model.Message) error {
	s.count++
	if s.fail != nil {
		if err := s.fail(s.count, message); err != nil {
			return err
		}
	}
	return s.base.Append(ctx, message)
}

func (s *interceptSession) AppendCompaction(ctx context.Context, checkpoint session.CompactionCheckpoint) (session.CompactionMetadata, error) {
	return s.base.AppendCompaction(ctx, checkpoint)
}

func (s *interceptSession) LatestCompaction() (session.CompactionMetadata, bool) {
	return s.base.LatestCompaction()
}

func (s *interceptSession) Path() string { return s.base.Path() }

func (s *interceptSession) Close() error { return s.base.Close() }

func fixedClock() time.Time {
	return time.Unix(10, 0).UTC()
}

func fixedIDs() func() string {
	next := 0
	return func() string {
		next++
		return fmt.Sprintf("id-%d", next)
	}
}

func testHeader(t *testing.T) session.Header {
	t.Helper()
	return session.Header{Version: 1, ID: "test", Workspace: t.TempDir(), Provider: "test", Model: "test", CreatedAt: fixedClock()}
}

func eventTypes(events []Event) []EventType {
	types := make([]EventType, len(events))
	for i, event := range events {
		types[i] = event.Type
	}
	return types
}

func testCloneRequest(request provider.Request) provider.Request {
	cloned := request
	cloned.Messages = testCloneMessages(request.Messages)
	if request.Tools != nil {
		cloned.Tools = make([]model.ToolDefinition, len(request.Tools))
		copy(cloned.Tools, request.Tools)
	}
	return cloned
}

func testCloneResponse(response provider.Response) provider.Response {
	cloned := response
	cloned.Message = testCloneMessage(response.Message)
	return cloned
}

func testCloneMessages(messages []model.Message) []model.Message {
	cloned := make([]model.Message, len(messages))
	for i, message := range messages {
		cloned[i] = testCloneMessage(message)
	}
	return cloned
}

func testCloneMessage(message model.Message) model.Message {
	cloned := message
	if message.Blocks != nil {
		cloned.Blocks = make([]model.Block, len(message.Blocks))
		for i, block := range message.Blocks {
			cloned.Blocks[i] = block
			if block.Arguments != nil {
				cloned.Blocks[i].Arguments = append(json.RawMessage(nil), block.Arguments...)
			}
		}
	}
	if message.Usage != nil {
		usage := *message.Usage
		cloned.Usage = &usage
	}
	return cloned
}

func toolpkgNewRegistry(tools ...tool.Tool) (*tool.Registry, error) {
	return tool.NewRegistry(tools...)
}
