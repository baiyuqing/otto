package openaicompat

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
)

func TestCompleteStreamsTextAndAssemblesToolCall(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/v1/chat/completions" || r.Header.Get("Authorization") != "Bearer test-key" {
			t.Errorf("unexpected request: path=%s auth=%s", r.URL.Path, r.Header.Get("Authorization"))
		}
		w.Header().Set("Content-Type", "text/event-stream")
		flusher := w.(http.Flusher)
		chunks := []string{
			`{"choices":[{"delta":{"content":"I will read. "}}]}`,
			`{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"read","arguments":"{\"pa"}}]}}]}`,
			`{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"README.md\"}"}}]},"finish_reason":"tool_calls"}]}`,
			`{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7}}`,
		}
		for _, chunk := range chunks {
			fmt.Fprintf(w, "data: %s\n\n", chunk)
			flusher.Flush()
		}
		fmt.Fprint(w, "data: [DONE]\n\n")
	}))
	defer server.Close()

	client := New(server.URL+"/v1", "test-key", server.Client())
	var events []provider.StreamEvent
	response, err := client.Complete(context.Background(), provider.Request{
		Model: "test-model",
		Messages: []model.Message{{
			Role:   model.RoleUser,
			Blocks: []model.Block{{Type: model.BlockText, Text: "inspect"}},
		}},
	}, func(event provider.StreamEvent) {
		events = append(events, event)
	})
	if err != nil {
		t.Fatal(err)
	}

	var deltas strings.Builder
	for _, event := range events {
		if event.Type == provider.StreamTextDelta {
			deltas.WriteString(event.Text)
		}
	}
	if deltas.String() != "I will read. " || response.FinishReason != model.FinishToolCalls {
		t.Fatalf("unexpected response: deltas=%q response=%#v", deltas.String(), response)
	}
	if len(response.Message.Blocks) != 2 {
		t.Fatalf("response blocks = %#v, want text and tool call", response.Message.Blocks)
	}
	call := response.Message.Blocks[1]
	if call.ToolCallID != "call-1" || call.ToolName != "read" || string(call.Arguments) != `{"path":"README.md"}` {
		t.Fatalf("bad tool call: %#v", call)
	}
	if response.Usage != (model.Usage{InputTokens: 11, OutputTokens: 7}) {
		t.Fatalf("usage = %#v", response.Usage)
	}
	if len(events) != 3 || events[1].Type != provider.StreamToolCallDelta || events[1].Arguments != `{"pa` || events[2].Arguments != `th":"README.md"}` {
		t.Fatalf("stream events = %#v", events)
	}
}

func TestCompleteHandlesSSEFramingAndMultipleIndexedCalls(t *testing.T) {
	large := strings.Repeat("x", 70<<10)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, ": keepalive\r\n\r\n")
		fmt.Fprintf(w, "data: {\"choices\":[{\"delta\":{\"content\":%q}}]}\r\n\r\n", large)
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":3,\"id\":\"third\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"pa\"}},{\"index\":1,\"id\":\"first\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"A\\\"}\"}}]}}]}\r\n\r\n")
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":3,\"function\":{\"arguments\":\"th\\\":\\\"B\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\r\n\r\n")
		fmt.Fprint(w, "data: {\"choices\":[],\r\ndata: \"usage\":{\"prompt_tokens\":5,\"completion_tokens\":9}}\r\n\r\n")
		fmt.Fprint(w, "event: completion\r\ndata: [DONE]\r\n\r\n")
	}))
	defer server.Close()

	response, err := New(server.URL, "key", server.Client()).Complete(context.Background(), provider.Request{Model: "model"}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(response.Message.Blocks) != 3 || response.Message.Blocks[0].Text != large {
		t.Fatalf("unexpected blocks: %#v", response.Message.Blocks)
	}
	if got := response.Message.Blocks[1]; got.ToolCallID != "third" || got.ToolName != "write" || string(got.Arguments) != `{"path":"B"}` {
		t.Fatalf("first-seen call = %#v", got)
	}
	if got := response.Message.Blocks[2]; got.ToolCallID != "first" || got.ToolName != "read" || string(got.Arguments) != `{"path":"A"}` {
		t.Fatalf("second call = %#v", got)
	}
	if response.Usage != (model.Usage{InputTokens: 5, OutputTokens: 9}) {
		t.Fatalf("usage = %#v", response.Usage)
	}
}

func TestCompleteEmitsStableToolCallIdentityForInterleavedContinuations(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call-b\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"pa\"}},{\"index\":0,\"id\":\"call-a\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n")
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"A\\\"}\"}}]}}]}\n\n")
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"th\\\":\\\"B\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n")
		fmt.Fprint(w, "data: [DONE]\n\n")
	}))
	defer server.Close()

	var events []provider.StreamEvent
	response, err := New(server.URL, "key", server.Client()).Complete(context.Background(), provider.Request{Model: "model"}, func(event provider.StreamEvent) {
		if event.Type == provider.StreamToolCallDelta {
			events = append(events, event)
		}
	})
	if err != nil {
		t.Fatal(err)
	}

	want := []provider.StreamEvent{
		{Type: provider.StreamToolCallDelta, ToolCallID: "call-b", ToolName: "write", Arguments: `{"pa`},
		{Type: provider.StreamToolCallDelta, ToolCallID: "call-a", ToolName: "read", Arguments: `{"pa`},
		{Type: provider.StreamToolCallDelta, ToolCallID: "call-a", ToolName: "read", Arguments: `th":"A"}`},
		{Type: provider.StreamToolCallDelta, ToolCallID: "call-b", ToolName: "write", Arguments: `th":"B"}`},
	}
	if len(events) != len(want) {
		t.Fatalf("events = %#v, want %#v", events, want)
	}
	for i := range want {
		if events[i] != want[i] {
			t.Fatalf("event[%d] = %#v, want %#v", i, events[i], want[i])
		}
	}
	if len(response.Message.Blocks) != 2 {
		t.Fatalf("blocks = %#v, want two tool calls", response.Message.Blocks)
	}
	if got := response.Message.Blocks[0]; got.ToolCallID != "call-b" || got.ToolName != "write" || string(got.Arguments) != `{"path":"B"}` {
		t.Fatalf("call-b = %#v", got)
	}
	if got := response.Message.Blocks[1]; got.ToolCallID != "call-a" || got.ToolName != "read" || string(got.Arguments) != `{"path":"A"}` {
		t.Fatalf("call-a = %#v", got)
	}
}

func TestCompleteRejectsMalformedOrIncompleteStreams(t *testing.T) {
	tests := []struct {
		name string
		body string
		want string
	}{
		{name: "malformed JSON", body: "data: {not-json}\n\n", want: "decode chat completion stream"},
		{name: "missing done", body: "data: {\"choices\":[]}\n\n", want: "without [DONE]"},
		{name: "malformed tool arguments", body: "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"bad\",\"function\":{\"name\":\"read\",\"arguments\":\"{\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n", want: "malformed arguments"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
				w.Header().Set("Content-Type", "text/event-stream")
				fmt.Fprint(w, test.body)
			}))
			defer server.Close()

			_, err := New(server.URL, "key", server.Client()).Complete(context.Background(), provider.Request{Model: "model"}, nil)
			if err == nil || !strings.Contains(err.Error(), test.want) {
				t.Fatalf("error = %v, want containing %q", err, test.want)
			}
		})
	}
}

func TestCompleteMapsUnknownFinishReason(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n\ndata: [DONE]\n\n")
	}))
	defer server.Close()

	response, err := New(server.URL, "key", server.Client()).Complete(context.Background(), provider.Request{Model: "model"}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if response.FinishReason != model.FinishUnknown || response.Message.FinishReason != model.FinishUnknown {
		t.Fatalf("finish reasons = %q / %q", response.FinishReason, response.Message.FinishReason)
	}
}

func TestCompleteParsesOpenAICachedTokens(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":7,\"prompt_tokens_details\":{\"cached_tokens\":64}}}\n\n")
		fmt.Fprint(w, "data: [DONE]\n\n")
	}))
	defer server.Close()

	response, err := New(server.URL, "key", server.Client()).Complete(context.Background(), provider.Request{Model: "model"}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if response.Usage != (model.Usage{InputTokens: 100, OutputTokens: 7, CachedInputTokens: 64}) {
		t.Fatalf("usage = %#v", response.Usage)
	}
}

func TestCompleteParsesDeepSeekCachedTokens(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":7,\"prompt_cache_hit_tokens\":80,\"prompt_cache_miss_tokens\":20}}\n\n")
		fmt.Fprint(w, "data: [DONE]\n\n")
	}))
	defer server.Close()

	response, err := New(server.URL, "key", server.Client()).Complete(context.Background(), provider.Request{Model: "model"}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if response.Usage != (model.Usage{InputTokens: 100, OutputTokens: 7, CachedInputTokens: 80}) {
		t.Fatalf("usage = %#v", response.Usage)
	}
}

func TestCompletePropagatesCancellation(t *testing.T) {
	started := make(chan struct{})
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		w.(http.Flusher).Flush()
		close(started)
		<-r.Context().Done()
	}))
	defer server.Close()

	ctx, cancel := context.WithCancel(context.Background())
	errCh := make(chan error, 1)
	go func() {
		_, err := New(server.URL, "key", server.Client()).Complete(ctx, provider.Request{Model: "model"}, nil)
		errCh <- err
	}()
	<-started
	cancel()
	if err := <-errCh; !errors.Is(err, context.Canceled) {
		t.Fatalf("error = %v, want context cancellation", err)
	}
}
