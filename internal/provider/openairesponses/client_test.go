package openairesponses

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"golang.org/x/oauth2"
)

const cannedStream = `event: response.output_text.delta
data: {"type":"response.output_text.delta","delta":"Hello "}

event: response.output_text.delta
data: {"type":"response.output_text.delta","delta":"world"}

event: response.output_item.added
data: {"type":"response.output_item.added","item":{"type":"function_call","id":"item_1","call_id":"call_abc","name":"get_time"}}

event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","item_id":"item_1","delta":"{\"tz\":"}

event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","item_id":"item_1","delta":"\"utc\"}"}

event: response.completed
data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":5,"input_tokens_details":{"cached_tokens":2}}}}

`

func TestCompleteParsesTextAndToolCall(t *testing.T) {
	var gotAuth, gotAccount string
	var gotBody responsesRequest
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotAuth = r.Header.Get("Authorization")
		gotAccount = r.Header.Get("chatgpt-account-id")
		raw, _ := io.ReadAll(r.Body)
		if err := json.Unmarshal(raw, &gotBody); err != nil {
			t.Errorf("decode request body: %v", err)
		}
		w.Header().Set("Content-Type", "text/event-stream")
		_, _ = io.WriteString(w, cannedStream)
	}))
	defer server.Close()

	client := newWithBaseURL(server.URL,
		oauth2.StaticTokenSource(&oauth2.Token{AccessToken: "test-token"}),
		"acct-1", server.Client())

	request := provider.Request{
		Model:        "gpt-5-codex",
		SystemPrompt: "sys",
		Messages: []model.Message{
			{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "hi"}}},
		},
		Tools: []model.ToolDefinition{
			{Name: "get_time", Description: "get the time", Parameters: map[string]any{"type": "object"}},
		},
	}

	var events []provider.StreamEvent
	resp, err := client.Complete(context.Background(), request, func(e provider.StreamEvent) {
		events = append(events, e)
	})
	if err != nil {
		t.Fatalf("Complete: %v", err)
	}

	if gotAuth != "Bearer test-token" {
		t.Errorf("Authorization = %q, want Bearer test-token", gotAuth)
	}
	if gotAccount != "acct-1" {
		t.Errorf("chatgpt-account-id = %q, want acct-1", gotAccount)
	}
	if gotBody.Instructions != "sys" {
		t.Errorf("instructions = %q, want sys", gotBody.Instructions)
	}
	if len(gotBody.Input) != 1 || gotBody.Input[0].Type != "message" || gotBody.Input[0].Role != "user" {
		t.Fatalf("unexpected input items: %+v", gotBody.Input)
	}
	if len(gotBody.Tools) != 1 || gotBody.Tools[0].Type != "function" || gotBody.Tools[0].Name != "get_time" {
		t.Fatalf("unexpected tools: %+v", gotBody.Tools)
	}

	if len(resp.Message.Blocks) != 2 {
		t.Fatalf("blocks = %d, want 2: %+v", len(resp.Message.Blocks), resp.Message.Blocks)
	}
	if resp.Message.Blocks[0].Type != model.BlockText || resp.Message.Blocks[0].Text != "Hello world" {
		t.Errorf("text block = %+v", resp.Message.Blocks[0])
	}
	call := resp.Message.Blocks[1]
	if call.Type != model.BlockToolCall || call.ToolCallID != "call_abc" || call.ToolName != "get_time" {
		t.Errorf("tool call block = %+v", call)
	}
	if string(call.Arguments) != `{"tz":"utc"}` {
		t.Errorf("arguments = %s, want {\"tz\":\"utc\"}", call.Arguments)
	}
	if resp.FinishReason != model.FinishToolCalls {
		t.Errorf("finish = %q, want tool_calls", resp.FinishReason)
	}
	if resp.Usage != (model.Usage{InputTokens: 10, OutputTokens: 5, CachedInputTokens: 2}) {
		t.Errorf("usage = %+v", resp.Usage)
	}

	if len(events) == 0 {
		t.Fatal("no stream events emitted")
	}
}

func TestCompleteHTTPErrorRedactsToken(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
		// Echo the token back to prove the provider strips it from errors.
		_, _ = io.WriteString(w, "invalid token secret-abc")
	}))
	defer server.Close()

	client := newWithBaseURL(server.URL,
		oauth2.StaticTokenSource(&oauth2.Token{AccessToken: "secret-abc"}),
		"acct-1", server.Client())

	_, err := client.Complete(context.Background(), provider.Request{Model: "m"}, nil)
	if err == nil {
		t.Fatal("expected error on HTTP 401")
	}
	if got := err.Error(); strings.Contains(got, "secret-abc") {
		t.Fatalf("error leaked token: %q", got)
	}
}
