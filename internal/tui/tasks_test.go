package tui

import (
	"context"
	"encoding/json"
	"strings"
	"testing"
	"time"

	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

type taskWakeRunner struct {
	tasks *agent.Tasks
	run   func(context.Context) error
}

func (r taskWakeRunner) Tasks() *agent.Tasks { return r.tasks }

func (r taskWakeRunner) Run(ctx context.Context, _ string, _ func(agent.Event)) error {
	if r.run != nil {
		return r.run(ctx)
	}
	r.tasks.Notifications().Drain()
	return nil
}

func (r taskWakeRunner) Compact(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
	return agent.CompactionResult{Noop: true}, nil
}

func taskBackendWithWake(t *testing.T, tasks *agent.Tasks) *fakeBackend {
	t.Helper()
	sess := session.NewMemory(session.Header{ID: "tui-wake", Workspace: t.TempDir()})
	controller, err := app.New(app.SessionReplacement{Session: sess, Runner: taskWakeRunner{tasks: tasks}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = controller.Close() })
	return &fakeBackend{tasks: tasks, wake: controller}
}

func addTask(t *testing.T, tasks *agent.Tasks, status agent.TaskStatus, agentName string) agent.Task {
	t.Helper()
	task, err := tasks.Add(agent.Task{Agent: agentName, Prompt: "do work"}, func() {}, nil)
	if err != nil {
		t.Fatalf("Add() = %v", err)
	}
	if status != agent.TaskQueued {
		tasks.MarkRunning(task.ID, time.Now().Add(-2*time.Second))
		if status != agent.TaskRunning {
			tasks.Finish(task.ID, status, time.Now(), "", "")
		}
	}
	task, _ = tasks.Get(task.ID)
	return task
}

func TestTaskPanelContentTwoRunningOneQueued(t *testing.T) {
	tasks := agent.NewTasks()
	addTask(t, tasks, agent.TaskRunning, "explorer")
	addTask(t, tasks, agent.TaskRunning, "")
	addTask(t, tasks, agent.TaskQueued, "reviewer")

	active := activeTasksFromRegistry(tasks)
	if len(active) != 3 {
		t.Fatalf("active tasks = %d, want 3", len(active))
	}
	content := taskPanelContent(active, time.Now(), "⠋", 80)
	lines := strings.Split(content, "\n")
	if len(lines) != 4 {
		t.Fatalf("panel lines = %d, want 4 (header + 3 rows): %q", len(lines), content)
	}
	if lines[0] != "tasks  2 running · 1 queued" {
		t.Fatalf("header = %q", lines[0])
	}
	if taskPanelLineCount(active) != 4 {
		t.Fatalf("taskPanelLineCount() = %d, want 4", taskPanelLineCount(active))
	}
}

func TestTaskPanelContentTruncatesToFiveRowsPlusMore(t *testing.T) {
	tasks := agent.NewTasks()
	for i := 0; i < 7; i++ {
		addTask(t, tasks, agent.TaskRunning, "explorer")
	}
	active := activeTasksFromRegistry(tasks)
	content := taskPanelContent(active, time.Now(), "⠋", 80)
	lines := strings.Split(content, "\n")
	// header + 5 rows + "+2 more"
	if len(lines) != 7 {
		t.Fatalf("panel lines = %d, want 7: %q", len(lines), content)
	}
	if lines[6] != "+2 more" {
		t.Fatalf("last line = %q, want '+2 more'", lines[6])
	}
	if taskPanelLineCount(active) != 7 {
		t.Fatalf("taskPanelLineCount() = %d, want 7", taskPanelLineCount(active))
	}
}

func TestTaskPanelContentEmptyWhenNoActiveTasks(t *testing.T) {
	if got := taskPanelContent(nil, time.Now(), "⠋", 80); got != "" {
		t.Fatalf("taskPanelContent(nil) = %q, want empty", got)
	}
	if got := taskPanelLineCount(nil); got != 0 {
		t.Fatalf("taskPanelLineCount(nil) = %d, want 0", got)
	}
}

func TestModelTaskPanelLinesZeroWithoutActiveTasks(t *testing.T) {
	m := newTestModel(t)
	if got := m.taskPanelLines(); got != 0 {
		t.Fatalf("taskPanelLines() = %d, want 0", got)
	}
}

func TestModelTaskPanelLinesReflectsBackendTasks(t *testing.T) {
	tasks := agent.NewTasks()
	addTask(t, tasks, agent.TaskRunning, "explorer")
	m := newTestModelWithBackend(t, &fakeBackend{tasks: tasks})
	if got := m.taskPanelLines(); got != 2 {
		t.Fatalf("taskPanelLines() = %d, want 2 (header + 1 row)", got)
	}
}

func TestArmTaskUpdatesNilWithoutTaskLister(t *testing.T) {
	m := newTestModel(t)
	if cmd := m.armTaskUpdates(); cmd != nil {
		t.Fatal("armTaskUpdates() = non-nil, want nil without a task registry")
	}
}

func TestWaitTaskUpdateReportsClosed(t *testing.T) {
	updates := make(chan struct{})
	close(updates)
	msg := waitTaskUpdate(updates)()
	got, ok := msg.(taskUpdateMsg)
	if !ok || !got.closed {
		t.Fatalf("waitTaskUpdate() on closed channel = %#v, want taskUpdateMsg{closed:true}", msg)
	}
}

func TestWaitTaskUpdateReportsOpen(t *testing.T) {
	updates := make(chan struct{}, 1)
	updates <- struct{}{}
	msg := waitTaskUpdate(updates)()
	got, ok := msg.(taskUpdateMsg)
	if !ok || got.closed {
		t.Fatalf("waitTaskUpdate() on open channel = %#v, want taskUpdateMsg{closed:false}", msg)
	}
}

// TestDispatchTaskUpdateMsgRearmsOnNewRegistry exercises the taskUpdateMsg
// case: after a closed signal, dispatch re-reads the backend's current task
// registry and arms a new wait when one is present.
func TestDispatchTaskUpdateMsgRearmsOnNewRegistry(t *testing.T) {
	tasks := agent.NewTasks()
	backend := &fakeBackend{tasks: tasks}
	m := newTestModelWithBackend(t, backend)

	next, cmd := m.dispatch(taskUpdateMsg{closed: true})
	if _, ok := next.(Model); !ok {
		t.Fatalf("dispatch returned %T, want Model", next)
	}
	if cmd == nil {
		t.Fatal("dispatch(taskUpdateMsg{closed:true}) cmd = nil, want a re-armed wait when a registry is active")
	}
}

func TestDispatchTaskUpdateMsgNoRearmWithoutRegistry(t *testing.T) {
	m := newTestModel(t)
	_, cmd := m.dispatch(taskUpdateMsg{closed: true})
	if cmd != nil {
		t.Fatal("dispatch(taskUpdateMsg{closed:true}) cmd = non-nil, want nil without a task registry")
	}
}

func TestInitArmsTaskUpdatesWhenRegistryPresent(t *testing.T) {
	tasks := agent.NewTasks()
	m := newTestModelWithBackend(t, &fakeBackend{tasks: tasks})
	initMsg := m.Init()()
	batch, ok := initMsg.(tea.BatchMsg)
	if !ok || len(batch) != 3 {
		t.Fatalf("Init() message = %T %#v, want three-command batch (background, spinner, task updates)", initMsg, initMsg)
	}
}

// --- maybeWake (item 4) ---

func TestMaybeWakeStartsEmptyTurnWhenPendingAndIdle(t *testing.T) {
	tasks := agent.NewTasks()
	taskID := addTask(t, tasks, agent.TaskRunning, "explorer").ID
	tasks.Notifications().Push(agent.Notification{TaskID: taskID, Kind: agent.NotificationTaskReport, Text: "progress"})

	m := newTestModelWithBackend(t, taskBackendWithWake(t, tasks))
	entriesBefore := len(m.entries)

	next, cmd := m.maybeWake()
	if cmd == nil {
		t.Fatal("maybeWake() cmd = nil, want a started turn")
	}
	if !next.running {
		t.Fatal("maybeWake() did not mark the model running")
	}
	if len(next.entries) != entriesBefore {
		t.Fatalf("maybeWake() appended %d entries, want none (no EntryUser for a wake turn)", len(next.entries)-entriesBefore)
	}
}

func TestWakeEscapeCancelsAdmittedWakeContext(t *testing.T) {
	tasks := agent.NewTasks()
	started := make(chan struct{})
	canceled := make(chan struct{})
	sess := session.NewMemory(session.Header{ID: "tui-wake-cancel", Workspace: t.TempDir()})
	runner := taskWakeRunner{tasks: tasks, run: func(ctx context.Context) error {
		close(started)
		<-ctx.Done()
		close(canceled)
		return ctx.Err()
	}}
	controller, err := app.New(app.SessionReplacement{Session: sess, Runner: runner})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = controller.Close() })
	backend := &fakeBackend{tasks: tasks, wake: controller}
	task, err := tasks.Add(agent.Task{}, nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	tasks.Notifications().Push(agent.Notification{TaskID: task.ID, Kind: agent.NotificationTaskReport, Text: "progress"})

	m := newTestModelWithBackend(t, backend)
	next, cmd := m.maybeWake()
	if cmd == nil || !next.running {
		t.Fatalf("maybeWake() = running %v, cmd %v; want an active wake", next.running, cmd)
	}
	cmdDone := make(chan tea.Msg, 1)
	go func() { cmdDone <- cmd() }()
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("wake runner did not start")
	}
	updated, _ := next.Update(keyPress(tea.KeyEscape))
	if !updated.(Model).running {
		t.Fatal("escape ended the turn before the canceled wake completed")
	}
	select {
	case <-canceled:
	case <-time.After(time.Second):
		t.Fatal("escape did not cancel the wake context")
	}
	select {
	case <-cmdDone:
	case <-time.After(time.Second):
		t.Fatal("wake command did not complete after cancellation")
	}
}

type wakeCompactionRunner struct {
	tasks *agent.Tasks
}

func (r wakeCompactionRunner) Tasks() *agent.Tasks { return r.tasks }

func (r wakeCompactionRunner) Run(_ context.Context, _ string, emit func(agent.Event)) error {
	emit(agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{CheckpointID: "wake-checkpoint"}})
	return nil
}

func (r wakeCompactionRunner) Compact(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
	return agent.CompactionResult{Noop: true}, nil
}

func TestWakeWorkerPreservesCompactionAggregateUsage(t *testing.T) {
	tasks := agent.NewTasks()
	tasks.Notifications().Push(agent.Notification{TaskID: "t1", Kind: agent.NotificationTaskReport, Text: "progress"})
	usage := model.Usage{InputTokens: 7, OutputTokens: 9}
	backend := &fakeBackend{tasks: tasks, info: app.Info{Usage: usage, UsagePresent: true}}
	sess := session.NewMemory(session.Header{ID: "tui-wake-compaction", Workspace: t.TempDir()})
	controller, err := app.New(app.SessionReplacement{Session: sess, Runner: wakeCompactionRunner{tasks: tasks}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = controller.Close() })
	wake, err := controller.PrepareWake(context.Background())
	if err != nil || wake == nil {
		t.Fatalf("PrepareWake() = %v, %v", wake, err)
	}
	stream := newTurnStream()
	go runTurnWorker(context.Background(), backend, "", stream, wake)

	envelope := <-stream.channel
	if envelope.event == nil || envelope.aggregateUsage != usage || !envelope.aggregateUsagePresent {
		t.Fatalf("compaction envelope = %#v, want aggregate usage %+v present", envelope, usage)
	}
	envelope.applicationAck.acknowledge()
	done := <-stream.channel
	if !done.done || done.err != nil {
		t.Fatalf("wake completion = %#v, want successful completion", done)
	}
}

func TestMaybeWakeNoOpWhenNoPendingNotification(t *testing.T) {
	tasks := agent.NewTasks()
	addTask(t, tasks, agent.TaskRunning, "explorer")
	m := newTestModelWithBackend(t, taskBackendWithWake(t, tasks))

	next, cmd := m.maybeWake()
	if cmd != nil {
		t.Fatal("maybeWake() cmd = non-nil, want nil without a pending notification")
	}
	if next.running {
		t.Fatal("maybeWake() marked the model running without a pending notification")
	}
}

func TestMaybeWakeNoOpWhileTurnActive(t *testing.T) {
	tasks := agent.NewTasks()
	taskID := addTask(t, tasks, agent.TaskRunning, "explorer").ID
	tasks.Notifications().Push(agent.Notification{TaskID: taskID, Kind: agent.NotificationTaskReport, Text: "progress"})

	m := newTestModelWithBackend(t, taskBackendWithWake(t, tasks))
	m.running = true

	_, cmd := m.maybeWake()
	if cmd != nil {
		t.Fatal("maybeWake() cmd = non-nil, want nil while a turn is already running")
	}
}

func TestMaybeWakeNoOpWhileOverlayOpen(t *testing.T) {
	tasks := agent.NewTasks()
	taskID := addTask(t, tasks, agent.TaskRunning, "explorer").ID
	tasks.Notifications().Push(agent.Notification{TaskID: taskID, Kind: agent.NotificationTaskReport, Text: "progress"})

	m := newTestModelWithBackend(t, taskBackendWithWake(t, tasks))
	m.overlay = overlayHelp

	_, cmd := m.maybeWake()
	if cmd != nil {
		t.Fatal("maybeWake() cmd = non-nil, want nil while an overlay is open")
	}
}

func TestMaybeWakeNoOpDuringResumePicker(t *testing.T) {
	tasks := agent.NewTasks()
	taskID := addTask(t, tasks, agent.TaskRunning, "explorer").ID
	tasks.Notifications().Push(agent.Notification{TaskID: taskID, Kind: agent.NotificationTaskReport, Text: "progress"})

	m := newTestModelWithBackend(t, taskBackendWithWake(t, tasks))
	m.resume.mode = resumeLoaded

	_, cmd := m.maybeWake()
	if cmd != nil {
		t.Fatal("maybeWake() cmd = non-nil, want nil while the resume picker is active")
	}
}

func TestHandleSubmitDuringWakeTurnKeepsEditorTextAndReportsBusy(t *testing.T) {
	m := newTestModel(t)
	m.running = true
	m.editor.SetValue("still typing")

	next, cmd := m.handleSubmit()
	updated, ok := next.(Model)
	if !ok {
		t.Fatalf("handleSubmit() returned %T, want Model", next)
	}
	if cmd != nil {
		t.Fatal("handleSubmit() cmd = non-nil, want nil while a turn (wake or otherwise) is active")
	}
	if updated.editor.Value() != "still typing" {
		t.Fatalf("editor value = %q, want unchanged", updated.editor.Value())
	}
	if updated.statusText != app.ErrPromptActive.Error() {
		t.Fatalf("statusText = %q, want %q", updated.statusText, app.ErrPromptActive.Error())
	}
}

func TestFinishTurnWakesWhenNotificationArrivedDuringTurn(t *testing.T) {
	tasks := agent.NewTasks()
	m := newTestModelWithBackend(t, taskBackendWithWake(t, tasks))
	m.running = true
	m.cancel = func() {}

	taskID := addTask(t, tasks, agent.TaskRunning, "explorer").ID
	tasks.Notifications().Push(agent.Notification{TaskID: taskID, Kind: agent.NotificationTaskReport, Text: "progress"})

	next, cmd := m.finishTurn(turnEnvelope{})
	updated, ok := next.(Model)
	if !ok {
		t.Fatalf("finishTurn() returned %T, want Model", next)
	}
	if cmd == nil {
		t.Fatal("finishTurn() cmd = nil, want a wake turn started")
	}
	if !updated.running {
		t.Fatal("finishTurn() did not start the wake turn")
	}
}

// --- /tasks and /task commands (item 5) ---

func lastEntryText(t *testing.T, m Model) string {
	t.Helper()
	if len(m.entries) == 0 {
		t.Fatal("no entries appended")
	}
	return m.entries[len(m.entries)-1].Raw
}

func TestTasksCommandListsTasks(t *testing.T) {
	tasks := agent.NewTasks()
	addTask(t, tasks, agent.TaskRunning, "explorer")
	addTask(t, tasks, agent.TaskQueued, "reviewer")
	m := resizeModel(t, newTestModelWithBackend(t, &fakeBackend{tasks: tasks}), 80, 24)

	got, _ := submitCommand(t, m, "/tasks")
	text := lastEntryText(t, got)
	lines := strings.Split(text, "\n")
	if len(lines) != 2 {
		t.Fatalf("entry lines = %d, want 2: %q", len(lines), text)
	}
}

func TestTasksCommandNoTasks(t *testing.T) {
	m := resizeModel(t, newTestModelWithBackend(t, &fakeBackend{tasks: agent.NewTasks()}), 80, 24)
	got, _ := submitCommand(t, m, "/tasks")
	if text := lastEntryText(t, got); text != "no tasks in this session" {
		t.Fatalf("entry = %q", text)
	}
}

func TestTasksCommandWithoutRegistry(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 24)
	got, _ := submitCommand(t, m, "/tasks")
	if text := lastEntryText(t, got); text != "sub-agents are not available" {
		t.Fatalf("entry = %q", text)
	}
}

func TestTaskCommandShowsDetail(t *testing.T) {
	tasks := agent.NewTasks()
	history := []model.Message{
		{Role: model.RoleAssistant, Blocks: []model.Block{
			{Type: model.BlockToolCall, ToolName: "read", Arguments: json.RawMessage(`{"path":"main.go"}`)},
		}},
	}
	added, err := tasks.Add(agent.Task{Description: "review", Model: "gpt-4o-mini"}, nil, func() []model.Message { return history })
	if err != nil {
		t.Fatalf("Add() = %v", err)
	}
	tasks.Finish(added.ID, agent.TaskSucceeded, time.Now(), "no issues found", "")

	m := resizeModel(t, newTestModelWithBackend(t, &fakeBackend{tasks: tasks}), 80, 24)
	got, _ := submitCommand(t, m, "/task "+added.ID)
	text := lastEntryText(t, got)
	for _, want := range []string{"model: gpt-4o-mini", "read", "result: no issues found"} {
		if !strings.Contains(text, want) {
			t.Fatalf("entry missing %q: %q", want, text)
		}
	}
}

func TestTaskCommandByName(t *testing.T) {
	tasks := agent.NewTasks()
	canceled := false
	added, err := tasks.Add(agent.Task{Name: "lint-check", Description: "review"}, func() { canceled = true }, nil)
	if err != nil {
		t.Fatalf("Add() = %v", err)
	}
	m := resizeModel(t, newTestModelWithBackend(t, &fakeBackend{tasks: tasks}), 80, 24)

	got, _ := submitCommand(t, m, "/task lint-check")
	if text := lastEntryText(t, got); !strings.Contains(text, added.ID) {
		t.Fatalf("entry = %q, want it to include the resolved id %q", text, added.ID)
	}

	got, _ = submitCommand(t, got, "/task cancel lint-check")
	if !canceled {
		t.Fatal("cancel func was not called")
	}
	if text := lastEntryText(t, got); text != "canceled lint-check" {
		t.Fatalf("entry = %q", text)
	}
}

func TestTaskCommandCancel(t *testing.T) {
	tasks := agent.NewTasks()
	canceled := false
	added, err := tasks.Add(agent.Task{}, func() { canceled = true }, nil)
	if err != nil {
		t.Fatalf("Add() = %v", err)
	}
	m := resizeModel(t, newTestModelWithBackend(t, &fakeBackend{tasks: tasks}), 80, 24)

	got, _ := submitCommand(t, m, "/task cancel "+added.ID)
	if !canceled {
		t.Fatal("cancel func was not called")
	}
	if text := lastEntryText(t, got); text != "canceled "+added.ID {
		t.Fatalf("entry = %q", text)
	}
}

func TestTaskCommandCancelUnknown(t *testing.T) {
	m := resizeModel(t, newTestModelWithBackend(t, &fakeBackend{tasks: agent.NewTasks()}), 80, 24)
	got, _ := submitCommand(t, m, "/task cancel nope")
	if text := lastEntryText(t, got); !strings.Contains(text, "nope") {
		t.Fatalf("entry = %q, want an error naming the unknown id", text)
	}
}

func TestTaskCommandUnknownID(t *testing.T) {
	m := resizeModel(t, newTestModelWithBackend(t, &fakeBackend{tasks: agent.NewTasks()}), 80, 24)
	got, _ := submitCommand(t, m, "/task nope")
	if text := lastEntryText(t, got); text != "unknown task: nope" {
		t.Fatalf("entry = %q", text)
	}
}

// --- cancel-on-replacement notice (item 5) ---

func TestApplyNewSessionResultNotesCanceledTasks(t *testing.T) {
	tasks := agent.NewTasks()
	addTask(t, tasks, agent.TaskRunning, "explorer")
	addTask(t, tasks, agent.TaskRunning, "reviewer")
	backend := &fakeBackend{tasks: tasks, newSession: func() error { return nil }}
	m := newTestModelWithBackend(t, backend)
	m.newSessionPending = true
	m.pendingCanceledTasks = m.countActiveTasks()
	if m.pendingCanceledTasks != 2 {
		t.Fatalf("pendingCanceledTasks = %d, want 2", m.pendingCanceledTasks)
	}

	next, cmd := m.applyNewSessionResult(newSessionResultMsg{generation: m.newSessionGeneration})
	updated := next.(Model)
	if text := lastEntryText(t, updated); text != "canceled 2 running tasks" {
		t.Fatalf("entry = %q", text)
	}
	if cmd != nil {
		t.Fatal("applyNewSessionResult() cmd = non-nil, want nil (dispatch's taskUpdateMsg case is the sole re-arm point)")
	}
}

func TestApplyNewSessionResultNoNoticeWithoutCanceledTasks(t *testing.T) {
	m := newTestModel(t)
	m.newSessionPending = true
	entriesBefore := len(m.entries)

	next, _ := m.applyNewSessionResult(newSessionResultMsg{generation: m.newSessionGeneration})
	updated := next.(Model)
	if len(updated.entries) != entriesBefore {
		t.Fatalf("entries = %d, want unchanged at %d", len(updated.entries), entriesBefore)
	}
}
