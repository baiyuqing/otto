package server

import (
	"context"
	"errors"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/tool"
)

func TestTurnEmitAccumulatesTextAndUsage(t *testing.T) {
	tr := newTurn("t1", func() {})
	emit := tr.emit(newMetrics())
	emit(agent.Event{Type: agent.EventAgentStarted})
	emit(agent.Event{Type: agent.EventTextDelta, Text: "hel"})
	emit(agent.Event{Type: agent.EventTextDelta, Text: "lo"})
	emit(agent.Event{Type: agent.EventProviderUsage, Usage: model.Usage{InputTokens: 1, OutputTokens: 2, CachedInputTokens: 3}})
	emit(agent.Event{Type: agent.EventProviderUsage, Usage: model.Usage{InputTokens: 10, OutputTokens: 20}})

	events, done, _ := tr.snapshot(0)
	if done {
		t.Fatal("turn should not be done yet")
	}
	if len(events) != 5 {
		t.Fatalf("len(events) = %d, want 5", len(events))
	}
	if events[0].TurnID != "t1" {
		t.Fatalf("agent_started TurnID = %q, want t1", events[0].TurnID)
	}
	if events[1].TurnID != "" {
		t.Fatalf("text_delta TurnID = %q, want empty", events[1].TurnID)
	}

	s := tr.summary()
	if s.Text != "hello" {
		t.Fatalf("summary text = %q, want hello", s.Text)
	}
	want := model.Usage{InputTokens: 11, OutputTokens: 22, CachedInputTokens: 3}
	if s.Usage != want {
		t.Fatalf("summary usage = %+v, want %+v", s.Usage, want)
	}
	if s.Status != turnRunning {
		t.Fatalf("status = %q, want %q", s.Status, turnRunning)
	}
	if s.FinishedAt != nil {
		t.Fatal("FinishedAt should be nil while running")
	}
	if tr.isDone() {
		t.Fatal("isDone() should be false while running")
	}
}

func TestTurnEmitAccumulatesNotificationUsage(t *testing.T) {
	tr := newTurn("t1", func() {})
	emit := tr.emit(newMetrics())
	emit(agent.Event{Type: agent.EventProviderUsage, Usage: model.Usage{InputTokens: 1, OutputTokens: 2, CachedInputTokens: 3}})
	emit(agent.Event{Type: agent.EventNotification, TaskID: "t1", Text: "[task-notification] task t1 succeeded", Usage: model.Usage{InputTokens: 10, OutputTokens: 20, CachedInputTokens: 1}})

	s := tr.summary()
	want := model.Usage{InputTokens: 11, OutputTokens: 22, CachedInputTokens: 4}
	if s.Usage != want {
		t.Fatalf("summary usage = %+v, want %+v", s.Usage, want)
	}
}

func TestTurnEmitRecordsToolCallMetrics(t *testing.T) {
	m := newMetrics()
	tr := newTurn("t1", func() {})
	emit := tr.emit(m)
	emit(agent.Event{Type: agent.EventToolCallStarted, ToolName: "bash", ToolCallID: "c1"})
	time.Sleep(2 * time.Millisecond)
	emit(agent.Event{Type: agent.EventToolCallFinished, ToolName: "bash", ToolCallID: "c1", ToolResult: tool.Result{Content: "ok"}})

	body := render(t, m)
	if !strings.Contains(body, `otto_tool_calls_total{tool="bash",status="ok"} 1`) {
		t.Fatalf("missing tool call metric:\n%s", body)
	}
}

func TestTurnEmitRecordsProviderTokenMetrics(t *testing.T) {
	m := newMetrics()
	tr := newTurn("t1", func() {})
	emit := tr.emit(m)
	emit(agent.Event{Type: agent.EventProviderUsage, Usage: model.Usage{InputTokens: 5, OutputTokens: 7}})

	body := render(t, m)
	if !strings.Contains(body, `otto_provider_tokens_total{kind="input"} 5`) {
		t.Fatalf("missing input token metric:\n%s", body)
	}
	if !strings.Contains(body, `otto_provider_tokens_total{kind="output"} 7`) {
		t.Fatalf("missing output token metric:\n%s", body)
	}
}

func TestTurnFinishSetsStatus(t *testing.T) {
	cases := []struct {
		name      string
		err       error
		want      string
		wantError bool // whether the error text field should be populated
	}{
		{"ok", nil, turnOK, false},
		{"canceled", context.Canceled, turnCanceled, false},
		{"error", errors.New("boom"), turnError, true},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			tr := newTurn("t1", func() {})
			tr.finish(c.err)
			if !tr.isDone() {
				t.Fatal("isDone() should be true after finish")
			}
			s := tr.summary()
			if s.Status != c.want {
				t.Fatalf("status = %q, want %q", s.Status, c.want)
			}
			if s.FinishedAt == nil {
				t.Fatal("FinishedAt should be set once done")
			}
			if c.wantError && s.Error == "" {
				t.Fatal("expected non-empty error text")
			}
			if !c.wantError && s.Error != "" {
				t.Fatalf("expected empty error text, got %q", s.Error)
			}
		})
	}
}

func TestTurnSnapshotFromArbitrarySeq(t *testing.T) {
	tr := newTurn("t1", func() {})
	emit := tr.emit(newMetrics())
	for i := 0; i < 5; i++ {
		emit(agent.Event{Type: agent.EventTextDelta, Text: strconv.Itoa(i)})
	}
	events, done, _ := tr.snapshot(3)
	if done {
		t.Fatal("should not be done")
	}
	if len(events) != 2 {
		t.Fatalf("len(events) = %d, want 2", len(events))
	}
	if events[0].Text != "3" || events[1].Text != "4" {
		t.Fatalf("unexpected events: %+v", events)
	}

	events, _, _ = tr.snapshot(5)
	if len(events) != 0 {
		t.Fatalf("snapshot(5) len = %d, want 0", len(events))
	}
}

func TestTurnConcurrentReadersObserveAllEvents(t *testing.T) {
	tr := newTurn("t1", func() {})
	emit := tr.emit(newMetrics())

	const n = 50
	var wg sync.WaitGroup
	results := make([][]wireEvent, 4)
	for i := range results {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			after := 0
			for {
				events, done, changed := tr.snapshot(after)
				results[i] = append(results[i], events...)
				after += len(events)
				if done && after >= n {
					return
				}
				select {
				case <-changed:
				case <-time.After(2 * time.Second):
					return
				}
			}
		}(i)
	}

	for i := 0; i < n; i++ {
		emit(agent.Event{Type: agent.EventTextDelta, Text: strconv.Itoa(i)})
	}
	tr.finish(nil)
	wg.Wait()

	for i, got := range results {
		if len(got) != n {
			t.Fatalf("reader %d got %d events, want %d", i, len(got), n)
		}
	}
}
