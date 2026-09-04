package agent

import (
	"sync"

	"github.com/baiyuqing/otto/internal/model"
)

// NotificationKind identifies why a Notification was pushed, which selects
// its ContextType and, for the parent-facing Runner, its rendered text
// format.
type NotificationKind string

const (
	NotificationTaskFinished NotificationKind = "task_finished"
	NotificationTaskReport   NotificationKind = "task_report"
	NotificationMessage      NotificationKind = "message"
)

// Notification is one item queued for delivery into an agent's next
// provider request: a task's terminal result or progress report for the
// parent, or a message for a child.
type Notification struct {
	TaskID string
	Kind   NotificationKind
	Text   string
	Usage  *model.Usage
}

// ContextType is the model.Message.ContextType value a delivered
// Notification is persisted and emitted under.
func (n Notification) ContextType() string {
	if n.Kind == NotificationMessage {
		return "parent_message"
	}
	return "task_notification"
}

// Inbox is a mutex-protected FIFO of Notifications. All methods are safe to
// call on a nil *Inbox, which behaves as permanently empty.
type Inbox struct {
	mu       sync.Mutex
	items    []Notification
	onChange func()
}

// NewInbox creates an Inbox. onChange, if non-nil, is called after every
// Push, after a Drain that removed items, and after a Remove that removed an
// item. It is called without the Inbox lock held.
func NewInbox(onChange func()) *Inbox {
	return &Inbox{onChange: onChange}
}

// Push appends a notification. A nil receiver is a no-op.
func (b *Inbox) Push(n Notification) {
	if b == nil {
		return
	}
	b.mu.Lock()
	b.items = append(b.items, n)
	b.mu.Unlock()
	b.notify()
}

// Drain returns all queued notifications in order and empties the inbox.
func (b *Inbox) Drain() []Notification {
	if b == nil {
		return nil
	}
	b.mu.Lock()
	items := b.items
	b.items = nil
	b.mu.Unlock()
	if len(items) > 0 {
		b.notify()
	}
	return items
}

// Remove removes and returns the first notification matching taskID and
// kind, if any.
func (b *Inbox) Remove(taskID string, kind NotificationKind) (Notification, bool) {
	if b == nil {
		return Notification{}, false
	}
	b.mu.Lock()
	var removed Notification
	found := false
	for i, item := range b.items {
		if item.TaskID == taskID && item.Kind == kind {
			removed = item
			found = true
			b.items = append(b.items[:i], b.items[i+1:]...)
			break
		}
	}
	b.mu.Unlock()
	if found {
		b.notify()
	}
	return removed, found
}

// Len reports the number of queued notifications.
func (b *Inbox) Len() int {
	if b == nil {
		return 0
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	return len(b.items)
}

func (b *Inbox) notify() {
	if b.onChange != nil {
		b.onChange()
	}
}
