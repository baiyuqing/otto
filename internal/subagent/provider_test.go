package subagent

import (
	"context"
	"encoding/json"
	"fmt"
	"sync"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/tool"
)

// routeStep is one scripted response for a matched route.
type routeStep struct {
	resp provider.Response
	err  error
}

// fakeRoute matches a request by predicate and serves its steps in order;
// once exhausted, the last step repeats.
type fakeRoute struct {
	match func(provider.Request) bool
	steps []routeStep
}

// fakeProvider is a goroutine-safe scripted provider.Provider for tests:
// concurrent children can call Complete at once. Routes are matched in
// registration order; the first match wins.
type fakeProvider struct {
	mu     sync.Mutex
	routes []*fakeRoute
	calls  []provider.Request
	hook   func(context.Context, provider.Request)
}

func newFakeProvider() *fakeProvider {
	return &fakeProvider{}
}

// addRoute registers a route matching every request for which match
// returns true, serving steps in order (the last step repeats once
// exhausted).
func (f *fakeProvider) addRoute(match func(provider.Request) bool, steps ...routeStep) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.routes = append(f.routes, &fakeRoute{match: match, steps: steps})
}

// setHook installs a callback invoked before route resolution on every
// Complete call, with that call's context, so a test can block a child on
// ctx.Done(), count concurrent calls, or record requests.
func (f *fakeProvider) setHook(hook func(context.Context, provider.Request)) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.hook = hook
}

func (f *fakeProvider) requests() []provider.Request {
	f.mu.Lock()
	defer f.mu.Unlock()
	out := make([]provider.Request, len(f.calls))
	copy(out, f.calls)
	return out
}

func (f *fakeProvider) Complete(ctx context.Context, req provider.Request, stream func(provider.StreamEvent)) (provider.Response, error) {
	f.mu.Lock()
	f.calls = append(f.calls, req)
	hook := f.hook
	f.mu.Unlock()

	if hook != nil {
		hook(ctx, req)
	}
	if err := ctx.Err(); err != nil {
		return provider.Response{}, err
	}

	f.mu.Lock()
	var step routeStep
	found := false
	for _, route := range f.routes {
		if !route.match(req) {
			continue
		}
		found = true
		switch {
		case len(route.steps) > 1:
			step = route.steps[0]
			route.steps = route.steps[1:]
		case len(route.steps) == 1:
			step = route.steps[0]
		}
		break
	}
	f.mu.Unlock()
	if !found {
		return provider.Response{}, fmt.Errorf("fakeProvider: no route for request (last user text: %q)", lastUserText(req))
	}

	// Simulate streaming: emit each text block whole, as the agent's
	// EventTextDelta accounting depends on the stream callback, not on the
	// final response's blocks.
	if step.err == nil {
		for _, block := range step.resp.Message.Blocks {
			if block.Type == model.BlockText && block.Text != "" {
				stream(provider.StreamEvent{Type: provider.StreamTextDelta, Text: block.Text})
			}
		}
	}
	return step.resp, step.err
}

// lastUserText returns the text of the last user-role message in req, or
// "" when there is none.
func lastUserText(req provider.Request) string {
	for i := len(req.Messages) - 1; i >= 0; i-- {
		if req.Messages[i].Role == model.RoleUser {
			return req.Messages[i].Text()
		}
	}
	return ""
}

// matchPrompt returns a route predicate matching requests whose last
// user-role message text equals text exactly.
func matchPrompt(text string) func(provider.Request) bool {
	return func(req provider.Request) bool { return lastUserText(req) == text }
}

// matchAny matches every request; use it to build a catch-all default
// route.
func matchAny(provider.Request) bool { return true }

// assistantText builds a provider.Response carrying one assistant text
// block, no tool calls, and the given usage.
func assistantText(text string, usage model.Usage) provider.Response {
	return provider.Response{
		Message: model.Message{FinishReason: model.FinishStop, Usage: &usage,
			Role:   model.RoleAssistant,
			Blocks: []model.Block{{Type: model.BlockText, Text: text}},
		},
	}
}

// assistantToolCall builds a provider.Response carrying one tool call
// block and no text, so the agent loop executes the call and makes a
// further provider request.
func assistantToolCall(callID, toolName, arguments string, usage model.Usage) provider.Response {
	return provider.Response{
		Message: model.Message{FinishReason: model.FinishToolCalls, Usage: &usage,
			Role: model.RoleAssistant,
			Blocks: []model.Block{{
				Type:       model.BlockToolCall,
				ToolCallID: callID,
				ToolName:   toolName,
				Arguments:  json.RawMessage(arguments),
			}},
		},
	}
}

// stubTool is a minimal tool.Tool for tests: it returns a fixed result and
// records every call's arguments.
type stubTool struct {
	name   string
	result tool.Result

	mu    sync.Mutex
	calls []string
}

func newStubTool(name string, result tool.Result) *stubTool {
	return &stubTool{name: name, result: result}
}

func (s *stubTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{
		Name:        s.name,
		Description: "test stub tool",
		Parameters: map[string]any{
			"type":                 "object",
			"additionalProperties": false,
			"properties":           map[string]any{},
		},
	}
}

func (s *stubTool) Execute(ctx context.Context, arguments json.RawMessage) tool.Result {
	s.mu.Lock()
	s.calls = append(s.calls, string(arguments))
	s.mu.Unlock()
	return s.result
}

func (s *stubTool) callCount() int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return len(s.calls)
}

func toolDefNames(defs []model.ToolDefinition) []string {
	names := make([]string, len(defs))
	for i, d := range defs {
		names[i] = d.Name
	}
	return names
}
