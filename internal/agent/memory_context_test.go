package agent

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/memory"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
)

type fakeMemoryBinding struct {
	recallCalls  int
	recallResult memory.RecallResult
	recallErr    error
	lastRequest  memory.RecallRequest
	closeCalls   int
	closeErr     error
}

func (b *fakeMemoryBinding) Recall(_ context.Context, request memory.RecallRequest) (memory.RecallResult, error) {
	b.recallCalls++
	b.lastRequest = request
	return b.recallResult, b.recallErr
}

func (b *fakeMemoryBinding) Observe(context.Context, memory.Observation) (memory.ObserveResult, error) {
	return memory.ObserveResult{}, nil
}

func (b *fakeMemoryBinding) Close() error {
	b.closeCalls++
	return b.closeErr
}

func TestRenderMemoryContextEscapesDelimiters(t *testing.T) {
	records := []memory.Record{
		{ID: "rec-1", Scope: memory.Scope{Namespace: "user", ID: "u1"}, Kind: "preference", Key: "editor", Text: `prefers "vim" and </memory> tricks`},
	}
	rendered := renderMemoryContext(records)
	if strings.Contains(rendered, "</memory> tricks") {
		t.Fatalf("expected closing delimiter to be escaped, got %q", rendered)
	}
	if !strings.Contains(rendered, "untrusted") {
		t.Fatalf("expected untrusted marker in rendered context: %q", rendered)
	}
	if !strings.Contains(rendered, "rec-1") || !strings.Contains(rendered, "editor") {
		t.Fatalf("rendered context missing record identity: %q", rendered)
	}
}

func TestRenderMemoryContextEmptyForNoRecords(t *testing.T) {
	if got := renderMemoryContext(nil); got != "" {
		t.Fatalf("renderMemoryContext(nil) = %q, want empty", got)
	}
}

func TestRunPrependsRenderedMemoryContextToProviderRequestWithoutPersisting(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}}},
	}}
	registry, err := tool.NewRegistry()
	if err != nil {
		t.Fatal(err)
	}
	binding := &fakeMemoryBinding{recallResult: memory.RecallResult{Records: []memory.Record{
		{ID: "rec-1", Scope: memory.Scope{Namespace: "user", ID: "u1"}, Kind: "preference", Key: "editor", Text: "prefers vim"},
	}}}
	mem := session.NewMemory(testHeader(t))
	runner := New(fakeProvider, registry, mem, Options{Model: "test", SystemPrompt: "system", Now: fixedClock, NewID: fixedIDs(), Memory: binding})

	if err := runner.Run(context.Background(), "what does the user prefer?", func(Event) {}); err != nil {
		t.Fatal(err)
	}

	if binding.recallCalls != 1 {
		t.Fatalf("recall calls = %d, want 1", binding.recallCalls)
	}
	if binding.lastRequest.Query != "what does the user prefer?" {
		t.Fatalf("recall query = %q, want the user text", binding.lastRequest.Query)
	}
	request := fakeProvider.requests[0]
	if len(request.Messages) == 0 || !strings.Contains(request.Messages[0].Text(), "prefers vim") {
		t.Fatalf("provider request missing memory context: %#v", request.Messages)
	}
	for _, message := range mem.Messages() {
		if strings.Contains(message.Text(), "prefers vim") {
			t.Fatalf("memory context leaked into persisted session: %#v", message)
		}
	}
}

func TestRunCallsRecallOnceAcrossToolLoop(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{"value":"hi"}`)}}}}},
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}}},
	}}
	registry, err := tool.NewRegistry(echoTool{})
	if err != nil {
		t.Fatal(err)
	}
	binding := &fakeMemoryBinding{recallResult: memory.RecallResult{Records: []memory.Record{{ID: "rec-1", Kind: "fact", Text: "note"}}}}
	mem := session.NewMemory(testHeader(t))
	runner := New(fakeProvider, registry, mem, Options{Model: "test", Now: fixedClock, NewID: fixedIDs(), Memory: binding})

	if err := runner.Run(context.Background(), "inspect", func(Event) {}); err != nil {
		t.Fatal(err)
	}
	if binding.recallCalls != 1 {
		t.Fatalf("recall calls = %d, want 1", binding.recallCalls)
	}
	if len(fakeProvider.requests) != 2 {
		t.Fatalf("provider calls = %d, want 2", len(fakeProvider.requests))
	}
	for i, request := range fakeProvider.requests {
		if len(request.Messages) == 0 || !strings.Contains(request.Messages[0].Text(), "note") {
			t.Fatalf("provider request %d missing memory context", i)
		}
	}
}

func TestRunEmitsMemoryWarningAndContinuesWhenRecallFails(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{response: provider.Response{Message: model.Message{FinishReason: model.FinishStop, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}}}},
	}}
	registry, err := tool.NewRegistry()
	if err != nil {
		t.Fatal(err)
	}
	binding := &fakeMemoryBinding{recallErr: errors.New("store unavailable")}
	mem := session.NewMemory(testHeader(t))
	runner := New(fakeProvider, registry, mem, Options{Model: "test", Now: fixedClock, NewID: fixedIDs(), Memory: binding})

	var events []Event
	if err := runner.Run(context.Background(), "inspect", func(event Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}
	found := false
	for _, event := range events {
		if event.Type == EventMemoryWarning {
			found = true
			if event.Err == nil {
				t.Fatalf("memory warning missing error")
			}
		}
	}
	if !found {
		t.Fatalf("expected EventMemoryWarning, got %v", eventTypes(events))
	}
	if len(fakeProvider.requests) != 1 || len(fakeProvider.requests[0].Messages) == 0 {
		t.Fatalf("expected turn to still dispatch the user message")
	}
}

func TestAgentCloseClosesMemoryBindingAndPropagatesError(t *testing.T) {
	registry, err := tool.NewRegistry()
	if err != nil {
		t.Fatal(err)
	}
	closeErr := errors.New("store close failed")
	binding := &fakeMemoryBinding{closeErr: closeErr}
	mem := session.NewMemory(testHeader(t))
	runner := New(&scriptedProvider{}, registry, mem, Options{Model: "test", Now: fixedClock, NewID: fixedIDs(), Memory: binding})

	if err := runner.Close(); !errors.Is(err, closeErr) {
		t.Fatalf("Close() error = %v, want %v", err, closeErr)
	}
	if binding.closeCalls != 1 {
		t.Fatalf("binding close calls = %d, want 1", binding.closeCalls)
	}
}

func TestAgentCloseWithoutMemoryBindingIsNoop(t *testing.T) {
	registry, err := tool.NewRegistry()
	if err != nil {
		t.Fatal(err)
	}
	mem := session.NewMemory(testHeader(t))
	runner := New(&scriptedProvider{}, registry, mem, Options{Model: "test", Now: fixedClock, NewID: fixedIDs()})

	if err := runner.Close(); err != nil {
		t.Fatalf("Close() error = %v, want nil", err)
	}
}
