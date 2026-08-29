package tui

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"sync"
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

func TestFullCompactionQueueDrainsCompletionsAndDone(t *testing.T) {
	for _, completionCount := range []int{1, 3} {
		t.Run(fmt.Sprintf("completions=%d", completionCount), func(t *testing.T) {
			backend := &fakeBackend{prompt: func(_ context.Context, _ string, emit func(agent.Event)) error {
				for index := 0; index < turnChannelCapacity-1; index++ {
					emit(agent.Event{Type: agent.EventProviderUsage})
				}
				for index := 0; index < completionCount; index++ {
					emit(agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{CheckpointID: fmt.Sprintf("checkpoint-%d", index)}})
				}
				return nil
			}}
			stream := newTurnStream()
			workerFinished := make(chan struct{})
			go func() {
				runTurnWorker(context.Background(), backend, "question", stream)
				close(workerFinished)
			}()

			var ordinary, completions, done int
			for done == 0 {
				message := runCommandWithin(t, waitTurn(stream), time.Second).(turnMsg)
				switch {
				case message.value.done:
					done++
				case message.value.event != nil && message.value.event.Type == agent.EventCompactionCompleted:
					completions++
				case message.value.event != nil:
					ordinary++
				}
			}
			select {
			case <-workerFinished:
			case <-time.After(time.Second):
				t.Fatal("worker remained blocked after the stream was drained")
			}
			if ordinary != turnChannelCapacity-1 || completions != completionCount || done != 1 {
				t.Fatalf("ordinary=%d completions=%d done=%d", ordinary, completions, done)
			}
			if got := len(stream.regularEventSlots); got != 0 {
				t.Fatalf("regular event permits after drain = %d, want 0", got)
			}
		})
	}
}

func TestAbandonUnblocksFullTurnWorkerAndReleasesBlockedRegularPermit(t *testing.T) {
	backend := &fakeBackend{prompt: func(_ context.Context, _ string, emit func(agent.Event)) error {
		for index := 0; index < turnChannelCapacity-2; index++ {
			emit(agent.Event{Type: agent.EventProviderUsage})
		}
		for index := 0; index < 2; index++ {
			emit(agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{CheckpointID: fmt.Sprintf("checkpoint-%d", index)}})
		}
		emit(agent.Event{Type: agent.EventProviderUsage})
		return nil
	}}
	stream := newTurnStream()
	workerFinished := make(chan struct{})
	go func() {
		runTurnWorker(context.Background(), backend, "question", stream)
		close(workerFinished)
	}()

	deadline := time.Now().Add(time.Second)
	for (len(stream.channel) != turnChannelCapacity || len(stream.regularEventSlots) != turnChannelCapacity-1) && time.Now().Before(deadline) {
		time.Sleep(time.Millisecond)
	}
	if len(stream.channel) != turnChannelCapacity || len(stream.regularEventSlots) != turnChannelCapacity-1 {
		t.Fatalf("blocked stream queue/permits = %d/%d", len(stream.channel), len(stream.regularEventSlots))
	}
	stream.abandon()
	stream.abandon()
	select {
	case <-workerFinished:
	case <-time.After(time.Second):
		t.Fatal("abandoned worker remained blocked")
	}
	drainClosedTurnStream(t, stream)
	if got := len(stream.regularEventSlots); got != 0 {
		t.Fatalf("regular event permits after abandoned drain = %d, want 0", got)
	}
}

func TestNilCompactionCompletionDoesNotUseDurableSlot(t *testing.T) {
	stream := newTurnStream()
	for index := 0; index < turnChannelCapacity-1; index++ {
		event := agent.Event{Type: agent.EventProviderUsage}
		if !sendTurnEvent(context.Background(), stream, turnEnvelope{event: &event}) {
			t.Fatalf("ordinary event %d was dropped", index)
		}
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	malformed := agent.Event{Type: agent.EventCompactionCompleted}
	result := make(chan bool, 1)
	go func() { result <- sendTurnEvent(ctx, stream, turnEnvelope{event: &malformed}) }()

	select {
	case delivered := <-result:
		if delivered {
			t.Fatal("nil compaction payload consumed durable delivery")
		}
	case <-time.After(100 * time.Millisecond):
		_ = waitTurn(stream)()
		delivered := <-result
		t.Fatalf("nil compaction payload blocked on the durable slot (eventually delivered=%v)", delivered)
	}
	if got := len(stream.channel); got != turnChannelCapacity-1 {
		t.Fatalf("queue length = %d, want unchanged %d", got, turnChannelCapacity-1)
	}
	if got := len(stream.regularEventSlots); got != turnChannelCapacity-1 {
		t.Fatalf("regular permits = %d, want unchanged %d", got, turnChannelCapacity-1)
	}
}

func TestQuitAbandonsBlockedCommittedCompletion(t *testing.T) {
	stream := newTurnStream()
	for index := 0; index < turnChannelCapacity-1; index++ {
		event := agent.Event{Type: agent.EventProviderUsage}
		if !sendTurnEvent(context.Background(), stream, turnEnvelope{event: &event}) {
			t.Fatalf("ordinary event %d was dropped", index)
		}
	}
	completion := agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{CheckpointID: "first"}}
	if !sendTurnEvent(context.Background(), stream, turnEnvelope{event: &completion}) {
		t.Fatal("first completion was dropped")
	}
	blockedCompletion := agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{CheckpointID: "blocked"}}
	blockedResult := make(chan bool, 1)
	go func() {
		blockedResult <- sendTurnEvent(context.Background(), stream, turnEnvelope{event: &blockedCompletion})
	}()

	ctx, cancel := context.WithCancel(context.Background())
	m := resizeModel(t, newTestModel(t), 80, 12)
	m.running = true
	m.cancel = cancel
	m.activeTurnStream = stream
	m.activeTurnChannel = stream.channel
	_, quit := m.quit()
	if quit == nil {
		t.Fatal("quit command = nil")
	}
	if !errors.Is(ctx.Err(), context.Canceled) {
		t.Fatalf("operation context error = %v, want context.Canceled", ctx.Err())
	}
	select {
	case delivered := <-blockedResult:
		if delivered {
			t.Fatal("blocked completion reported delivery after quit")
		}
	case <-time.After(time.Second):
		t.Fatal("quit did not abandon blocked completion")
	}
	stream.abandon()
	drainOpenTurnStream(stream)
}

func TestSessionResetAbandonsBlockedCommittedCompletion(t *testing.T) {
	stream := newTurnStream()
	for index := 0; index < turnChannelCapacity; index++ {
		event := agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{CheckpointID: fmt.Sprintf("checkpoint-%d", index)}}
		if !sendTurnEvent(context.Background(), stream, turnEnvelope{event: &event}) {
			t.Fatalf("completion %d was dropped", index)
		}
	}
	blocked := agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{CheckpointID: "blocked"}}
	blockedResult := make(chan bool, 1)
	go func() { blockedResult <- sendTurnEvent(context.Background(), stream, turnEnvelope{event: &blocked}) }()

	m := resizeModel(t, newTestModel(t), 80, 12)
	m.running = true
	m.cancel = func() {}
	m.activeTurnStream = stream
	m.activeTurnChannel = stream.channel
	m.resetSessionViewFromBackend("replaced")
	select {
	case delivered := <-blockedResult:
		if delivered {
			t.Fatal("blocked completion reported delivery after reset")
		}
	case <-time.After(time.Second):
		t.Fatal("session reset did not abandon blocked completion")
	}
	if m.activeTurnStream != nil || m.activeTurnChannel != nil || m.running {
		t.Fatalf("reset active stream=%p channel=%v running=%v", m.activeTurnStream, m.activeTurnChannel, m.running)
	}
	drainOpenTurnStream(stream)
}

func TestStaleIndependentTurnStreamContinuesDraining(t *testing.T) {
	active := newTurnStream()
	stale := newTurnStream()
	event := agent.Event{Type: agent.EventProviderUsage}
	if !sendTurnEvent(context.Background(), stale, turnEnvelope{event: &event}) {
		t.Fatal("stale event was not queued")
	}
	stale.channel <- turnEnvelope{done: true}

	m := resizeModel(t, newTestModel(t), 80, 12)
	m.running = true
	m.turnGeneration = 2
	active.generation = 2
	stale.generation = 1
	m.activeTurnStream = active
	m.activeTurnChannel = active.channel
	first := waitTurn(stale)().(turnMsg)
	updated, next := m.Update(first)
	if next == nil || !updated.(Model).running {
		t.Fatalf("stale stream drain cmd=%v running=%v", next, updated.(Model).running)
	}
	select {
	case <-stale.abandonSignal:
		t.Fatal("stale independent stream was abandoned instead of drained")
	default:
	}
	message := runCommandWithin(t, next, time.Second).(turnMsg)
	if !message.value.done {
		t.Fatalf("stale drain envelope = %#v, want done", message.value)
	}
}

func TestCompactionCallbackDeepCopiesMutablePayload(t *testing.T) {
	payload := &agent.CompactionEvent{CheckpointID: "original", TokensBefore: 9000, EstimatedTokensAfter: 2000}
	mutated := make(chan struct{})
	backend := &fakeBackend{prompt: func(_ context.Context, _ string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventCompactionCompleted, Compaction: payload})
		payload.CheckpointID = "mutated"
		payload.EstimatedTokensAfter = 8000
		close(mutated)
		return nil
	}}
	stream := newTurnStream()
	go runTurnWorker(context.Background(), backend, "question", stream)
	message := runCommandWithin(t, waitTurn(stream), time.Second).(turnMsg)
	<-mutated
	if message.value.event == nil || message.value.event.Compaction == nil {
		t.Fatalf("queued event = %#v", message.value.event)
	}
	if message.value.event.Compaction == payload {
		t.Fatal("queued compaction payload aliases callback payload")
	}
	if got := message.value.event.Compaction; got.CheckpointID != "original" || got.EstimatedTokensAfter != 2000 {
		t.Fatalf("queued compaction payload = %#v, want original values", got)
	}
}

func TestCompactionCallbackMutationDoesNotRaceQueuedPayload(t *testing.T) {
	payload := &agent.CompactionEvent{CheckpointID: "original", TokensBefore: 9000, EstimatedTokensAfter: 2000}
	startMutation := make(chan struct{})
	mutationDone := make(chan struct{})
	releaseBackend := make(chan struct{})
	backend := &fakeBackend{prompt: func(_ context.Context, _ string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventCompactionCompleted, Compaction: payload})
		<-startMutation
		for index := 0; index < 10000; index++ {
			payload.EstimatedTokensAfter = 8000 + index%2
		}
		close(mutationDone)
		<-releaseBackend
		return nil
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	message := runCommandWithin(t, cmd, time.Second)
	close(startMutation)
	applied, _ := updated.(Model).Update(message)
	<-mutationDone
	close(releaseBackend)
	if got := applied.(Model).statusText; !strings.Contains(got, "2k") {
		t.Fatalf("completion status = %q, want immutable queued estimate", got)
	}
}

func TestCompactionCompletionUsesCallbackUsageSnapshotBeforeWorkerAdvances(t *testing.T) {
	advanced := make(chan struct{})
	backend := &usageSnapshotBackend{info: app.Info{Usage: model.Usage{InputTokens: 100, OutputTokens: 10}, UsagePresent: true}}
	backend.prompt = func(_ context.Context, _ string, emit func(agent.Event)) error {
		backend.setInfo(app.Info{Usage: model.Usage{InputTokens: 130, OutputTokens: 14}, UsagePresent: true})
		emit(agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{CheckpointID: "checkpoint"}})
		backend.setInfo(app.Info{Usage: model.Usage{InputTokens: 135, OutputTokens: 16}, UsagePresent: true})
		emit(agent.Event{Type: agent.EventProviderUsage, Usage: model.Usage{InputTokens: 5, OutputTokens: 2}})
		close(advanced)
		return nil
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("question")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	completion := runCommandWithin(t, cmd, time.Second)
	<-advanced

	updated, next := updated.(Model).Update(completion)
	state := updated.(Model)
	if state.usage != (model.Usage{InputTokens: 130, OutputTokens: 14}) {
		t.Fatalf("usage after completion = %#v, want callback boundary", state.usage)
	}
	updated, next = state.Update(runCommandWithin(t, next, time.Second))
	state = updated.(Model)
	if state.usage != (model.Usage{InputTokens: 135, OutputTokens: 16}) {
		t.Fatalf("usage after queued provider event = %#v, want counted once", state.usage)
	}
	updated, _ = state.Update(runCommandWithin(t, next, time.Second))
	if got := updated.(Model).usage; got != (model.Usage{InputTokens: 135, OutputTokens: 16}) {
		t.Fatalf("final usage = %#v, want counted once", got)
	}
}

func TestManualCompactionFallbackUsesReturnBoundaryUsageSnapshot(t *testing.T) {
	backend := &usageSnapshotBackend{info: app.Info{Usage: model.Usage{InputTokens: 100}, UsagePresent: true}}
	backend.compact = func(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
		backend.setInfo(app.Info{Usage: model.Usage{InputTokens: 130, OutputTokens: 4}, UsagePresent: true})
		return agent.CompactionResult{CheckpointID: "checkpoint", TokensBefore: 5000, EstimatedTokensAfter: 1000}, nil
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	m.editor.SetValue("/compact")
	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	done := runCommandWithin(t, cmd, time.Second)
	backend.setInfo(app.Info{Usage: model.Usage{InputTokens: 170, OutputTokens: 9}, UsagePresent: true})

	updated, _ = updated.(Model).Update(done)
	if got := updated.(Model).usage; got != (model.Usage{InputTokens: 130, OutputTokens: 4}) {
		t.Fatalf("manual fallback usage = %#v, want return boundary", got)
	}
}

func drainClosedTurnStream(t *testing.T, stream *turnStream) {
	t.Helper()
	deadline := time.After(time.Second)
	for {
		select {
		case envelope, ok := <-stream.channel:
			if !ok {
				return
			}
			if envelope.usesRegularEventSlot {
				<-stream.regularEventSlots
			}
		case <-deadline:
			t.Fatal("turn stream did not close")
		}
	}
}

func drainOpenTurnStream(stream *turnStream) {
	for len(stream.channel) > 0 {
		envelope := <-stream.channel
		if envelope.usesRegularEventSlot {
			<-stream.regularEventSlots
		}
	}
}

type usageSnapshotBackend struct {
	mu      sync.Mutex
	info    app.Info
	prompt  func(context.Context, string, func(agent.Event)) error
	compact func(context.Context, string, func(agent.Event)) (agent.CompactionResult, error)
}

func (b *usageSnapshotBackend) Prompt(ctx context.Context, text string, emit func(agent.Event)) error {
	if b.prompt == nil {
		return nil
	}
	return b.prompt(ctx, text, emit)
}

func (b *usageSnapshotBackend) Compact(ctx context.Context, focus string, emit func(agent.Event)) (agent.CompactionResult, error) {
	if b.compact == nil {
		return agent.CompactionResult{Noop: true}, nil
	}
	return b.compact(ctx, focus, emit)
}

func (b *usageSnapshotBackend) NewSession() error { return nil }

func (b *usageSnapshotBackend) Info() app.Info {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.info
}

func (b *usageSnapshotBackend) History() []model.Message { return nil }

func (b *usageSnapshotBackend) setInfo(info app.Info) {
	b.mu.Lock()
	b.info = info
	b.mu.Unlock()
}
