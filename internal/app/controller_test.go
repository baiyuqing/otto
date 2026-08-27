package app

import (
	"context"
	"encoding/json"
	"errors"
	"reflect"
	"sync"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

func TestControllerRejectsConcurrentPrompt(t *testing.T) {
	started := make(chan struct{})
	release := make(chan struct{})
	runner := runnerFunc(func(ctx context.Context, text string, emit func(agent.Event)) error {
		close(started)
		<-release
		return nil
	})
	controller := newTestController(t, runner)
	done := make(chan error, 1)
	go func() { done <- controller.Prompt(context.Background(), "one", func(agent.Event) {}) }()
	<-started
	if err := controller.Prompt(context.Background(), "two", func(agent.Event) {}); !errors.Is(err, ErrPromptActive) {
		t.Fatalf("error = %v, want ErrPromptActive", err)
	}
	close(release)
	if err := <-done; err != nil {
		t.Fatal(err)
	}
}

func TestControllerCreatesReplacementBeforeClosingCurrent(t *testing.T) {
	var order []string
	current := &fakeSession{header: testHeader("old"), onClose: func() { order = append(order, "close-old") }}
	next := &fakeSession{header: testHeader("new")}
	controller, err := New(current, func() (session.Session, error) {
		order = append(order, "create-new")
		return next, nil
	}, func(session.Session) Runner { return runnerFunc(noopRun) })
	if err != nil {
		t.Fatal(err)
	}
	if err := controller.NewSession(); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(order, []string{"create-new", "close-old"}) {
		t.Fatalf("order = %v", order)
	}
	if controller.Info().SessionID != "new" {
		t.Fatalf("info = %#v", controller.Info())
	}
}

func TestControllerNewSessionCreationFailureKeepsCurrent(t *testing.T) {
	current := &fakeSession{header: testHeader("old")}
	controller, err := New(current, func() (session.Session, error) {
		return nil, errors.New("create failed")
	}, func(session.Session) Runner { return runnerFunc(noopRun) })
	if err != nil {
		t.Fatal(err)
	}
	if err := controller.NewSession(); err == nil || err.Error() != "create failed" {
		t.Fatalf("NewSession() error = %v, want create failed", err)
	}
	if current.CloseCalls() != 0 {
		t.Fatalf("old close calls = %d, want 0", current.CloseCalls())
	}
	if info := controller.Info(); info.SessionID != "old" {
		t.Fatalf("info = %#v", info)
	}
	if err := controller.Prompt(context.Background(), "still-open", func(agent.Event) {}); err != nil {
		t.Fatalf("Prompt() after create failure = %v", err)
	}
}

func TestControllerRejectsNewSessionWhilePrompting(t *testing.T) {
	started := make(chan struct{})
	release := make(chan struct{})
	runner := runnerFunc(func(ctx context.Context, text string, emit func(agent.Event)) error {
		close(started)
		<-release
		return nil
	})
	controller := newTestController(t, runner)
	done := make(chan error, 1)
	go func() { done <- controller.Prompt(context.Background(), "one", func(agent.Event) {}) }()
	<-started
	if err := controller.NewSession(); !errors.Is(err, ErrPromptActive) {
		t.Fatalf("error = %v, want ErrPromptActive", err)
	}
	close(release)
	if err := <-done; err != nil {
		t.Fatal(err)
	}
}

func TestControllerForwardsEvents(t *testing.T) {
	events := []agent.Event{{Type: agent.EventAgentStarted}, {Type: agent.EventTextDelta, Text: "hello"}}
	controller := newTestController(t, runnerFunc(func(ctx context.Context, text string, emit func(agent.Event)) error {
		for _, event := range events {
			emit(event)
		}
		return nil
	}))
	var got []agent.Event
	if err := controller.Prompt(context.Background(), "prompt", func(event agent.Event) {
		got = append(got, event)
	}); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(got, events) {
		t.Fatalf("events = %#v, want %#v", got, events)
	}
}

func TestControllerHistoryReturnsDefensiveSnapshot(t *testing.T) {
	current := &fakeSession{header: testHeader("history"), messages: []model.Message{{
		ID:        "m1",
		Role:      model.RoleAssistant,
		CreatedAt: time.Unix(2, 0).UTC(),
		Blocks: []model.Block{{
			Type:       model.BlockToolCall,
			Text:       "before",
			ToolCallID: "call-1",
			ToolName:   "read",
			Arguments:  json.RawMessage(`{"path":"README.md"}`),
		}},
		Usage: &model.Usage{InputTokens: 1, OutputTokens: 2},
	}}}
	controller, err := New(current, func() (session.Session, error) {
		return &fakeSession{header: testHeader("next")}, nil
	}, func(session.Session) Runner { return runnerFunc(noopRun) })
	if err != nil {
		t.Fatal(err)
	}

	history := controller.History()
	history[0].Blocks[0].Text = "changed"
	history[0].Blocks[0].Arguments[0] = '{'
	history[0].Usage.InputTokens = 99
	history[0].Blocks = append(history[0].Blocks, model.Block{Type: model.BlockText, Text: "extra"})
	history = append(history, model.Message{ID: "m2"})
	if len(history) != 2 {
		t.Fatalf("mutated history length = %d, want 2", len(history))
	}

	got := controller.History()
	if got[0].Blocks[0].Text != "before" {
		t.Fatalf("text = %q, want before", got[0].Blocks[0].Text)
	}
	if string(got[0].Blocks[0].Arguments) != `{"path":"README.md"}` {
		t.Fatalf("arguments = %s", string(got[0].Blocks[0].Arguments))
	}
	if got[0].Usage == nil || got[0].Usage.InputTokens != 1 {
		t.Fatalf("usage = %#v, want input tokens 1", got[0].Usage)
	}
	if len(got) != 1 || len(got[0].Blocks) != 1 {
		t.Fatalf("history = %#v, want unchanged snapshot", got)
	}
}

func TestControllerCloseIsIdempotent(t *testing.T) {
	current := &fakeSession{header: testHeader("close")}
	controller, err := New(current, func() (session.Session, error) {
		return &fakeSession{header: testHeader("next")}, nil
	}, func(session.Session) Runner { return runnerFunc(noopRun) })
	if err != nil {
		t.Fatal(err)
	}
	if err := controller.Close(); err != nil {
		t.Fatal(err)
	}
	if err := controller.Close(); err != nil {
		t.Fatal(err)
	}
	if current.CloseCalls() != 1 {
		t.Fatalf("close calls = %d, want 1", current.CloseCalls())
	}
}

func TestControllerCloseWaitsForActivePrompt(t *testing.T) {
	started := make(chan struct{})
	release := make(chan struct{})
	runner := runnerFunc(func(ctx context.Context, text string, emit func(agent.Event)) error {
		close(started)
		<-release
		return nil
	})
	current := &fakeSession{header: testHeader("close-wait")}
	controller, err := New(current, func() (session.Session, error) {
		return &fakeSession{header: testHeader("next")}, nil
	}, func(session.Session) Runner { return runner })
	if err != nil {
		t.Fatal(err)
	}
	promptDone := make(chan error, 1)
	go func() { promptDone <- controller.Prompt(context.Background(), "one", func(agent.Event) {}) }()
	<-started

	closeDone := make(chan error, 1)
	go func() { closeDone <- controller.Close() }()

	select {
	case err := <-closeDone:
		t.Fatalf("Close() returned early: %v", err)
	case <-time.After(50 * time.Millisecond):
	}
	if current.CloseCalls() != 0 {
		t.Fatalf("close calls before release = %d, want 0", current.CloseCalls())
	}

	close(release)
	if err := <-promptDone; err != nil {
		t.Fatal(err)
	}
	if err := <-closeDone; err != nil {
		t.Fatal(err)
	}
	if current.CloseCalls() != 1 {
		t.Fatalf("close calls after release = %d, want 1", current.CloseCalls())
	}
}

func TestControllerPreservesFatalPersistenceIdentity(t *testing.T) {
	fatalErr := errors.Join(session.ErrFatalPersistence, errors.New("disk full"))
	controller := newTestController(t, runnerFunc(func(ctx context.Context, text string, emit func(agent.Event)) error {
		return fatalErr
	}))
	err := controller.Prompt(context.Background(), "prompt", func(agent.Event) {})
	if !errors.Is(err, session.ErrFatalPersistence) {
		t.Fatalf("error = %v, want fatal persistence identity", err)
	}
}

func TestControllerCloseFailureClosesReplacementAndController(t *testing.T) {
	closeErr := errors.New("close old failed")
	current := &fakeSession{header: testHeader("old"), closeErr: closeErr}
	replacement := &fakeSession{header: testHeader("new")}
	controller, err := New(current, func() (session.Session, error) {
		return replacement, nil
	}, func(session.Session) Runner { return runnerFunc(noopRun) })
	if err != nil {
		t.Fatal(err)
	}
	if err := controller.NewSession(); !errors.Is(err, closeErr) {
		t.Fatalf("NewSession() error = %v, want %v", err, closeErr)
	}
	if current.CloseCalls() != 1 {
		t.Fatalf("old close calls = %d, want 1", current.CloseCalls())
	}
	if replacement.CloseCalls() != 1 {
		t.Fatalf("replacement close calls = %d, want 1", replacement.CloseCalls())
	}
	if err := controller.Prompt(context.Background(), "prompt", func(agent.Event) {}); !errors.Is(err, ErrClosed) {
		t.Fatalf("Prompt() error = %v, want ErrClosed", err)
	}
	if err := controller.NewSession(); !errors.Is(err, ErrClosed) {
		t.Fatalf("NewSession() after failure = %v, want ErrClosed", err)
	}
	if err := controller.Close(); !errors.Is(err, closeErr) {
		t.Fatalf("Close() error = %v, want %v", err, closeErr)
	}
	if current.CloseCalls() != 1 {
		t.Fatalf("old close calls after Close = %d, want 1", current.CloseCalls())
	}
}

func TestControllerMethodsAfterClose(t *testing.T) {
	current := &fakeSession{header: testHeader("closed"), messages: []model.Message{{ID: "m1", Role: model.RoleUser}}}
	controller, err := New(current, func() (session.Session, error) {
		return &fakeSession{header: testHeader("next")}, nil
	}, func(session.Session) Runner { return runnerFunc(noopRun) })
	if err != nil {
		t.Fatal(err)
	}
	if err := controller.Close(); err != nil {
		t.Fatal(err)
	}
	if err := controller.Prompt(context.Background(), "prompt", func(agent.Event) {}); !errors.Is(err, ErrClosed) {
		t.Fatalf("Prompt() error = %v, want ErrClosed", err)
	}
	if err := controller.NewSession(); !errors.Is(err, ErrClosed) {
		t.Fatalf("NewSession() error = %v, want ErrClosed", err)
	}
	if info := controller.Info(); info.SessionID != "closed" {
		t.Fatalf("info = %#v, want closed session info", info)
	}
	if history := controller.History(); len(history) != 1 || history[0].ID != "m1" {
		t.Fatalf("history = %#v, want existing history", history)
	}
}

type runnerFunc func(context.Context, string, func(agent.Event)) error

func (f runnerFunc) Run(ctx context.Context, text string, emit func(agent.Event)) error {
	return f(ctx, text, emit)
}

func noopRun(context.Context, string, func(agent.Event)) error { return nil }

func testHeader(id string) session.Header {
	return session.Header{Version: 1, ID: id, Workspace: "/workspace", Provider: "openai-compatible", Profile: "test", Model: "model", CreatedAt: time.Unix(1, 0).UTC()}
}

type fakeSession struct {
	mu         sync.Mutex
	header     session.Header
	messages   []model.Message
	closeErr   error
	closed     bool
	closeCalls int
	onClose    func()
}

func (f *fakeSession) Header() session.Header {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.header
}

func (f *fakeSession) Messages() []model.Message {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]model.Message(nil), f.messages...)
}

func (f *fakeSession) Append(context.Context, model.Message) error { return nil }

func (f *fakeSession) Path() string {
	f.mu.Lock()
	defer f.mu.Unlock()
	return "/sessions/" + f.header.ID + ".jsonl"
}

func (f *fakeSession) Close() error {
	f.mu.Lock()
	alreadyClosed := f.closed
	if !alreadyClosed {
		f.closed = true
	}
	f.closeCalls++
	onClose := f.onClose
	closeErr := f.closeErr
	f.mu.Unlock()
	if !alreadyClosed && onClose != nil {
		onClose()
	}
	return closeErr
}

func (f *fakeSession) CloseCalls() int {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.closeCalls
}

func newTestController(t *testing.T, runner Runner) *Controller {
	t.Helper()
	controller, err := New(&fakeSession{header: testHeader("initial")}, func() (session.Session, error) {
		return &fakeSession{header: testHeader("next")}, nil
	}, func(session.Session) Runner { return runner })
	if err != nil {
		t.Fatal(err)
	}
	return controller
}
