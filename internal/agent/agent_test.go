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

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
)

func TestRunExecutesToolAndReturnsToProvider(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{
			response: provider.Response{
				Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{
					{Type: model.BlockText, Text: "checking"},
					{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{"value":"hello"}`)},
				}},
				FinishReason: model.FinishToolCalls,
				Usage:        model.Usage{InputTokens: 11, OutputTokens: 7},
			},
		},
		{
			response: provider.Response{
				Message:      model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}},
				FinishReason: model.FinishStop,
				Usage:        model.Usage{InputTokens: 3, OutputTokens: 2},
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
			Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{
				{Type: model.BlockText, Text: "hello"},
				{Type: model.BlockText, Text: " world"},
			}},
			FinishReason: model.FinishStop,
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

func TestRunEmitsToolCallEvents(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{
			response: provider.Response{
				Message:      model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{"value":"hello"}`)}}},
				FinishReason: model.FinishToolCalls,
			},
		},
		{response: provider.Response{Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}, FinishReason: model.FinishStop}},
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
				Message:      model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "missing", Arguments: json.RawMessage(`{"value":"hello"}`)}}},
				FinishReason: model.FinishToolCalls,
			},
		},
		{response: provider.Response{Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}, FinishReason: model.FinishStop}},
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

func TestRunDelegatesInvalidArgumentsToTool(t *testing.T) {
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
				Message:      model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{`)}}},
				FinishReason: model.FinishToolCalls,
			},
		},
		{response: provider.Response{Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}, FinishReason: model.FinishStop}},
	}}
	memory := session.NewMemory(testHeader(t))
	runner := New(fakeProvider, registry, memory, Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs()})

	if err := runner.Run(context.Background(), "inspect", nil); err != nil {
		t.Fatal(err)
	}
	if got, want := len(recorder.calls), 1; got != want {
		t.Fatalf("tool calls = %d, want %d", got, want)
	}
	if got := memory.Messages()[2].Blocks[0]; !got.IsError || got.Text == "" {
		t.Fatalf("unexpected invalid-argument result: %#v", got)
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
				Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{
					{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{"value":"one"}`)},
					{Type: model.BlockToolCall, ToolCallID: "call-2", ToolName: "echo", Arguments: json.RawMessage(`{"value":"two"}`)},
				}},
				FinishReason: model.FinishToolCalls,
			},
		},
		{response: provider.Response{Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}, FinishReason: model.FinishStop}},
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
				Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{
					{Type: model.BlockToolCall, ToolCallID: fmt.Sprintf("call-%d", i), ToolName: "echo", Arguments: json.RawMessage(`{"value":"x"}`)},
				}},
				FinishReason: model.FinishToolCalls,
			},
		})
	}
	scripts = append(scripts, providerScript{
		response: provider.Response{Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}, FinishReason: model.FinishStop},
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
				Message:      model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{"value":"hello"}`)}}},
				FinishReason: model.FinishToolCalls,
			},
		},
		{
			response: provider.Response{Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}, FinishReason: model.FinishStop},
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
				Message:      model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "wait", Arguments: json.RawMessage(`{}`)}}},
				FinishReason: model.FinishToolCalls,
			},
		},
		{response: provider.Response{Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "recovered"}}}, FinishReason: model.FinishStop}},
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
			Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{
				{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "wait", Arguments: json.RawMessage(`{}`)},
				{Type: model.BlockToolCall, ToolCallID: "call-2", ToolName: "later", Arguments: json.RawMessage(`{}`)},
			}},
			FinishReason: model.FinishToolCalls,
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
	registry, err := tool.NewRegistry(tool.NewBashTool(workspace, "/bin/sh", time.Second, 51200, tool.BashSecurity{RedactValues: []string{credential}}))
	if err != nil {
		t.Fatal(err)
	}
	store, err := session.Create(t.TempDir(), session.Header{Version: 1, ID: "secret-session", Workspace: workspaceRoot, Provider: "openai-compatible", Model: "test", CreatedAt: fixedClock()})
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()

	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{response: provider.Response{Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "bash", Arguments: json.RawMessage(`{"command":"cat .part-1 .part-2"}`)}}}, FinishReason: model.FinishToolCalls}},
		{response: provider.Response{Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}, FinishReason: model.FinishStop}},
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
		if strings.Contains(content, credential) || !strings.Contains(content, "[REDACTED]") {
			t.Fatalf("%s did not safely redact credential: %q", location, content)
		}
	}
}

func TestRunRedactsProviderTextArgumentsAndToolResultsAtAgentBoundary(t *testing.T) {
	credential := fmt.Sprintf("agent-boundary-secret-%d", time.Now().UnixNano())
	recorder := &recordingTool{name: "echo"}
	recorder.execute = func(_ context.Context, arguments json.RawMessage) tool.Result {
		recorder.calls = append(recorder.calls, string(arguments))
		return tool.Result{Content: "tool returned " + credential}
	}
	registry, err := toolpkgNewRegistry(recorder)
	if err != nil {
		t.Fatal(err)
	}
	arguments := json.RawMessage(fmt.Sprintf(`{%q:"echo","value":%q,"nested":{%q:%q},"duplicates":{"safe":"first","safe":"attacker-exact","a":"first","\u0061":"attacker-alias","secret-\ud800":"first","secret-\ud801":"attacker-surrogate"},"collision":{%q:"first","█":"attacker-redacted"}}`, credential, credential, "prefix-"+credential, "Bearer "+credential, credential))
	stream := []provider.StreamEvent{{Type: provider.StreamTextDelta, Text: "text "}}
	for _, character := range credential {
		stream = append(stream, provider.StreamEvent{Type: provider.StreamTextDelta, Text: string(character)})
	}
	stream = append(stream, provider.StreamEvent{Type: provider.StreamTextDelta, Text: " done"})
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{
			stream: stream,
			response: provider.Response{
				Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{
					{Type: model.BlockText, Text: "text " + credential + " done"},
					{Type: model.BlockToolCall, ToolCallID: "call-" + credential, ToolName: "echo", Arguments: arguments},
				}},
				FinishReason: model.FinishToolCalls,
			},
		},
		{response: provider.Response{Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "finished"}}}, FinishReason: model.FinishStop}},
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
				{response: provider.Response{Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{"value":"hello"}`)}}}, FinishReason: model.FinishToolCalls}},
				{response: provider.Response{Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "must not run"}}}, FinishReason: model.FinishStop}},
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
			Message:      model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{"value":"hello"}`)}}},
			FinishReason: model.FinishToolCalls,
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
				Message:      model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{"value":"hello"}`)}}},
				FinishReason: model.FinishToolCalls,
			},
		},
		{response: provider.Response{Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}, FinishReason: model.FinishStop}},
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
	if response.Usage != (model.Usage{}) {
		cloned.Usage = response.Usage
	}
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
