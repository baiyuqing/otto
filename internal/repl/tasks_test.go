package repl

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

type taskWakeRunner struct {
	tasks   *agent.Tasks
	backend *fakeBackend
}

func (r taskWakeRunner) Tasks() *agent.Tasks { return r.tasks }

func (r taskWakeRunner) Run(ctx context.Context, text string, emit func(agent.Event)) error {
	if r.backend != nil && r.backend.prompt != nil {
		return r.backend.prompt(ctx, text, emit)
	}
	r.tasks.Notifications().Drain()
	return nil
}

func (r taskWakeRunner) Compact(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
	return agent.CompactionResult{Noop: true}, nil
}

func taskBackendWithWake(t *testing.T, tasks *agent.Tasks) *fakeBackend {
	t.Helper()
	backend := &fakeBackend{tasks: tasks}
	sess := session.NewMemory(session.Header{ID: "repl-wake", Workspace: t.TempDir()})
	controller, err := app.New(app.SessionReplacement{Session: sess, Runner: taskWakeRunner{tasks: tasks, backend: backend}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = controller.Close() })
	backend.wake = controller
	return backend
}

func TestREPLRendersNotificationEvent(t *testing.T) {
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{prompt: func(_ context.Context, _ string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventNotification, Text: "[task-notification] task t1 (explorer) succeeded · 1s · 1 tool call\nall good"})
		return nil
	}}
	r := New(strings.NewReader(""), &stdout, &stderr, backend)
	if err := r.RunOnce(context.Background(), "go"); err != nil {
		t.Fatalf("RunOnce() = %v", err)
	}
	want := "\n[task-notification] task t1 (explorer) succeeded · 1s · 1 tool call\nall good\n"
	if !strings.Contains(stdout.String(), want) {
		t.Fatalf("stdout = %q, want contains %q", stdout.String(), want)
	}
}

func TestREPLWakesOnlyWhenNotificationIsPending(t *testing.T) {
	reader, writer := io.Pipe()
	var stdout, stderr bytes.Buffer
	tasks := agent.NewTasks()
	var mu sync.Mutex
	var promptCalls []string
	woke := make(chan struct{}, 1)
	backend := taskBackendWithWake(t, tasks)
	backend.prompt = func(_ context.Context, prompt string, emit func(agent.Event)) error {
		mu.Lock()
		promptCalls = append(promptCalls, prompt)
		mu.Unlock()
		emit(agent.Event{Type: agent.EventTextDelta, Text: "woke up"})
		if prompt == "" {
			woke <- struct{}{}
		}
		return nil
	}
	r := New(reader, &stdout, &stderr, backend)
	errCh := make(chan error, 1)
	go func() { errCh <- r.Run(context.Background()) }()

	// A registry signal with nothing pending must not trigger a wake turn.
	if _, err := tasks.Add(agent.Task{}, nil, nil); err != nil {
		t.Fatalf("Add() = %v", err)
	}
	select {
	case <-woke:
		t.Fatal("wake turn ran before any notification was pending")
	case <-time.After(50 * time.Millisecond):
	}
	mu.Lock()
	calls := len(promptCalls)
	mu.Unlock()
	if calls != 0 {
		t.Fatalf("prompt called %d times before any notification was pending", calls)
	}

	tasks.Notifications().Push(agent.Notification{Text: "[task-notification] task t1 succeeded"})

	select {
	case <-woke:
	case <-time.After(2 * time.Second):
		t.Fatal("wake turn was not triggered by the pending notification")
	}

	// Run() has finished rendering the wake turn (the send on woke happens
	// after emit); ending the input now and waiting for Run() to return
	// makes the stdout read below race-free.
	writer.Close()
	if err := <-errCh; err != nil {
		t.Fatalf("Run() = %v", err)
	}

	mu.Lock()
	defer mu.Unlock()
	if len(promptCalls) != 1 || promptCalls[0] != "" {
		t.Fatalf("promptCalls = %q, want exactly one empty-text wake call", promptCalls)
	}
	if !strings.Contains(stdout.String(), "\nwoke up") {
		t.Fatalf("stdout = %q, want wake turn output", stdout.String())
	}
}

func TestTasksCommandEmpty(t *testing.T) {
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{tasks: agent.NewTasks()}
	r := New(strings.NewReader("/tasks\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatalf("Run() = %v", err)
	}
	if !strings.Contains(stdout.String(), "no tasks in this session") {
		t.Fatalf("stdout = %q", stdout.String())
	}
}

func TestTasksCommandListsTasks(t *testing.T) {
	tasks := agent.NewTasks()
	added, err := tasks.Add(agent.Task{Description: "review the diff", Prompt: "review please"}, nil, nil)
	if err != nil {
		t.Fatalf("Add() = %v", err)
	}
	started := time.Now().Add(-42 * time.Second)
	tasks.MarkRunning(added.ID, started)
	for i := 0; i < 7; i++ {
		tasks.RecordToolCall(added.ID, "read")
	}
	tasks.RecordProviderStep(added.ID, model.Usage{InputTokens: 10000, OutputTokens: 2310}, "", true)
	tasks.Finish(added.ID, agent.TaskSucceeded, started.Add(42*time.Second), "", "")
	// A task canceled while still queued has a zero StartedAt and a non-zero
	// FinishedAt; its elapsed column must read "0s", not the (meaningless)
	// span from the zero time to FinishedAt.
	canceled, err := tasks.Add(agent.Task{Description: "abort early"}, nil, nil)
	if err != nil {
		t.Fatalf("Add() = %v", err)
	}
	tasks.Finish(canceled.ID, agent.TaskCanceled, time.Now(), "", "")
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{tasks: tasks}
	r := New(strings.NewReader("/tasks\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatalf("Run() = %v", err)
	}
	out := stdout.String()
	for _, want := range []string{added.ID, "succeeded", "42s", "7 tools", "12,310 tokens", "review the diff"} {
		if !strings.Contains(out, want) {
			t.Fatalf("stdout missing %q: %q", want, out)
		}
	}
	var canceledLine string
	for _, line := range strings.Split(out, "\n") {
		if strings.HasPrefix(line, canceled.ID) {
			canceledLine = line
			break
		}
	}
	if canceledLine == "" {
		t.Fatalf("no output line for %s: %q", canceled.ID, out)
	}
	if !strings.Contains(canceledLine, "canceled") || !strings.Contains(canceledLine, "0s") {
		t.Fatalf("canceled task line = %q, want status canceled and elapsed 0s", canceledLine)
	}
	if stderr.Len() != 0 {
		t.Fatalf("stderr = %q, want empty", stderr.String())
	}
}

func TestTaskCommandShowsStepsAndResult(t *testing.T) {
	tasks := agent.NewTasks()
	history := []model.Message{
		{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "review please"}}},
		{Role: model.RoleAssistant, Blocks: []model.Block{
			{Type: model.BlockToolCall, ToolName: "read", Arguments: json.RawMessage(`{"path":"main.go"}`)},
		}},
		{Role: model.RoleTool, Blocks: []model.Block{{Type: model.BlockToolResult, Text: "package main"}}},
		{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "looks fine"}}},
	}
	added, err := tasks.Add(agent.Task{Description: "review", Model: "gpt-test-model"}, nil, func() []model.Message { return history })
	if err != nil {
		t.Fatalf("Add() = %v", err)
	}
	tasks.Finish(added.ID, agent.TaskSucceeded, time.Now(), "no issues found", "")
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{tasks: tasks}
	r := New(strings.NewReader("/task "+added.ID+"\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatalf("Run() = %v", err)
	}
	out := stdout.String()
	for _, want := range []string{
		"model: gpt-test-model",
		"→ read " + `{"path":"main.go"}`,
		"assistant: looks fine",
		"result: no issues found",
	} {
		if !strings.Contains(out, want) {
			t.Fatalf("stdout missing %q: %q", want, out)
		}
	}
	lines := strings.Split(out, "\n")
	modelIdx := -1
	for i, line := range lines {
		if line == "model: gpt-test-model" {
			modelIdx = i
			break
		}
	}
	if modelIdx <= 0 || !strings.Contains(lines[modelIdx-1], added.ID) {
		t.Fatalf("expected model line right after the task line, got: %q", out)
	}
	if strings.Contains(out, "review please") {
		t.Fatalf("stdout should not render the delegated prompt as a step: %q", out)
	}
	if strings.Contains(out, "package main") {
		t.Fatalf("stdout should not render tool results: %q", out)
	}
}

func TestTaskCommandCancel(t *testing.T) {
	tasks := agent.NewTasks()
	canceled := false
	added, err := tasks.Add(agent.Task{}, func() { canceled = true }, nil)
	if err != nil {
		t.Fatalf("Add() = %v", err)
	}
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{tasks: tasks}
	r := New(strings.NewReader("/task cancel "+added.ID+"\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatalf("Run() = %v", err)
	}
	if !canceled {
		t.Fatal("Cancel func was not invoked")
	}
	if !strings.Contains(stdout.String(), "canceled "+added.ID) {
		t.Fatalf("stdout = %q", stdout.String())
	}
}

func TestTaskCommandByName(t *testing.T) {
	tasks := agent.NewTasks()
	canceled := false
	added, err := tasks.Add(agent.Task{Name: "lint-check", Description: "review"}, func() { canceled = true }, nil)
	if err != nil {
		t.Fatalf("Add() = %v", err)
	}
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{tasks: tasks}
	r := New(strings.NewReader("/task lint-check\n/task cancel lint-check\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatalf("Run() = %v", err)
	}
	out := stdout.String()
	if !strings.Contains(out, added.ID) {
		t.Fatalf("stdout missing task id %q shown by name: %q", added.ID, out)
	}
	if !canceled {
		t.Fatal("Cancel func was not invoked by name")
	}
	if !strings.Contains(out, "canceled lint-check") {
		t.Fatalf("stdout = %q, want it to confirm the cancel by name", out)
	}
}

func TestTaskCommandUnknownID(t *testing.T) {
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{tasks: agent.NewTasks(), prompt: func(_ context.Context, prompt string, _ func(agent.Event)) error {
		if prompt != "hello" {
			t.Fatalf("prompt = %q, want %q", prompt, "hello")
		}
		return nil
	}}
	r := New(strings.NewReader("/task t9\nhello\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatalf("Run() = %v, want the loop to continue past the unknown id", err)
	}
	if !strings.Contains(stderr.String(), "unknown task: t9") {
		t.Fatalf("stderr = %q", stderr.String())
	}
}

func TestTasksCommandWithoutTaskLister(t *testing.T) {
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/tasks\n/task t1\n/exit\n"), &stdout, &stderr, &fakeBackend{})
	if err := r.Run(context.Background()); err != nil {
		t.Fatalf("Run() = %v", err)
	}
	if got := strings.Count(stderr.String(), "sub-agents are not available"); got != 2 {
		t.Fatalf("stderr = %q, want the message twice (once per command)", stderr.String())
	}
}

func TestRunOnceWaitsForRunningTaskAndWakes(t *testing.T) {
	var stdout, stderr bytes.Buffer
	tasks := agent.NewTasks()
	taskID := ""
	calls := 0
	backend := taskBackendWithWake(t, tasks)
	backend.prompt = func(_ context.Context, prompt string, emit func(agent.Event)) error {
		calls++
		if prompt != "" {
			added, err := tasks.Add(agent.Task{}, func() {}, nil)
			if err != nil {
				t.Fatalf("Add() = %v", err)
			}
			taskID = added.ID
			tasks.MarkRunning(taskID, time.Now())
			go func() {
				time.Sleep(20 * time.Millisecond)
				// Push before the final update, as Runner.finish does: Wait
				// unblocks on the update, and drainTasks must then find the
				// notification already pending.
				tasks.Notifications().Push(agent.Notification{TaskID: taskID, Text: "[task-notification] task " + taskID + " succeeded\nchild done"})
				tasks.Finish(taskID, agent.TaskSucceeded, time.Now(), "child done", "")
			}()
			return nil
		}
		// The wake turn: drain the inbox, as Agent.Run would.
		tasks.Notifications().Drain()
		emit(agent.Event{Type: agent.EventTextDelta, Text: "reported"})
		return nil
	}
	r := New(strings.NewReader(""), &stdout, &stderr, backend)
	if err := r.RunOnce(context.Background(), "go"); err != nil {
		t.Fatalf("RunOnce() = %v", err)
	}
	if calls != 2 {
		t.Fatalf("prompt calls = %d, want 2 (initial turn + wake turn)", calls)
	}
	if !strings.Contains(stdout.String(), "reported") {
		t.Fatalf("stdout = %q, want the wake turn's output", stdout.String())
	}
}

func TestRunOnceReturnsWakeError(t *testing.T) {
	var stdout, stderr bytes.Buffer
	providerErr := errors.New("wake provider failed")
	tasks := agent.NewTasks()
	tasks.Notifications().Push(agent.Notification{TaskID: "t1", Kind: agent.NotificationTaskReport, Text: "progress"})
	backend := taskBackendWithWake(t, tasks)
	backend.prompt = func(_ context.Context, prompt string, _ func(agent.Event)) error {
		if prompt == "" {
			return providerErr
		}
		return nil
	}

	r := New(strings.NewReader(""), &stdout, &stderr, backend)
	if err := r.RunOnce(context.Background(), "inspect"); !errors.Is(err, providerErr) {
		t.Fatalf("RunOnce() error = %v, want %v", err, providerErr)
	}
}
