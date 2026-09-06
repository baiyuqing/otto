package server

import (
	"context"
	"errors"
	"strings"
	"sync"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/model"
)

const (
	turnRunning  = "running"
	turnOK       = "ok"
	turnError    = "error"
	turnCanceled = "canceled"
)

// Trigger identifies why a turn started: a user request, or an automatic
// wake turn draining pending sub-agent task notifications.
const (
	triggerUser = "user"
	triggerTask = "task"
)

// turn buffers one session's most-recent turn: the wire events emitted so
// far, accumulated text/usage, and terminal status. emit runs on the agent
// goroutine and performs no I/O, so a slow HTTP reader never applies
// backpressure to the agent. Readers (SSE handlers) take a snapshot and wait
// on the changed channel for more events.
//
// ponytail: only the latest turn per session is retained; add a ring buffer
// of turns if event replay across turns is needed.
type turn struct {
	id      string
	cancel  context.CancelFunc
	trigger string // set once before the turn is published; read-only after

	mu           sync.Mutex
	events       []wireEvent
	changed      chan struct{}
	done         bool
	status       string
	errText      string
	text         strings.Builder
	usage        model.Usage
	usagePresent bool
	toolStart    time.Time
	startedAt    time.Time
	finishedAt   time.Time
}

func newTurn(id string, cancel context.CancelFunc) *turn {
	return &turn{
		id:        id,
		cancel:    cancel,
		changed:   make(chan struct{}),
		status:    turnRunning,
		startedAt: time.Now(),
	}
}

// broadcastLocked wakes every reader blocked in snapshot's returned channel.
// Callers must hold t.mu.
func (t *turn) broadcastLocked() {
	close(t.changed)
	t.changed = make(chan struct{})
}

// emit returns a callback suitable for app.Controller.Prompt: it converts
// each agent.Event to wire form, accumulates text/usage/tool timing, records
// metrics, and buffers the event for streaming readers.
func (t *turn) emit(m *metrics) func(agent.Event) {
	return func(event agent.Event) {
		w := toWire(event)

		t.mu.Lock()
		switch event.Type {
		case agent.EventAgentStarted:
			w.TurnID = t.id
		case agent.EventTextDelta:
			t.text.WriteString(event.Text)
		case agent.EventProviderUsage:
			if event.UsagePresent {
				t.usagePresent = true
				t.usage.InputTokens += event.Usage.InputTokens
				t.usage.OutputTokens += event.Usage.OutputTokens
				t.usage.CachedInputTokens += event.Usage.CachedInputTokens
			}
		case agent.EventNotification:
			if event.UsagePresent {
				t.usagePresent = true
				t.usage.InputTokens += event.Usage.InputTokens
				t.usage.OutputTokens += event.Usage.OutputTokens
				t.usage.CachedInputTokens += event.Usage.CachedInputTokens
			}
		case agent.EventProviderAPICall:
			m.providerAPIRequest(event.ProviderName, event.Model, event.APIStatus, event.APIDuration)
			t.mu.Unlock()
			return
		case agent.EventToolCallStarted:
			t.toolStart = time.Now()
		case agent.EventToolCallFinished:
			m.toolCall(event.ToolName, event.ToolResult.IsError, time.Since(t.toolStart))
		}
		t.events = append(t.events, w)
		t.broadcastLocked()
		t.mu.Unlock()

		if event.Type == agent.EventProviderUsage && event.UsagePresent {
			m.tokens(event.Usage)
		}
	}
}

// finish records the turn's terminal status. err is the error returned by
// app.Controller.Prompt: nil means ok, context.Canceled means canceled,
// anything else means error.
func (t *turn) finish(err error) {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.done = true
	t.finishedAt = time.Now()
	switch {
	case err == nil:
		t.status = turnOK
	case errors.Is(err, context.Canceled):
		t.status = turnCanceled
	default:
		t.status = turnError
		t.errText = err.Error()
	}
	t.broadcastLocked()
}

func (t *turn) isDone() bool {
	t.mu.Lock()
	defer t.mu.Unlock()
	return t.done
}

// duration returns finishedAt - startedAt; only meaningful once isDone().
func (t *turn) duration() time.Duration {
	t.mu.Lock()
	defer t.mu.Unlock()
	return t.finishedAt.Sub(t.startedAt)
}

// snapshot returns every event after index after (seq == index), whether the
// turn is done, and the channel to wait on for more events. Passing the
// result of a previous call's changed channel as a wait signal, then calling
// snapshot again, never misses an event: append always happens before close.
func (t *turn) snapshot(after int) (events []wireEvent, done bool, changed <-chan struct{}) {
	t.mu.Lock()
	defer t.mu.Unlock()
	if after < 0 {
		after = 0
	}
	if after < len(t.events) {
		events = append([]wireEvent(nil), t.events[after:]...)
	}
	return events, t.done, t.changed
}

type turnSummary struct {
	ID           string      `json:"id"`
	Trigger      string      `json:"trigger"`
	Status       string      `json:"status"`
	Error        string      `json:"error,omitempty"`
	Text         string      `json:"text"`
	Usage        model.Usage `json:"usage"`
	UsagePresent bool        `json:"usage_present"`
	StartedAt    time.Time   `json:"started_at"`
	FinishedAt   *time.Time  `json:"finished_at,omitempty"`
}

func (t *turn) summary() turnSummary {
	t.mu.Lock()
	defer t.mu.Unlock()
	s := turnSummary{
		ID:           t.id,
		Trigger:      t.trigger,
		Status:       t.status,
		Error:        t.errText,
		Text:         t.text.String(),
		Usage:        t.usage,
		UsagePresent: t.usagePresent,
		StartedAt:    t.startedAt,
	}
	if t.done {
		finishedAt := t.finishedAt
		s.FinishedAt = &finishedAt
	}
	return s
}
