package trace

import (
	"bytes"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
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
	if rec["req_body"] != reqBody {
		t.Fatalf("req_body = %v, want %q", rec["req_body"], reqBody)
	}
	if rec["resp_body"] != sse {
		t.Fatalf("resp_body = %v, want the raw SSE", rec["resp_body"])
	}
	if rec["status"].(float64) != 200 {
		t.Fatalf("status = %v, want 200", rec["status"])
	}
	if rec["method"] != http.MethodPost {
		t.Fatalf("method = %v, want POST", rec["method"])
	}
	if !strings.Contains(rec["url"].(string), "/chat/completions") {
		t.Fatalf("url = %v, want it to contain the path", rec["url"])
	}
	if _, ok := rec["duration_ms"].(float64); !ok {
		t.Fatalf("duration_ms missing or not numeric: %v", rec["duration_ms"])
	}
	headers, ok := rec["req_headers"].(map[string]any)
	if !ok {
		t.Fatalf("req_headers not an object: %v", rec["req_headers"])
	}
	auth, _ := headers["Authorization"].([]any)
	if len(auth) != 1 || auth[0] != "Bearer [redacted]" {
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
	if !strings.Contains(records[0]["resp_body"].(string), "rate limited") {
		t.Fatalf("resp_body = %v, want the error body captured", records[0]["resp_body"])
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
