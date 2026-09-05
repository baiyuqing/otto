package agent

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
)

func finalResponse(text string) providerScript {
	return providerScript{response: provider.Response{
		Message: model.Message{FinishReason: model.FinishStop, Usage: &model.Usage{InputTokens: 1, OutputTokens: 1}, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: text}}},
	}}
}

func TestRunDeliversPendingNotificationsAfterUserMessage(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{finalResponse("ok")}}
	tasks := NewTasks()
	tasks.Notifications().Push(Notification{TaskID: "t1", Kind: NotificationTaskFinished, Text: "[task-notification] task t1 succeeded\nreport", Usage: &model.Usage{InputTokens: 5}})
	memory := session.NewMemory(testHeader(t))
	runner := New(fakeProvider, nil, memory, Options{Model: "test", Now: fixedClock, NewID: fixedIDs(), Tasks: tasks, Inbox: tasks.Notifications()})

	var events []Event
	if err := runner.Run(context.Background(), "hello", func(event Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}

	messages := memory.Messages()
	if len(messages) != 3 || messages[0].Role != model.RoleUser || messages[1].Role != model.RoleContext || messages[2].Role != model.RoleAssistant {
		t.Fatalf("unexpected message sequence: %#v", messages)
	}
	notification := messages[1]
	if notification.ContextType != "task_notification" || !notification.Display || notification.Text() != "[task-notification] task t1 succeeded\nreport" {
		t.Fatalf("notification message = %#v", notification)
	}
	if notification.ContextMetadata == nil || notification.ContextMetadata.TaskID != "t1" {
		t.Fatalf("notification metadata = %#v, want task id t1", notification.ContextMetadata)
	}
	if notification.Usage == nil || notification.Usage.InputTokens != 5 {
		t.Fatalf("notification usage = %#v", notification.Usage)
	}
	request := fakeProvider.requests[0]
	if len(request.Messages) != 2 || request.Messages[1].Role != model.RoleContext {
		t.Fatalf("request messages = %#v", request.Messages)
	}
	var seen []Event
	for _, event := range events {
		if event.Type == EventNotification {
			seen = append(seen, event)
		}
	}
	if len(seen) != 1 || seen[0].TaskID != "t1" || seen[0].Text != notification.Text() || seen[0].Usage.InputTokens != 5 || !seen[0].UsagePresent {
		t.Fatalf("notification events = %#v", seen)
	}
	if tasks.Pending() != 0 {
		t.Fatalf("Pending() = %d after drain", tasks.Pending())
	}
}

func TestRunDrainsInboxBeforeNextProviderRequest(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{
		{response: provider.Response{
			Message: model.Message{FinishReason: model.FinishToolCalls, Role: model.RoleAssistant, Blocks: []model.Block{
				{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "start", Arguments: json.RawMessage(`{}`)},
			}},
		}},
		finalResponse("done"),
	}}
	inbox := NewInbox(nil)
	start := &recordingTool{name: "start", execute: func(context.Context, json.RawMessage) tool.Result {
		inbox.Push(Notification{TaskID: "t1", Kind: NotificationTaskFinished, Text: "[task-notification] task t1 succeeded"})
		return tool.Result{Content: "task t1 started"}
	}}
	registry, err := tool.NewRegistry(start)
	if err != nil {
		t.Fatal(err)
	}
	memory := session.NewMemory(testHeader(t))
	runner := New(fakeProvider, registry, memory, Options{Model: "test", Now: fixedClock, NewID: fixedIDs(), Inbox: inbox})

	if err := runner.Run(context.Background(), "delegate", nil); err != nil {
		t.Fatal(err)
	}

	second := fakeProvider.requests[1].Messages
	if len(second) != 4 || second[2].Role != model.RoleTool || second[3].Role != model.RoleContext || second[3].ContextType != "task_notification" {
		t.Fatalf("second request messages = %#v", second)
	}
	if got := memory.Messages(); len(got) != 5 || got[3].Role != model.RoleContext {
		t.Fatalf("persisted messages = %#v", got)
	}
}

func TestRunEmptyTextWithoutNotificationsFails(t *testing.T) {
	fakeProvider := &scriptedProvider{}
	tasks := NewTasks()
	runner := New(fakeProvider, nil, session.NewMemory(testHeader(t)), Options{Model: "test", Tasks: tasks, Inbox: tasks.Notifications()})

	err := runner.Run(context.Background(), "  ", nil)
	if !errors.Is(err, ErrEmptyUserText) {
		t.Fatalf("Run() error = %v, want ErrEmptyUserText", err)
	}
	if len(fakeProvider.requests) != 0 {
		t.Fatal("provider was called")
	}
}

func TestRunEmptyTextWithNotificationIsAWakeTurn(t *testing.T) {
	fakeProvider := &scriptedProvider{scripts: []providerScript{finalResponse("noted")}}
	inbox := NewInbox(nil)
	inbox.Push(Notification{TaskID: "t2", Kind: NotificationTaskFinished, Text: "[task-notification] task t2 failed"})
	memory := session.NewMemory(testHeader(t))
	runner := New(fakeProvider, nil, memory, Options{Model: "test", Now: fixedClock, NewID: fixedIDs(), Inbox: inbox})

	var events []Event
	if err := runner.Run(context.Background(), "", func(event Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}

	messages := memory.Messages()
	if len(messages) != 2 || messages[0].Role != model.RoleContext || messages[1].Role != model.RoleAssistant {
		t.Fatalf("wake turn messages = %#v", messages)
	}
	if got := eventTypes(events); got[0] != EventAgentStarted || got[1] != EventNotification || got[len(got)-1] != EventAgentFinished {
		t.Fatalf("event flow = %v", got)
	}
}

func TestInboxPushDrainRemoveAndNotify(t *testing.T) {
	changes := 0
	inbox := NewInbox(func() { changes++ })
	inbox.Push(Notification{TaskID: "t1", Kind: NotificationTaskFinished, Text: "one"})
	inbox.Push(Notification{TaskID: "t1", Kind: NotificationTaskReport, Text: "two"})
	inbox.Push(Notification{TaskID: "t2", Kind: NotificationTaskFinished, Text: "three"})
	if inbox.Len() != 3 || changes != 3 {
		t.Fatalf("Len() = %d, changes = %d", inbox.Len(), changes)
	}
	removed, ok := inbox.Remove("t1", NotificationTaskFinished)
	if !ok || removed.Text != "one" {
		t.Fatalf("Remove() = %#v, %v", removed, ok)
	}
	if _, ok := inbox.Remove("t9", NotificationTaskFinished); ok {
		t.Fatal("Remove() found a missing notification")
	}
	drained := inbox.Drain()
	if len(drained) != 2 || drained[0].Text != "two" || drained[1].Text != "three" || inbox.Len() != 0 {
		t.Fatalf("Drain() = %#v", drained)
	}
}

func TestTasksLifecycle(t *testing.T) {
	tasks := NewTasks()
	canceled := false
	history := []model.Message{{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "child prompt"}}}}
	task, err := tasks.Add(Task{Agent: "explorer", Prompt: "find sessions"}, func() { canceled = true }, func() []model.Message { return history })
	if err != nil {
		t.Fatal(err)
	}
	if task.ID != "t1" || task.Status != TaskQueued {
		t.Fatalf("Add() = %#v", task)
	}
	select {
	case <-tasks.Updates():
	default:
		t.Fatal("Add() did not signal Updates()")
	}
	if got, ok := tasks.Get("t1"); !ok || got.Prompt != "find sessions" {
		t.Fatalf("Get() = %#v, %v", got, ok)
	}
	if got, ok := tasks.History("t1"); !ok || len(got) != 1 || got[0].Text() != "child prompt" {
		t.Fatalf("History() = %#v, %v", got, ok)
	}
	tasks.MarkRunning("t1", time.Unix(1, 0))
	tasks.RecordProviderStep("t1", model.Usage{}, "", false)
	tasks.RecordProviderStep("t1", model.Usage{}, "", true)
	waitErr := make(chan error, 1)
	go func() {
		_, err := tasks.Wait(context.Background(), "t1")
		waitErr <- err
	}()
	select {
	case <-waitErr:
		t.Fatal("Wait() returned before the task finished")
	case <-time.After(20 * time.Millisecond):
	}
	if err := tasks.Cancel("t1"); err != nil || !canceled {
		t.Fatalf("Cancel() err = %v, canceled = %v", err, canceled)
	}
	tasks.Finish("t1", TaskCanceled, time.Unix(2, 0), "", "")
	if err := <-waitErr; err != nil {
		t.Fatalf("Wait() error = %v", err)
	}
	if got := tasks.List(); len(got) != 1 || got[0].Status != TaskCanceled || got[0].Steps != 2 {
		t.Fatalf("List() = %#v", got)
	}
	if got := tasks.List()[0]; !got.UsagePresent || got.Usage != (model.Usage{}) {
		t.Fatalf("usage presence = %v, usage=%+v", got.UsagePresent, got.Usage)
	}
	if err := tasks.Cancel("t1"); err == nil {
		t.Fatal("Cancel() of a finished task succeeded")
	}
	if err := tasks.Cancel("missing"); err == nil {
		t.Fatal("Cancel() of an unknown task succeeded")
	}

	tasks.Notifications().Push(Notification{TaskID: "t1", Kind: NotificationTaskFinished, Text: "x"})
	if tasks.Pending() != 1 {
		t.Fatalf("Pending() = %d", tasks.Pending())
	}
	second, _ := tasks.Add(Task{Prompt: "second"}, func() { canceled = true }, nil)
	if second.ID != "t2" {
		t.Fatalf("second task id = %q", second.ID)
	}
	canceled = false
	tasks.Close()
	if !canceled {
		t.Fatal("Close() did not cancel the running task")
	}
	if _, ok := <-tasks.Updates(); ok {
		t.Fatal("Updates() still open after Close()")
	}
	if _, err := tasks.Add(Task{}, nil, nil); err == nil {
		t.Fatal("Add() after Close() succeeded")
	}
}

func TestTasksExplicitTransitions(t *testing.T) {
	tasks := NewTasks()
	created, err := tasks.Add(Task{
		Prompt:     "inspect",
		Status:     TaskSucceeded,
		StartedAt:  time.Unix(1, 0),
		FinishedAt: time.Unix(2, 0),
		Steps:      9,
		ToolCalls:  8,
		LastTool:   "stale",
		LastText:   "stale",
		Usage:      model.Usage{InputTokens: 10},
		Result:     "stale",
		Error:      "stale",
	}, nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	if created.Status != TaskQueued || !created.StartedAt.IsZero() || !created.FinishedAt.IsZero() || created.Steps != 0 || created.ToolCalls != 0 || created.LastTool != "" || created.LastText != "" || created.Usage != (model.Usage{}) || created.Result != "" || created.Error != "" {
		t.Fatalf("Add() retained stale runtime fields: %#v", created)
	}
	startedAt := time.Unix(10, 0)
	tasks.MarkRunning(created.ID, startedAt)
	tasks.MarkRunning(created.ID, time.Unix(11, 0))
	tasks.RecordToolCall(created.ID, "grep \"session\"")
	tasks.RecordProviderStep(created.ID, model.Usage{InputTokens: 3, OutputTokens: 2}, "found it", true)

	started, ok := tasks.Get(created.ID)
	if !ok || started.Status != TaskRunning || !started.StartedAt.Equal(startedAt) || started.ToolCalls != 1 || started.Steps != 1 || started.LastTool != "grep \"session\"" || started.LastText != "found it" {
		t.Fatalf("running snapshot = %#v, %v", started, ok)
	}

	tasks.Finish(created.ID, TaskSucceeded, time.Unix(20, 0), "done", "")
	tasks.Finish(created.ID, TaskFailed, time.Unix(30, 0), "overwritten", "bad")
	tasks.RecordToolCall(created.ID, "late")
	tasks.RecordProviderStep(created.ID, model.Usage{InputTokens: 1}, "late", true)
	finished, ok := tasks.Get(created.ID)
	if !ok || finished.Status != TaskSucceeded || finished.Result != "done" || finished.Error != "" || !finished.FinishedAt.Equal(time.Unix(20, 0)) {
		t.Fatalf("finished snapshot = %#v, %v", finished, ok)
	}
}

func TestTasksNames(t *testing.T) {
	t.Run("Get/History/Cancel/Wait resolve a name to the same task as its id", func(t *testing.T) {
		tasks := NewTasks()
		canceled := false
		history := []model.Message{{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "child prompt"}}}}
		task, err := tasks.Add(Task{Name: "lint-check", Prompt: "lint"}, func() { canceled = true }, func() []model.Message { return history })
		if err != nil {
			t.Fatal(err)
		}
		if task.ID != "t1" || task.Name != "lint-check" {
			t.Fatalf("Add() = %#v", task)
		}
		byID, ok := tasks.Get("t1")
		if !ok {
			t.Fatal("Get(id) missing")
		}
		if byName, ok := tasks.Get("lint-check"); !ok || byName != byID {
			t.Fatalf("Get(name) = %#v, %v, want %#v", byName, ok, byID)
		}
		if h, ok := tasks.History("lint-check"); !ok || len(h) != 1 || h[0].Text() != "child prompt" {
			t.Fatalf("History(name) = %#v, %v", h, ok)
		}

		waitErr := make(chan error, 1)
		go func() {
			_, err := tasks.Wait(context.Background(), "lint-check")
			waitErr <- err
		}()
		select {
		case <-waitErr:
			t.Fatal("Wait(name) returned before the task finished")
		case <-time.After(20 * time.Millisecond):
		}

		if err := tasks.Cancel("lint-check"); err != nil || !canceled {
			t.Fatalf("Cancel(name) err = %v, canceled = %v", err, canceled)
		}
		tasks.Finish("t1", TaskCanceled, time.Unix(2, 0), "", "")
		if err := <-waitErr; err != nil {
			t.Fatalf("Wait(name) error = %v", err)
		}
	})

	t.Run("duplicate name is rejected and names the existing id", func(t *testing.T) {
		tasks := NewTasks()
		if _, err := tasks.Add(Task{Name: "lint-check"}, nil, nil); err != nil {
			t.Fatal(err)
		}
		_, err := tasks.Add(Task{Name: "lint-check"}, nil, nil)
		if err == nil || !strings.Contains(err.Error(), `"lint-check" already used by t1`) {
			t.Fatalf("Add() duplicate name error = %v", err)
		}
	})

	t.Run("a name of the form tN is reserved for task ids, even when no such task exists", func(t *testing.T) {
		tasks := NewTasks()
		_, err := tasks.Add(Task{Name: "t7"}, nil, nil)
		if err == nil || !strings.Contains(err.Error(), `"t7" is reserved for task ids`) {
			t.Fatalf("Add() reserved-name error = %v", err)
		}
		if _, ok := tasks.Get("t7"); ok {
			t.Fatal("Get(\"t7\") found a task that was never created")
		}
	})

	t.Run("an invalid name is rejected", func(t *testing.T) {
		tasks := NewTasks()
		for _, name := range []string{"bad name", strings.Repeat("a", 65)} {
			_, err := tasks.Add(Task{Name: name}, nil, nil)
			if err == nil || !strings.Contains(err.Error(), "is invalid") {
				t.Fatalf("Add(%q) error = %v, want it to mention \"is invalid\"", name, err)
			}
		}
	})

	t.Run("a rejected Add does not consume an id", func(t *testing.T) {
		tasks := NewTasks()
		if _, err := tasks.Add(Task{Name: "bad name"}, nil, nil); err == nil {
			t.Fatal("expected an error")
		}
		task, err := tasks.Add(Task{}, nil, nil)
		if err != nil || task.ID != "t1" {
			t.Fatalf("Add() after a rejected Add = %#v, %v, want id t1", task, err)
		}
	})

	t.Run("two unnamed tasks do not collide", func(t *testing.T) {
		tasks := NewTasks()
		first, err := tasks.Add(Task{}, nil, nil)
		if err != nil {
			t.Fatal(err)
		}
		second, err := tasks.Add(Task{}, nil, nil)
		if err != nil {
			t.Fatal(err)
		}
		if first.ID == second.ID {
			t.Fatalf("unnamed tasks got the same id: %q", first.ID)
		}
	})
}

func TestTasksWaitHonorsContext(t *testing.T) {
	tasks := NewTasks()
	if _, err := tasks.Add(Task{}, nil, nil); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Millisecond)
	defer cancel()
	if _, err := tasks.Wait(ctx, "t1"); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("Wait() error = %v", err)
	}
	if _, err := tasks.Wait(context.Background(), "missing"); err == nil {
		t.Fatal("Wait() on an unknown task succeeded")
	}
}
