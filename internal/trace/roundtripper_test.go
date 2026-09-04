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

func TestTracingCapturesRequestAndResponse(t *testing.T) {
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
	if len(records) != 1 {
		t.Fatalf("got %d records, want 1", len(records))
	}
	rec := records[0]
	if rec["url"] != redactedValue {
		t.Fatalf("url = %v, want omission marker", rec["url"])
	}
	if rec["req_body"] != redactedValue {
		t.Fatalf("req_body = %v, want payload omission marker", rec["req_body"])
	}
	if rec["resp_body"] != redactedValue {
		t.Fatalf("resp_body = %v, want payload omission marker", rec["resp_body"])
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
	if len(auth) != 1 || auth[0] != "[redacted]" {
		t.Fatalf("Authorization header = %v, want redacted", headers["Authorization"])
	}
}

func TestTracingCapturesErrorResponse(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusTooManyRequests)
		io.WriteString(w, `{"error":"rate limited"}`)
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
	if len(records) != 1 {
		t.Fatalf("got %d records, want 1", len(records))
	}
	if records[0]["status"].(float64) != http.StatusTooManyRequests {
		t.Fatalf("status = %v, want 429", records[0]["status"])
	}
	if records[0]["resp_body"] != redactedValue {
		t.Fatalf("resp_body = %v, want payload omission marker", records[0]["resp_body"])
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

	if len(calls) != 2*n {
		t.Fatalf("got %d Write calls, want %d (one per record)", len(calls), 2*n)
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

// chatgpt-account-id identifies the account behind a ChatGPT subscription
// request. It is not a credential, but trace files sit in the working tree
// where they are easy to commit by accident, so it is redacted too.
func TestRedactHeadersRedactsAccountID(t *testing.T) {
	const accountID = "df8db0e8-0000-0000-0000-000000000000"
	redacted := redactHeaders(http.Header{
		"Authorization":      []string{"Bearer secret-token-value"},
		"Chatgpt-Account-Id": []string{accountID},
		"Cookie":             []string{"session=secret-cookie"},
		"Content-Type":       []string{"application/json"},
	})
	if got := redacted.Get("Chatgpt-Account-Id"); got != "[redacted]" {
		t.Fatalf("Chatgpt-Account-Id = %q, want %q", got, "[redacted]")
	}
	if got := redacted.Get("Authorization"); got != "[redacted]" {
		t.Fatalf("Authorization = %q, want %q", got, "[redacted]")
	}
	if got := redacted.Get("Cookie"); got != "[redacted]" {
		t.Fatalf("Cookie = %q, want %q", got, "[redacted]")
	}
	if got := redacted.Get("Content-Type"); got != "application/json" {
		t.Fatalf("Content-Type = %q, want it left untouched", got)
	}
}

func TestTracingRedactsResponseHeadersAndOmitsPayloads(t *testing.T) {
	const secret = "secret-cookie"
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Set-Cookie", "session="+secret)
		w.Header().Set("X-Echo", r.Header.Get("Cookie"))
		w.WriteHeader(http.StatusBadGateway)
		_, _ = io.WriteString(w, "echo="+secret)
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
	if string(body) != "echo="+secret {
		t.Fatalf("client body changed: %q", body)
	}
	if bytes.Contains(buf.Bytes(), []byte(secret)) {
		t.Fatal("trace leaked a secret from headers or body")
	}
	records := decodeRecords(t, buf)
	if got := records[0]["req_body"]; got != redactedValue {
		t.Fatalf("req_body = %v, want omission marker", got)
	}
	if got := records[0]["resp_body"]; got != redactedValue {
		t.Fatalf("resp_body = %v, want omission marker", got)
	}
	headers := records[0]["resp_headers"].(map[string]any)
	if _, ok := headers["X-Echo"]; ok {
		t.Fatalf("unknown response header was retained: %#v", headers["X-Echo"])
	}
	if got := headers["Set-Cookie"].([]any)[0]; got != redactedValue {
		t.Fatalf("Set-Cookie = %v, want redacted", got)
	}
}

func TestTracingOmitsUntrustedURLPartsAndErrorText(t *testing.T) {
	const secret = "fresh-oauth-token"
	buf := &bytes.Buffer{}
	client := &http.Client{Transport: NewRoundTripper(roundTripFunc(func(req *http.Request) (*http.Response, error) {
		return nil, errors.New("transport failed with " + secret)
	}), buf)}
	req, err := http.NewRequest(http.MethodGet, "https://example.test/"+secret+"?token="+secret, nil)
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Authorization", "Bearer "+secret)
	_, err = client.Do(req)
	if err == nil || !strings.Contains(err.Error(), secret) {
		t.Fatalf("client error = %v, want original transport error", err)
	}
	records := decodeRecords(t, buf)
	if len(records) != 1 {
		t.Fatalf("got %d records, want 1", len(records))
	}
	if records[0]["url"] != redactedValue {
		t.Fatalf("url = %v, want omission marker for untrusted path", records[0]["url"])
	}
	if records[0]["error"] != redactedValue {
		t.Fatalf("error = %v, want omission marker", records[0]["error"])
	}
	if bytes.Contains(buf.Bytes(), []byte(secret)) {
		t.Fatal("trace leaked fresh bearer token")
	}
}

type roundTripFunc func(*http.Request) (*http.Response, error)

func (f roundTripFunc) RoundTrip(req *http.Request) (*http.Response, error) {
	return f(req)
}
