package openaicompat

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
)

func TestCompleteRetriesRateLimitsAndServerErrorsAtMostTwice(t *testing.T) {
	for _, status := range []int{http.StatusTooManyRequests, http.StatusServiceUnavailable} {
		t.Run(http.StatusText(status), func(t *testing.T) {
			attempts := 0
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
				attempts++
				if attempts < 3 {
					http.Error(w, "try again", status)
					return
				}
				w.Header().Set("Content-Type", "text/event-stream")
				fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
			}))
			defer server.Close()

			client := New(server.URL, "key", server.Client())
			var delays []time.Duration
			client.sleep = func(_ context.Context, delay time.Duration) error {
				delays = append(delays, delay)
				return nil
			}
			if _, err := client.Complete(context.Background(), provider.Request{Model: "model"}, nil); err != nil {
				t.Fatal(err)
			}
			if attempts != 3 || !reflect.DeepEqual(delays, []time.Duration{250 * time.Millisecond, 500 * time.Millisecond}) {
				t.Fatalf("attempts = %d, delays = %v", attempts, delays)
			}
		})
	}
}

func TestCompleteHonorsRetryAfter(t *testing.T) {
	tests := []struct {
		name       string
		retryAfter func() string
		check      func(*testing.T, time.Duration)
	}{
		{
			name:       "zero seconds",
			retryAfter: func() string { return "0" },
			check: func(t *testing.T, delay time.Duration) {
				if delay != 0 {
					t.Fatalf("delay = %v, want zero", delay)
				}
			},
		},
		{
			name:       "HTTP date",
			retryAfter: func() string { return time.Now().Add(10 * time.Second).UTC().Format(http.TimeFormat) },
			check: func(t *testing.T, delay time.Duration) {
				if delay < 8*time.Second || delay > 10*time.Second {
					t.Fatalf("delay = %v, want HTTP-date delay", delay)
				}
			},
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			attempts := 0
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
				attempts++
				if attempts == 1 {
					w.Header().Set("Retry-After", test.retryAfter())
					http.Error(w, "retry", http.StatusTooManyRequests)
					return
				}
				w.Header().Set("Content-Type", "text/event-stream")
				fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
			}))
			defer server.Close()

			client := New(server.URL, "key", server.Client())
			var delays []time.Duration
			client.sleep = func(_ context.Context, delay time.Duration) error {
				delays = append(delays, delay)
				return nil
			}
			if _, err := client.Complete(context.Background(), provider.Request{Model: "model"}, nil); err != nil {
				t.Fatal(err)
			}
			if attempts != 2 || len(delays) != 1 {
				t.Fatalf("attempts = %d, delays = %v", attempts, delays)
			}
			test.check(t, delays[0])
		})
	}
}

func TestCompleteDoesNotRetryRedirectPolicyErrors(t *testing.T) {
	attempts := 0
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		attempts++
		http.Redirect(w, r, "/elsewhere", http.StatusFound)
	}))
	defer server.Close()

	httpClient := server.Client()
	httpClient.CheckRedirect = func(*http.Request, []*http.Request) error {
		return errors.New("redirect blocked")
	}
	client := New(server.URL, "key", httpClient)
	client.sleep = noSleep
	_, err := client.Complete(context.Background(), provider.Request{Model: "model"}, nil)
	if err == nil || attempts != 1 {
		t.Fatalf("error = %v, attempts = %d", err, attempts)
	}
}

func TestCompleteRetriesConnectionErrors(t *testing.T) {
	attempts := 0
	client := New("https://example.test/v1", "key", &http.Client{Transport: roundTripFunc(func(*http.Request) (*http.Response, error) {
		attempts++
		if attempts < 3 {
			return nil, errors.New("dial unavailable")
		}
		return streamHTTPResponse("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"), nil
	})})
	client.sleep = noSleep

	if _, err := client.Complete(context.Background(), provider.Request{Model: "model"}, nil); err != nil {
		t.Fatal(err)
	}
	if attempts != 3 {
		t.Fatalf("attempts = %d, want 3", attempts)
	}
}

func TestCompleteRetriesStreamReadErrorBeforeDelta(t *testing.T) {
	attempts := 0
	client := New("https://example.test/v1", "key", &http.Client{Transport: roundTripFunc(func(*http.Request) (*http.Response, error) {
		attempts++
		if attempts < 3 {
			return &http.Response{StatusCode: http.StatusOK, Header: make(http.Header), Body: io.NopCloser(errorReader{})}, nil
		}
		return streamHTTPResponse("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"), nil
	})})
	client.sleep = noSleep

	if _, err := client.Complete(context.Background(), provider.Request{Model: "model"}, nil); err != nil {
		t.Fatal(err)
	}
	if attempts != 3 {
		t.Fatalf("attempts = %d, want 3", attempts)
	}
}

func TestCompleteDoesNotRetryAfterVisibleDelta(t *testing.T) {
	tests := []struct {
		name string
		data string
	}{
		{name: "text", data: `{"choices":[{"delta":{"content":"visible"}}]}`},
		{name: "tool call", data: `{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":"read","arguments":"{"}}]}}]}`},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			attempts := 0
			client := New("https://example.test/v1", "key", &http.Client{Transport: roundTripFunc(func(*http.Request) (*http.Response, error) {
				attempts++
				body := io.MultiReader(strings.NewReader("data: "+test.data+"\n\n"), errorReader{})
				return &http.Response{StatusCode: http.StatusOK, Header: make(http.Header), Body: io.NopCloser(body)}, nil
			})})
			client.sleep = noSleep

			_, err := client.Complete(context.Background(), provider.Request{Model: "model"}, func(provider.StreamEvent) {})
			if err == nil || !strings.Contains(err.Error(), "stream connection lost") {
				t.Fatalf("error = %v", err)
			}
			if attempts != 1 {
				t.Fatalf("attempts = %d, want 1", attempts)
			}
		})
	}
}

func TestCompleteStopsAfterThreeRetryableAttempts(t *testing.T) {
	attempts := 0
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		attempts++
		http.Error(w, "still unavailable", http.StatusServiceUnavailable)
	}))
	defer server.Close()

	client := New(server.URL, "key", server.Client())
	var sleeps int
	client.sleep = func(context.Context, time.Duration) error { sleeps++; return nil }
	_, err := client.Complete(context.Background(), provider.Request{Model: "model"}, nil)
	if err == nil || attempts != 3 || sleeps != 2 {
		t.Fatalf("error = %v, attempts = %d, sleeps = %d", err, attempts, sleeps)
	}
}

func TestCompletePreservesCancellationIdentityDuringRetryBackoff(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		http.Error(w, "retry later", http.StatusServiceUnavailable)
	}))
	defer server.Close()

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	client := New(server.URL, "key", server.Client())
	sleepStarted := make(chan struct{})
	client.sleep = func(ctx context.Context, _ time.Duration) error {
		close(sleepStarted)
		<-ctx.Done()
		return ctx.Err()
	}

	errCh := make(chan error, 1)
	go func() {
		_, err := client.Complete(ctx, provider.Request{Model: "model"}, nil)
		errCh <- err
	}()

	<-sleepStarted
	cancel()

	if err := <-errCh; !errors.Is(err, context.Canceled) {
		t.Fatalf("error = %v, want context cancellation identity", err)
	}
}

func TestCompleteDoesNotRetryUnauthorizedAndRedactsBoundedError(t *testing.T) {
	const key = "very-secret-key"
	attempts := 0
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		attempts++
		body := r.Header.Get("Authorization") + " / " + key + " / " + strings.Repeat("x", 40<<10) + "TAIL-MARKER"
		http.Error(w, body, http.StatusUnauthorized)
	}))
	defer server.Close()

	client := New(server.URL, key, server.Client())
	client.sleep = func(context.Context, time.Duration) error {
		t.Fatal("non-retryable response slept")
		return nil
	}
	_, err := client.Complete(context.Background(), provider.Request{Model: "model"}, nil)
	if err == nil {
		t.Fatal("expected unauthorized error")
	}
	message := err.Error()
	if attempts != 1 || strings.Contains(message, key) || strings.Contains(message, "Bearer "+key) || strings.Contains(message, "TAIL-MARKER") || len(message) > maxErrorBody+256 {
		t.Fatalf("attempts = %d, unsafe or unbounded error (%d bytes): %.200s", attempts, len(message), message)
	}
}

func TestNewRejectsInvalidBaseURLs(t *testing.T) {
	for _, baseURL := range []string{"", "ftp://example.test/v1", "http:///v1", "https://example.test/v1?tenant=x", "https://example.test/v1?", "https://example.test/v1#fragment", "https://username@example.test/v1", "https://username:password@example.test/v1"} {
		t.Run(baseURL, func(t *testing.T) {
			_, err := New(baseURL, "key", nil).Complete(context.Background(), provider.Request{Model: "model"}, nil)
			if err == nil || !strings.Contains(err.Error(), "invalid OpenAI-compatible base URL") {
				t.Fatalf("error = %v", err)
			}
		})
	}
}

func TestCompleteClosesEveryResponseBody(t *testing.T) {
	for _, status := range []int{http.StatusOK, http.StatusUnauthorized} {
		t.Run(http.StatusText(status), func(t *testing.T) {
			bodyText := "denied"
			if status == http.StatusOK {
				bodyText = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
			}
			body := &trackingReadCloser{Reader: strings.NewReader(bodyText)}
			client := New("https://example.test/v1", "key", &http.Client{Transport: roundTripFunc(func(*http.Request) (*http.Response, error) {
				return &http.Response{StatusCode: status, Header: make(http.Header), Body: body}, nil
			})})
			_, _ = client.Complete(context.Background(), provider.Request{Model: "model"}, nil)
			if !body.closed {
				t.Fatal("response body was not closed")
			}
		})
	}
}

func TestRequestIncludesEveryFunctionSchemaField(t *testing.T) {
	payload, err := json.Marshal(translateRequest(provider.Request{
		Model: "model",
		Tools: []model.ToolDefinition{{Name: "empty"}},
	}))
	if err != nil {
		t.Fatal(err)
	}
	var decoded struct {
		Tools []struct {
			Function map[string]json.RawMessage `json:"function"`
		} `json:"tools"`
	}
	if err := json.Unmarshal(payload, &decoded); err != nil {
		t.Fatal(err)
	}
	function := decoded.Tools[0].Function
	if _, ok := function["description"]; !ok {
		t.Fatalf("function schema omitted description: %s", payload)
	}
	if _, ok := function["parameters"]; !ok {
		t.Fatalf("function schema omitted parameters: %s", payload)
	}
}

func TestTranslateRequestSetsReasoningEffort(t *testing.T) {
	payload, err := json.Marshal(translateRequest(provider.Request{Model: "model", Thinking: "xhigh"}))
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(payload), `"reasoning_effort":"xhigh"`) {
		t.Fatalf("payload = %s, want reasoning_effort xhigh", payload)
	}
	payload, err = json.Marshal(translateRequest(provider.Request{Model: "model"}))
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(payload), "reasoning_effort") {
		t.Fatalf("payload = %s, want no reasoning_effort", payload)
	}
}

func TestTranslateRequestMapsContextMessagesToWireUsers(t *testing.T) {
	request := translateRequest(provider.Request{Messages: []model.Message{
		{Role: model.RoleContext, Display: true, Blocks: []model.Block{{Type: model.BlockText, Text: "visible context"}}},
		{Role: model.RoleContext, Display: false, Blocks: []model.Block{{Type: model.BlockText, Text: "hidden context"}}},
	}})
	want := []chatMessage{
		{Role: "user", Content: "visible context"},
		{Role: "user", Content: "hidden context"},
	}
	if !reflect.DeepEqual(request.Messages, want) {
		t.Fatalf("messages = %#v, want %#v", request.Messages, want)
	}
}

func TestCompleteTranslatesNeutralRequestToChatCompletions(t *testing.T) {
	var got chatRequest
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost || r.URL.Path != "/gateway/v1/chat/completions" {
			t.Errorf("request = %s %s", r.Method, r.URL.Path)
		}
		if r.Header.Get("Authorization") != "Bearer secret" || r.Header.Get("Content-Type") != "application/json" || r.Header.Get("Accept") != "text/event-stream" {
			t.Errorf("unexpected headers: %#v", r.Header)
		}
		if err := json.NewDecoder(r.Body).Decode(&got); err != nil {
			t.Errorf("decode request: %v", err)
		}
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
	}))
	defer server.Close()

	parameters := map[string]any{
		"type": "object",
		"properties": map[string]any{
			"path": map[string]any{"type": "string"},
		},
		"required": []any{"path"},
	}
	client := New(server.URL+"/gateway/v1/", "secret", server.Client())
	_, err := client.Complete(context.Background(), provider.Request{
		Model:        "chat-model",
		SystemPrompt: "Be concise.",
		Messages: []model.Message{
			{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "read "}, {Type: model.BlockText, Text: "it"}}},
			{Role: model.RoleAssistant, Blocks: []model.Block{
				{Type: model.BlockText, Text: "Calling read."},
				{Type: model.BlockToolCall, ToolCallID: "call-7", ToolName: "read", Arguments: json.RawMessage(`{"path":"README.md"}`)},
				{Type: model.BlockToolCall, ToolCallID: "call-8", ToolName: "read", Arguments: json.RawMessage(`{"path":"AGENTS.md"}`)},
			}},
			{Role: model.RoleTool, Blocks: []model.Block{
				{Type: model.BlockToolResult, ToolCallID: "call-7", Text: "contents"},
				{Type: model.BlockToolResult, ToolCallID: "call-8", Text: "second"},
			}},
			{Role: model.RoleContext, Display: false, Blocks: []model.Block{{Type: model.BlockText, Text: "after tools"}}},
			{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "continue"}}},
		},
		Tools: []model.ToolDefinition{{Name: "read", Description: "Read a file", Parameters: parameters}},
	}, nil)
	if err != nil {
		t.Fatal(err)
	}

	if got.Model != "chat-model" || !got.Stream || !got.StreamOptions.IncludeUsage {
		t.Fatalf("request flags = %#v", got)
	}
	wantMessages := []chatMessage{
		{Role: "system", Content: "Be concise."},
		{Role: "user", Content: "read it"},
		{Role: "assistant", Content: "Calling read.", ToolCalls: []chatToolCall{
			{ID: "call-7", Type: "function", Function: chatToolCallFunction{Name: "read", Arguments: `{"path":"README.md"}`}},
			{ID: "call-8", Type: "function", Function: chatToolCallFunction{Name: "read", Arguments: `{"path":"AGENTS.md"}`}},
		}},
		{Role: "tool", Content: "contents", ToolCallID: "call-7"},
		{Role: "tool", Content: "second", ToolCallID: "call-8"},
		{Role: "user", Content: "after tools"},
		{Role: "user", Content: "continue"},
	}
	if !reflect.DeepEqual(got.Messages, wantMessages) {
		t.Fatalf("messages mismatch\nwant: %#v\n got: %#v", wantMessages, got.Messages)
	}
	wantTools := []chatTool{{Type: "function", Function: chatFunction{Name: "read", Description: "Read a file", Parameters: parameters}}}
	if !reflect.DeepEqual(got.Tools, wantTools) {
		t.Fatalf("tools mismatch\nwant: %#v\n got: %#v", wantTools, got.Tools)
	}
}

type roundTripFunc func(*http.Request) (*http.Response, error)

func (fn roundTripFunc) RoundTrip(request *http.Request) (*http.Response, error) {
	return fn(request)
}

type trackingReadCloser struct {
	io.Reader
	closed bool
}

func (body *trackingReadCloser) Close() error {
	body.closed = true
	return nil
}

type errorReader struct{}

func (errorReader) Read([]byte) (int, error) {
	return 0, errors.New("stream connection lost")
}

func noSleep(context.Context, time.Duration) error { return nil }

func streamHTTPResponse(body string) *http.Response {
	return &http.Response{
		StatusCode: http.StatusOK,
		Header:     http.Header{"Content-Type": []string{"text/event-stream"}},
		Body:       io.NopCloser(strings.NewReader(body)),
	}
}
