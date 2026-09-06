package trace

import (
	"bytes"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
)

// decodeRecords splits the trace buffer into one decoded record per JSONL line.
func decodeRecords(t *testing.T, buf *bytes.Buffer) []map[string]any {
	t.Helper()
	var records []map[string]any
	for _, line := range bytes.Split(bytes.TrimRight(buf.Bytes(), "\n"), []byte("\n")) {
		if len(line) == 0 {
			continue
		}
		var rec map[string]any
		if err := json.Unmarshal(line, &rec); err != nil {
			t.Fatalf("record is not valid JSON: %v\nline: %s", err, line)
		}
		records = append(records, rec)
	}
	return records
}

// recordOfKind returns the single record with the given kind.
func recordOfKind(t *testing.T, records []map[string]any, kind string) map[string]any {
	t.Helper()
	var found map[string]any
	for _, rec := range records {
		if rec["kind"] == kind {
			if found != nil {
				t.Fatalf("got more than one %q record", kind)
			}
			found = rec
		}
	}
	if found == nil {
		t.Fatalf("no %q record in %v", kind, records)
	}
	return found
}

func TestTracingCapturesRequestAndResponseBodies(t *testing.T) {
	const sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n"
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		io.WriteString(w, sse)
	}))
	defer srv.Close()

	buf := &bytes.Buffer{}
	client := &http.Client{Transport: NewRoundTripper(http.DefaultTransport, buf)}

	const reqBody = `{"model":"m","messages":[{"role":"user","content":"hi"}]}`
	req, err := http.NewRequest(http.MethodPost, srv.URL+"/chat/completions", strings.NewReader(reqBody))
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Authorization", "Bearer secret-xyz")
	resp, err := client.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	body, _ := io.ReadAll(resp.Body)
	resp.Body.Close()

	if string(body) != sse {
		t.Fatalf("client received %q, want the streamed body unchanged", body)
	}

	// The key must never appear anywhere in the trace file (AGENTS.md secrets rule).
	if bytes.Contains(buf.Bytes(), []byte("secret-xyz")) {
		t.Fatal("trace leaked the API key")
	}

	records := decodeRecords(t, buf)
	if len(records) != 2 {
		t.Fatalf("got %d records, want 2 (request + response body)", len(records))
	}

	rec := recordOfKind(t, records, kindRequest)
	if want := srv.URL + "/chat/completions"; rec["url"] != want {
		t.Fatalf("url = %v, want %q", rec["url"], want)
	}
	if rec["req_body"] != reqBody {
		t.Fatalf("req_body = %v, want the request payload", rec["req_body"])
	}
	if rec["status"].(float64) != 200 {
		t.Fatalf("status = %v, want 200", rec["status"])
	}
	if rec["method"] != http.MethodPost {
		t.Fatalf("method = %v, want POST", rec["method"])
	}
	if _, ok := rec["duration_ms"].(float64); !ok {
		t.Fatalf("duration_ms missing or not numeric: %v", rec["duration_ms"])
	}
	headers, ok := rec["req_headers"].(map[string]any)
	if !ok {
		t.Fatalf("req_headers not an object: %v", rec["req_headers"])
	}
	auth, _ := headers["Authorization"].([]any)
	if len(auth) != 1 || auth[0] != redactedValue {
		t.Fatalf("Authorization header = %v, want redacted", headers["Authorization"])
	}
	if _, ok := rec["resp_body"]; ok {
		t.Fatalf("request record carries resp_body = %v, want it only in the body record", rec["resp_body"])
	}

	bodyRec := recordOfKind(t, records, kindResponseBody)
	if bodyRec["seq"] != rec["seq"] {
		t.Fatalf("body record seq = %v, want %v to join with the request", bodyRec["seq"], rec["seq"])
	}
	if bodyRec["resp_body"] != sse {
		t.Fatalf("resp_body = %v, want the streamed payload", bodyRec["resp_body"])
	}
	if got := bodyRec["resp_bytes"].(float64); int(got) != len(sse) {
		t.Fatalf("resp_bytes = %v, want %d", got, len(sse))
	}
}

func TestTracingCapturesErrorResponseBody(t *testing.T) {
	const errBody = `{"error":"rate limited"}`
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusTooManyRequests)
		io.WriteString(w, errBody)
	}))
	defer srv.Close()

	buf := &bytes.Buffer{}
	client := &http.Client{Transport: NewRoundTripper(http.DefaultTransport, buf)}
	resp, err := client.Do(mustRequest(t, srv.URL))
	if err != nil {
		t.Fatal(err)
	}
	io.ReadAll(resp.Body)
	resp.Body.Close()

	records := decodeRecords(t, buf)
	if got := recordOfKind(t, records, kindRequest)["status"].(float64); got != http.StatusTooManyRequests {
		t.Fatalf("status = %v, want 429", got)
	}
	if got := recordOfKind(t, records, kindResponseBody)["resp_body"]; got != errBody {
		t.Fatalf("resp_body = %v, want the error payload", got)
	}
}

// TestTracingRecordsBodyWhenClosedEarly covers a caller that abandons a stream
// without reading it to EOF: the partial body must still reach the trace.
func TestTracingRecordsBodyWhenClosedEarly(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		io.WriteString(w, strings.Repeat("x", 4096))
	}))
	defer srv.Close()

	buf := &bytes.Buffer{}
	client := &http.Client{Transport: NewRoundTripper(http.DefaultTransport, buf)}
	resp, err := client.Do(mustRequest(t, srv.URL))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := io.ReadFull(resp.Body, make([]byte, 16)); err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()

	bodyRec := recordOfKind(t, decodeRecords(t, buf), kindResponseBody)
	got, _ := bodyRec["resp_body"].(string)
	if len(got) == 0 || !strings.HasPrefix(got, "xxxx") {
		t.Fatalf("resp_body = %q, want the bytes read before Close", got)
	}
}

// callRecorder records the exact byte slice passed to every Write call. It
// lets a test assert that one full record (JSON line + trailing newline)
// reaches the underlying writer in a single Write, which is the only way to
// guarantee no interleaving when multiple RoundTrippers share one writer
// (concurrent agent runners writing to one trace file).
type callRecorder struct {
	mu    sync.Mutex
	calls [][]byte
}

func (c *callRecorder) Write(p []byte) (int, error) {
	cp := append([]byte(nil), p...)
	c.mu.Lock()
	c.calls = append(c.calls, cp)
	c.mu.Unlock()
	return len(p), nil
}

// TestConcurrentRoundTrippersWriteRecordsAtomically drives two RoundTrippers
// that share one writer from two goroutines. Each record must reach the
// writer as a single Write call so records from different trippers can never
// interleave mid-line, regardless of scheduling.
func TestConcurrentRoundTrippersWriteRecordsAtomically(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		io.WriteString(w, `{"ok":true}`)
	}))
	defer srv.Close()

	rec := &callRecorder{}
	rt1 := NewRoundTripper(http.DefaultTransport, rec)
	rt2 := NewRoundTripper(http.DefaultTransport, rec)

	const n = 50
	var wg sync.WaitGroup
	wg.Add(2)
	for _, rt := range []*RoundTripper{rt1, rt2} {
		client := &http.Client{Transport: rt}
		go func() {
			defer wg.Done()
			for i := 0; i < n; i++ {
				resp, err := client.Do(mustRequest(t, srv.URL))
				if err != nil {
					t.Errorf("round trip: %v", err)
					continue
				}
				io.ReadAll(resp.Body)
				resp.Body.Close()
			}
		}()
	}
	wg.Wait()

	rec.mu.Lock()
	calls := rec.calls
	rec.mu.Unlock()

	if len(calls) != 2*2*n {
		t.Fatalf("got %d Write calls, want %d (two records per exchange)", len(calls), 2*2*n)
	}
	for i, call := range calls {
		if len(call) == 0 || call[len(call)-1] != '\n' {
			t.Fatalf("call %d does not end with a newline: %q", i, call)
		}
		if got := bytes.Count(call, []byte("\n")); got != 1 {
			t.Fatalf("call %d has %d newlines, want exactly 1 (a split write would interleave): %q", i, got, call)
		}
		if !json.Valid(bytes.TrimSuffix(call, []byte("\n"))) {
			t.Fatalf("call %d is not one complete JSON record: %q", i, call)
		}
	}
}

func mustRequest(t *testing.T, url string) *http.Request {
	t.Helper()
	req, err := http.NewRequest(http.MethodPost, url+"/chat/completions", strings.NewReader("{}"))
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Authorization", "Bearer secret-xyz")
	return req
}

// Credential headers stay redacted even though payloads are recorded in full:
// trace files sit in the working tree where they are easy to commit by
// accident. chatgpt-account-id is not a credential but identifies the account.
func TestRedactHeadersKeepsPayloadHeadersAndRedactsCredentials(t *testing.T) {
	const accountID = "df8db0e8-0000-0000-0000-000000000000"
	redacted := redactHeaders(http.Header{
		"Authorization":      []string{"Bearer secret-token-value"},
		"Chatgpt-Account-Id": []string{accountID},
		"Cookie":             []string{"session=secret-cookie"},
		"Content-Type":       []string{"application/json"},
		"X-Request-Id":       []string{"req-123"},
	})
	for _, name := range []string{"Authorization", "Chatgpt-Account-Id", "Cookie"} {
		if got := redacted.Get(name); got != redactedValue {
			t.Fatalf("%s = %q, want %q", name, got, redactedValue)
		}
	}
	if got := redacted.Get("Content-Type"); got != "application/json" {
		t.Fatalf("Content-Type = %q, want it left untouched", got)
	}
	if got := redacted.Get("X-Request-Id"); got != "req-123" {
		t.Fatalf("X-Request-Id = %q, want it retained for debugging", got)
	}
}

func TestTracingRecordsResponseHeadersAndBody(t *testing.T) {
	const secret = "secret-cookie"
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Set-Cookie", "session="+secret)
		w.Header().Set("X-Echo", "echoed")
		w.WriteHeader(http.StatusBadGateway)
		_, _ = io.WriteString(w, `{"error":"upstream"}`)
	}))
	defer srv.Close()

	buf := &bytes.Buffer{}
	client := &http.Client{Transport: NewRoundTripper(http.DefaultTransport, buf)}
	req := mustRequest(t, srv.URL)
	req.Header.Set("Cookie", "session="+secret)
	resp, err := client.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	body, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatal(err)
	}
	_ = resp.Body.Close()
	if string(body) != `{"error":"upstream"}` {
		t.Fatalf("client body changed: %q", body)
	}
	if bytes.Contains(buf.Bytes(), []byte(secret)) {
		t.Fatal("trace leaked a credential header value")
	}
	records := decodeRecords(t, buf)
	reqRec := recordOfKind(t, records, kindRequest)
	headers := reqRec["resp_headers"].(map[string]any)
	if got := headers["X-Echo"].([]any)[0]; got != "echoed" {
		t.Fatalf("X-Echo = %v, want it retained for debugging", got)
	}
	if got := headers["Set-Cookie"].([]any)[0]; got != redactedValue {
		t.Fatalf("Set-Cookie = %v, want redacted", got)
	}
	if got := recordOfKind(t, records, kindResponseBody)["resp_body"]; got != `{"error":"upstream"}` {
		t.Fatalf("resp_body = %v, want the error payload", got)
	}
}

func TestTracingRecordsTransportErrorAndURL(t *testing.T) {
	const token = "fresh-oauth-token"
	buf := &bytes.Buffer{}
	client := &http.Client{Transport: NewRoundTripper(roundTripFunc(func(req *http.Request) (*http.Response, error) {
		return nil, errors.New("transport failed: connection refused")
	}), buf)}
	req, err := http.NewRequest(http.MethodGet, "https://example.test/v1/responses", nil)
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Authorization", "Bearer "+token)
	_, err = client.Do(req)
	if err == nil || !strings.Contains(err.Error(), "connection refused") {
		t.Fatalf("client error = %v, want original transport error", err)
	}
	records := decodeRecords(t, buf)
	if len(records) != 1 {
		t.Fatalf("got %d records, want 1", len(records))
	}
	if records[0]["url"] != "https://example.test/v1/responses" {
		t.Fatalf("url = %v, want the full request URL", records[0]["url"])
	}
	if got, _ := records[0]["error"].(string); !strings.Contains(got, "connection refused") {
		t.Fatalf("error = %v, want the transport error text", records[0]["error"])
	}
	if bytes.Contains(buf.Bytes(), []byte(token)) {
		t.Fatal("trace leaked the bearer token")
	}
}

type roundTripFunc func(*http.Request) (*http.Response, error)

func (f roundTripFunc) RoundTrip(req *http.Request) (*http.Response, error) {
	return f(req)
}
