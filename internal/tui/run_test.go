package tui

import (
	"bytes"
	"context"
	"errors"
	"io"
	"reflect"
	"strings"
	"testing"
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

type programRunnerFunc func() (tea.Model, error)

func (f programRunnerFunc) Run() (tea.Model, error) {
	return f()
}

type testBackend struct{}

func (testBackend) Prompt(context.Context, string, func(agent.Event)) error { return nil }
func (testBackend) NewSession() error                                       { return nil }
func (testBackend) Info() app.Info                                          { return app.Info{} }
func (testBackend) History() []model.Message                                { return nil }

func unexportedProgramField[T any](t *testing.T, program *tea.Program, name string) T {
	t.Helper()
	field := reflect.ValueOf(program).Elem().FieldByName(name)
	return reflect.NewAt(field.Type(), unsafe.Pointer(field.UnsafeAddr())).Elem().Interface().(T)
}
