// Package trace captures provider HTTP exchanges (request + response, headers
// and bodies) to a JSONL file for offline debugging. It is a development tool,
// gated by OTTO_TRACE, off by default. It plugs in as an http.RoundTripper so
// no provider, agent, or session code needs to change.
//
// Bodies are recorded verbatim, so a trace file contains conversation content.
// Credential and account headers are the one exception: they are replaced with
// a redaction marker (AGENTS.md secrets rule).
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
// before the exchange is recorded. The value is never written to the trace.
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

// Record kinds. Each exchange produces one kindRequest record as soon as the
// response headers arrive, then one kindResponseBody record carrying the same
// seq once the response body is closed or drained. Splitting them keeps the
// request visible even when a stream hangs or is interrupted.
const (
	kindRequest      = "request"
	kindResponseBody = "response_body"
)

// unreplayableBody marks a request whose body could not be read a second time
// (no GetBody), so an empty req_body is never mistaken for an empty request.
const unreplayableBody = "[body not replayable]"

type record struct {
	TS          string      `json:"ts"`
	Seq         int64       `json:"seq"`
	Kind        string      `json:"kind"`
	Method      string      `json:"method,omitempty"`
	URL         string      `json:"url,omitempty"`
	ReqHeaders  http.Header `json:"req_headers,omitempty"`
	ReqBody     string      `json:"req_body,omitempty"`
	Status      int         `json:"status,omitempty"`
	DurationMS  int64       `json:"duration_ms"`
	RespHeaders http.Header `json:"resp_headers,omitempty"`
	RespBody    string      `json:"resp_body,omitempty"`
	RespBytes   int         `json:"resp_bytes,omitempty"`
	Error       string      `json:"error,omitempty"`
}

// RoundTripper wraps a base transport and appends JSONL records per HTTP call.
// The response body is passed through unchanged; a copy accumulates in memory
// and is written once the caller closes or drains it.
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
		Kind:       kindRequest,
		Method:     req.Method,
		URL:        req.URL.String(),
		ReqHeaders: redactHeaders(req.Header),
		ReqBody:    requestBody(req),
	}

	resp, err := rt.base.RoundTrip(req)
	if err != nil {
		rec.Error = err.Error()
		rec.DurationMS = rt.now().Sub(start).Milliseconds()
		rt.write(rec)
		return nil, err
	}

	rec.Status = resp.StatusCode
	rec.RespHeaders = redactHeaders(resp.Header)
	rec.DurationMS = rt.now().Sub(start).Milliseconds()
	rt.write(rec)

	if resp.Body != nil {
		resp.Body = &bodyRecorder{rc: resp.Body, rt: rt, seq: seq, start: start}
	}
	return resp, nil
}

// requestBody re-reads the request payload through GetBody, which
// http.NewRequest sets for the in-memory readers the providers use. The
// original req.Body is left untouched so the transport still sends it.
func requestBody(req *http.Request) string {
	if req.Body == nil || req.Body == http.NoBody {
		return ""
	}
	if req.GetBody == nil {
		return unreplayableBody
	}
	body, err := req.GetBody()
	if err != nil {
		return unreplayableBody
	}
	defer body.Close()
	// ponytail: whole payload held in memory; cap it if traces ever grow
	// large enough to matter for a debug-only tool.
	data, err := io.ReadAll(body)
	if err != nil {
		return unreplayableBody
	}
	return string(data)
}

// bodyRecorder passes the response body through unchanged while accumulating a
// copy, and writes the copy once the stream ends or the caller closes it.
type bodyRecorder struct {
	rc    io.ReadCloser
	rt    *RoundTripper
	seq   int64
	start time.Time
	buf   bytes.Buffer
	once  sync.Once
}

func (b *bodyRecorder) Read(p []byte) (int, error) {
	n, err := b.rc.Read(p)
	b.buf.Write(p[:n])
	if err != nil {
		b.flush(err)
	}
	return n, err
}

func (b *bodyRecorder) Close() error {
	err := b.rc.Close()
	b.flush(nil)
	return err
}

func (b *bodyRecorder) flush(err error) {
	b.once.Do(func() {
		rec := record{
			TS:         b.rt.now().UTC().Format(time.RFC3339Nano),
			Seq:        b.seq,
			Kind:       kindResponseBody,
			DurationMS: b.rt.now().Sub(b.start).Milliseconds(),
			RespBody:   b.buf.String(),
			RespBytes:  b.buf.Len(),
		}
		if err != nil && err != io.EOF {
			rec.Error = err.Error()
		}
		b.rt.write(rec)
	})
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

// redactHeaders replaces credential and account headers and keeps everything
// else as sent. The original headers are left untouched.
func redactHeaders(h http.Header) http.Header {
	redacted := make(http.Header, len(h))
	for name, values := range h {
		canonical := http.CanonicalHeaderKey(name)
		if redactedHeaders[canonical] {
			redacted[canonical] = []string{redactedValue}
			continue
		}
		redacted[canonical] = append([]string(nil), values...)
	}
	return redacted
}
