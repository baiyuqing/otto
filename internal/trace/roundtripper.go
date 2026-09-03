// Package trace captures the raw provider HTTP wire (request + response) to a
// JSONL file for offline audit and analysis. It is a development tool, gated by
// OTTO_TRACE, off by default. It plugs in as an http.RoundTripper so no
// provider, agent, or session code needs to change.
package trace

import (
	"bytes"
	"encoding/json"
	"io"
	"net/http"
	"sync"
	"sync/atomic"
	"time"
)

// redactedHeaders carry credentials or account identity and are replaced
// before the request is recorded. The value is never written to the trace
// (AGENTS.md secrets rule). chatgpt-account-id is not a credential, but trace
// files are written into the working tree, so it is redacted with the rest.
var redactedHeaders = map[string]bool{
	"Authorization":       true,
	"Proxy-Authorization": true,
	"Api-Key":             true,
	"X-Api-Key":           true,
	"Chatgpt-Account-Id":  true,
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
// call. Response records are written when the response body is closed, so the
// captured body is the full raw stream (including SSE).
type RoundTripper struct {
	base http.RoundTripper
	mu   sync.Mutex
	w    io.Writer
	seq  atomic.Int64
	now  func() time.Time
}

// NewRoundTripper traces calls made through base, writing JSONL to w. If base is
// nil, http.DefaultTransport is used.
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
		Method:     req.Method,
		URL:        req.URL.String(),
		ReqHeaders: redactHeaders(req.Header),
		ReqBody:    readAndRestoreBody(req),
	}

	resp, err := rt.base.RoundTrip(req)
	if err != nil {
		rec.Error = err.Error()
		rec.DurationMS = rt.now().Sub(start).Milliseconds()
		rt.write(rec)
		return nil, err
	}

	rec.Status = resp.StatusCode
	rec.RespHeaders = resp.Header.Clone()
	// Tee the body so we capture the raw stream as the client reads it; the
	// record lands when the client closes the body.
	// ponytail: buffers the whole response in memory before writing one record;
	// fine for dev traces, memory use on very large responses is the known ceiling.
	resp.Body = &tee{
		src: resp.Body,
		buf: &bytes.Buffer{},
		onClose: func(buf *bytes.Buffer) {
			rec.RespBody = buf.String()
			rec.DurationMS = rt.now().Sub(start).Milliseconds()
			rt.write(rec)
		},
	}
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

// redactHeaders returns a clone with credential headers replaced. The original
// request headers are left untouched.
func redactHeaders(h http.Header) http.Header {
	clone := h.Clone()
	if clone == nil {
		clone = http.Header{}
	}
	for name := range clone {
		if redactedHeaders[http.CanonicalHeaderKey(name)] {
			clone.Set(name, redactedValue)
		}
	}
	return clone
}

// readAndRestoreBody reads the request body for the record and restores it so
// the real request can still be sent. Request bodies here are small JSON.
func readAndRestoreBody(req *http.Request) string {
	if req.Body == nil {
		return ""
	}
	body, err := io.ReadAll(req.Body)
	req.Body.Close()
	if err != nil {
		return ""
	}
	req.Body = io.NopCloser(bytes.NewReader(body))
	req.ContentLength = int64(len(body))
	return string(body)
}

// tee copies everything read from src into buf and runs onClose once, when the
// caller closes the body.
type tee struct {
	src     io.ReadCloser
	buf     *bytes.Buffer
	onClose func(*bytes.Buffer)
	closed  bool
}

func (t *tee) Read(p []byte) (int, error) {
	n, err := t.src.Read(p)
	if n > 0 {
		t.buf.Write(p[:n])
	}
	return n, err
}

func (t *tee) Close() error {
	err := t.src.Close()
	if !t.closed {
		t.closed = true
		t.onClose(t.buf)
	}
	return err
}
