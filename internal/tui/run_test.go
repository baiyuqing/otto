package tui

import (
	"bytes"
	"context"
	"errors"
	"io"
	"reflect"
	"strings"
	"testing"
	"time"
	"unsafe"

	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

func TestRunBuildsBubbleTeaProgramWithContextAndIO(t *testing.T) {
	input := strings.NewReader("")
	output := &bytes.Buffer{}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	oldNewProgram := newProgram
	defer func() { newProgram = oldNewProgram }()

	var capturedModel tea.Model
	var capturedProgram *tea.Program
	var runCalls int
	newProgram = func(model tea.Model, opts ...tea.ProgramOption) programRunner {
		capturedModel = model
		capturedProgram = tea.NewProgram(model, opts...)
		return programRunnerFunc(func() (tea.Model, error) {
			runCalls++
			return model, nil
		})
	}

	if err := Run(ctx, input, output, testBackend{}); err != nil {
		t.Fatalf("Run() error = %v", err)
	}
	if runCalls != 1 {
		t.Fatalf("program Run calls = %d, want 1", runCalls)
	}
	if _, ok := capturedModel.(Model); !ok {
		t.Fatalf("model type = %T, want tui.Model", capturedModel)
	}
	if got := unexportedProgramField[context.Context](t, capturedProgram, "externalCtx"); got != ctx {
		t.Fatalf("external context = %#v, want provided context", got)
	}
	if got := unexportedProgramField[io.Reader](t, capturedProgram, "input"); got != input {
		t.Fatalf("input = %#v, want provided input", got)
	}
	if got := unexportedProgramField[io.Writer](t, capturedProgram, "output"); got != output {
		t.Fatalf("output = %#v, want provided output", got)
	}
	if !reflect.ValueOf(capturedProgram).Elem().FieldByName("disableSignalHandler").Bool() {
		t.Fatal("disableSignalHandler = false, want true")
	}
}

func TestRunReturnsProgramError(t *testing.T) {
	wantErr := errors.New("program failed")
	oldNewProgram := newProgram
	defer func() { newProgram = oldNewProgram }()
	newProgram = func(model tea.Model, opts ...tea.ProgramOption) programRunner {
		return programRunnerFunc(func() (tea.Model, error) {
			return model, wantErr
		})
	}

	err := Run(context.Background(), strings.NewReader(""), &bytes.Buffer{}, testBackend{})
	if !errors.Is(err, wantErr) {
		t.Fatalf("Run() error = %v, want %v", err, wantErr)
	}
}

func TestRunReturnsManualCompactionFatalPersistenceError(t *testing.T) {
	fatalErr := errors.Join(session.ErrFatalPersistence, errors.New("disk full"))
	backend := &fakeBackend{compact: func(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
		return agent.CompactionResult{}, fatalErr
	}}
	oldNewProgram := newProgram
	defer func() { newProgram = oldNewProgram }()
	newProgram = func(model tea.Model, opts ...tea.ProgramOption) programRunner {
		return programRunnerFunc(func() (tea.Model, error) {
			current := model.(Model)
			current.editor.SetValue("/compact")
			started, cmd := current.Update(keyPress(tea.KeyEnter))
			if cmd == nil || !started.(Model).View().AltScreen {
				t.Fatal("manual compaction did not start in alternate screen")
			}
			finished, quitCmd := started.(Model).Update(runCommandWithin(t, cmd, time.Second))
			if quitCmd == nil || !finished.(Model).View().AltScreen {
				t.Fatal("fatal compaction did not request quit from alternate screen")
			}
			_ = runCommandWithin(t, quitCmd, time.Second)
			return finished, nil
		})
	}

	err := Run(context.Background(), strings.NewReader(""), &bytes.Buffer{}, backend)
	if !errors.Is(err, session.ErrFatalPersistence) || !errors.Is(err, fatalErr) {
		t.Fatalf("Run() error = %v, want fatal persistence identity", err)
	}
}

func TestRunReturnsFatalPersistenceErrorFromFinalModel(t *testing.T) {
	fatalErr := errors.Join(session.ErrFatalPersistence, errors.New("disk full"))
	oldNewProgram := newProgram
	defer func() { newProgram = oldNewProgram }()
	newProgram = func(model tea.Model, opts ...tea.ProgramOption) programRunner {
		return programRunnerFunc(func() (tea.Model, error) {
			final := model.(Model)
			final.fatalErr = fatalErr
			return final, nil
		})
	}

	err := Run(context.Background(), strings.NewReader(""), &bytes.Buffer{}, testBackend{})
	if !errors.Is(err, session.ErrFatalPersistence) || !errors.Is(err, fatalErr) {
		t.Fatalf("Run() error = %v, want fatal persistence identity", err)
	}
}

func TestRunAbandonsBlockedTurnBeforeControllerClose(t *testing.T) {
	completionAttempted := make(chan struct{})
	runner := &fullCompletionRunner{completionAttempted: completionAttempted}
	initial := session.NewMemory(session.Header{Version: session.CurrentVersion, ID: "tui-close", Workspace: t.TempDir()})
	controller, err := app.New(initial, func() (session.Session, error) {
		return session.NewMemory(session.Header{Version: session.CurrentVersion, ID: "next", Workspace: t.TempDir()}), nil
	}, func(session.Session) app.Runner { return runner })
	if err != nil {
		t.Fatal(err)
	}

	oldNewProgram := newProgram
	defer func() { newProgram = oldNewProgram }()
	var stream *turnStream
	newProgram = func(model tea.Model, opts ...tea.ProgramOption) programRunner {
		return programRunnerFunc(func() (tea.Model, error) {
			current := model.(Model)
			current.editor.SetValue("question")
			started, cmd := current.Update(keyPress(tea.KeyEnter))
			first := runCommandWithin(t, cmd, time.Second).(turnMsg)
			stream = first.stream
			select {
			case <-completionAttempted:
			case <-time.After(time.Second):
				t.Fatal("runner did not reach completion awaiting application acknowledgement")
			}
			deadline := time.Now().Add(time.Second)
			for len(stream.channel) != turnChannelCapacity-1 && time.Now().Before(deadline) {
				time.Sleep(time.Millisecond)
			}
			if got := len(stream.channel); got != turnChannelCapacity-1 {
				t.Fatalf("turn channel length = %d, want %d queued envelopes", got, turnChannelCapacity-1)
			}
			return started, nil
		})
	}

	if err := Run(context.Background(), strings.NewReader(""), &bytes.Buffer{}, controller); err != nil {
		t.Fatalf("Run() error = %v", err)
	}
	if stream == nil {
		t.Fatal("turn stream was not captured")
	}
	closeDone := make(chan error, 1)
	go func() { closeDone <- controller.Close() }()
	select {
	case err := <-closeDone:
		if err != nil {
			t.Fatalf("Controller.Close() error = %v", err)
		}
	case <-time.After(200 * time.Millisecond):
		go drainClosedTurnStreamForCleanup(stream)
		select {
		case <-closeDone:
		case <-time.After(time.Second):
			t.Fatal("Controller.Close remained blocked after test cleanup drain")
		}
		t.Fatal("Controller.Close blocked after the TUI frontend returned")
	}
	drainClosedTurnStream(t, stream)
}

func TestRunInspectsEveryFinalModelWhenProgramReturnsError(t *testing.T) {
	fatalErr := errors.Join(session.ErrFatalPersistence, errors.New("disk full"))
	programErr := errors.New("program failed")
	tests := []struct {
		name      string
		final     func(Model) tea.Model
		wantFatal bool
	}{
		{name: "value", final: func(model Model) tea.Model { model.fatalErr = fatalErr; return model }, wantFatal: true},
		{name: "pointer", final: func(model Model) tea.Model { model.fatalErr = fatalErr; return &model }, wantFatal: true},
		{name: "nil pointer", final: func(Model) tea.Model { return (*Model)(nil) }},
		{name: "nil interface", final: func(Model) tea.Model { return nil }},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			oldNewProgram := newProgram
			defer func() { newProgram = oldNewProgram }()
			newProgram = func(model tea.Model, opts ...tea.ProgramOption) programRunner {
				return programRunnerFunc(func() (tea.Model, error) {
					return test.final(model.(Model)), programErr
				})
			}

			err := Run(context.Background(), strings.NewReader(""), &bytes.Buffer{}, testBackend{})
			if !errors.Is(err, programErr) {
				t.Fatalf("Run() error = %v, want program error identity", err)
			}
			if got := errors.Is(err, session.ErrFatalPersistence); got != test.wantFatal {
				t.Fatalf("Run() fatal persistence identity = %v, want %v: %v", got, test.wantFatal, err)
			}
			if !test.wantFatal && err != programErr {
				t.Fatalf("Run() error = %#v, want sole program error %#v", err, programErr)
			}
		})
	}
}

type fullCompletionRunner struct {
	completionAttempted chan struct{}
}

func (r *fullCompletionRunner) Run(_ context.Context, _ string, emit func(agent.Event)) error {
	for index := 0; index < turnChannelCapacity-1; index++ {
		emit(agent.Event{Type: agent.EventProviderUsage})
	}
	close(r.completionAttempted)
	emit(agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{CheckpointID: "committed"}})
	return nil
}

func (*fullCompletionRunner) Compact(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
	return agent.CompactionResult{Noop: true}, nil
}

func drainClosedTurnStreamForCleanup(stream *turnStream) {
	for envelope := range stream.channel {
		envelope.applicationAck.acknowledge()
		if envelope.usesRegularEventSlot {
			<-stream.regularEventSlots
		}
	}
}

type programRunnerFunc func() (tea.Model, error)

func (f programRunnerFunc) Run() (tea.Model, error) {
	return f()
}

type testBackend struct{}

func (testBackend) Prompt(context.Context, string, func(agent.Event)) error { return nil }
func (testBackend) Compact(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
	return agent.CompactionResult{Noop: true}, nil
}
func (testBackend) NewSession() error        { return nil }
func (testBackend) Info() app.Info           { return app.Info{} }
func (testBackend) History() []model.Message { return nil }

func unexportedProgramField[T any](t *testing.T, program *tea.Program, name string) T {
	t.Helper()
	field := reflect.ValueOf(program).Elem().FieldByName(name)
	return reflect.NewAt(field.Type(), unsafe.Pointer(field.UnsafeAddr())).Elem().Interface().(T)
}
