package agent

import (
	"context"
	"errors"
	"fmt"
	"sync"
	"time"

	"github.com/baiyuqing/otto/internal/model"
)

// TaskStatus is a sub-agent task's position in its lifecycle:
// queued -> running -> {succeeded, failed, canceled}.
type TaskStatus string

const (
	TaskQueued    TaskStatus = "queued"
	TaskRunning   TaskStatus = "running"
	TaskSucceeded TaskStatus = "succeeded"
	TaskFailed    TaskStatus = "failed"
	TaskCanceled  TaskStatus = "canceled"
)

// Task is one sub-agent task record, as shown by agent_status and the
// frontends.
type Task struct {
	ID          string
	Agent       string
	Description string
	Prompt      string
	Context     string
	Status      TaskStatus
	CreatedAt   time.Time
	StartedAt   time.Time
	FinishedAt  time.Time
	Steps       int
	ToolCalls   int
	LastTool    string
	LastText    string
	Usage       model.Usage
	Result      string
	Error       string
}

// Final reports whether the task has reached a terminal status.
func (t Task) Final() bool {
	switch t.Status {
	case TaskSucceeded, TaskFailed, TaskCanceled:
		return true
	default:
		return false
	}
}

var errTasksClosed = errors.New("task registry closed")

type taskEntry struct {
	task    Task
	cancel  func()
	history func() []model.Message
	done    chan struct{}
}

// Tasks is the per-session sub-agent task registry: it tracks task records,
// their cancel/history hooks, and the parent's notification inbox, and
// signals frontends on every change. All methods are safe to call on a nil
// *Tasks where noted.
type Tasks struct {
	mu      sync.Mutex
	closed  bool
	counter int
	order   []string
	entries map[string]*taskEntry
	updates chan struct{}
	inbox   *Inbox
}

// NewTasks creates an empty registry.
func NewTasks() *Tasks {
	t := &Tasks{
		entries: make(map[string]*taskEntry),
		updates: make(chan struct{}, 1),
	}
	t.inbox = NewInbox(t.signal)
	return t
}

// Add registers a new task, assigning it the next "tN" id and TaskQueued
// status regardless of the status passed in. cancel and history may be nil.
func (t *Tasks) Add(task Task, cancel func(), history func() []model.Message) (Task, error) {
	t.mu.Lock()
	defer t.mu.Unlock()
	if t.closed {
		return Task{}, errTasksClosed
	}
	t.counter++
	task.ID = fmt.Sprintf("t%d", t.counter)
	task.Status = TaskQueued
	entry := &taskEntry{task: task, cancel: cancel, history: history, done: make(chan struct{})}
	t.entries[task.ID] = entry
	t.order = append(t.order, task.ID)
	t.signalLocked()
	return entry.task, nil
}

// Update applies fn to the task under the registry lock. When the task
// becomes final for the first time, its Wait channel is closed. Unknown id
// is a no-op.
func (t *Tasks) Update(id string, fn func(*Task)) {
	if t == nil || fn == nil {
		return
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	entry, ok := t.entries[id]
	if !ok {
		return
	}
	wasFinal := entry.task.Final()
	fn(&entry.task)
	if !wasFinal && entry.task.Final() {
		close(entry.done)
	}
	t.signalLocked()
}

// List returns a copy of every task, in creation order.
func (t *Tasks) List() []Task {
	if t == nil {
		return nil
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	list := make([]Task, 0, len(t.order))
	for _, id := range t.order {
		if entry, ok := t.entries[id]; ok {
			list = append(list, entry.task)
		}
	}
	return list
}

// Get returns a copy of one task.
func (t *Tasks) Get(id string) (Task, bool) {
	if t == nil {
		return Task{}, false
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	entry, ok := t.entries[id]
	if !ok {
		return Task{}, false
	}
	return entry.task, true
}

// History returns the task's child session messages, calling the stored
// history func outside the registry lock. It returns nil, true when the
// task exists but has no history func.
func (t *Tasks) History(id string) ([]model.Message, bool) {
	if t == nil {
		return nil, false
	}
	t.mu.Lock()
	entry, ok := t.entries[id]
	t.mu.Unlock()
	if !ok {
		return nil, false
	}
	if entry.history == nil {
		return nil, true
	}
	return entry.history(), true
}

// Cancel invokes the task's cancel func outside the registry lock. It
// errors for an unknown id or an already-final task; it does not itself
// change the task's status.
func (t *Tasks) Cancel(id string) error {
	if t == nil {
		return fmt.Errorf("task %q not found", id)
	}
	t.mu.Lock()
	entry, ok := t.entries[id]
	if !ok {
		t.mu.Unlock()
		return fmt.Errorf("task %q not found", id)
	}
	if entry.task.Final() {
		t.mu.Unlock()
		return fmt.Errorf("task %q already finished", id)
	}
	cancel := entry.cancel
	t.mu.Unlock()
	if cancel != nil {
		cancel()
	}
	return nil
}

// Wait blocks until the task reaches a final status or ctx is done.
func (t *Tasks) Wait(ctx context.Context, id string) (Task, error) {
	if t == nil {
		return Task{}, fmt.Errorf("task %q not found", id)
	}
	t.mu.Lock()
	entry, ok := t.entries[id]
	t.mu.Unlock()
	if !ok {
		return Task{}, fmt.Errorf("task %q not found", id)
	}
	select {
	case <-entry.done:
	case <-ctx.Done():
		return Task{}, ctx.Err()
	}
	t.mu.Lock()
	task := entry.task
	t.mu.Unlock()
	return task, nil
}

// Pending reports the number of notifications waiting for the parent.
func (t *Tasks) Pending() int {
	if t == nil {
		return 0
	}
	return t.inbox.Len()
}

// Updates returns a channel signaled (non-blocking, capacity 1) on every
// registry or inbox change, and closed by Close.
func (t *Tasks) Updates() <-chan struct{} {
	if t == nil {
		return nil
	}
	return t.updates
}

// Notifications returns the parent's notification inbox.
func (t *Tasks) Notifications() *Inbox {
	if t == nil {
		return nil
	}
	return t.inbox
}

// Close cancels every non-final task and closes Updates. It is idempotent
// and safe on a nil receiver.
func (t *Tasks) Close() {
	if t == nil {
		return
	}
	t.mu.Lock()
	if t.closed {
		t.mu.Unlock()
		return
	}
	t.closed = true
	var cancels []func()
	for _, id := range t.order {
		entry := t.entries[id]
		if !entry.task.Final() && entry.cancel != nil {
			cancels = append(cancels, entry.cancel)
		}
	}
	select {
	case <-t.updates:
	default:
	}
	close(t.updates)
	t.mu.Unlock()
	for _, cancel := range cancels {
		cancel()
	}
}

// signalLocked sends a non-blocking signal on updates. Caller must hold t.mu.
func (t *Tasks) signalLocked() {
	if t.closed {
		return
	}
	select {
	case t.updates <- struct{}{}:
	default:
	}
}

// signal is the Inbox onChange callback for the parent inbox; it acquires
// the registry lock itself.
func (t *Tasks) signal() {
	t.mu.Lock()
	t.signalLocked()
	t.mu.Unlock()
}
