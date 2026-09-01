package openairesponses

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/safetext"
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

func TestCompleteSuccessfulStreamAndResponseRedactRotatedCredentials(t *testing.T) {
	const (
		accessToken = "rotated-token-123"
		accountID   = "acct-rotated-456"
	)
	marker := requestRedactionMarker(t, accessToken, accountID)
	stream := strings.Join([]string{
		`event: response.output_text.delta`,
		`data: {"type":"response.output_text.delta","delta":"before ` + accessToken[:8] + `"}`,
		``,
		`event: response.output_text.delta`,
		`data: {"type":"response.output_text.delta","delta":"` + accessToken[8:] + ` after ` + accountID[:7] + `"}`,
		``,
		`event: response.output_text.delta`,
		`data: {"type":"response.output_text.delta","delta":"` + accountID[7:] + ` done"}`,
		``,
		`event: response.output_item.added`,
		`data: {"type":"response.output_item.added","item":{"type":"function_call","id":"item_1","call_id":"call-` + accessToken + `","name":"tool-` + accountID + `"}}`,
		``,
		`event: response.function_call_arguments.delta`,
		`data: {"type":"response.function_call_arguments.delta","item_id":"item_1","delta":"{\"token\":\"` + accessToken[:8] + `"}`,
		``,
		`event: response.function_call_arguments.delta`,
		`data: {"type":"response.function_call_arguments.delta","item_id":"item_1","delta":"` + accessToken[8:] + `\",\"account\":\"` + accountID[:7] + `"}`,
		``,
		`event: response.function_call_arguments.delta`,
		`data: {"type":"response.function_call_arguments.delta","item_id":"item_1","delta":"` + accountID[7:] + `\"}"}`,
		``,
		`event: response.completed`,
		`data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1}}}`,
		``,
	}, "\n")
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		_, _ = io.WriteString(w, stream)
	}))
	defer server.Close()

	client := newWithBaseURL(server.URL,
		oauth2.StaticTokenSource(&oauth2.Token{AccessToken: accessToken}),
		accountID, server.Client())

	var events []provider.StreamEvent
	response, err := client.Complete(context.Background(), provider.Request{Model: "m"}, func(event provider.StreamEvent) {
		events = append(events, event)
	})
	if err != nil {
		t.Fatalf("Complete() error = %v", err)
	}
	if len(events) == 0 {
		t.Fatal("Complete() emitted no events")
	}
	for _, secret := range []string{accessToken, accountID} {
		for _, event := range events {
			if strings.Contains(event.Text, secret) || strings.Contains(event.ToolCallID, secret) || strings.Contains(event.ToolName, secret) || strings.Contains(event.Arguments, secret) {
				t.Fatalf("event leaked %q: %#v", secret, event)
			}
		}
		if strings.Contains(response.Message.Text(), secret) || strings.Contains(response.Message.ID, secret) || strings.Contains(string(response.Message.Blocks[1].Arguments), secret) {
			t.Fatalf("response leaked %q: %#v", secret, response)
		}
	}
	if !strings.Contains(strings.Join([]string{events[0].Text, events[len(events)-1].Arguments, response.Message.Text(), response.Message.Blocks[1].ToolCallID, response.Message.Blocks[1].ToolName, string(response.Message.Blocks[1].Arguments)}, "\n"), marker) {
		t.Fatalf("redacted output did not contain marker %q: events=%#v response=%#v", marker, events, response)
	}
}

func TestCompleteHTTPErrorReturnsFixedStatusOnlyError(t *testing.T) {
	const (
		accessToken = "secret-abc"
		accountID   = "acct-secret"
		oldBoundary = 32 << 10
	)
	body := strings.Repeat("x", oldBoundary-len(accessToken)/2) + accessToken + " account " + accountID
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
		_, _ = io.WriteString(w, body)
	}))
	defer server.Close()

	client := newWithBaseURL(server.URL,
		oauth2.StaticTokenSource(&oauth2.Token{AccessToken: accessToken}),
		accountID, server.Client())

	_, err := client.Complete(context.Background(), provider.Request{Model: "m"}, nil)
	if err == nil {
		t.Fatal("expected error on HTTP 401")
	}
	if got, want := err.Error(), "chatgpt responses HTTP 401"; got != want {
		t.Fatalf("error = %q, want %q", got, want)
	}
	for _, secret := range []string{accessToken, accountID} {
		if strings.Contains(err.Error(), secret) {
			t.Fatalf("error leaked %q: %q", secret, err.Error())
		}
	}
}

func TestCompleteNon2XXDoesNotInspectBodyReadError(t *testing.T) {
	hostile := &hostileBoundaryError{}
	client := newWithBaseURL("https://example.test",
		oauth2.StaticTokenSource(&oauth2.Token{AccessToken: "access-token"}),
		"acct-1",
		&http.Client{Transport: roundTripperFunc(func(*http.Request) (*http.Response, error) {
			return &http.Response{StatusCode: http.StatusBadGateway, Header: make(http.Header), Body: failingReadCloser{err: hostile}}, nil
		})},
	)
	_, err := client.Complete(context.Background(), provider.Request{Model: "m"}, nil)
	if err == nil {
		t.Fatal("Complete() succeeded")
	}
	if got, want := err.Error(), "chatgpt responses HTTP 502"; got != want {
		t.Fatalf("error = %q, want %q", got, want)
	}
	if hostile.calls() != 0 {
		t.Fatalf("body read error methods called %d times", hostile.calls())
	}
}

func TestCompleteDefaultClientBlocks307RedirectWithoutForwardingRequest(t *testing.T) {
	var sourceCalls atomic.Int32
	var targetCalls atomic.Int32
	target := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		targetCalls.Add(1)
		http.Error(w, "redirect target must not be reached", http.StatusInternalServerError)
	}))
	defer target.Close()

	source := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		sourceCalls.Add(1)
		http.Redirect(w, r, target.URL+"/responses", http.StatusTemporaryRedirect)
	}))
	defer source.Close()

	client := newWithBaseURL(source.URL,
		oauth2.StaticTokenSource(&oauth2.Token{AccessToken: "redirect-access-token"}),
		"redirect-account-id", nil)
	_, err := client.Complete(context.Background(), provider.Request{
		Model:        "m",
		SystemPrompt: "authorization and account must not reach redirect target",
		Messages: []model.Message{{
			Role:   model.RoleUser,
			Blocks: []model.Block{{Type: model.BlockText, Text: "redirect body must not reach target"}},
		}},
	}, nil)
	if err == nil {
		t.Fatal("Complete() succeeded")
	}
	if got, want := err.Error(), "chatgpt responses HTTP 307"; got != want {
		t.Fatalf("error = %q, want %q", got, want)
	}
	if sourceCalls.Load() != 1 || targetCalls.Load() != 0 {
		t.Fatalf("source/target calls = %d/%d, want 1/0", sourceCalls.Load(), targetCalls.Load())
	}
}

func TestCompleteUsesTurnContextAwareTokenSource(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		_, _ = io.WriteString(w, cannedStream)
	}))
	defer server.Close()

	type contextKey string
	const key contextKey = "request-id"
	source := &contextAwareTokenSource{token: &oauth2.Token{AccessToken: "ctx-token"}}
	client := newWithBaseURL(server.URL, source, "acct-1", server.Client())
	ctx := context.WithValue(context.Background(), key, "turn-ctx")
	if _, err := client.Complete(ctx, provider.Request{Model: "m"}, nil); err != nil {
		t.Fatalf("Complete() error = %v", err)
	}
	if source.tokenCalls.Load() != 0 || source.tokenContextCalls.Load() != 1 {
		t.Fatalf("Token/TokenContext calls = %d/%d, want 0/1", source.tokenCalls.Load(), source.tokenContextCalls.Load())
	}
	if got := source.lastCtx.Value(key); got != "turn-ctx" {
		t.Fatalf("TokenContext() saw context value %v, want turn-ctx", got)
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

func TestCompleteTokenSourceFailureDoesNotInspectArbitraryError(t *testing.T) {
	hostile := &hostileBoundaryError{}
	client := newWithBaseURL("https://example.test",
		failingTokenSource{err: hostile},
		"acct-1",
		&http.Client{Transport: roundTripperFunc(func(*http.Request) (*http.Response, error) {
			t.Fatal("transport must not be called when authorization fails")
			return nil, nil
		})},
	)
	_, err := client.Complete(context.Background(), provider.Request{Model: "m"}, nil)
	if err != errChatGPTAuthorizationFailed {
		t.Fatalf("Complete() error = %v, want fixed authorization failure", err)
	}
	if hostile.calls() != 0 {
		t.Fatalf("token source error methods called %d times", hostile.calls())
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

func TestCompleteRejectsUnrepresentableRequestBoundaryBeforeHTTP(t *testing.T) {
	var calls atomic.Int32
	client := newWithBaseURL("https://example.test",
		oauth2.StaticTokenSource(&oauth2.Token{AccessToken: strings.Repeat("x", safetext.MaxDynamicValueBytes+1)}),
		"acct-1",
		&http.Client{Transport: roundTripperFunc(func(*http.Request) (*http.Response, error) {
			calls.Add(1)
			return nil, nil
		})},
	)
	var emitted atomic.Int32
	_, err := client.Complete(context.Background(), provider.Request{Model: "m"}, func(provider.StreamEvent) {
		emitted.Add(1)
	})
	if err != errChatGPTRequestFailed {
		t.Fatalf("Complete() error = %v, want fixed request failure", err)
	}
	if calls.Load() != 0 || emitted.Load() != 0 {
		t.Fatalf("transport/emits = %d/%d, want 0/0", calls.Load(), emitted.Load())
	}
}

func TestCompleteTransportFailureDoesNotInspectArbitraryError(t *testing.T) {
	hostile := &hostileBoundaryError{}
	client := newWithBaseURL("https://example.test",
		oauth2.StaticTokenSource(&oauth2.Token{AccessToken: "access-token"}),
		"acct-1",
		&http.Client{Transport: roundTripperFunc(func(*http.Request) (*http.Response, error) {
			return nil, hostile
		})},
	)
	_, err := client.Complete(context.Background(), provider.Request{Model: "m"}, nil)
	if err != errChatGPTRequestFailed {
		t.Fatalf("Complete() error = %v, want fixed request failure", err)
	}
	if hostile.calls() != 0 {
		t.Fatalf("transport error methods called %d times", hostile.calls())
	}
}

func TestCompleteBodyReadFailureDoesNotInspectArbitraryError(t *testing.T) {
	hostile := &hostileBoundaryError{}
	client := newWithBaseURL("https://example.test",
		oauth2.StaticTokenSource(&oauth2.Token{AccessToken: "access-token"}),
		"acct-1",
		&http.Client{Transport: roundTripperFunc(func(*http.Request) (*http.Response, error) {
			return &http.Response{StatusCode: http.StatusOK, Header: make(http.Header), Body: failingReadCloser{err: hostile}}, nil
		})},
	)
	_, err := client.Complete(context.Background(), provider.Request{Model: "m"}, nil)
	if err != errChatGPTRequestFailed {
		t.Fatalf("Complete() error = %v, want fixed request failure", err)
	}
	if hostile.calls() != 0 {
		t.Fatalf("body read error methods called %d times", hostile.calls())
	}
}

type failingTokenSource struct {
	token *oauth2.Token
	err   error
}

func (s failingTokenSource) Token() (*oauth2.Token, error) {
	return s.token, s.err
}

type contextAwareTokenSource struct {
	token             *oauth2.Token
	tokenCalls        atomic.Int32
	tokenContextCalls atomic.Int32
	lastCtx           context.Context
}

func (s *contextAwareTokenSource) Token() (*oauth2.Token, error) {
	s.tokenCalls.Add(1)
	return nil, errors.New("Token() must not be used when TokenContext is available")
}

func (s *contextAwareTokenSource) TokenContext(ctx context.Context) (*oauth2.Token, error) {
	s.tokenContextCalls.Add(1)
	s.lastCtx = ctx
	return s.token, nil
}

type roundTripperFunc func(*http.Request) (*http.Response, error)

func (f roundTripperFunc) RoundTrip(request *http.Request) (*http.Response, error) {
	return f(request)
}

type failingReadCloser struct{ err error }

func (f failingReadCloser) Read([]byte) (int, error) { return 0, f.err }
func (f failingReadCloser) Close() error             { return nil }

func requestRedactionMarker(t *testing.T, values ...string) string {
	t.Helper()
	collector := safetext.NewSecretCollector()
	for _, value := range values {
		if !collector.Add(value) {
			t.Fatalf("collector rejected %q", value)
		}
	}
	marker, ok := safetext.DynamicRedactionMarker(collector.Values())
	if !ok {
		t.Fatal("DynamicRedactionMarker() rejected bounded test values")
	}
	return marker
}

type hostileBoundaryError struct{ callsCount atomic.Int32 }

func (e *hostileBoundaryError) Error() string {
	e.callsCount.Add(1)
	return "hostile error"
}

func (e *hostileBoundaryError) Is(error) bool {
	e.callsCount.Add(1)
	return false
}

func (e *hostileBoundaryError) Unwrap() error {
	e.callsCount.Add(1)
	return nil
}

func (e *hostileBoundaryError) calls() int { return int(e.callsCount.Load()) }
