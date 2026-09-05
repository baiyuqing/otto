package server

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
)

// runnerFunc adapts a plain function to app.Runner, mirroring the test
// double in internal/app/controller_test.go.
type runnerFunc func(context.Context, string, func(agent.Event)) error

func (f runnerFunc) Run(ctx context.Context, text string, emit func(agent.Event)) error {
	return f(ctx, text, emit)
}

func (f runnerFunc) Compact(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
	return agent.CompactionResult{Noop: true}, nil
}

func noopRun(context.Context, string, func(agent.Event)) error { return nil }

var testSandbox = app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true}

func newTestController(t *testing.T, id string, run func(context.Context, string, func(agent.Event)) error) *app.Controller {
	t.Helper()
	hdr := session.Header{ID: id, Workspace: "/tmp/ws", Provider: "openai-compatible", Model: "test-model"}
	sess := session.NewMemory(hdr)
	ctrl, err := app.New(app.SessionReplacement{Session: sess, Runner: runnerFunc(run)},
		app.WithRuntimeInfo(app.RuntimeInfo{Provider: "openai-compatible", Model: "test-model", ContextWindow: 128000, Sandbox: testSandbox}),
	)
	if err != nil {
		t.Fatalf("app.New: %v", err)
	}
	return ctrl
}

func newServerForTest(t *testing.T, opts Options) (*Server, *httptest.Server) {
	t.Helper()
	if opts.Logger == nil {
		opts.Logger = slog.New(slog.NewTextHandler(io.Discard, nil))
	}
	s := New(context.Background(), opts)
	ts := httptest.NewServer(s.Handler())
	t.Cleanup(ts.Close)
	return s, ts
}

func doJSON(t *testing.T, ts *httptest.Server, method, path string, body any) *http.Response {
	t.Helper()
	var r io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			t.Fatalf("marshal body: %v", err)
		}
		r = bytes.NewReader(b)
	}
	req, err := http.NewRequest(method, ts.URL+path, r)
	if err != nil {
		t.Fatalf("new request: %v", err)
	}
	resp, err := ts.Client().Do(req)
	if err != nil {
		t.Fatalf("do %s %s: %v", method, path, err)
	}
	return resp
}

func decodeJSON(t *testing.T, resp *http.Response, v any) {
	t.Helper()
	defer resp.Body.Close()
	if err := json.NewDecoder(resp.Body).Decode(v); err != nil {
		t.Fatalf("decode response: %v", err)
	}
}

func getBody(t *testing.T, ts *httptest.Server, path string) string {
	t.Helper()
	resp, err := ts.Client().Get(ts.URL + path)
	if err != nil {
		t.Fatalf("GET %s: %v", path, err)
	}
	defer resp.Body.Close()
	b, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatalf("read body: %v", err)
	}
	return string(b)
}

func waitTurnDone(t *testing.T, ts *httptest.Server, sessionID, turnID string) turnSummary {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	var sum turnSummary
	for time.Now().Before(deadline) {
		resp := doJSON(t, ts, "GET", "/v1/sessions/"+sessionID+"/turns/"+turnID, nil)
		decodeJSON(t, resp, &sum)
		if sum.Status != turnRunning {
			return sum
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatalf("turn %s did not finish in time, last status %q", turnID, sum.Status)
	return sum
}

// sseFrame is one parsed "id/event/data" SSE frame.
type sseFrame struct{ id, event, data string }

type sseReader struct{ r *bufio.Reader }

func newSSEReader(r io.Reader) *sseReader { return &sseReader{r: bufio.NewReader(r)} }

// next blocks until one full frame is available, or returns an error (e.g.
// io.EOF) once the underlying stream ends.
func (s *sseReader) next() (sseFrame, error) {
	var f sseFrame
	for {
		line, err := s.r.ReadString('\n')
		if err != nil {
			return f, err
		}
		line = strings.TrimRight(line, "\n")
		switch {
		case line == "":
			if f.event != "" {
				return f, nil
			}
		case strings.HasPrefix(line, "id: "):
			f.id = strings.TrimPrefix(line, "id: ")
		case strings.HasPrefix(line, "event: "):
			f.event = strings.TrimPrefix(line, "event: ")
		case strings.HasPrefix(line, "data: "):
			f.data = strings.TrimPrefix(line, "data: ")
		}
	}
}

func readAllSSEFrames(t *testing.T, r io.Reader) []sseFrame {
	t.Helper()
	sr := newSSEReader(r)
	var frames []sseFrame
	for {
		f, err := sr.next()
		if err != nil {
			return frames
		}
		frames = append(frames, f)
	}
}

func TestCreateSession(t *testing.T) {
	var createCalls int32
	opts := Options{
		Create: func(ctx context.Context) (*app.Controller, error) {
			atomic.AddInt32(&createCalls, 1)
			id, _ := newID()
			return newTestController(t, id, noopRun), nil
		},
	}
	_, ts := newServerForTest(t, opts)

	resp := doJSON(t, ts, "POST", "/v1/sessions", nil)
	if resp.StatusCode != http.StatusCreated {
		t.Fatalf("status = %d, want 201", resp.StatusCode)
	}
	var got sessionWire
	decodeJSON(t, resp, &got)
	if got.ID == "" {
		t.Fatal("expected non-empty session id")
	}
	if got.Provider != "openai-compatible" || got.Model != "test-model" {
		t.Fatalf("unexpected session object: %+v", got)
	}
	if got.Sandbox.Summary == "" {
		t.Fatal("expected non-empty sandbox summary")
	}

	metricsBody := getBody(t, ts, "/metrics")
	if !strings.Contains(metricsBody, "otto_sessions_open 1") {
		t.Fatalf("expected otto_sessions_open 1 in metrics:\n%s", metricsBody)
	}

	healthResp := doJSON(t, ts, "GET", "/healthz", nil)
	var health healthzWire
	decodeJSON(t, healthResp, &health)
	if health.SessionsOpen != 1 {
		t.Fatalf("healthz sessions_open = %d, want 1", health.SessionsOpen)
	}
	if atomic.LoadInt32(&createCalls) != 1 {
		t.Fatalf("Create called %d times, want 1", createCalls)
	}
}

func TestResumeUnknownSession(t *testing.T) {
	opts := Options{
		Open: func(ctx context.Context, id string) (*app.Controller, error) {
			return nil, fmt.Errorf("%w: %s", ErrSessionNotFound, id)
		},
	}
	_, ts := newServerForTest(t, opts)
	resp := doJSON(t, ts, "POST", "/v1/sessions", map[string]string{"resume": "nope"})
	if resp.StatusCode != http.StatusNotFound {
		t.Fatalf("status = %d, want 404", resp.StatusCode)
	}
	var body errorBody
	decodeJSON(t, resp, &body)
	if body.Error.Code != "not_found" {
		t.Fatalf("error code = %q, want not_found", body.Error.Code)
	}
}

func TestResumeAlreadyOpenDoesNotCallOpenAgain(t *testing.T) {
	id, _ := newID()
	var openCalls int32
	opts := Options{
		Create: func(ctx context.Context) (*app.Controller, error) {
			return newTestController(t, id, noopRun), nil
		},
		Open: func(ctx context.Context, gotID string) (*app.Controller, error) {
			atomic.AddInt32(&openCalls, 1)
			return newTestController(t, gotID, noopRun), nil
		},
	}
	_, ts := newServerForTest(t, opts)

	resp := doJSON(t, ts, "POST", "/v1/sessions", nil)
	if resp.StatusCode != http.StatusCreated {
		t.Fatalf("create status = %d", resp.StatusCode)
	}
	resp.Body.Close()

	resp2 := doJSON(t, ts, "POST", "/v1/sessions", map[string]string{"resume": id})
	if resp2.StatusCode != http.StatusOK {
		t.Fatalf("resume status = %d, want 200", resp2.StatusCode)
	}
	resp2.Body.Close()
	if atomic.LoadInt32(&openCalls) != 0 {
		t.Fatalf("Open called %d times, want 0", openCalls)
	}
}

func TestResumeConcurrentCallsOpenOnce(t *testing.T) {
	id, _ := newID()
	var openCalls int32
	release := make(chan struct{})
	opts := Options{
		Open: func(ctx context.Context, gotID string) (*app.Controller, error) {
			atomic.AddInt32(&openCalls, 1)
			<-release
			return newTestController(t, gotID, noopRun), nil
		},
	}
	_, ts := newServerForTest(t, opts)

	const n = 8
	var wg sync.WaitGroup
	statuses := make([]int, n)
	start := make(chan struct{})
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			<-start
			resp := doJSON(t, ts, "POST", "/v1/sessions", map[string]string{"resume": id})
			statuses[i] = resp.StatusCode
			resp.Body.Close()
		}(i)
	}
	close(start)

	deadline := time.Now().Add(2 * time.Second)
	for atomic.LoadInt32(&openCalls) == 0 && time.Now().Before(deadline) {
		time.Sleep(time.Millisecond)
	}
	close(release)
	wg.Wait()

	if got := atomic.LoadInt32(&openCalls); got != 1 {
		t.Fatalf("Open called %d times, want 1", got)
	}
	for i, status := range statuses {
		if status != http.StatusOK {
			t.Fatalf("resume %d status = %d, want 200", i, status)
		}
	}
}

func TestGetSessionNotFoundThenDelete(t *testing.T) {
	opts := Options{
		Create: func(ctx context.Context) (*app.Controller, error) {
			id, _ := newID()
			return newTestController(t, id, noopRun), nil
		},
	}
	_, ts := newServerForTest(t, opts)

	resp := doJSON(t, ts, "GET", "/v1/sessions/does-not-exist", nil)
	if resp.StatusCode != http.StatusNotFound {
		t.Fatalf("status = %d, want 404", resp.StatusCode)
	}
	resp.Body.Close()

	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)

	getResp := doJSON(t, ts, "GET", "/v1/sessions/"+sess.ID, nil)
	if getResp.StatusCode != http.StatusOK {
		t.Fatalf("get status = %d, want 200", getResp.StatusCode)
	}
	getResp.Body.Close()

	delResp := doJSON(t, ts, "DELETE", "/v1/sessions/"+sess.ID, nil)
	if delResp.StatusCode != http.StatusNoContent {
		t.Fatalf("delete status = %d, want 204", delResp.StatusCode)
	}
	delResp.Body.Close()

	getResp2 := doJSON(t, ts, "GET", "/v1/sessions/"+sess.ID, nil)
	if getResp2.StatusCode != http.StatusNotFound {
		t.Fatalf("get after delete status = %d, want 404", getResp2.StatusCode)
	}
	getResp2.Body.Close()

	metricsBody := getBody(t, ts, "/metrics")
	if !strings.Contains(metricsBody, "otto_sessions_open 0") {
		t.Fatalf("expected otto_sessions_open 0 after delete:\n%s", metricsBody)
	}
}

func TestHistoryEmptyArray(t *testing.T) {
	opts := Options{Create: func(ctx context.Context) (*app.Controller, error) {
		id, _ := newID()
		return newTestController(t, id, noopRun), nil
	}}
	_, ts := newServerForTest(t, opts)
	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)

	body := getBody(t, ts, "/v1/sessions/"+sess.ID+"/history")
	if strings.TrimSpace(body) != "[]" {
		t.Fatalf("history body = %q, want []", body)
	}
}

func TestListSessionsWorksWithEmptyList(t *testing.T) {
	opts := Options{
		List: func(ctx context.Context) (session.ListResult, error) {
			return session.ListResult{}, nil
		},
	}
	_, ts := newServerForTest(t, opts)
	body := getBody(t, ts, "/v1/sessions")
	var got sessionListResponse
	if err := json.Unmarshal([]byte(body), &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if len(got.Sessions) != 0 {
		t.Fatalf("sessions = %+v, want empty", got.Sessions)
	}
}

func TestListSessionsMergesOpenSessions(t *testing.T) {
	opts := Options{
		Create: func(ctx context.Context) (*app.Controller, error) {
			id, _ := newID()
			return newTestController(t, id, noopRun), nil
		},
		List: func(ctx context.Context) (session.ListResult, error) {
			return session.ListResult{}, nil
		},
	}
	_, ts := newServerForTest(t, opts)
	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)

	body := getBody(t, ts, "/v1/sessions")
	var got sessionListResponse
	if err := json.Unmarshal([]byte(body), &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if len(got.Sessions) != 1 {
		t.Fatalf("sessions = %+v, want 1 row", got.Sessions)
	}
	row := got.Sessions[0]
	if row.ID != sess.ID || !row.Open || row.Path != "" {
		t.Fatalf("row = %+v", row)
	}
}

func TestPostTurnStreamsSSE(t *testing.T) {
	run := func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventAgentStarted})
		emit(agent.Event{Type: agent.EventTextDelta, Text: "hel"})
		emit(agent.Event{Type: agent.EventTextDelta, Text: "lo"})
		emit(agent.Event{Type: agent.EventToolCallStarted, ToolName: "bash", ToolCallID: "c1"})
		emit(agent.Event{Type: agent.EventToolCallFinished, ToolName: "bash", ToolCallID: "c1", ToolResult: tool.Result{Content: "ok"}})
		emit(agent.Event{Type: agent.EventProviderUsage, Usage: model.Usage{InputTokens: 5, OutputTokens: 7}, UsagePresent: true})
		emit(agent.Event{Type: agent.EventAgentFinished})
		return nil
	}
	opts := Options{Create: func(ctx context.Context) (*app.Controller, error) {
		id, _ := newID()
		return newTestController(t, id, run), nil
	}}
	_, ts := newServerForTest(t, opts)
	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)

	req, _ := http.NewRequest("POST", ts.URL+"/v1/sessions/"+sess.ID+"/turns", strings.NewReader(`{"text":"hi"}`))
	resp, err := ts.Client().Do(req)
	if err != nil {
		t.Fatalf("post turn: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status = %d, want 200", resp.StatusCode)
	}
	if ct := resp.Header.Get("Content-Type"); ct != "text/event-stream" {
		t.Fatalf("Content-Type = %q", ct)
	}

	frames := readAllSSEFrames(t, resp.Body)
	if len(frames) == 0 {
		t.Fatal("expected at least one SSE frame")
	}
	if frames[0].id != "0" {
		t.Fatalf("first frame id = %q, want 0", frames[0].id)
	}
	if frames[0].event != "agent_started" {
		t.Fatalf("first frame event = %q, want agent_started", frames[0].event)
	}
	var started wireEvent
	if err := json.Unmarshal([]byte(frames[0].data), &started); err != nil {
		t.Fatalf("unmarshal agent_started data: %v", err)
	}
	if started.TurnID == "" {
		t.Fatal("expected non-empty turn_id in agent_started data")
	}

	sum := waitTurnDone(t, ts, sess.ID, started.TurnID)
	if sum.Text != "hello" {
		t.Fatalf("summary text = %q, want hello", sum.Text)
	}
	if sum.Status != turnOK {
		t.Fatalf("status = %q, want ok", sum.Status)
	}
	want := model.Usage{InputTokens: 5, OutputTokens: 7}
	if sum.Usage != want {
		t.Fatalf("usage = %+v, want %+v", sum.Usage, want)
	}

	metricsBody := getBody(t, ts, "/metrics")
	for _, want := range []string{
		`otto_turns_total{status="ok"} 1`,
		`otto_tool_calls_total{tool="bash",status="ok"} 1`,
		`otto_provider_tokens_total{kind="input"} 5`,
		`otto_provider_tokens_total{kind="output"} 7`,
	} {
		if !strings.Contains(metricsBody, want) {
			t.Errorf("missing metric %q:\n%s", want, metricsBody)
		}
	}
}

func TestPostTurnStreamFalse(t *testing.T) {
	run := func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventTextDelta, Text: "ok"})
		return nil
	}
	opts := Options{Create: func(ctx context.Context) (*app.Controller, error) {
		id, _ := newID()
		return newTestController(t, id, run), nil
	}}
	_, ts := newServerForTest(t, opts)
	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)

	resp := doJSON(t, ts, "POST", "/v1/sessions/"+sess.ID+"/turns", map[string]any{"text": "hi", "stream": false})
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status = %d, want 200", resp.StatusCode)
	}
	if ct := resp.Header.Get("Content-Type"); !strings.HasPrefix(ct, "application/json") {
		t.Fatalf("Content-Type = %q", ct)
	}
	var sum turnSummary
	decodeJSON(t, resp, &sum)
	if sum.Status != turnOK || sum.Text != "ok" {
		t.Fatalf("summary = %+v", sum)
	}
}

func TestStartTurnAndGetTurnCarryUserTrigger(t *testing.T) {
	opts := Options{Create: func(ctx context.Context) (*app.Controller, error) {
		id, _ := newID()
		return newTestController(t, id, noopRun), nil
	}}
	_, ts := newServerForTest(t, opts)
	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)

	resp := doJSON(t, ts, "POST", "/v1/sessions/"+sess.ID+"/turns", map[string]any{"text": "hi", "stream": false})
	var sum turnSummary
	decodeJSON(t, resp, &sum)
	if sum.Trigger != triggerUser {
		t.Fatalf("POST turn trigger = %q, want %q", sum.Trigger, triggerUser)
	}

	getResp := doJSON(t, ts, "GET", "/v1/sessions/"+sess.ID+"/turns/"+sum.ID, nil)
	var got turnSummary
	decodeJSON(t, getResp, &got)
	if got.Trigger != triggerUser {
		t.Fatalf("GET turn trigger = %q, want %q", got.Trigger, triggerUser)
	}

	sessResp := doJSON(t, ts, "GET", "/v1/sessions/"+sess.ID, nil)
	var sessGot sessionWire
	decodeJSON(t, sessResp, &sessGot)
	if sessGot.Turn == nil || sessGot.Turn.Trigger != triggerUser {
		t.Fatalf("session turn = %+v, want trigger %q", sessGot.Turn, triggerUser)
	}
}

func TestTurnEventsAfterAndLastEventID(t *testing.T) {
	gate := make(chan struct{})
	run := func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventAgentStarted})
		emit(agent.Event{Type: agent.EventTextDelta, Text: "a"})
		emit(agent.Event{Type: agent.EventTextDelta, Text: "b"})
		<-gate
		emit(agent.Event{Type: agent.EventTextDelta, Text: "c"})
		emit(agent.Event{Type: agent.EventAgentFinished})
		return nil
	}
	opts := Options{Create: func(ctx context.Context) (*app.Controller, error) {
		id, _ := newID()
		return newTestController(t, id, run), nil
	}}
	_, ts := newServerForTest(t, opts)
	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)

	primaryReq, _ := http.NewRequest("POST", ts.URL+"/v1/sessions/"+sess.ID+"/turns", strings.NewReader(`{"text":"hi"}`))
	primaryResp, err := ts.Client().Do(primaryReq)
	if err != nil {
		t.Fatalf("post turn: %v", err)
	}
	defer primaryResp.Body.Close()
	primary := newSSEReader(primaryResp.Body)

	f0, err := primary.next()
	if err != nil {
		t.Fatalf("read agent_started: %v", err)
	}
	var started wireEvent
	if err := json.Unmarshal([]byte(f0.data), &started); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	turnID := started.TurnID
	if turnID == "" {
		t.Fatal("expected turn id")
	}
	if _, err := primary.next(); err != nil { // "a"
		t.Fatalf("read a: %v", err)
	}
	if _, err := primary.next(); err != nil { // "b"
		t.Fatalf("read b: %v", err)
	}

	// after=1 -> resume from seq 2 ("b").
	afterResp := doJSON(t, ts, "GET", "/v1/sessions/"+sess.ID+"/turns/"+turnID+"/events?after=1", nil)
	if afterResp.StatusCode != http.StatusOK {
		t.Fatalf("events status = %d, want 200", afterResp.StatusCode)
	}
	afterReader := newSSEReader(afterResp.Body)
	fb, err := afterReader.next()
	if err != nil {
		t.Fatalf("read after seq1: %v", err)
	}
	if fb.id != "2" {
		t.Fatalf("first frame id = %q, want 2", fb.id)
	}

	close(gate)

	fc, err := afterReader.next()
	if err != nil || fc.id != "3" {
		t.Fatalf("frame c: id=%q err=%v", fc.id, err)
	}
	ffin, err := afterReader.next()
	if err != nil || ffin.event != "agent_finished" {
		t.Fatalf("frame finished: event=%q err=%v", ffin.event, err)
	}
	afterResp.Body.Close()

	waitTurnDone(t, ts, sess.ID, turnID)

	req2, _ := http.NewRequest("GET", ts.URL+"/v1/sessions/"+sess.ID+"/turns/"+turnID+"/events", nil)
	req2.Header.Set("Last-Event-ID", "1")
	resp2, err := ts.Client().Do(req2)
	if err != nil {
		t.Fatalf("last-event-id request: %v", err)
	}
	frames2 := readAllSSEFrames(t, resp2.Body)
	if len(frames2) != 3 || frames2[0].id != "2" {
		t.Fatalf("Last-Event-ID resume frames = %+v", frames2)
	}

	badResp := doJSON(t, ts, "GET", "/v1/sessions/"+sess.ID+"/turns/"+turnID+"/events?after=x", nil)
	if badResp.StatusCode != http.StatusBadRequest {
		t.Fatalf("after=x status = %d, want 400", badResp.StatusCode)
	}
	badResp.Body.Close()
}

func TestStartTurnValidationAndConflict(t *testing.T) {
	block := make(chan struct{})
	run := func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventAgentStarted})
		<-block
		return nil
	}
	opts := Options{Create: func(ctx context.Context) (*app.Controller, error) {
		id, _ := newID()
		return newTestController(t, id, run), nil
	}}
	_, ts := newServerForTest(t, opts)
	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)

	for _, text := range []string{"", "   "} {
		resp := doJSON(t, ts, "POST", "/v1/sessions/"+sess.ID+"/turns", map[string]string{"text": text})
		if resp.StatusCode != http.StatusBadRequest {
			t.Fatalf("text %q status = %d, want 400", text, resp.StatusCode)
		}
		resp.Body.Close()
	}

	req, _ := http.NewRequest("POST", ts.URL+"/v1/sessions/"+sess.ID+"/turns", strings.NewReader(`{"text":"hi"}`))
	resp1, err := ts.Client().Do(req)
	if err != nil {
		t.Fatalf("start turn: %v", err)
	}
	defer resp1.Body.Close()
	sr := newSSEReader(resp1.Body)
	if _, err := sr.next(); err != nil {
		t.Fatalf("read agent_started: %v", err)
	}

	resp2 := doJSON(t, ts, "POST", "/v1/sessions/"+sess.ID+"/turns", map[string]string{"text": "again"})
	if resp2.StatusCode != http.StatusConflict {
		t.Fatalf("second turn status = %d, want 409", resp2.StatusCode)
	}
	var body errorBody
	decodeJSON(t, resp2, &body)
	if body.Error.Code != "turn_active" {
		t.Fatalf("error code = %q, want turn_active", body.Error.Code)
	}

	close(block)
}

func TestCancelTurn(t *testing.T) {
	run := func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventAgentStarted})
		<-ctx.Done()
		return ctx.Err()
	}
	opts := Options{Create: func(ctx context.Context) (*app.Controller, error) {
		id, _ := newID()
		return newTestController(t, id, run), nil
	}}
	_, ts := newServerForTest(t, opts)
	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)

	req, _ := http.NewRequest("POST", ts.URL+"/v1/sessions/"+sess.ID+"/turns", strings.NewReader(`{"text":"hi"}`))
	resp, err := ts.Client().Do(req)
	if err != nil {
		t.Fatalf("start turn: %v", err)
	}
	defer resp.Body.Close()
	sr := newSSEReader(resp.Body)
	f0, err := sr.next()
	if err != nil {
		t.Fatalf("read agent_started: %v", err)
	}
	var started wireEvent
	_ = json.Unmarshal([]byte(f0.data), &started)
	turnID := started.TurnID

	cancelResp := doJSON(t, ts, "POST", "/v1/sessions/"+sess.ID+"/turns/"+turnID+"/cancel", nil)
	if cancelResp.StatusCode != http.StatusAccepted {
		t.Fatalf("cancel status = %d, want 202", cancelResp.StatusCode)
	}
	cancelResp.Body.Close()

	sum := waitTurnDone(t, ts, sess.ID, turnID)
	if sum.Status != turnCanceled {
		t.Fatalf("status = %q, want canceled", sum.Status)
	}
}

func TestDeleteSessionCancelsActiveTurn(t *testing.T) {
	run := func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventAgentStarted})
		<-ctx.Done()
		return ctx.Err()
	}
	opts := Options{Create: func(ctx context.Context) (*app.Controller, error) {
		id, _ := newID()
		return newTestController(t, id, run), nil
	}}
	srv, ts := newServerForTest(t, opts)
	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)

	req, _ := http.NewRequest("POST", ts.URL+"/v1/sessions/"+sess.ID+"/turns", strings.NewReader(`{"text":"hi"}`))
	resp, err := ts.Client().Do(req)
	if err != nil {
		t.Fatalf("start turn: %v", err)
	}
	sr := newSSEReader(resp.Body)
	if _, err := sr.next(); err != nil {
		t.Fatalf("read agent_started: %v", err)
	}

	srv.mu.Lock()
	os := srv.sessions[sess.ID]
	srv.mu.Unlock()
	os.mu.Lock()
	tr := os.turn
	os.mu.Unlock()
	if tr == nil {
		t.Fatal("expected an active turn on the session")
	}

	start := time.Now()
	delResp := doJSON(t, ts, "DELETE", "/v1/sessions/"+sess.ID, nil)
	elapsed := time.Since(start)
	if delResp.StatusCode != http.StatusNoContent {
		t.Fatalf("delete status = %d, want 204", delResp.StatusCode)
	}
	delResp.Body.Close()
	if elapsed > 2*time.Second {
		t.Fatalf("DELETE took %s, want well under 2s", elapsed)
	}
	resp.Body.Close()

	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) && !tr.isDone() {
		time.Sleep(5 * time.Millisecond)
	}
	if s := tr.summary(); s.Status != turnCanceled {
		t.Fatalf("status = %q, want canceled", s.Status)
	}
}

func TestConcurrentSessionsRunTurnsConcurrently(t *testing.T) {
	gateA := make(chan struct{})
	gateB := make(chan struct{})
	startedA := make(chan struct{})
	startedB := make(chan struct{})
	runA := func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventAgentStarted})
		close(startedA)
		<-gateA
		return nil
	}
	runB := func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventAgentStarted})
		close(startedB)
		<-gateB
		return nil
	}
	var which int32
	opts := Options{Create: func(ctx context.Context) (*app.Controller, error) {
		id, _ := newID()
		if atomic.AddInt32(&which, 1) == 1 {
			return newTestController(t, id, runA), nil
		}
		return newTestController(t, id, runB), nil
	}}
	_, ts := newServerForTest(t, opts)

	createdA := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sessA sessionWire
	decodeJSON(t, createdA, &sessA)
	createdB := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sessB sessionWire
	decodeJSON(t, createdB, &sessB)

	reqA, _ := http.NewRequest("POST", ts.URL+"/v1/sessions/"+sessA.ID+"/turns", strings.NewReader(`{"text":"a"}`))
	respA, err := ts.Client().Do(reqA)
	if err != nil {
		t.Fatalf("start A: %v", err)
	}
	defer respA.Body.Close()

	select {
	case <-startedA:
	case <-time.After(2 * time.Second):
		t.Fatal("A did not start")
	}

	reqB, _ := http.NewRequest("POST", ts.URL+"/v1/sessions/"+sessB.ID+"/turns", strings.NewReader(`{"text":"b"}`))
	respB, err := ts.Client().Do(reqB)
	if err != nil {
		t.Fatalf("start B: %v", err)
	}
	defer respB.Body.Close()

	select {
	case <-startedB:
	case <-time.After(2 * time.Second):
		t.Fatal("B did not start while A was still blocked")
	}

	close(gateA)
	close(gateB)
}

func TestTurnRunnerError(t *testing.T) {
	run := func(ctx context.Context, text string, emit func(agent.Event)) error {
		return errors.New("boom")
	}
	opts := Options{Create: func(ctx context.Context) (*app.Controller, error) {
		id, _ := newID()
		return newTestController(t, id, run), nil
	}}
	_, ts := newServerForTest(t, opts)
	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)

	resp := doJSON(t, ts, "POST", "/v1/sessions/"+sess.ID+"/turns", map[string]any{"text": "hi", "stream": false})
	var sum turnSummary
	decodeJSON(t, resp, &sum)
	if sum.Status != turnError || sum.Error == "" {
		t.Fatalf("summary = %+v, want error status with message", sum)
	}

	metricsBody := getBody(t, ts, "/metrics")
	if !strings.Contains(metricsBody, `otto_turns_total{status="error"} 1`) {
		t.Fatalf("missing error metric:\n%s", metricsBody)
	}
}

func TestSSEDisconnectDoesNotCancelTurn(t *testing.T) {
	proceed := make(chan struct{})
	run := func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventAgentStarted})
		<-proceed
		if err := ctx.Err(); err != nil {
			return err
		}
		emit(agent.Event{Type: agent.EventAgentFinished})
		return nil
	}
	opts := Options{Create: func(ctx context.Context) (*app.Controller, error) {
		id, _ := newID()
		return newTestController(t, id, run), nil
	}}
	_, ts := newServerForTest(t, opts)
	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)

	req, _ := http.NewRequest("POST", ts.URL+"/v1/sessions/"+sess.ID+"/turns", strings.NewReader(`{"text":"hi"}`))
	resp, err := ts.Client().Do(req)
	if err != nil {
		t.Fatalf("start turn: %v", err)
	}
	sr := newSSEReader(resp.Body)
	f0, err := sr.next()
	if err != nil {
		t.Fatalf("read agent_started: %v", err)
	}
	var started wireEvent
	_ = json.Unmarshal([]byte(f0.data), &started)
	turnID := started.TurnID

	resp.Body.Close() // client disconnects mid-stream
	close(proceed)    // the runner keeps going; it must not observe cancellation

	sum := waitTurnDone(t, ts, sess.ID, turnID)
	if sum.Status != turnOK {
		t.Fatalf("status = %q, want ok (disconnect must not cancel the turn)", sum.Status)
	}
}

func TestServerCloseCancelsActiveTurn(t *testing.T) {
	run := func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventAgentStarted})
		<-ctx.Done()
		return ctx.Err()
	}
	opts := Options{Create: func(ctx context.Context) (*app.Controller, error) {
		id, _ := newID()
		return newTestController(t, id, run), nil
	}}
	srv, ts := newServerForTest(t, opts)
	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)

	req, _ := http.NewRequest("POST", ts.URL+"/v1/sessions/"+sess.ID+"/turns", strings.NewReader(`{"text":"hi"}`))
	resp, err := ts.Client().Do(req)
	if err != nil {
		t.Fatalf("start turn: %v", err)
	}
	sr := newSSEReader(resp.Body)
	if _, err := sr.next(); err != nil {
		t.Fatalf("read agent_started: %v", err)
	}

	srv.mu.Lock()
	os := srv.sessions[sess.ID]
	srv.mu.Unlock()
	os.mu.Lock()
	tr := os.turn
	os.mu.Unlock()

	start := time.Now()
	if err := srv.Close(); err != nil {
		t.Fatalf("Close: %v", err)
	}
	elapsed := time.Since(start)
	if elapsed > 2*time.Second {
		t.Fatalf("Close took %s, want well under 2s", elapsed)
	}
	resp.Body.Close()

	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) && !tr.isDone() {
		time.Sleep(5 * time.Millisecond)
	}
	if s := tr.summary(); s.Status != turnCanceled {
		t.Fatalf("status = %q, want canceled", s.Status)
	}
}

func TestCreateSessionInternalError(t *testing.T) {
	var buf bytes.Buffer
	opts := Options{
		Create: func(ctx context.Context) (*app.Controller, error) {
			return nil, errors.New("boom: disk full")
		},
		Logger: slog.New(slog.NewTextHandler(&buf, nil)),
	}
	_, ts := newServerForTest(t, opts)

	resp := doJSON(t, ts, "POST", "/v1/sessions", nil)
	if resp.StatusCode != http.StatusInternalServerError {
		t.Fatalf("status = %d, want 500", resp.StatusCode)
	}
	body, _ := io.ReadAll(resp.Body)
	resp.Body.Close()
	want := `{"error":{"code":"internal","message":"internal error"}}`
	if string(body) != want {
		t.Fatalf("body = %q, want %q", body, want)
	}
	if !strings.Contains(buf.String(), "boom: disk full") {
		t.Fatalf("log missing real error message:\n%s", buf.String())
	}
}

func TestLogsAndMetricsExcludePromptText(t *testing.T) {
	var buf bytes.Buffer
	secret := "the-quick-brown-fox-prompt-marker"
	run := func(ctx context.Context, text string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventTextDelta, Text: text})
		return nil
	}
	opts := Options{
		Create: func(ctx context.Context) (*app.Controller, error) {
			id, _ := newID()
			return newTestController(t, id, run), nil
		},
		Logger: slog.New(slog.NewTextHandler(&buf, nil)),
	}
	_, ts := newServerForTest(t, opts)
	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)

	resp := doJSON(t, ts, "POST", "/v1/sessions/"+sess.ID+"/turns", map[string]any{"text": secret, "stream": false})
	var sum turnSummary
	decodeJSON(t, resp, &sum)
	if sum.Text != secret {
		t.Fatal("expected echoed text, run must have executed")
	}

	if strings.Contains(buf.String(), secret) {
		t.Fatalf("log contains prompt text:\n%s", buf.String())
	}
	metricsBody := getBody(t, ts, "/metrics")
	if strings.Contains(metricsBody, secret) {
		t.Fatalf("metrics contain prompt text:\n%s", metricsBody)
	}
}

func TestRequestIDHeader(t *testing.T) {
	_, ts := newServerForTest(t, Options{})

	// \x85 is valid obs-text per RFC 7230 (net/http's client-side header
	// validator accepts it) but not printable ASCII, so it exercises the
	// stripping half of requestID without tripping Go's own header check.
	// It sits inside the first 64 bytes so truncation doesn't remove it
	// before the ASCII filter gets a chance to.
	raw := strings.Repeat("a", 30) + "\x85" + strings.Repeat("a", 40)
	req, _ := http.NewRequest("GET", ts.URL+"/healthz", nil)
	req.Header.Set("X-Request-ID", raw)
	resp, err := ts.Client().Do(req)
	if err != nil {
		t.Fatalf("request: %v", err)
	}
	defer resp.Body.Close()
	got := resp.Header.Get("X-Request-ID")
	if len(got) > 64 {
		t.Fatalf("request id length = %d, want <= 64", len(got))
	}
	if got != strings.Repeat("a", 63) {
		t.Fatalf("request id = %q, want 63 a's (64-byte truncation then \\x85 stripped)", got)
	}

	resp2, err := ts.Client().Get(ts.URL + "/healthz")
	if err != nil {
		t.Fatalf("request: %v", err)
	}
	defer resp2.Body.Close()
	if resp2.Header.Get("X-Request-ID") == "" {
		t.Fatal("expected generated request id when header absent")
	}
}

func TestInfoEndpoint(t *testing.T) {
	info := Info{Workspace: "/ws", Provider: "openai-compatible", Profile: "default", Model: "test-model", Sandbox: "seatbelt", Profiles: []string{"default"}}
	_, ts := newServerForTest(t, Options{Info: info})

	resp := doJSON(t, ts, "GET", "/v1/info", nil)
	var got Info
	decodeJSON(t, resp, &got)
	if !reflect.DeepEqual(got, info) {
		t.Fatalf("info = %+v, want %+v", got, info)
	}
}

func TestOpenAPIEndpoint(t *testing.T) {
	_, ts := newServerForTest(t, Options{})
	resp := doJSON(t, ts, "GET", "/v1/openapi.yaml", nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status = %d, want 200", resp.StatusCode)
	}
	if ct := resp.Header.Get("Content-Type"); ct != "application/yaml" {
		t.Fatalf("Content-Type = %q", ct)
	}
	body, _ := io.ReadAll(resp.Body)
	resp.Body.Close()
	if !strings.HasPrefix(string(body), "openapi: 3.1") {
		t.Fatalf("body does not start with openapi: 3.1:\n%s", body[:min(80, len(body))])
	}
}

func TestOpenAPICoversEveryRoute(t *testing.T) {
	s := New(context.Background(), Options{})
	for _, e := range s.routeTable() {
		_, path, ok := strings.Cut(e.pattern, " ")
		if !ok {
			t.Fatalf("malformed route pattern %q", e.pattern)
		}
		want := "\n  " + path + ":"
		if !strings.Contains(string(openapiYAML), want) {
			t.Errorf("openapi.yaml missing path %q (looked for %q)", path, want)
		}
	}
}

func TestServeStopsOnContextCancel(t *testing.T) {
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	srv := New(context.Background(), Options{})
	ctx, cancel := context.WithCancel(context.Background())

	errCh := make(chan error, 1)
	go func() { errCh <- Serve(ctx, l, srv) }()

	time.Sleep(20 * time.Millisecond)
	cancel()

	select {
	case err := <-errCh:
		if err != nil {
			t.Fatalf("Serve returned %v, want nil", err)
		}
	case <-time.After(6 * time.Second):
		t.Fatal("Serve did not return after context cancel")
	}
}
