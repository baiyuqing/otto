package agent

import (
	"context"
	"errors"
	"fmt"
	"regexp"
	"strings"
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
	ID           string
	Name         string
	Agent        string
	Description  string
	Prompt       string
	Context      string
	Model        string
	Status       TaskStatus
	CreatedAt    time.Time
	StartedAt    time.Time
	FinishedAt   time.Time
	Steps        int
	ToolCalls    int
	LastTool     string
	LastText     string
	Usage        model.Usage
	UsagePresent bool
	Result       string
	Error        string
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

// taskNamePattern matches a valid task name: 1 to 64 letters, digits, '_'
// or '-', starting with a letter or digit. The length is checked
// separately so the error message can report it precisely.
var taskNamePattern = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9_-]*$`)

// reservedTaskNamePattern matches the shape of an auto-assigned task id
// ("t" followed by digits), which a caller-supplied name may not use.
var reservedTaskNamePattern = regexp.MustCompile(`^t[0-9]+$`)

// ErrTaskNotFound and ErrTaskFinished are wrapped by Cancel and Wait so
// callers can classify the failure with errors.Is.
var (
	ErrTaskNotFound = errors.New("not found")
	ErrTaskFinished = errors.New("already finished")
)

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
	names   map[string]string
	updates chan struct{}
	inbox   *Inbox
}

// NewTasks creates an empty registry.
func NewTasks() *Tasks {
	t := &Tasks{
		entries: make(map[string]*taskEntry),
		names:   make(map[string]string),
		updates: make(chan struct{}, 1),
	}
	t.inbox = NewInbox(t.signal)
	return t
}

// Add registers a new task, assigning it the next "tN" id and TaskQueued
// status regardless of the status passed in. cancel and history may be nil.
//
// task.Name, after trimming whitespace, is validated and reserved for the
// life of the registry (finished tasks included): empty means no name;
// otherwise it must be 1 to 64 bytes of letters, digits, '_' or '-'
// starting with a letter or digit, must not look like a task id ("t"
// followed by digits), and must not already be in use. Any validation
// error leaves the registry unchanged: no id is consumed.
func (t *Tasks) Add(task Task, cancel func(), history func() []model.Message) (Task, error) {
	t.mu.Lock()
	defer t.mu.Unlock()
	if t.closed {
		return Task{}, errTasksClosed
	}
	name := strings.TrimSpace(task.Name)
	if name != "" {
		if len(name) > 64 || !taskNamePattern.MatchString(name) {
			return Task{}, fmt.Errorf("task name %q is invalid: 1 to 64 letters, digits, '_' or '-', starting with a letter or digit", name)
		}
		if reservedTaskNamePattern.MatchString(name) {
			return Task{}, fmt.Errorf("task name %q is reserved for task ids", name)
		}
		if existingID, ok := t.names[name]; ok {
			return Task{}, fmt.Errorf("task name %q already used by %s", name, existingID)
		}
	}
	t.counter++
	task.ID = fmt.Sprintf("t%d", t.counter)
	task.Name = name
	task.Status = TaskQueued
	task.StartedAt = time.Time{}
	task.FinishedAt = time.Time{}
	task.Steps = 0
	task.ToolCalls = 0
	task.LastTool = ""
	task.LastText = ""
	task.Usage = model.Usage{}
	task.UsagePresent = false
	task.Result = ""
	task.Error = ""
	entry := &taskEntry{task: task, cancel: cancel, history: history, done: make(chan struct{})}
	t.entries[task.ID] = entry
	t.order = append(t.order, task.ID)
	if name != "" {
		t.names[name] = task.ID
	}
	t.signalLocked()
	return entry.task, nil
}

// entryLocked resolves ref as a task id, then as a task name. Caller must
// hold t.mu.
func (t *Tasks) entryLocked(ref string) (*taskEntry, bool) {
	if entry, ok := t.entries[ref]; ok {
		return entry, true
	}
	if id, ok := t.names[ref]; ok {
		entry, ok := t.entries[id]
		return entry, ok
	}
	return nil, false
}

// MarkRunning moves a queued task to running. Invalid or repeated
// transitions, including unknown IDs, are no-ops.
func (t *Tasks) MarkRunning(id string, startedAt time.Time) {
	if t == nil {
		return
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	entry, ok := t.entries[id]
	if !ok || entry.task.Status != TaskQueued {
		return
	}
	entry.task.Status = TaskRunning
	entry.task.StartedAt = startedAt
	t.signalLocked()
}

// RecordProviderStep records one completed provider round trip. It only
// applies while the task is running; invalid or unknown updates are no-ops.
func (t *Tasks) RecordProviderStep(id string, usage model.Usage, text string, usagePresent bool) {
	if t == nil {
		return
	}
	if usage.Validate() != nil {
		return
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	entry, ok := t.entries[id]
	if !ok || entry.task.Status != TaskRunning {
		return
	}
	entry.task.Steps++
	if usagePresent {
		entry.task.UsagePresent = true
		entry.task.Usage.InputTokens += usage.InputTokens
		entry.task.Usage.OutputTokens += usage.OutputTokens
		entry.task.Usage.CachedInputTokens += usage.CachedInputTokens
	}
	entry.task.LastText = text
	t.signalLocked()
}

// RecordToolCall records the latest tool call while the task is running.
// Invalid or unknown updates are no-ops.
func (t *Tasks) RecordToolCall(id, lastTool string) {
	if t == nil {
		return
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	entry, ok := t.entries[id]
	if !ok || entry.task.Status != TaskRunning {
		return
	}
	entry.task.ToolCalls++
	entry.task.LastTool = lastTool
	t.signalLocked()
}

// Finish moves a queued or running task to a terminal status, records its
// final fields, and releases Wait callers. Repeated or invalid transitions,
// including unknown IDs, are no-ops.
func (t *Tasks) Finish(id string, status TaskStatus, finishedAt time.Time, result, taskError string) {
	if t == nil || !(Task{Status: status}).Final() {
		return
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	entry, ok := t.entries[id]
	if !ok || entry.task.Final() {
		return
	}
	entry.task.Status = status
	entry.task.FinishedAt = finishedAt
	entry.task.Result = result
	entry.task.Error = taskError
	close(entry.done)
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

// Get returns a copy of one task, resolving ref as a task id or a task
// name.
func (t *Tasks) Get(ref string) (Task, bool) {
	if t == nil {
		return Task{}, false
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	entry, ok := t.entryLocked(ref)
	if !ok {
		return Task{}, false
	}
	return entry.task, true
}

// History returns the task's child session messages, calling the stored
// history func outside the registry lock. It returns nil, true when the
// task exists but has no history func. ref may be a task id or a task
// name.
func (t *Tasks) History(ref string) ([]model.Message, bool) {
	if t == nil {
		return nil, false
	}
	t.mu.Lock()
	entry, ok := t.entryLocked(ref)
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
// errors for an unknown ref (a task id or a task name) or an already-final
// task; it does not itself change the task's status.
func (t *Tasks) Cancel(ref string) error {
	if t == nil {
		return fmt.Errorf("task %q %w", ref, ErrTaskNotFound)
	}
	t.mu.Lock()
	entry, ok := t.entryLocked(ref)
	if !ok {
		t.mu.Unlock()
		return fmt.Errorf("task %q %w", ref, ErrTaskNotFound)
	}
	if entry.task.Final() {
		t.mu.Unlock()
		return fmt.Errorf("task %q %w", ref, ErrTaskFinished)
	}
	cancel := entry.cancel
	t.mu.Unlock()
	if cancel != nil {
		cancel()
	}
	return nil
}

// Wait blocks until the task reaches a final status or ctx is done. ref may
// be a task id or a task name.
func (t *Tasks) Wait(ctx context.Context, ref string) (Task, error) {
	if t == nil {
		return Task{}, fmt.Errorf("task %q %w", ref, ErrTaskNotFound)
	}
	t.mu.Lock()
	entry, ok := t.entryLocked(ref)
	t.mu.Unlock()
	if !ok {
		return Task{}, fmt.Errorf("task %q %w", ref, ErrTaskNotFound)
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
