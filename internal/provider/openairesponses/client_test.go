package openairesponses

import (
	"context"
	"encoding/json"
	"errors"
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

func TestCompleteHTTPErrorRedactsTokenAndAccountID(t *testing.T) {
	const (
		accessToken = "secret-abc"
		accountID   = "acct-secret"
	)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
		_, _ = io.WriteString(w, "invalid token "+accessToken+" account "+accountID)
	}))
	defer server.Close()

	client := newWithBaseURL(server.URL,
		oauth2.StaticTokenSource(&oauth2.Token{AccessToken: accessToken}),
		accountID, server.Client())

	_, err := client.Complete(context.Background(), provider.Request{Model: "m"}, nil)
	if err == nil {
		t.Fatal("expected error on HTTP 401")
	}
	for _, secret := range []string{accessToken, accountID} {
		if strings.Contains(err.Error(), secret) {
			t.Fatalf("error leaked %q: %q", secret, err.Error())
		}
	}
}

func TestCompleteTokenSourceFailuresReturnFixedAuthError(t *testing.T) {
	for _, test := range []struct {
		name string
		src  oauth2.TokenSource
	}{
		{name: "error", src: failingTokenSource{err: errors.New("access-secret refresh-secret acct-secret")}},
		{name: "nil token", src: failingTokenSource{}},
		{name: "empty token", src: failingTokenSource{token: &oauth2.Token{AccessToken: ""}}},
	} {
		t.Run(test.name, func(t *testing.T) {
			client := newWithBaseURL("https://example.test", test.src, "acct-secret", &http.Client{Transport: roundTripperFunc(func(*http.Request) (*http.Response, error) {
				t.Fatal("transport must not be called when authorization fails")
				return nil, nil
			})})
			_, err := client.Complete(context.Background(), provider.Request{Model: "m"}, nil)
			if err == nil || err.Error() != errChatGPTAuthorizationFailed.Error() {
				t.Fatalf("err = %v, want fixed auth failure", err)
			}
			for _, secret := range []string{"access-secret", "refresh-secret", "acct-secret"} {
				if strings.Contains(err.Error(), secret) {
					t.Fatalf("authorization error leaked %q: %v", secret, err)
				}
			}
		})
	}
}

func TestCompleteStreamReadErrorRedactsTokenAndAccountID(t *testing.T) {
	const (
		accessToken = "stream-token-secret"
		accountID   = "stream-account-secret"
	)
	client := newWithBaseURL("https://example.test",
		oauth2.StaticTokenSource(&oauth2.Token{AccessToken: accessToken}),
		accountID,
		&http.Client{Transport: roundTripperFunc(func(*http.Request) (*http.Response, error) {
			return &http.Response{
				StatusCode: http.StatusOK,
				Header:     make(http.Header),
				Body:       failingReadCloser{err: errors.New("stream read failed for " + accessToken + " " + accountID)},
			}, nil
		})},
	)
	_, err := client.Complete(context.Background(), provider.Request{Model: "m"}, nil)
	if err == nil {
		t.Fatal("Complete() succeeded")
	}
	for _, secret := range []string{accessToken, accountID} {
		if strings.Contains(err.Error(), secret) {
			t.Fatalf("stream error leaked %q: %v", secret, err)
		}
	}
}

type failingTokenSource struct {
	token *oauth2.Token
	err   error
}

func (s failingTokenSource) Token() (*oauth2.Token, error) {
	return s.token, s.err
}

type roundTripperFunc func(*http.Request) (*http.Response, error)

func (f roundTripperFunc) RoundTrip(request *http.Request) (*http.Response, error) {
	return f(request)
}

type failingReadCloser struct{ err error }

func (f failingReadCloser) Read([]byte) (int, error) { return 0, f.err }
func (f failingReadCloser) Close() error             { return nil }
