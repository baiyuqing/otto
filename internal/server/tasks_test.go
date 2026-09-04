package server

import (
	"context"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

// taskRunnerFake is a runnerFunc-based Runner test double that also
// implements app.TaskLister and io.Closer, mirroring
// internal/app/controller_test.go's taskRunner and the real agent.Agent's
// Close, which closes its Tasks registry.
type taskRunnerFake struct {
	runnerFunc
	tasks *agent.Tasks
}

func (r taskRunnerFake) Tasks() *agent.Tasks { return r.tasks }
func (r taskRunnerFake) Close() error        { r.tasks.Close(); return nil }

func newTaskTestController(t *testing.T, id string, tasks *agent.Tasks, run func(context.Context, string, func(agent.Event)) error) *app.Controller {
	t.Helper()
	hdr := session.Header{ID: id, Workspace: "/tmp/ws", Provider: "openai-compatible", Model: "test-model"}
	sess := session.NewMemory(hdr)
	create := func() (session.Session, error) { return session.NewMemory(hdr), nil }
	build := func(session.Session) app.Runner { return taskRunnerFake{runnerFunc: runnerFunc(run), tasks: tasks} }
	ctrl, err := app.New(sess, create, build,
		app.WithRuntimeInfo(app.RuntimeInfo{Provider: "openai-compatible", Model: "test-model", ContextWindow: 128000, Sandbox: testSandbox}),
	)
	if err != nil {
		t.Fatalf("app.New: %v", err)
	}
	return ctrl
}

// promptLog records every os.ctrl.Prompt text argument, safe for concurrent
// use by a fake runner's Run function.
type promptLog struct {
	mu    sync.Mutex
	calls []string
}

func (p *promptLog) record(text string) {
	p.mu.Lock()
	p.calls = append(p.calls, text)
	p.mu.Unlock()
}

func (p *promptLog) snapshot() []string {
	p.mu.Lock()
	defer p.mu.Unlock()
	return append([]string(nil), p.calls...)
}

func waitForCalls(t *testing.T, p *promptLog, n int) []string {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if calls := p.snapshot(); len(calls) >= n {
			return calls
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatalf("prompt called %d times in time, want >= %d", len(p.snapshot()), n)
	return nil
}

func TestWakeTurnStartsOnPendingNotificationWhenIdle(t *testing.T) {
	tasks := agent.NewTasks()
	var calls promptLog
	// run drains the inbox on every call, mirroring agent.Agent.Run, which
	// drains pending notifications before each provider request; without
	// this the wake loop's end-of-turn check would see Pending() > 0
	// forever and start a wake turn after every wake turn.
	run := func(ctx context.Context, text string, emit func(agent.Event)) error {
		tasks.Notifications().Drain()
		calls.record(text)
		return nil
	}
	opts := Options{Create: func(ctx context.Context) (*app.Controller, error) {
		id, _ := newID()
		return newTaskTestController(t, id, tasks, run), nil
	}}
	_, ts := newServerForTest(t, opts)
	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)

	tasks.Notifications().Push(agent.Notification{TaskID: "t1", Kind: agent.NotificationTaskFinished, Text: "done"})

	got := waitForCalls(t, &calls, 1)
	if len(got) != 1 || got[0] != "" {
		t.Fatalf("promptCalls = %q, want one empty-text wake call", got)
	}

	sessResp := doJSON(t, ts, "GET", "/v1/sessions/"+sess.ID, nil)
	var sessGot sessionWire
	decodeJSON(t, sessResp, &sessGot)
	if sessGot.Turn == nil || sessGot.Turn.Trigger != triggerTask {
		t.Fatalf("session turn = %+v, want trigger %q", sessGot.Turn, triggerTask)
	}

	turnSum := waitTurnDone(t, ts, sess.ID, sessGot.Turn.ID)
	if turnSum.Trigger != triggerTask {
		t.Fatalf("GET turn trigger = %q, want %q", turnSum.Trigger, triggerTask)
	}
}

func TestWakeTurnSkippedWhileUserTurnActive(t *testing.T) {
	tasks := agent.NewTasks()
	block := make(chan struct{})
	var calls promptLog
	run := func(ctx context.Context, text string, emit func(agent.Event)) error {
		tasks.Notifications().Drain() // mirrors agent.Agent.Run's pre-request drain
		calls.record(text)
		if text != "" {
			emit(agent.Event{Type: agent.EventAgentStarted}) // flushes SSE headers so the POST below returns
			<-block                                          // hold the user turn open
		}
		return nil
	}
	opts := Options{Create: func(ctx context.Context) (*app.Controller, error) {
		id, _ := newID()
		return newTaskTestController(t, id, tasks, run), nil
	}}
	_, ts := newServerForTest(t, opts)
	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)

	resp := doJSON(t, ts, "POST", "/v1/sessions/"+sess.ID+"/turns", map[string]any{"text": "hi"})
	defer resp.Body.Close()
	// The turn goroutine has not necessarily called Prompt yet the instant
	// the HTTP handler returns; wait for the first recorded call ("hi").
	waitForCalls(t, &calls, 1)

	// A notification while the user turn is active must not start a second
	// (task-triggered) turn: the active turn's own inbox drain (agent-side,
	// not exercised by this fake runner) handles it instead.
	tasks.Notifications().Push(agent.Notification{TaskID: "t1", Text: "done"})
	time.Sleep(50 * time.Millisecond)
	if got := calls.snapshot(); len(got) != 1 {
		t.Fatalf("promptCalls = %q, want exactly 1 while user turn is active", got)
	}

	close(block)

	// The user turn's own end-of-turn check now finds the notification
	// still pending (this fake runner does not drain mid-turn) and starts
	// exactly one wake turn.
	got := waitForCalls(t, &calls, 2)
	if len(got) != 2 || got[1] != "" {
		t.Fatalf("promptCalls = %q, want [\"hi\" \"\"]", got)
	}
	time.Sleep(50 * time.Millisecond)
	if got := calls.snapshot(); len(got) != 2 {
		t.Fatalf("promptCalls = %q, want exactly 2 (no extra wake turn)", got)
	}
}

func TestWakeTurnFollowsUserTurnFinishingWithPendingNotification(t *testing.T) {
	tasks := agent.NewTasks()
	var calls promptLog
	run := func(ctx context.Context, text string, emit func(agent.Event)) error {
		tasks.Notifications().Drain() // mirrors agent.Agent.Run's pre-request drain
		calls.record(text)
		if text != "" {
			// A notification lands after the parent's last provider request
			// (simulated here as arriving right before Prompt returns), so
			// no further Tasks().Updates() signal follows; only the
			// end-of-turn check in startTurn can catch it.
			tasks.Notifications().Push(agent.Notification{TaskID: "t1", Text: "done"})
		}
		return nil
	}
	opts := Options{Create: func(ctx context.Context) (*app.Controller, error) {
		id, _ := newID()
		return newTaskTestController(t, id, tasks, run), nil
	}}
	_, ts := newServerForTest(t, opts)
	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)

	resp := doJSON(t, ts, "POST", "/v1/sessions/"+sess.ID+"/turns", map[string]any{"text": "hi", "stream": false})
	resp.Body.Close()

	got := waitForCalls(t, &calls, 2)
	if len(got) != 2 || got[0] != "hi" || got[1] != "" {
		t.Fatalf("promptCalls = %q, want [\"hi\" \"\"]", got)
	}

	// Give any incorrect extra wake turn a chance to start, then confirm
	// exactly one task-triggered turn ran.
	time.Sleep(50 * time.Millisecond)
	if got := calls.snapshot(); len(got) != 2 {
		t.Fatalf("promptCalls = %q, want exactly 2 (no extra wake turn)", got)
	}
}

func TestServerCloseEndsWakeLoop(t *testing.T) {
	tasks := agent.NewTasks()
	opts := Options{Create: func(ctx context.Context) (*app.Controller, error) {
		id, _ := newID()
		return newTaskTestController(t, id, tasks, noopRun), nil
	}}
	s, ts := newServerForTest(t, opts)
	resp := doJSON(t, ts, "POST", "/v1/sessions", nil)
	resp.Body.Close()

	done := make(chan error, 1)
	go func() { done <- s.Close() }()
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("Close() = %v", err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("Close() did not return in time; wake loop goroutine leaked")
	}
}

func newTasksTestServer(t *testing.T, tasks *agent.Tasks) (*httptest.Server, string) {
	t.Helper()
	opts := Options{Create: func(ctx context.Context) (*app.Controller, error) {
		id, _ := newID()
		return newTaskTestController(t, id, tasks, noopRun), nil
	}}
	_, ts := newServerForTest(t, opts)
	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)
	return ts, sess.ID
}

func TestListTasksRoute(t *testing.T) {
	tasks := agent.NewTasks()
	if _, err := tasks.Add(agent.Task{Agent: "reviewer", Description: "review PR"}, nil, nil); err != nil {
		t.Fatalf("Add: %v", err)
	}
	if _, err := tasks.Add(agent.Task{Agent: "writer", Description: "draft docs"}, nil, nil); err != nil {
		t.Fatalf("Add: %v", err)
	}
	ts, sessID := newTasksTestServer(t, tasks)

	resp := doJSON(t, ts, "GET", "/v1/sessions/"+sessID+"/tasks", nil)
	var got struct {
		Tasks []taskWire `json:"tasks"`
	}
	decodeJSON(t, resp, &got)
	if len(got.Tasks) != 2 {
		t.Fatalf("tasks = %+v, want 2 entries", got.Tasks)
	}
	if got.Tasks[0].ID != "t1" || got.Tasks[0].Agent != "reviewer" {
		t.Fatalf("tasks[0] = %+v, want id t1 agent reviewer", got.Tasks[0])
	}
	if got.Tasks[1].ID != "t2" || got.Tasks[1].Agent != "writer" {
		t.Fatalf("tasks[1] = %+v, want id t2 agent writer", got.Tasks[1])
	}
}

func TestListTasksRouteEmptyWithoutRegistry(t *testing.T) {
	opts := Options{Create: func(ctx context.Context) (*app.Controller, error) {
		id, _ := newID()
		return newTestController(t, id, noopRun), nil
	}}
	_, ts := newServerForTest(t, opts)
	created := doJSON(t, ts, "POST", "/v1/sessions", nil)
	var sess sessionWire
	decodeJSON(t, created, &sess)

	resp := doJSON(t, ts, "GET", "/v1/sessions/"+sess.ID+"/tasks", nil)
	var got struct {
		Tasks []taskWire `json:"tasks"`
	}
	decodeJSON(t, resp, &got)
	if len(got.Tasks) != 0 {
		t.Fatalf("tasks = %+v, want empty list", got.Tasks)
	}
}

func TestGetTaskRouteWithHistory(t *testing.T) {
	tasks := agent.NewTasks()
	history := []model.Message{
		{
			ID:   "m1",
			Role: model.RoleAssistant,
			Blocks: []model.Block{
				{Type: model.BlockToolCall, ToolCallID: "c1", ToolName: "read"},
			},
		},
		{
			ID:     "m2",
			Role:   model.RoleAssistant,
			Blocks: []model.Block{{Type: model.BlockText, Text: "done reading"}},
		},
	}
	task, err := tasks.Add(agent.Task{Agent: "reviewer", Description: "review PR"}, nil, func() []model.Message { return history })
	if err != nil {
		t.Fatalf("Add: %v", err)
	}
	tasks.Update(task.ID, func(tk *agent.Task) { tk.Status = agent.TaskRunning; tk.Steps = 3 })
	ts, sessID := newTasksTestServer(t, tasks)

	resp := doJSON(t, ts, "GET", "/v1/sessions/"+sessID+"/tasks/"+task.ID, nil)
	var got struct {
		taskWire
		History []model.Message `json:"history"`
	}
	decodeJSON(t, resp, &got)
	if got.ID != task.ID || got.Status != string(agent.TaskRunning) || got.Steps != 3 {
		t.Fatalf("task = %+v, want id %q status running steps 3", got.taskWire, task.ID)
	}
	if len(got.History) != 2 || got.History[0].Blocks[0].ToolName != "read" || got.History[1].Text() != "done reading" {
		t.Fatalf("history = %+v, want the 2 seeded messages", got.History)
	}
}

func TestGetTaskRouteByName(t *testing.T) {
	tasks := agent.NewTasks()
	task, err := tasks.Add(agent.Task{Name: "lint-check", Agent: "reviewer", Description: "review PR"}, nil, nil)
	if err != nil {
		t.Fatalf("Add: %v", err)
	}
	ts, sessID := newTasksTestServer(t, tasks)

	resp := doJSON(t, ts, "GET", "/v1/sessions/"+sessID+"/tasks/lint-check", nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status = %d, want 200", resp.StatusCode)
	}
	var got struct {
		taskWire
		History []model.Message `json:"history"`
	}
	decodeJSON(t, resp, &got)
	if got.ID != task.ID || got.Name != "lint-check" {
		t.Fatalf("task = %+v, want id %q name lint-check", got.taskWire, task.ID)
	}
}

func TestGetTaskRouteUnknownID(t *testing.T) {
	tasks := agent.NewTasks()
	ts, sessID := newTasksTestServer(t, tasks)

	resp := doJSON(t, ts, "GET", "/v1/sessions/"+sessID+"/tasks/nope", nil)
	if resp.StatusCode != http.StatusNotFound {
		t.Fatalf("status = %d, want 404", resp.StatusCode)
	}
}

func TestCancelTaskRouteRunning(t *testing.T) {
	tasks := agent.NewTasks()
	canceled := false
	task, err := tasks.Add(agent.Task{Agent: "reviewer"}, func() { canceled = true }, nil)
	if err != nil {
		t.Fatalf("Add: %v", err)
	}
	tasks.Update(task.ID, func(tk *agent.Task) { tk.Status = agent.TaskRunning })
	ts, sessID := newTasksTestServer(t, tasks)

	resp := doJSON(t, ts, "POST", "/v1/sessions/"+sessID+"/tasks/"+task.ID+"/cancel", nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status = %d, want 200", resp.StatusCode)
	}
	var got taskWire
	decodeJSON(t, resp, &got)
	if got.ID != task.ID {
		t.Fatalf("task id = %q, want %q", got.ID, task.ID)
	}
	if !canceled {
		t.Fatal("cancel func was not called")
	}
}

func TestCancelTaskRouteAlreadyFinished(t *testing.T) {
	tasks := agent.NewTasks()
	task, err := tasks.Add(agent.Task{Agent: "reviewer"}, nil, nil)
	if err != nil {
		t.Fatalf("Add: %v", err)
	}
	tasks.Update(task.ID, func(tk *agent.Task) { tk.Status = agent.TaskSucceeded })
	ts, sessID := newTasksTestServer(t, tasks)

	resp := doJSON(t, ts, "POST", "/v1/sessions/"+sessID+"/tasks/"+task.ID+"/cancel", nil)
	if resp.StatusCode != http.StatusConflict {
		t.Fatalf("status = %d, want 409", resp.StatusCode)
	}
}

func TestCancelTaskRouteUnknownID(t *testing.T) {
	tasks := agent.NewTasks()
	ts, sessID := newTasksTestServer(t, tasks)

	resp := doJSON(t, ts, "POST", "/v1/sessions/"+sessID+"/tasks/nope/cancel", nil)
	if resp.StatusCode != http.StatusNotFound {
		t.Fatalf("status = %d, want 404", resp.StatusCode)
	}
}

// waitMetricsContain polls GET /metrics until the exposition text contains
// substr, needed because the wake loop diffs task metrics asynchronously on
// its own goroutine after Tasks.Update signals it.
func waitMetricsContain(t *testing.T, ts *httptest.Server, substr string) string {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	var body string
	for time.Now().Before(deadline) {
		body = getBody(t, ts, "/metrics")
		if strings.Contains(body, substr) {
			return body
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatalf("metrics did not contain %q in time:\n%s", substr, body)
	return ""
}

func TestWakeLoopUpdatesTaskMetrics(t *testing.T) {
	tasks := agent.NewTasks()
	ts, _ := newTasksTestServer(t, tasks)

	task, err := tasks.Add(agent.Task{Agent: "reviewer"}, nil, nil)
	if err != nil {
		t.Fatalf("Add: %v", err)
	}
	tasks.Update(task.ID, func(tk *agent.Task) { tk.Status = agent.TaskRunning })
	waitMetricsContain(t, ts, "otto_tasks_running 1")

	tasks.Update(task.ID, func(tk *agent.Task) { tk.Status = agent.TaskSucceeded })
	got := waitMetricsContain(t, ts, `otto_tasks_finished_total{status="succeeded"} 1`)

	if !strings.Contains(got, "otto_tasks_started_total 1") {
		t.Fatalf("metrics missing otto_tasks_started_total 1:\n%s", got)
	}
	if !strings.Contains(got, "otto_tasks_running 0") {
		t.Fatalf("otto_tasks_running should be back to 0 after the task finished:\n%s", got)
	}
}
