package tui

import (
	"context"
	"errors"
	"strings"
	"testing"
	"time"

	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

func TestCompactCommandPassesTrimmedFocusWithoutUserTranscriptEntry(t *testing.T) {
	var gotFocus string
	backend := &fakeBackend{compact: func(_ context.Context, focus string, emit func(agent.Event)) (agent.CompactionResult, error) {
		gotFocus = focus
		emit(agent.Event{Type: agent.EventCompactionStarted, Compaction: &agent.CompactionEvent{Reason: agent.CompactionManual}})
		return agent.CompactionResult{Noop: true}, nil
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("  /compact   focus on auth   \n")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	running := updated.(Model)
	if cmd == nil || !running.running || running.editor.Value() != "" {
		t.Fatalf("submit: cmd=%v running=%v editor=%q", cmd, running.running, running.editor.Value())
	}
	if len(running.entries) != 0 {
		t.Fatalf("manual compact appended transcript entries: %#v", running.entries)
	}

	first := runCommandWithin(t, cmd, time.Second)
	afterStarted, next := running.Update(first)
	started := afterStarted.(Model)
	if gotFocus != "focus on auth" {
		t.Fatalf("focus = %q, want trimmed focus", gotFocus)
	}
	if len(started.entries) != 0 || !started.running || !strings.Contains(started.statusText, "compacting") {
		t.Fatalf("started entries=%#v running=%v status=%q", started.entries, started.running, started.statusText)
	}

	afterDone, _ := started.Update(runCommandWithin(t, next, time.Second))
	if got := afterDone.(Model); got.running || len(got.entries) != 0 || !strings.Contains(got.statusText, "no-op") {
		t.Fatalf("done running=%v entries=%#v status=%q", got.running, got.entries, got.statusText)
	}
}

func TestCompactlyIsRejectedExactly(t *testing.T) {
	called := false
	backend := &fakeBackend{compact: func(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
		called = true
		return agent.CompactionResult{}, nil
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("/compactly")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	got := updated.(Model)
	if cmd != nil || called || got.running || got.statusText != "unknown command: /compactly" || got.editor.Value() != "/compactly" {
		t.Fatalf("cmd=%v called=%v running=%v status=%q editor=%q", cmd, called, got.running, got.statusText, got.editor.Value())
	}
}

func TestCompactResultIsFallbackWhenCompletionEventIsAbsent(t *testing.T) {
	backend := &fakeBackend{compact: func(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
		return agent.CompactionResult{CheckpointID: "deadbeef", TokensBefore: 5000, EstimatedTokensAfter: 1200}, nil
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("/compact")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	done := runCommandWithin(t, cmd, time.Second)
	finished, next := updated.(Model).Update(done)
	got := finished.(Model)
	if next != nil || got.running || !strings.Contains(got.statusText, "compacted") || strings.Contains(got.statusText, "no-op") {
		t.Fatalf("cmd=%v running=%v status=%q", next, got.running, got.statusText)
	}
}

func TestCompactCompletionEventWinsOverReturnedFallback(t *testing.T) {
	backend := &fakeBackend{compact: func(_ context.Context, _ string, emit func(agent.Event)) (agent.CompactionResult, error) {
		emit(agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{
			CheckpointID: "event", TokensBefore: 9000, EstimatedTokensAfter: 2000,
		}})
		return agent.CompactionResult{CheckpointID: "result", TokensBefore: 9000, EstimatedTokensAfter: 3000}, nil
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("/compact")

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	afterEvent, next := updated.(Model).Update(runCommandWithin(t, cmd, time.Second))
	eventState := afterEvent.(Model)
	status := eventState.statusText
	if !strings.Contains(status, "2k") {
		t.Fatalf("event status = %q, want event estimate", status)
	}
	afterDone, _ := eventState.Update(runCommandWithin(t, next, time.Second))
	if got := afterDone.(Model).statusText; got != status || strings.Contains(got, "3k") {
		t.Fatalf("fallback replaced completion event: before=%q after=%q", status, got)
	}
}

func TestStartingPromptOrCompactClearsPriorCtrlCExitArm(t *testing.T) {
	for _, test := range []struct {
		name  string
		value string
	}{
		{name: "prompt", value: "question"},
		{name: "compact", value: "/compact"},
	} {
		t.Run(test.name, func(t *testing.T) {
			m := resizeModel(t, newTestModel(t), 80, 12)
			m.ctrlCArmed = true
			m.ctrlCArmedAt = time.Now()
			m.statusText = ctrlCExitStatus
			m.editor.SetValue(test.value)

			updated, cmd := m.Update(keyPress(tea.KeyEnter))
			got := updated.(Model)
			if cmd == nil || !got.running || got.ctrlCArmed || got.statusText == ctrlCExitStatus {
				t.Fatalf("start state: cmd=%v running=%v armed=%v status=%q", cmd, got.running, got.ctrlCArmed, got.statusText)
			}
		})
	}
}

func TestCompactCancellationWithEscapeAndCtrlC(t *testing.T) {
	for _, test := range []struct {
		name string
		key  tea.KeyPressMsg
	}{
		{name: "escape", key: keyPress(tea.KeyEscape)},
		{name: "ctrl+c", key: keyPress('c', tea.ModCtrl)},
	} {
		t.Run(test.name, func(t *testing.T) {
			started := make(chan struct{})
			canceled := make(chan struct{})
			backend := &fakeBackend{compact: func(ctx context.Context, _ string, _ func(agent.Event)) (agent.CompactionResult, error) {
				close(started)
				<-ctx.Done()
				close(canceled)
				return agent.CompactionResult{}, ctx.Err()
			}}
			m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
			m.editor.SetValue("/compact")
			updated, cmd := m.Update(keyPress(tea.KeyEnter))
			running := updated.(Model)
			done := make(chan tea.Msg, 1)
			go func() { done <- cmd() }()
			select {
			case <-started:
			case <-time.After(time.Second):
				t.Fatal("compact did not start")
			}

			updated, _ = running.Update(test.key)
			canceling := updated.(Model)
			if !canceling.running {
				t.Fatal("operation stopped before backend completion")
			}
			select {
			case <-canceled:
			case <-time.After(time.Second):
				t.Fatal("key did not cancel compact context")
			}
			select {
			case msg := <-done:
				updated, _ = canceling.Update(msg)
				if updated.(Model).running {
					t.Fatal("compact remained active after done")
				}
			case <-time.After(time.Second):
				t.Fatal("canceled compact did not finish")
			}
		})
	}
}

func TestStaleGenerationCannotApplyCompactionEventOnActiveChannel(t *testing.T) {
	release := make(chan struct{})
	backend := &fakeBackend{prompt: func(_ context.Context, _ string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventAgentStarted})
		<-release
		return nil
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	running := updated.(Model)
	first := runCommandWithin(t, cmd, time.Second).(turnMsg)

	completion := agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{CheckpointID: "stale", TokensBefore: 10}}
	updated, next := running.Update(turnMsg{
		channel:    first.channel,
		stream:     first.stream,
		generation: first.generation - 1,
		value:      turnEnvelope{event: &completion},
	})
	got := updated.(Model)
	if next != nil || got.statusText != "" || got.usage != (model.Usage{}) || !got.running {
		t.Fatalf("stale generation changed state: cmd=%v status=%q usage=%#v running=%v", next, got.statusText, got.usage, got.running)
	}
	close(release)
}

func TestAutomaticCompactionEventsApplyInsidePromptStream(t *testing.T) {
	warningErr := errors.New("automatic compaction warning")
	backend := &fakeBackend{info: app.Info{Usage: model.Usage{InputTokens: 10}, UsagePresent: true}}
	backend.prompt = func(_ context.Context, _ string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventCompactionStarted, Compaction: &agent.CompactionEvent{Automatic: true, Reason: agent.CompactionThreshold}})
		emit(agent.Event{Type: agent.EventCompactionWarning, Err: warningErr})
		backend.info.Usage = model.Usage{InputTokens: 30, OutputTokens: 4}
		emit(agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{
			CheckpointID: "auto", Automatic: true, TokensBefore: 64000, EstimatedTokensAfter: 12000,
		}})
		return nil
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	state := updated.(Model)

	updated, next := state.Update(runCommandWithin(t, cmd, time.Second))
	state = updated.(Model)
	if !strings.Contains(state.statusText, "compacting") {
		t.Fatalf("started status = %q", state.statusText)
	}
	updated, next = state.Update(runCommandWithin(t, next, time.Second))
	state = updated.(Model)
	if state.statusText != warningErr.Error() {
		t.Fatalf("warning status = %q", state.statusText)
	}
	updated, next = state.Update(runCommandWithin(t, next, time.Second))
	state = updated.(Model)
	if !strings.Contains(state.statusText, "compacted") || state.usage != backend.info.Usage {
		t.Fatalf("completed status=%q usage=%#v want=%#v", state.statusText, state.usage, backend.info.Usage)
	}
	updated, _ = state.Update(runCommandWithin(t, next, time.Second))
	if got := updated.(Model); got.running || len(got.entries) != 1 || got.entries[0].Kind != EntryUser {
		t.Fatalf("done running=%v entries=%#v", got.running, got.entries)
	}
}

func TestCompactionCompletionReconcilesAggregateUsageWithoutDuplication(t *testing.T) {
	backend := &fakeBackend{info: app.Info{Usage: model.Usage{InputTokens: 100, OutputTokens: 10}, UsagePresent: true}}
	backend.prompt = func(_ context.Context, _ string, emit func(agent.Event)) error {
		backend.info.Usage = model.Usage{InputTokens: 130, OutputTokens: 14}
		emit(agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{
			CheckpointID: "checkpoint", UsagePresent: true, Usage: model.Usage{InputTokens: 30, OutputTokens: 4},
		}})
		emit(agent.Event{Type: agent.EventProviderUsage, Usage: model.Usage{InputTokens: 5, OutputTokens: 2}})
		return nil
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	state := updated.(Model)

	updated, next := state.Update(runCommandWithin(t, cmd, time.Second))
	state = updated.(Model)
	if state.usage != (model.Usage{InputTokens: 130, OutputTokens: 14}) {
		t.Fatalf("usage after completion = %#v", state.usage)
	}
	updated, next = state.Update(runCommandWithin(t, next, time.Second))
	state = updated.(Model)
	if state.usage != (model.Usage{InputTokens: 135, OutputTokens: 16}) {
		t.Fatalf("usage after ordinary provider call = %#v", state.usage)
	}
	updated, _ = state.Update(runCommandWithin(t, next, time.Second))
	if got := updated.(Model).usage; got != (model.Usage{InputTokens: 135, OutputTokens: 16}) {
		t.Fatalf("final usage = %#v", got)
	}
}

func TestCommittedCompactionCompletionSurvivesCanceledTurnContext(t *testing.T) {
	stream := newTurnStream()
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	event := agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{CheckpointID: "deadbeef"}}
	done := make(chan bool, 1)
	go func() { done <- sendTurnEvent(ctx, stream, turnEnvelope{event: &event}) }()
	select {
	case envelope := <-stream.channel:
		if envelope.event == nil || envelope.event.Compaction == nil || envelope.event.Compaction.CheckpointID != "deadbeef" {
			t.Fatalf("envelope=%#v", envelope)
		}
	case <-time.After(time.Second):
		t.Fatal("committed completion was dropped")
	}
	select {
	case delivered := <-done:
		if !delivered {
			t.Fatal("committed completion reported dropped")
		}
	case <-time.After(time.Second):
		t.Fatal("committed completion sender blocked")
	}
}

func TestCompactionStreamKeepsBoundedReservedCommittedDelivery(t *testing.T) {
	stream := newTurnStream()
	for index := 0; index < turnChannelCapacity-1; index++ {
		event := agent.Event{Type: agent.EventProviderUsage}
		if !sendTurnEvent(context.Background(), stream, turnEnvelope{event: &event}) {
			t.Fatalf("ordinary event %d was dropped", index)
		}
	}
	if got := len(stream.channel); got != turnChannelCapacity-1 {
		t.Fatalf("ordinary queue length = %d, want %d", got, turnChannelCapacity-1)
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	completion := agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{CheckpointID: "committed"}}
	if !sendTurnEvent(ctx, stream, turnEnvelope{event: &completion}) {
		t.Fatal("committed event did not use reserved bounded delivery")
	}
	if len(stream.channel) != turnChannelCapacity || cap(stream.channel) != turnChannelCapacity {
		t.Fatalf("queue len/cap = %d/%d, want %d/%d", len(stream.channel), cap(stream.channel), turnChannelCapacity, turnChannelCapacity)
	}
}

func TestManualCompactionCommittedCompletionPrecedesDoneAfterCancellation(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	backend := &fakeBackend{compact: func(_ context.Context, _ string, emit func(agent.Event)) (agent.CompactionResult, error) {
		result := agent.CompactionResult{CheckpointID: "committed", TokensBefore: 100, EstimatedTokensAfter: 20}
		emit(agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{
			CheckpointID: result.CheckpointID, TokensBefore: result.TokensBefore, EstimatedTokensAfter: result.EstimatedTokensAfter,
		}})
		return result, context.Canceled
	}}
	stream := newTurnStream()
	go runCompactionWorker(ctx, backend, "focus", stream)

	first := runCommandWithin(t, waitTurn(stream), time.Second).(turnMsg)
	if first.value.event == nil || first.value.event.Type != agent.EventCompactionCompleted {
		t.Fatalf("first envelope = %#v, want committed completion", first.value)
	}
	second := runCommandWithin(t, waitTurn(stream), time.Second).(turnMsg)
	if !second.value.done || !errors.Is(second.value.err, context.Canceled) || second.value.compactionResult == nil || second.value.compactionResult.CheckpointID != "committed" {
		t.Fatalf("done envelope = %#v", second.value)
	}
}

func TestManualCompactionFatalPersistenceQuits(t *testing.T) {
	fatalErr := errors.Join(session.ErrFatalPersistence, errors.New("disk full"))
	backend := &fakeBackend{compact: func(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
		return agent.CompactionResult{}, fatalErr
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("/compact")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	finished, quitCmd := updated.(Model).Update(runCommandWithin(t, cmd, time.Second))
	got := finished.(Model)
	if quitCmd == nil || got.running || !errors.Is(got.fatalErr, session.ErrFatalPersistence) {
		t.Fatalf("quit=%v running=%v fatal=%v", quitCmd, got.running, got.fatalErr)
	}
	if msg := runCommandWithin(t, quitCmd, time.Second); msg == nil {
		t.Fatal("quit command returned nil")
	}
}
