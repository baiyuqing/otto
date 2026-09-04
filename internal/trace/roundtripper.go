// Package trace captures safe HTTP metadata (request + response) to a JSONL
// file for offline audit and analysis. It is a development tool, gated by
// OTTO_TRACE, off by default. It plugs in as an http.RoundTripper so no
// provider, agent, or session code needs to change.
package trace

import (
	"encoding/json"
	"io"
	"net/http"
	"sync"
	"sync/atomic"
	"time"
)

// redactedHeaders carry credentials or account identity and are replaced
// before the request is recorded. The value is never written to the trace
// (AGENTS.md secrets rule). Account identifiers are redacted as well.
var redactedHeaders = map[string]bool{
	"Authorization":       true,
	"Proxy-Authorization": true,
	"Api-Key":             true,
	"X-Api-Key":           true,
	"Chatgpt-Account-Id":  true,
	"Cookie":              true,
	"Set-Cookie":          true,
}

// redactedValue replaces every redacted header value. It deliberately keeps no
// shape information about what it replaced.
const redactedValue = "[redacted]"

type record struct {
	TS          string      `json:"ts"`
	Seq         int64       `json:"seq"`
	Method      string      `json:"method"`
	URL         string      `json:"url"`
	ReqHeaders  http.Header `json:"req_headers"`
	ReqBody     string      `json:"req_body"`
	Status      int         `json:"status"`
	DurationMS  int64       `json:"duration_ms"`
	RespHeaders http.Header `json:"resp_headers"`
	RespBody    string      `json:"resp_body"`
	Error       string      `json:"error,omitempty"`
}

// RoundTripper wraps a base transport and appends one JSONL record per HTTP
// call. Records are written after response headers arrive; response bodies are
// returned untouched and never buffered.
type RoundTripper struct {
	base http.RoundTripper
	mu   sync.Mutex
	w    io.Writer
	seq  atomic.Int64
	now  func() time.Time
}

// NewRoundTripper traces calls made through base, writing JSONL to w. If base is
// nil, http.DefaultTransport is used. Payloads and error text are never stored.
func NewRoundTripper(base http.RoundTripper, w io.Writer) *RoundTripper {
	if base == nil {
		base = http.DefaultTransport
	}
	return &RoundTripper{base: base, w: w, now: time.Now}
}

func (rt *RoundTripper) RoundTrip(req *http.Request) (*http.Response, error) {
	seq := rt.seq.Add(1)
	start := rt.now()

	rec := record{
		TS:         start.UTC().Format(time.RFC3339Nano),
		Seq:        seq,
		Method:     safeMethod(req.Method),
		URL:        redactedValue,
		ReqHeaders: redactHeaders(req.Header),
		ReqBody:    redactedValue,
	}

	resp, err := rt.base.RoundTrip(req)
	if err != nil {
		rec.Error = redactedValue
		rec.DurationMS = rt.now().Sub(start).Milliseconds()
		rt.write(rec)
		return nil, err
	}

	rec.Status = resp.StatusCode
	rec.RespHeaders = redactHeaders(resp.Header)
	rec.RespBody = redactedValue
	rec.DurationMS = rt.now().Sub(start).Milliseconds()
	rt.write(rec)
	return resp, nil
}

func (rt *RoundTripper) write(rec record) {
	line, err := json.Marshal(rec)
	if err != nil {
		return
	}
	line = append(line, '\n')
	rt.mu.Lock()
	defer rt.mu.Unlock()
	rt.w.Write(line)
}

// redactHeaders retains only known-safe protocol headers and replaces known
// sensitive headers. The original headers are left untouched.
func redactHeaders(h http.Header) http.Header {
	redacted := make(http.Header)
	for name, values := range h {
		canonical := http.CanonicalHeaderKey(name)
		switch {
		case redactedHeaders[canonical]:
			redacted[canonical] = []string{redactedValue}
		case canonical == "Content-Type" || canonical == "Accept":
			for _, value := range values {
				if value == "application/json" || value == "text/event-stream" {
					redacted[canonical] = append(redacted[canonical], value)
				}
			}
		}
	}
	return redacted
}

func safeMethod(method string) string {
	switch method {
	case http.MethodGet, http.MethodHead, http.MethodPost, http.MethodPut,
		http.MethodPatch, http.MethodDelete, http.MethodConnect, http.MethodOptions,
		http.MethodTrace:
		return method
	default:
		return redactedValue
	}
}
