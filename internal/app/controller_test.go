package app

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"sync"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/memory"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

func TestNewValidatesRequiredDependencies(t *testing.T) {
	tests := []struct {
		name    string
		initial session.Session
		create  SessionFactory
		build   RunnerFactory
		wantErr string
	}{
		{
			name:    "nil initial session",
			create:  func() (session.Session, error) { return &fakeSession{header: testHeader("next")}, nil },
			build:   func(session.Session) Runner { return runnerFunc(noopRun) },
			wantErr: "initial session is required",
		},
		{
			name:    "nil session factory",
			initial: &fakeSession{header: testHeader("initial")},
			build:   func(session.Session) Runner { return runnerFunc(noopRun) },
			wantErr: "session factory is required",
		},
		{
			name:    "nil runner factory",
			initial: &fakeSession{header: testHeader("initial")},
			create:  func() (session.Session, error) { return &fakeSession{header: testHeader("next")}, nil },
			wantErr: "runner factory is required",
		},
		{
			name:    "nil initial runner",
			initial: &fakeSession{header: testHeader("initial")},
			create:  func() (session.Session, error) { return &fakeSession{header: testHeader("next")}, nil },
			build:   func(session.Session) Runner { return nil },
			wantErr: "runner factory returned nil runner",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			controller, err := New(tt.initial, tt.create, tt.build)
			if controller != nil {
				t.Fatalf("controller = %#v, want nil", controller)
			}
			if err == nil || err.Error() != tt.wantErr {
				t.Fatalf("New() error = %v, want %q", err, tt.wantErr)
			}
		})
	}
}

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

func TestControllerNewSessionCallbacksMayInspectControllerState(t *testing.T) {
	current := &fakeSession{header: testHeader("old"), messages: []model.Message{{ID: "m1", Role: model.RoleUser}}}
	next := &fakeSession{header: testHeader("new")}
	var controller *Controller
	var buildCalls int
	var callbackErr error
	var callbackMu sync.Mutex

	recordCallbackErr := func(err error) {
		callbackMu.Lock()
		defer callbackMu.Unlock()
		if callbackErr == nil {
			callbackErr = err
		}
	}
	checkCurrent := func(where string) {
		info := controller.Info()
		if info.SessionID != "old" {
			recordCallbackErr(fmt.Errorf("%s info session id = %q, want old", where, info.SessionID))
		}
		history := controller.History()
		if len(history) != 1 || history[0].ID != "m1" {
			recordCallbackErr(fmt.Errorf("%s history = %#v, want old history", where, history))
		}
	}

	current.onClose = func() {
		checkCurrent("close")
	}
	controller, err := New(current, func() (session.Session, error) {
		checkCurrent("create")
		return next, nil
	}, func(s session.Session) Runner {
		buildCalls++
		if buildCalls == 1 {
			return runnerFunc(noopRun)
		}
		if s != next {
			recordCallbackErr(fmt.Errorf("replacement build session = %#v, want next", s))
		}
		checkCurrent("build")
		return runnerFunc(noopRun)
	})
	if err != nil {
		t.Fatal(err)
	}

	done := make(chan error, 1)
	go func() { done <- controller.NewSession() }()

	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("NewSession() error = %v", err)
		}
	case <-time.After(250 * time.Millisecond):
		t.Fatal("NewSession() timed out; callback likely ran under controller lock")
	}

	callbackMu.Lock()
	err = callbackErr
	callbackMu.Unlock()
	if err != nil {
		t.Fatal(err)
	}
	if info := controller.Info(); info.SessionID != "new" {
		t.Fatalf("info = %#v, want new session info", info)
	}
}

func TestControllerRunnerFactoryMaySynchronouslyCloseController(t *testing.T) {
	current := &fakeSession{header: testHeader("old")}
	replacement := &fakeSession{header: testHeader("new")}
	var controller *Controller
	var buildCalls int
	var callbackErr error
	var callbackMu sync.Mutex

	recordCallbackErr := func(err error) {
		callbackMu.Lock()
		defer callbackMu.Unlock()
		if callbackErr == nil {
			callbackErr = err
		}
	}

	controller, err := New(current, func() (session.Session, error) {
		return replacement, nil
	}, func(s session.Session) Runner {
		buildCalls++
		if buildCalls == 1 {
			return runnerFunc(noopRun)
		}
		if s != replacement {
			recordCallbackErr(fmt.Errorf("replacement build session = %#v, want replacement", s))
		}
		if err := controller.Close(); err != nil {
			recordCallbackErr(fmt.Errorf("Close() error = %v, want nil", err))
		}
		return runnerFunc(noopRun)
	})
	if err != nil {
		t.Fatal(err)
	}

	newSessionDone := make(chan error, 1)
	go func() { newSessionDone <- controller.NewSession() }()

	select {
	case err := <-newSessionDone:
		if !errors.Is(err, ErrClosed) {
			t.Fatalf("NewSession() error = %v, want ErrClosed", err)
		}
	case <-time.After(250 * time.Millisecond):
		t.Fatal("NewSession() timed out; synchronous Close likely deadlocked")
	}

	callbackMu.Lock()
	err = callbackErr
	callbackMu.Unlock()
	if err != nil {
		t.Fatal(err)
	}
	if buildCalls != 2 {
		t.Fatalf("build calls = %d, want 2", buildCalls)
	}
	if current.CloseCalls() != 1 {
		t.Fatalf("old close calls = %d, want 1", current.CloseCalls())
	}
	if replacement.CloseCalls() != 1 {
		t.Fatalf("replacement close calls = %d, want 1", replacement.CloseCalls())
	}
	if info := controller.Info(); info.SessionID != "old" {
		t.Fatalf("info = %#v, want closed old session info", info)
	}
	if err := controller.Close(); err != nil {
		t.Fatalf("Close() after callback close = %v, want nil", err)
	}
	if current.CloseCalls() != 1 {
		t.Fatalf("old close calls after Close = %d, want 1", current.CloseCalls())
	}
	if replacement.CloseCalls() != 1 {
		t.Fatalf("replacement close calls after Close = %d, want 1", replacement.CloseCalls())
	}
}

func TestControllerInfoUsesActiveRuntimeUntilNewSessionRefreshesFromHeader(t *testing.T) {
	current := &fakeSession{header: session.Header{
		Version: 1, ID: "old", Workspace: "/old-workspace", Provider: "openai-compatible", Profile: "persisted", Model: "persisted-model",
	}}
	next := &fakeSession{header: session.Header{
		Version: 1, ID: "new", Workspace: "/new-workspace", Provider: "openai-compatible", Profile: "header-next", Model: "header-next-model",
	}}
	controller, err := New(current, func() (session.Session, error) {
		return next, nil
	}, func(session.Session) Runner {
		return runnerFunc(noopRun)
	}, WithRuntimeInfo(RuntimeInfo{Provider: "openai-compatible", Profile: "active", Model: "active-model"}))
	if err != nil {
		t.Fatal(err)
	}

	assertInfo := func(wantID, wantPath, wantWorkspace, wantProfile, wantModel string) {
		t.Helper()
		got := controller.Info()
		if got.SessionID != wantID || got.SessionPath != wantPath || got.Workspace != wantWorkspace {
			t.Fatalf("dynamic info = %#v", got)
		}
		if got.Provider != "openai-compatible" || got.Profile != wantProfile || got.Model != wantModel {
			t.Fatalf("runtime info = %#v, want profile %q model %q", got, wantProfile, wantModel)
		}
	}
	assertInfo("old", "/sessions/old.jsonl", "/old-workspace", "active", "active-model")
	if err := controller.NewSession(); err != nil {
		t.Fatal(err)
	}
	assertInfo("new", "/sessions/new.jsonl", "/new-workspace", "header-next", "header-next-model")
}

func TestControllerNewSessionAfterResumeResetsRuntimeInfoAndRunnerFromNewHeader(t *testing.T) {
	initial := &fakeSession{header: session.Header{
		Version: 1, ID: "initial", Workspace: "/workspace", Provider: "openai-compatible", Profile: "startup", Model: "startup-model",
	}}
	resumed := &fakeSession{header: session.Header{
		Version: 1, ID: "resumed", Workspace: "/workspace", Provider: "openai-compatible", Profile: "resumed", Model: "resumed-model",
	}}
	fresh := &fakeSession{header: session.Header{
		Version: 1, ID: "fresh", Workspace: "/workspace", Provider: "openai-compatible", Profile: "startup", Model: "startup-model",
	}}
	initialRunner := &recordingRunner{}
	resumedRunner := &recordingRunner{}
	freshRunner := &recordingRunner{}
	buildCalls := 0
	controller, err := New(initial, func() (session.Session, error) {
		return fresh, nil
	}, func(current session.Session) Runner {
		buildCalls++
		switch current {
		case initial:
			return initialRunner
		case fresh:
			return freshRunner
		default:
			return nil
		}
	}, WithRuntimeInfo(RuntimeInfo{Provider: "openai-compatible", Profile: "startup", Model: "startup-model"}), WithSessionBrowser(nil,
		func(context.Context, string) (SessionReplacement, error) {
			return SessionReplacement{
				Session: resumed, Runner: resumedRunner,
				RuntimeInfo: RuntimeInfo{Provider: "openai-compatible", Profile: "resumed", Model: "resumed-model"},
			}, nil
		}))
	if err != nil {
		t.Fatal(err)
	}

	if _, err := controller.ResumeSession(context.Background(), resumed.Path()); err != nil {
		t.Fatal(err)
	}
	if got := controller.Info(); got.SessionID != "resumed" || got.Profile != "resumed" || got.Model != "resumed-model" {
		t.Fatalf("info after resume = %#v", got)
	}
	if err := controller.NewSession(); err != nil {
		t.Fatal(err)
	}
	if got := controller.Info(); got.SessionID != "fresh" || got.Provider != fresh.header.Provider || got.Profile != fresh.header.Profile || got.Model != fresh.header.Model {
		t.Fatalf("info after new = %#v, want fresh header runtime", got)
	}
	if err := controller.Prompt(context.Background(), "use startup runner", nil); err != nil {
		t.Fatal(err)
	}
	if initialRunner.Calls() != 0 || resumedRunner.Calls() != 0 || freshRunner.Calls() != 1 || buildCalls != 2 {
		t.Fatalf("runner calls = initial %d resumed %d fresh %d; builds = %d", initialRunner.Calls(), resumedRunner.Calls(), freshRunner.Calls(), buildCalls)
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

func TestControllerNewSessionRejectsNilReplacementSessionAndKeepsCurrent(t *testing.T) {
	current := &fakeSession{header: testHeader("old")}
	controller, err := New(current, func() (session.Session, error) {
		return nil, nil
	}, func(session.Session) Runner { return runnerFunc(noopRun) })
	if err != nil {
		t.Fatal(err)
	}
	if err := controller.NewSession(); err == nil || err.Error() != "session factory returned nil session" {
		t.Fatalf("NewSession() error = %v, want nil session error", err)
	}
	if current.CloseCalls() != 0 {
		t.Fatalf("old close calls = %d, want 0", current.CloseCalls())
	}
	if info := controller.Info(); info.SessionID != "old" {
		t.Fatalf("info = %#v", info)
	}
	if err := controller.Prompt(context.Background(), "still-open", func(agent.Event) {}); err != nil {
		t.Fatalf("Prompt() after nil session = %v", err)
	}
}

func TestControllerNewSessionRejectsNilReplacementRunnerAndKeepsCurrent(t *testing.T) {
	current := &fakeSession{header: testHeader("old")}
	replacement := &fakeSession{header: testHeader("new")}
	var buildCalls int
	controller, err := New(current, func() (session.Session, error) {
		return replacement, nil
	}, func(session.Session) Runner {
		buildCalls++
		if buildCalls == 1 {
			return runnerFunc(noopRun)
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := controller.NewSession(); err == nil || err.Error() != "runner factory returned nil runner" {
		t.Fatalf("NewSession() error = %v, want nil runner error", err)
	}
	if current.CloseCalls() != 0 {
		t.Fatalf("old close calls = %d, want 0", current.CloseCalls())
	}
	if replacement.CloseCalls() != 1 {
		t.Fatalf("replacement close calls = %d, want 1", replacement.CloseCalls())
	}
	if info := controller.Info(); info.SessionID != "old" {
		t.Fatalf("info = %#v", info)
	}
	if err := controller.Prompt(context.Background(), "still-open", func(agent.Event) {}); err != nil {
		t.Fatalf("Prompt() after nil runner = %v", err)
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

func TestControllerCloseWaitsForInProgressReplacement(t *testing.T) {
	started := make(chan struct{})
	release := make(chan struct{})
	current := &fakeSession{header: testHeader("old")}
	replacement := &fakeSession{header: testHeader("new")}
	var buildCalls int
	controller, err := New(current, func() (session.Session, error) {
		close(started)
		<-release
		return replacement, nil
	}, func(session.Session) Runner {
		buildCalls++
		return runnerFunc(noopRun)
	})
	if err != nil {
		t.Fatal(err)
	}

	newSessionDone := make(chan error, 1)
	go func() { newSessionDone <- controller.NewSession() }()
	<-started

	closeDone := make(chan error, 1)
	go func() { closeDone <- controller.Close() }()

	select {
	case err := <-closeDone:
		t.Fatalf("Close() returned early: %v", err)
	case <-time.After(50 * time.Millisecond):
	}

	close(release)
	if err := <-newSessionDone; !errors.Is(err, ErrClosed) {
		t.Fatalf("NewSession() error = %v, want ErrClosed", err)
	}
	if err := <-closeDone; err != nil {
		t.Fatalf("Close() error = %v, want nil", err)
	}
	if buildCalls != 2 {
		t.Fatalf("build calls = %d, want 2", buildCalls)
	}
	if current.CloseCalls() != 1 {
		t.Fatalf("old close calls = %d, want 1", current.CloseCalls())
	}
	if replacement.CloseCalls() != 1 {
		t.Fatalf("replacement close calls = %d, want 1", replacement.CloseCalls())
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

func TestControllerInfoExposesOptionalAggregateUsageDefensively(t *testing.T) {
	current := &aggregateUsageSession{
		Session: session.NewMemory(testHeader("usage")),
		usage:   model.Usage{InputTokens: 20, OutputTokens: 6},
		present: true,
	}
	controller, err := New(current, func() (session.Session, error) {
		return session.NewMemory(testHeader("next")), nil
	}, func(session.Session) Runner { return runnerFunc(noopRun) })
	if err != nil {
		t.Fatal(err)
	}

	info := controller.Info()
	if !info.UsagePresent || info.Usage != current.usage {
		t.Fatalf("Info() = %#v", info)
	}
	info.Usage.InputTokens = 99
	if got := controller.Info(); got.Usage != current.usage {
		t.Fatalf("mutating Info usage changed controller state: %#v", got)
	}
}

func TestControllerInfoIncludesContextWindowAndSnapshotUsage(t *testing.T) {
	runtime := RuntimeInfo{Provider: "openai-compatible", Profile: "active", Model: "active-model"}
	setRuntimeContextWindow(t, &runtime, 128_000)
	current := &snapshotSession{
		Session: session.NewMemory(testHeader("usage")),
		snapshot: session.Snapshot{
			AggregateUsage:            model.Usage{InputTokens: 20, OutputTokens: 6},
			AggregateUsagePresent:     true,
			ContextInputTokens:        11,
			ContextInputTokensPresent: true,
		},
	}
	controller, err := New(current, func() (session.Session, error) {
		return session.NewMemory(testHeader("next")), nil
	}, func(session.Session) Runner { return runnerFunc(noopRun) }, WithRuntimeInfo(runtime))
	if err != nil {
		t.Fatal(err)
	}

	info := controller.Info()
	if !info.UsagePresent || info.Usage != current.snapshot.AggregateUsage {
		t.Fatalf("Info() = %#v", info)
	}
	assertInfoContext(t, info, 128_000, 11, true, false)
	info.Usage.InputTokens = 99
	if got := controller.Info(); got.Usage != current.snapshot.AggregateUsage {
		t.Fatalf("mutating Info usage changed controller state: %#v", got)
	}
}

func TestControllerNewSessionWithoutBuilderPreservesActiveContextWindow(t *testing.T) {
	runtime := RuntimeInfo{Provider: "openai-compatible", Profile: "active", Model: "active-model"}
	setRuntimeContextWindow(t, &runtime, 32_768)
	initial := &snapshotSession{Session: &fakeSession{header: testHeader("initial")}, snapshot: session.Snapshot{ContextInputTokensPending: true}}
	next := &snapshotSession{Session: &fakeSession{header: testHeader("next")}, snapshot: session.Snapshot{ContextInputTokensPending: true}}
	controller, err := New(initial, func() (session.Session, error) {
		return next, nil
	}, func(session.Session) Runner { return runnerFunc(noopRun) }, WithRuntimeInfo(runtime))
	if err != nil {
		t.Fatal(err)
	}

	if err := controller.NewSession(); err != nil {
		t.Fatal(err)
	}
	got := controller.Info()
	if got.SessionID != "next" {
		t.Fatalf("Info() session = %#v", got)
	}
	assertInfoContext(t, got, 32_768, 0, false, true)
}

func TestControllerResumePublishesContextWindowOnlyAfterSuccessfulReplacement(t *testing.T) {
	oldRuntime := RuntimeInfo{Provider: "openai-compatible", Profile: "old", Model: "old-model"}
	setRuntimeContextWindow(t, &oldRuntime, 16_384)
	newRuntime := RuntimeInfo{Provider: "openai-compatible", Profile: "new", Model: "new-model"}
	setRuntimeContextWindow(t, &newRuntime, 65_536)
	old := &snapshotSession{Session: &fakeSession{header: testHeader("old")}, snapshot: session.Snapshot{ContextInputTokens: 7, ContextInputTokensPresent: true}}
	next := &snapshotSession{Session: &fakeSession{header: testHeader("next")}, snapshot: session.Snapshot{ContextInputTokensPending: true}}

	controller, err := New(old, func() (session.Session, error) {
		return &fakeSession{header: testHeader("unused")}, nil
	}, func(session.Session) Runner { return runnerFunc(noopRun) }, WithRuntimeInfo(oldRuntime), WithSessionBrowser(nil,
		func(context.Context, string) (SessionReplacement, error) {
			return SessionReplacement{Session: next, Runner: runnerFunc(noopRun), RuntimeInfo: newRuntime}, nil
		}))
	if err != nil {
		t.Fatal(err)
	}

	assertInfoContext(t, controller.Info(), 16_384, 7, true, false)
	if _, err := controller.ResumeSession(context.Background(), next.Path()); err != nil {
		t.Fatal(err)
	}
	got := controller.Info()
	if got.SessionID != "next" {
		t.Fatalf("Info() after successful resume = %#v", got)
	}
	assertInfoContext(t, got, 65_536, 0, false, true)

	failing, err := New(old, func() (session.Session, error) {
		return &fakeSession{header: testHeader("unused")}, nil
	}, func(session.Session) Runner { return runnerFunc(noopRun) }, WithRuntimeInfo(oldRuntime), WithSessionBrowser(nil,
		func(context.Context, string) (SessionReplacement, error) {
			return SessionReplacement{Session: next, Runner: runnerFunc(noopRun), RuntimeInfo: newRuntime}, errors.New("resume failed")
		}))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := failing.ResumeSession(context.Background(), next.Path()); err == nil || err.Error() != "resume failed" {
		t.Fatalf("ResumeSession() error = %v, want resume failed", err)
	}
	stillOld := failing.Info()
	if stillOld.SessionID != "old" {
		t.Fatalf("Info() after failed resume = %#v", stillOld)
	}
	assertInfoContext(t, stillOld, 16_384, 7, true, false)
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

func TestControllerCloseClosesRunnerDirectly(t *testing.T) {
	current := &fakeSession{header: testHeader("close")}
	runner := &recordingRunner{}
	controller, err := New(current, func() (session.Session, error) {
		return &fakeSession{header: testHeader("next")}, nil
	}, func(session.Session) Runner { return runner })
	if err != nil {
		t.Fatal(err)
	}
	if err := controller.Close(); err != nil {
		t.Fatal(err)
	}
	if runner.CloseCalls() != 1 {
		t.Fatalf("runner close calls = %d, want 1", runner.CloseCalls())
	}
}

func TestControllerCloseWaitsForActivePromptThenClosesRunner(t *testing.T) {
	started := make(chan struct{})
	release := make(chan struct{})
	runner := &lifecycleRunner{run: func(ctx context.Context, text string, emit func(agent.Event)) error {
		close(started)
		<-release
		return nil
	}}
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
	if runner.CloseCalls() != 0 {
		t.Fatalf("runner close calls before release = %d, want 0", runner.CloseCalls())
	}

	close(release)
	if err := <-promptDone; err != nil {
		t.Fatal(err)
	}
	if err := <-closeDone; err != nil {
		t.Fatal(err)
	}
	if runner.CloseCalls() != 1 {
		t.Fatalf("runner close calls after release = %d, want 1", runner.CloseCalls())
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

func TestControllerResumeSwapsSessionRunnerAndRuntimeAtomically(t *testing.T) {
	old := &fakeSession{header: testHeader("old")}
	next := &fakeSession{header: testHeader("next")}
	oldRunner := &recordingRunner{}
	nextRunner := &recordingRunner{}
	warnings := []session.Warning{{Message: "repaired dangling tool call"}}
	controller := newControllerWithRunnerAndBrowser(t, old, oldRunner, nil,
		func(context.Context, string) (SessionReplacement, error) {
			return SessionReplacement{
				Session: next,
				Runner:  nextRunner,
				RuntimeInfo: RuntimeInfo{
					Provider: "openai-compatible",
					Profile:  "next",
					Model:    "next-model",
				},
				Warnings: warnings,
			}, nil
		})

	result, err := controller.ResumeSession(context.Background(), next.Path())
	if err != nil {
		t.Fatal(err)
	}
	if len(result.Warnings) != 1 || result.Warnings[0].Message != warnings[0].Message {
		t.Fatalf("warnings = %#v", result.Warnings)
	}
	if result.SessionPath != canonicalSessionPath(next.Path()) {
		t.Fatalf("session path = %q, want %q", result.SessionPath, canonicalSessionPath(next.Path()))
	}
	result.Warnings[0].Message = "changed"
	if warnings[0].Message != "repaired dangling tool call" {
		t.Fatalf("factory warnings mutated: %#v", warnings)
	}
	if got := controller.Info(); got.SessionID != "next" || got.Profile != "next" || got.Model != "next-model" {
		t.Fatalf("info = %#v", got)
	}
	if old.CloseCalls() != 1 {
		t.Fatalf("old close calls = %d, want 1", old.CloseCalls())
	}
	if oldRunner.CloseCalls() != 1 {
		t.Fatalf("old runner close calls = %d, want 1", oldRunner.CloseCalls())
	}
	if nextRunner.CloseCalls() != 0 {
		t.Fatalf("next runner close calls = %d, want 0", nextRunner.CloseCalls())
	}
	if err := controller.Prompt(context.Background(), "new runner", nil); err != nil {
		t.Fatal(err)
	}
	if oldRunner.Calls() != 0 || nextRunner.Calls() != 1 {
		t.Fatalf("runner calls = old %d, next %d", oldRunner.Calls(), nextRunner.Calls())
	}
}

func TestControllerResumeBuildFailureKeepsCurrentUsable(t *testing.T) {
	buildErr := errors.New("build failed")
	old := &fakeSession{header: testHeader("old")}
	oldRunner := &recordingRunner{}
	controller := newControllerWithRunnerAndBrowser(t, old, oldRunner, nil,
		func(context.Context, string) (SessionReplacement, error) {
			return SessionReplacement{}, buildErr
		})

	if _, err := controller.ResumeSession(context.Background(), "/sessions/next.jsonl"); err != buildErr {
		t.Fatalf("ResumeSession() error = %v, want exact build error", err)
	}
	if err := controller.Prompt(context.Background(), "still works", nil); err != nil {
		t.Fatal(err)
	}
	if oldRunner.Calls() != 1 {
		t.Fatalf("old runner calls = %d, want 1", oldRunner.Calls())
	}
	if old.CloseCalls() != 0 {
		t.Fatalf("old close calls = %d, want 0", old.CloseCalls())
	}
	if oldRunner.CloseCalls() != 0 {
		t.Fatalf("old runner close calls = %d, want 0", oldRunner.CloseCalls())
	}
}

func newControllerWithProfileSwitcher(t *testing.T, initial session.Session, runner Runner, profiles []string, switchProfile ProfileSwitchFactory) *Controller {
	t.Helper()
	controller, err := New(initial, func() (session.Session, error) {
		return &fakeSession{header: testHeader("new")}, nil
	}, func(session.Session) Runner { return runner },
		WithRuntimeInfo(RuntimeInfo{Provider: "openai-compatible", Profile: "old", Model: "old-model"}),
		WithProfileSwitcher(profiles, switchProfile))
	if err != nil {
		t.Fatal(err)
	}
	return controller
}

func TestControllerSwitchProfileSwapsSessionRunnerAndRuntimeAtomically(t *testing.T) {
	old := &fakeSession{header: testHeader("old")}
	next := &fakeSession{header: testHeader("next")}
	oldRunner := &recordingRunner{}
	nextRunner := &recordingRunner{}
	var gotProfile string
	controller := newControllerWithProfileSwitcher(t, old, oldRunner, []string{"alpha", "chatgpt"},
		func(_ context.Context, name string) (SessionReplacement, error) {
			gotProfile = name
			return SessionReplacement{
				Session: next,
				Runner:  nextRunner,
				RuntimeInfo: RuntimeInfo{
					Provider: "chatgpt",
					Profile:  "chatgpt",
					Model:    "gpt-5",
				},
			}, nil
		})

	result, err := controller.SwitchProfile(context.Background(), "chatgpt")
	if err != nil {
		t.Fatal(err)
	}
	if gotProfile != "chatgpt" {
		t.Fatalf("factory profile = %q, want chatgpt", gotProfile)
	}
	if result.SessionPath != canonicalSessionPath(next.Path()) {
		t.Fatalf("session path = %q, want %q", result.SessionPath, canonicalSessionPath(next.Path()))
	}
	if got := controller.Info(); got.Provider != "chatgpt" || got.Profile != "chatgpt" || got.Model != "gpt-5" {
		t.Fatalf("info = %#v", got)
	}
	if old.CloseCalls() != 1 || oldRunner.CloseCalls() != 1 {
		t.Fatalf("old close calls = session %d runner %d, want 1/1", old.CloseCalls(), oldRunner.CloseCalls())
	}
	if nextRunner.CloseCalls() != 0 {
		t.Fatalf("next runner close calls = %d, want 0", nextRunner.CloseCalls())
	}
	if err := controller.Prompt(context.Background(), "on new profile", nil); err != nil {
		t.Fatal(err)
	}
	if oldRunner.Calls() != 0 || nextRunner.Calls() != 1 {
		t.Fatalf("runner calls = old %d, next %d", oldRunner.Calls(), nextRunner.Calls())
	}
}

func TestControllerSwitchProfileBuildFailureKeepsCurrentUsable(t *testing.T) {
	buildErr := errors.New("profile \"chatgpt\" not found")
	old := &fakeSession{header: testHeader("old")}
	oldRunner := &recordingRunner{}
	controller := newControllerWithProfileSwitcher(t, old, oldRunner, []string{"alpha"},
		func(context.Context, string) (SessionReplacement, error) {
			return SessionReplacement{}, buildErr
		})

	if _, err := controller.SwitchProfile(context.Background(), "chatgpt"); err != buildErr {
		t.Fatalf("SwitchProfile() error = %v, want exact build error", err)
	}
	if err := controller.Prompt(context.Background(), "still works", nil); err != nil {
		t.Fatal(err)
	}
	if oldRunner.Calls() != 1 {
		t.Fatalf("old runner calls = %d, want 1", oldRunner.Calls())
	}
	if old.CloseCalls() != 0 || oldRunner.CloseCalls() != 0 {
		t.Fatalf("old close calls = session %d runner %d, want 0/0", old.CloseCalls(), oldRunner.CloseCalls())
	}
}

func TestControllerSwitchProfileRejectedWhilePrompting(t *testing.T) {
	started := make(chan struct{})
	release := make(chan struct{})
	runner := runnerFunc(func(ctx context.Context, text string, emit func(agent.Event)) error {
		close(started)
		<-release
		return nil
	})
	controller := newControllerWithProfileSwitcher(t, &fakeSession{header: testHeader("old")}, runner, []string{"alpha"},
		func(context.Context, string) (SessionReplacement, error) {
			t.Error("factory must not run while prompting")
			return SessionReplacement{}, nil
		})
	done := make(chan error, 1)
	go func() { done <- controller.Prompt(context.Background(), "one", func(agent.Event) {}) }()
	<-started
	if _, err := controller.SwitchProfile(context.Background(), "alpha"); !errors.Is(err, ErrPromptActive) {
		t.Fatalf("error = %v, want ErrPromptActive", err)
	}
	close(release)
	if err := <-done; err != nil {
		t.Fatal(err)
	}
}

func TestControllerProfilesReturnsDefensiveCopy(t *testing.T) {
	controller := newControllerWithProfileSwitcher(t, &fakeSession{header: testHeader("old")}, &recordingRunner{}, []string{"alpha", "chatgpt"}, nil)
	profiles := controller.Profiles()
	if len(profiles) != 2 || profiles[0] != "alpha" || profiles[1] != "chatgpt" {
		t.Fatalf("profiles = %#v", profiles)
	}
	profiles[0] = "mutated"
	if again := controller.Profiles(); again[0] != "alpha" {
		t.Fatalf("Profiles() mutated by caller: %#v", again)
	}
}

func TestControllerSwitchProfileWithoutFactoryReportsUnavailable(t *testing.T) {
	controller := newTestController(t, runnerFunc(noopRun))
	if _, err := controller.SwitchProfile(context.Background(), "alpha"); !errors.Is(err, ErrProfileSwitchUnavailable) {
		t.Fatalf("SwitchProfile() error = %v, want ErrProfileSwitchUnavailable", err)
	}
}

func TestControllerListSessionsMarksCurrentOnDefensiveCopy(t *testing.T) {
	old := &fakeSession{header: testHeader("old"), path: "/sessions/old.jsonl"}
	listed := session.ListResult{Sessions: []session.SessionInfo{
		{Path: "/sessions/nested/../old.jsonl", ID: "old"},
		{Path: "/sessions/other.jsonl", ID: "other", Current: true},
	}, Skipped: 2}
	var controller *Controller
	lister := func(context.Context, int) (session.ListResult, error) {
		if got := controller.Info().SessionID; got != "old" {
			t.Errorf("Info() in lister session ID = %q, want old", got)
		}
		return listed, nil
	}
	controller = newControllerWithRunnerAndBrowser(t, old, &recordingRunner{}, lister, nil)

	got, err := controller.ListSessions(context.Background(), 20)
	if err != nil {
		t.Fatal(err)
	}
	if got.Skipped != 2 || len(got.Sessions) != 2 || !got.Sessions[0].Current || got.Sessions[1].Current {
		t.Fatalf("list result = %#v", got)
	}
	if listed.Sessions[0].Current || !listed.Sessions[1].Current {
		t.Fatalf("lister-owned result mutated: %#v", listed)
	}
	got.Sessions[0].ID = "mutated"
	gotAgain, err := controller.ListSessions(context.Background(), 20)
	if err != nil {
		t.Fatal(err)
	}
	if gotAgain.Sessions[0].ID != "old" {
		t.Fatalf("second list result = %#v", gotAgain)
	}
}

func TestControllerListSessionsDoesNotMarkCurrentForInitialMemorySession(t *testing.T) {
	cwd, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	listed := session.ListResult{Sessions: []session.SessionInfo{
		{Path: cwd, ID: "cwd", Current: true},
		{Path: "", ID: "empty", Current: true},
	}}
	controller := newControllerWithRunnerAndBrowser(t, session.NewMemory(testHeader("memory")), &recordingRunner{},
		func(context.Context, int) (session.ListResult, error) { return listed, nil }, nil)

	got, err := controller.ListSessions(context.Background(), 20)
	if err != nil {
		t.Fatal(err)
	}
	for _, info := range got.Sessions {
		if info.Current {
			t.Fatalf("session %q was marked current for memory session: %#v", info.ID, got)
		}
	}
}

func TestControllerListSessionsAndResumeReportPersistenceDisabled(t *testing.T) {
	controller := newTestController(t, &recordingRunner{})
	var browser SessionBrowser = controller

	if _, err := browser.ListSessions(context.Background(), 20); !errors.Is(err, ErrPersistenceDisabled) {
		t.Fatalf("ListSessions() error = %v, want ErrPersistenceDisabled", err)
	}
	if _, err := browser.ResumeSession(context.Background(), "/sessions/next.jsonl"); !errors.Is(err, ErrPersistenceDisabled) {
		t.Fatalf("ResumeSession() error = %v, want ErrPersistenceDisabled", err)
	}
}

func TestControllerMemoryFacadeReportsUnavailableWithoutManager(t *testing.T) {
	controller := newTestController(t, &recordingRunner{})

	if _, err := controller.SearchMemory(context.Background(), memory.SearchRequest{}); !errors.Is(err, ErrMemoryUnavailable) {
		t.Fatalf("SearchMemory() error = %v, want ErrMemoryUnavailable", err)
	}
	if _, err := controller.RememberMemory(context.Background(), memory.RememberRequest{}); !errors.Is(err, ErrMemoryUnavailable) {
		t.Fatalf("RememberMemory() error = %v, want ErrMemoryUnavailable", err)
	}
	if _, err := controller.ForgetMemory(context.Background(), memory.ForgetRequest{}); !errors.Is(err, ErrMemoryUnavailable) {
		t.Fatalf("ForgetMemory() error = %v, want ErrMemoryUnavailable", err)
	}
	if _, err := controller.ReviewMemoryCandidate(context.Background(), memory.ReviewRequest{}); !errors.Is(err, ErrMemoryUnavailable) {
		t.Fatalf("ReviewMemoryCandidate() error = %v, want ErrMemoryUnavailable", err)
	}
}

func TestControllerMemoryFacadeDelegatesToManager(t *testing.T) {
	manager := &fakeMemoryManager{
		searchResult:   memory.SearchResult{Records: []memory.Record{{ID: "rec-1"}}},
		rememberResult: memory.Record{ID: "rec-2"},
		forgetResult:   memory.ForgetResult{Tombstone: memory.Tombstone{ID: "rec-3"}},
		reviewResult:   memory.ReviewResult{Record: &memory.Record{ID: "rec-4"}},
	}
	userScope := memory.Scope{Namespace: "user", ID: "u1"}
	workspaceScope := memory.Scope{Namespace: "workspace", ID: "w1"}
	controller, err := New(&fakeSession{header: testHeader("initial")}, func() (session.Session, error) {
		return &fakeSession{header: testHeader("next")}, nil
	}, func(session.Session) Runner { return &recordingRunner{} }, WithMemory(manager, userScope, workspaceScope))
	if err != nil {
		t.Fatal(err)
	}

	searchResult, err := controller.SearchMemory(context.Background(), memory.SearchRequest{Query: "vim"})
	if err != nil {
		t.Fatal(err)
	}
	if manager.searchRequest.Query != "vim" || len(searchResult.Records) != 1 || searchResult.Records[0].ID != "rec-1" {
		t.Fatalf("SearchMemory() = %#v, request = %#v", searchResult, manager.searchRequest)
	}

	rememberResult, err := controller.RememberMemory(context.Background(), memory.RememberRequest{Key: "editor"})
	if err != nil {
		t.Fatal(err)
	}
	if manager.rememberRequest.Key != "editor" || rememberResult.ID != "rec-2" {
		t.Fatalf("RememberMemory() = %#v, request = %#v", rememberResult, manager.rememberRequest)
	}

	forgetResult, err := controller.ForgetMemory(context.Background(), memory.ForgetRequest{Ref: memory.RecordRef{ID: "rec-1"}})
	if err != nil {
		t.Fatal(err)
	}
	if manager.forgetRequest.Ref.ID != "rec-1" || forgetResult.Tombstone.ID != "rec-3" {
		t.Fatalf("ForgetMemory() = %#v, request = %#v", forgetResult, manager.forgetRequest)
	}

	reviewResult, err := controller.ReviewMemoryCandidate(context.Background(), memory.ReviewRequest{Ref: memory.CandidateRef{ID: "cand-1"}})
	if err != nil {
		t.Fatal(err)
	}
	if manager.reviewRequest.Ref.ID != "cand-1" || reviewResult.Record == nil || reviewResult.Record.ID != "rec-4" {
		t.Fatalf("ReviewMemoryCandidate() = %#v, request = %#v", reviewResult, manager.reviewRequest)
	}
}

func TestControllerMemoryScopesReportsUnavailableWithoutManager(t *testing.T) {
	controller := newTestController(t, &recordingRunner{})

	if _, _, ok := controller.MemoryScopes(); ok {
		t.Fatal("MemoryScopes() ok = true, want false without a bound manager")
	}
	if _, err := controller.GetMemory(context.Background(), memory.RecordRef{ID: "rec-1"}); !errors.Is(err, ErrMemoryUnavailable) {
		t.Fatalf("GetMemory() error = %v, want ErrMemoryUnavailable", err)
	}
}

func TestControllerMemoryScopesAndGetDelegateToManager(t *testing.T) {
	manager := &fakeMemoryManager{getResult: memory.Record{ID: "rec-1", Revision: 3}}
	userScope := memory.Scope{Namespace: "user", ID: "u1"}
	workspaceScope := memory.Scope{Namespace: "workspace", ID: "w1"}
	controller, err := New(&fakeSession{header: testHeader("initial")}, func() (session.Session, error) {
		return &fakeSession{header: testHeader("next")}, nil
	}, func(session.Session) Runner { return &recordingRunner{} }, WithMemory(manager, userScope, workspaceScope))
	if err != nil {
		t.Fatal(err)
	}

	gotUser, gotWorkspace, ok := controller.MemoryScopes()
	if !ok || gotUser != userScope || gotWorkspace != workspaceScope {
		t.Fatalf("MemoryScopes() = %#v, %#v, %v, want %#v, %#v, true", gotUser, gotWorkspace, ok, userScope, workspaceScope)
	}

	record, err := controller.GetMemory(context.Background(), memory.RecordRef{Scope: userScope, ID: "rec-1"})
	if err != nil {
		t.Fatal(err)
	}
	if manager.getRequest.ID != "rec-1" || record.Revision != 3 {
		t.Fatalf("GetMemory() = %#v, request = %#v", record, manager.getRequest)
	}
}

func TestControllerResumeEmptyPathStillCallsFactoryForMemorySession(t *testing.T) {
	factoryErr := errors.New("empty path rejected by factory")
	factoryCalls := 0
	controller := newControllerWithRunnerAndBrowser(t, session.NewMemory(testHeader("memory")), &recordingRunner{}, nil,
		func(_ context.Context, path string) (SessionReplacement, error) {
			factoryCalls++
			if path != "" {
				t.Errorf("factory path = %q, want empty", path)
			}
			return SessionReplacement{}, factoryErr
		})

	if _, err := controller.ResumeSession(context.Background(), ""); err != factoryErr {
		t.Fatalf("ResumeSession() error = %v, want exact factory error", err)
	}
	if factoryCalls != 1 {
		t.Fatalf("factory calls = %d, want 1", factoryCalls)
	}
}

func TestControllerResumeCurrentCanonicalPathIsNoOp(t *testing.T) {
	directory := t.TempDir()
	currentPath := filepath.Join(directory, "current.jsonl")
	if err := os.WriteFile(currentPath, []byte("session"), 0o600); err != nil {
		t.Fatal(err)
	}
	aliasPath := filepath.Join(directory, "alias.jsonl")
	if err := os.Symlink(currentPath, aliasPath); err != nil {
		t.Fatal(err)
	}
	old := &fakeSession{header: testHeader("old"), path: currentPath}
	factoryCalls := 0
	controller := newControllerWithRunnerAndBrowser(t, old, &recordingRunner{}, nil,
		func(context.Context, string) (SessionReplacement, error) {
			factoryCalls++
			return SessionReplacement{}, errors.New("must not run")
		})

	result, err := controller.ResumeSession(context.Background(), aliasPath)
	if err != nil {
		t.Fatal(err)
	}
	if result.Warnings != nil {
		t.Fatalf("warnings = %#v, want nil", result.Warnings)
	}
	wantPath := canonicalSessionPath(currentPath)
	if result.SessionPath != wantPath {
		t.Fatalf("session path = %q, want canonical current path %q", result.SessionPath, wantPath)
	}
	if factoryCalls != 0 || old.CloseCalls() != 0 {
		t.Fatalf("factory calls = %d, old close calls = %d", factoryCalls, old.CloseCalls())
	}
}

func TestControllerResumeResultUsesCommittedCanonicalSessionPath(t *testing.T) {
	directory := t.TempDir()
	canonicalPath := filepath.Join(directory, "canonical.jsonl")
	if err := os.WriteFile(canonicalPath, []byte("session"), 0o600); err != nil {
		t.Fatal(err)
	}
	aliasPath := filepath.Join(directory, "alias.jsonl")
	if err := os.Symlink(canonicalPath, aliasPath); err != nil {
		t.Fatal(err)
	}

	old := &fakeSession{header: testHeader("old")}
	next := &fakeSession{header: testHeader("next"), path: aliasPath}
	controller := newControllerWithRunnerAndBrowser(t, old, &recordingRunner{}, nil,
		func(context.Context, string) (SessionReplacement, error) {
			return SessionReplacement{Session: next, Runner: &recordingRunner{}}, nil
		})

	result, err := controller.ResumeSession(context.Background(), aliasPath)
	if err != nil {
		t.Fatal(err)
	}
	wantPath := canonicalSessionPath(canonicalPath)
	if result.SessionPath != wantPath {
		t.Fatalf("session path = %q, want committed canonical path %q", result.SessionPath, wantPath)
	}
	if got := controller.Info().SessionPath; got != result.SessionPath {
		t.Fatalf("Info().SessionPath = %q, result path = %q", got, result.SessionPath)
	}
}

func TestControllerResumeRejectsInvalidCandidateAndKeepsCurrent(t *testing.T) {
	tests := []struct {
		name      string
		candidate *fakeSession
		runner    Runner
		wantErr   string
	}{
		{name: "nil session", runner: &recordingRunner{}, wantErr: "resume factory returned nil session"},
		{name: "nil runner", candidate: &fakeSession{header: testHeader("next")}, wantErr: "resume factory returned nil runner"},
		{name: "workspace mismatch", candidate: &fakeSession{header: session.Header{Version: 1, ID: "next", Workspace: "/other"}}, runner: &recordingRunner{}, wantErr: "replacement session workspace does not match current workspace"},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			old := &fakeSession{header: testHeader("old")}
			oldRunner := &recordingRunner{}
			controller := newControllerWithRunnerAndBrowser(t, old, oldRunner, nil,
				func(context.Context, string) (SessionReplacement, error) {
					var candidate session.Session
					if tt.candidate != nil {
						candidate = tt.candidate
					}
					return SessionReplacement{Session: candidate, Runner: tt.runner}, nil
				})

			if _, err := controller.ResumeSession(context.Background(), "/sessions/requested.jsonl"); err == nil || err.Error() != tt.wantErr {
				t.Fatalf("ResumeSession() error = %v, want %q", err, tt.wantErr)
			}
			if old.CloseCalls() != 0 {
				t.Fatalf("old close calls = %d, want 0", old.CloseCalls())
			}
			if oldRunner.CloseCalls() != 0 {
				t.Fatalf("old runner close calls = %d, want 0", oldRunner.CloseCalls())
			}
			if tt.candidate != nil && tt.candidate.CloseCalls() != 1 {
				t.Fatalf("candidate close calls = %d, want 1", tt.candidate.CloseCalls())
			}
			if r, ok := tt.runner.(*recordingRunner); ok && tt.candidate != nil && r.CloseCalls() != 1 {
				t.Fatalf("candidate runner close calls = %d, want 1", r.CloseCalls())
			}
			if err := controller.Prompt(context.Background(), "still works", nil); err != nil {
				t.Fatal(err)
			}
			if oldRunner.Calls() != 1 {
				t.Fatalf("old runner calls = %d, want 1", oldRunner.Calls())
			}
		})
	}
}

func TestControllerResumeIsMutuallyExclusiveWithPromptNewAndResume(t *testing.T) {
	t.Run("prompt active", func(t *testing.T) {
		entered, release := make(chan struct{}), make(chan struct{})
		oldRunner := runnerFunc(func(context.Context, string, func(agent.Event)) error {
			close(entered)
			<-release
			return nil
		})
		controller := newControllerWithRunnerAndBrowser(t, &fakeSession{header: testHeader("old")}, oldRunner, nil,
			func(context.Context, string) (SessionReplacement, error) {
				return SessionReplacement{}, errors.New("must not run")
			})
		promptDone := make(chan error, 1)
		go func() { promptDone <- controller.Prompt(context.Background(), "prompt", nil) }()
		awaitSignal(t, entered, "prompt start")
		if _, err := controller.ResumeSession(context.Background(), "/sessions/next.jsonl"); !errors.Is(err, ErrPromptActive) {
			t.Fatalf("ResumeSession() error = %v, want ErrPromptActive", err)
		}
		close(release)
		awaitError(t, promptDone, "prompt")
	})

	t.Run("new active", func(t *testing.T) {
		entered, release := make(chan struct{}), make(chan struct{})
		old := &fakeSession{header: testHeader("old")}
		controller, err := New(old, func() (session.Session, error) {
			close(entered)
			<-release
			return &fakeSession{header: testHeader("new")}, nil
		}, func(session.Session) Runner { return &recordingRunner{} }, WithSessionBrowser(nil,
			func(context.Context, string) (SessionReplacement, error) {
				return SessionReplacement{}, errors.New("must not run")
			}))
		if err != nil {
			t.Fatal(err)
		}
		newDone := make(chan error, 1)
		go func() { newDone <- controller.NewSession() }()
		awaitSignal(t, entered, "new session start")
		if _, err := controller.ResumeSession(context.Background(), "/sessions/next.jsonl"); !errors.Is(err, ErrPromptActive) {
			t.Fatalf("ResumeSession() error = %v, want ErrPromptActive", err)
		}
		close(release)
		if err := awaitError(t, newDone, "new session"); err != nil {
			t.Fatal(err)
		}
	})

	t.Run("resume active", func(t *testing.T) {
		entered, release := make(chan struct{}), make(chan struct{})
		controller := newControllerWithRunnerAndBrowser(t, &fakeSession{header: testHeader("old")}, &recordingRunner{}, nil,
			func(context.Context, string) (SessionReplacement, error) {
				close(entered)
				<-release
				return SessionReplacement{Session: &fakeSession{header: testHeader("next")}, Runner: &recordingRunner{}}, nil
			})
		resumeDone := make(chan error, 1)
		go func() {
			_, err := controller.ResumeSession(context.Background(), "/sessions/next.jsonl")
			resumeDone <- err
		}()
		awaitSignal(t, entered, "resume start")
		if err := controller.Prompt(context.Background(), "prompt", nil); !errors.Is(err, ErrPromptActive) {
			t.Fatalf("Prompt() error = %v, want ErrPromptActive", err)
		}
		if err := controller.NewSession(); !errors.Is(err, ErrPromptActive) {
			t.Fatalf("NewSession() error = %v, want ErrPromptActive", err)
		}
		if _, err := controller.ResumeSession(context.Background(), "/sessions/another.jsonl"); !errors.Is(err, ErrPromptActive) {
			t.Fatalf("second ResumeSession() error = %v, want ErrPromptActive", err)
		}
		close(release)
		if err := awaitError(t, resumeDone, "resume"); err != nil {
			t.Fatal(err)
		}
	})
}

func TestControllerResumeCancellationCleansCandidateAndPreservesCurrent(t *testing.T) {
	t.Run("factory returns candidate and cancellation error", func(t *testing.T) {
		old := &fakeSession{header: testHeader("old")}
		candidate := &fakeSession{header: testHeader("next")}
		ctx, cancel := context.WithCancel(context.Background())
		controller := newControllerWithRunnerAndBrowser(t, old, &recordingRunner{}, nil,
			func(ctx context.Context, _ string) (SessionReplacement, error) {
				cancel()
				return SessionReplacement{Session: candidate}, ctx.Err()
			})
		if _, err := controller.ResumeSession(ctx, candidate.Path()); !errors.Is(err, context.Canceled) {
			t.Fatalf("ResumeSession() error = %v, want context.Canceled", err)
		}
		if candidate.CloseCalls() != 1 || old.CloseCalls() != 0 {
			t.Fatalf("close calls = candidate %d, old %d", candidate.CloseCalls(), old.CloseCalls())
		}
	})

	t.Run("canceled after complete candidate before old close", func(t *testing.T) {
		old := &fakeSession{header: testHeader("old")}
		candidate := &fakeSession{header: testHeader("next")}
		candidateRunner := &recordingRunner{}
		ctx, cancel := context.WithCancel(context.Background())
		controller := newControllerWithRunnerAndBrowser(t, old, &recordingRunner{}, nil,
			func(context.Context, string) (SessionReplacement, error) {
				cancel()
				return SessionReplacement{Session: candidate, Runner: candidateRunner}, nil
			})
		if _, err := controller.ResumeSession(ctx, candidate.Path()); !errors.Is(err, context.Canceled) {
			t.Fatalf("ResumeSession() error = %v, want context.Canceled", err)
		}
		if candidate.CloseCalls() != 1 || old.CloseCalls() != 0 {
			t.Fatalf("close calls = candidate %d, old %d", candidate.CloseCalls(), old.CloseCalls())
		}
		if candidateRunner.CloseCalls() != 1 {
			t.Fatalf("candidate runner close calls = %d, want 1", candidateRunner.CloseCalls())
		}
	})
}

func TestControllerResumeIgnoresCancellationAfterOldCloseBegins(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	old := &fakeSession{header: testHeader("old"), onClose: cancel}
	next := &fakeSession{header: testHeader("next")}
	controller := newControllerWithRunnerAndBrowser(t, old, &recordingRunner{}, nil,
		func(context.Context, string) (SessionReplacement, error) {
			return SessionReplacement{Session: next, Runner: &recordingRunner{}}, nil
		})

	if _, err := controller.ResumeSession(ctx, next.Path()); err != nil {
		t.Fatalf("ResumeSession() error = %v, want success after commit", err)
	}
	if ctx.Err() != context.Canceled || controller.Info().SessionID != "next" {
		t.Fatalf("context error = %v, info = %#v", ctx.Err(), controller.Info())
	}
}

func TestControllerResumeOldCloseFailureIsFatalAndClosesCandidateOnce(t *testing.T) {
	closeErr := errors.New("close old failed")
	old := &fakeSession{header: testHeader("old"), closeErr: closeErr}
	candidate := &fakeSession{header: testHeader("next"), closeErr: errors.New("close candidate failed")}
	controller := newControllerWithRunnerAndBrowser(t, old, &recordingRunner{}, nil,
		func(context.Context, string) (SessionReplacement, error) {
			return SessionReplacement{Session: candidate, Runner: &recordingRunner{}}, nil
		})

	if _, err := controller.ResumeSession(context.Background(), candidate.Path()); err != closeErr {
		t.Fatalf("ResumeSession() error = %v, want exact old close error", err)
	}
	if old.CloseCalls() != 1 || candidate.CloseCalls() != 1 {
		t.Fatalf("close calls = old %d, candidate %d", old.CloseCalls(), candidate.CloseCalls())
	}
	if err := controller.Prompt(context.Background(), "prompt", nil); !errors.Is(err, ErrClosed) {
		t.Fatalf("Prompt() error = %v, want ErrClosed", err)
	}
	if err := controller.Close(); err != closeErr {
		t.Fatalf("Close() error = %v, want exact old close error", err)
	}
	if candidate.CloseCalls() != 1 {
		t.Fatalf("candidate close calls after Close = %d, want 1", candidate.CloseCalls())
	}
}

func TestControllerResumeFactoryMaySynchronouslyCloseController(t *testing.T) {
	old := &fakeSession{header: testHeader("old"), messages: []model.Message{{ID: "m1", Role: model.RoleUser}}}
	candidate := &fakeSession{header: testHeader("next")}
	var controller *Controller
	var callbackErr error
	controller = newControllerWithRunnerAndBrowser(t, old, &recordingRunner{},
		func(context.Context, int) (session.ListResult, error) { return session.ListResult{}, nil },
		func(context.Context, string) (SessionReplacement, error) {
			if controller.Info().SessionID != "old" || len(controller.History()) != 1 {
				callbackErr = errors.New("factory could not inspect current state")
			}
			if _, err := controller.ListSessions(context.Background(), 20); err != nil {
				callbackErr = err
			}
			if err := controller.Close(); err != nil {
				callbackErr = err
			}
			return SessionReplacement{Session: candidate, Runner: &recordingRunner{}}, nil
		})

	done := make(chan error, 1)
	go func() {
		_, err := controller.ResumeSession(context.Background(), candidate.Path())
		done <- err
	}()
	if err := awaitError(t, done, "reentrant resume factory"); !errors.Is(err, ErrClosed) {
		t.Fatalf("ResumeSession() error = %v, want ErrClosed", err)
	}
	if callbackErr != nil {
		t.Fatal(callbackErr)
	}
	if old.CloseCalls() != 1 || candidate.CloseCalls() != 1 {
		t.Fatalf("close calls = old %d, candidate %d", old.CloseCalls(), candidate.CloseCalls())
	}
}

func TestControllerExternalCloseWaitsForReentrantFactoryCloseAndCandidateCleanup(t *testing.T) {
	factoryCloseDone := make(chan error, 1)
	releaseFactory := make(chan struct{})
	cleanupEntered := make(chan struct{})
	releaseCleanup := make(chan struct{})
	old := &fakeSession{header: testHeader("old")}
	candidate := &fakeSession{header: testHeader("next"), onClose: func() {
		close(cleanupEntered)
		<-releaseCleanup
	}}
	var controller *Controller
	controller = newControllerWithRunnerAndBrowser(t, old, &recordingRunner{}, nil,
		func(context.Context, string) (SessionReplacement, error) {
			factoryCloseDone <- controller.Close()
			<-releaseFactory
			return SessionReplacement{Session: candidate, Runner: &recordingRunner{}}, nil
		})

	resumeDone := make(chan error, 1)
	go func() {
		_, err := controller.ResumeSession(context.Background(), candidate.Path())
		resumeDone <- err
	}()
	if err := awaitError(t, factoryCloseDone, "reentrant factory close"); err != nil {
		t.Fatalf("reentrant factory Close() error = %v", err)
	}

	externalCloseDone := make(chan error, 1)
	go func() { externalCloseDone <- controller.Close() }()
	externalCloseErr, returnedBeforeFactory := pollError(externalCloseDone)
	externalCloseReceived := returnedBeforeFactory

	close(releaseFactory)
	awaitSignal(t, cleanupEntered, "candidate cleanup start")
	returnedBeforeCleanup := false
	if !externalCloseReceived {
		externalCloseErr, returnedBeforeCleanup = pollError(externalCloseDone)
		externalCloseReceived = returnedBeforeCleanup
	}
	close(releaseCleanup)

	if err := awaitError(t, resumeDone, "resume after reentrant factory close"); !errors.Is(err, ErrClosed) {
		t.Fatalf("ResumeSession() error = %v, want ErrClosed", err)
	}
	if !externalCloseReceived {
		externalCloseErr = awaitError(t, externalCloseDone, "external close after candidate cleanup")
	}
	if externalCloseErr != nil {
		t.Fatalf("external Close() error = %v", externalCloseErr)
	}
	if returnedBeforeFactory {
		t.Fatal("external Close() returned before the reentrant factory returned")
	}
	if returnedBeforeCleanup {
		t.Fatal("external Close() returned before candidate cleanup finished")
	}
	if old.CloseCalls() != 1 || candidate.CloseCalls() != 1 {
		t.Fatalf("close calls = old %d, candidate %d; want 1 each", old.CloseCalls(), candidate.CloseCalls())
	}
}

func TestControllerExternalCloseWaitsForReentrantCandidateCleanupClose(t *testing.T) {
	cleanupEntered := make(chan struct{})
	cleanupCloseDone := make(chan error, 1)
	releaseCleanup := make(chan struct{})
	old := &fakeSession{header: testHeader("old")}
	candidate := &fakeSession{header: testHeader("next")}
	var controller *Controller
	candidate.onClose = func() {
		close(cleanupEntered)
		cleanupCloseDone <- controller.Close()
		<-releaseCleanup
	}
	buildErr := errors.New("build failed after opening candidate")
	controller = newControllerWithRunnerAndBrowser(t, old, &recordingRunner{}, nil,
		func(context.Context, string) (SessionReplacement, error) {
			return SessionReplacement{Session: candidate}, buildErr
		})

	resumeDone := make(chan error, 1)
	go func() {
		_, err := controller.ResumeSession(context.Background(), candidate.Path())
		resumeDone <- err
	}()
	awaitSignal(t, cleanupEntered, "candidate cleanup callback start")
	if err := awaitError(t, cleanupCloseDone, "reentrant candidate cleanup close"); err != nil {
		t.Fatalf("reentrant candidate cleanup Close() error = %v", err)
	}

	externalCloseDone := make(chan error, 1)
	go func() { externalCloseDone <- controller.Close() }()
	externalCloseErr, returnedBeforeCleanup := pollError(externalCloseDone)
	close(releaseCleanup)

	if err := awaitError(t, resumeDone, "resume after reentrant candidate cleanup close"); err != buildErr {
		t.Fatalf("ResumeSession() error = %v, want exact build error", err)
	}
	if !returnedBeforeCleanup {
		externalCloseErr = awaitError(t, externalCloseDone, "external close after reentrant cleanup")
	}
	if externalCloseErr != nil {
		t.Fatalf("external Close() error = %v", externalCloseErr)
	}
	if returnedBeforeCleanup {
		t.Fatal("external Close() returned before reentrant candidate cleanup finished")
	}
	if old.CloseCalls() != 1 || candidate.CloseCalls() != 1 {
		t.Fatalf("close calls = old %d, candidate %d; want 1 each", old.CloseCalls(), candidate.CloseCalls())
	}
}

func TestControllerReplacementCallbackCloseIsScopedToReceiver(t *testing.T) {
	bFactoryEntered := make(chan struct{})
	releaseBFactory := make(chan struct{})
	bCleanupEntered := make(chan struct{})
	releaseBCleanup := make(chan struct{})
	bOld := &fakeSession{header: testHeader("b-old")}
	bCandidate := &fakeSession{header: testHeader("b-next"), onClose: func() {
		close(bCleanupEntered)
		<-releaseBCleanup
	}}
	controllerB := newControllerWithRunnerAndBrowser(t, bOld, &recordingRunner{}, nil,
		func(context.Context, string) (SessionReplacement, error) {
			close(bFactoryEntered)
			<-releaseBFactory
			return SessionReplacement{Session: bCandidate, Runner: &recordingRunner{}}, nil
		})
	bResumeDone := make(chan error, 1)
	go func() {
		_, err := controllerB.ResumeSession(context.Background(), bCandidate.Path())
		bResumeDone <- err
	}()
	awaitSignal(t, bFactoryEntered, "controller B factory start")

	bCloseReturned := make(chan error, 1)
	aFactoryEntered := make(chan struct{})
	aOld := &fakeSession{header: testHeader("a-old")}
	aCandidate := &fakeSession{header: testHeader("a-next")}
	controllerA := newControllerWithRunnerAndBrowser(t, aOld, &recordingRunner{}, nil,
		func(context.Context, string) (SessionReplacement, error) {
			close(aFactoryEntered)
			bCloseReturned <- controllerB.Close()
			return SessionReplacement{Session: aCandidate, Runner: &recordingRunner{}}, nil
		})
	aResumeDone := make(chan error, 1)
	go func() {
		_, err := controllerA.ResumeSession(context.Background(), aCandidate.Path())
		aResumeDone <- err
	}()
	awaitSignal(t, aFactoryEntered, "controller A factory start")

	bCloseErr, returnedBeforeFactory := pollError(bCloseReturned)
	bCloseReceived := returnedBeforeFactory
	close(releaseBFactory)
	awaitSignal(t, bCleanupEntered, "controller B candidate cleanup start")
	returnedBeforeCleanup := false
	if !bCloseReceived {
		bCloseErr, returnedBeforeCleanup = pollError(bCloseReturned)
		bCloseReceived = returnedBeforeCleanup
	}
	close(releaseBCleanup)

	if err := awaitError(t, bResumeDone, "controller B resume"); !errors.Is(err, ErrClosed) {
		t.Fatalf("controller B ResumeSession() error = %v, want ErrClosed", err)
	}
	if !bCloseReceived {
		bCloseErr = awaitError(t, bCloseReturned, "controller B close")
	}
	if bCloseErr != nil {
		t.Fatalf("controller B Close() error = %v", bCloseErr)
	}
	if err := awaitError(t, aResumeDone, "controller A resume"); err != nil {
		t.Fatalf("controller A ResumeSession() error = %v", err)
	}
	if returnedBeforeFactory {
		t.Fatal("controller B Close() returned before its factory finished")
	}
	if returnedBeforeCleanup {
		t.Fatal("controller B Close() returned before its candidate cleanup finished")
	}
	if bOld.CloseCalls() != 1 || bCandidate.CloseCalls() != 1 {
		t.Fatalf("controller B close calls = old %d, candidate %d; want 1 each", bOld.CloseCalls(), bCandidate.CloseCalls())
	}
	if err := controllerA.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestControllerResumeFactoryDeeplyWrappedCloseIsReentrant(t *testing.T) {
	old := &fakeSession{header: testHeader("old")}
	candidate := &fakeSession{header: testHeader("next")}
	var controller *Controller
	controller = newControllerWithRunnerAndBrowser(t, old, &recordingRunner{}, nil,
		func(context.Context, string) (SessionReplacement, error) {
			if err := closeControllerDeeply(controller, 128); err != nil {
				return SessionReplacement{}, err
			}
			return SessionReplacement{Session: candidate, Runner: &recordingRunner{}}, nil
		})

	resumeDone := make(chan error, 1)
	go func() {
		_, err := controller.ResumeSession(context.Background(), candidate.Path())
		resumeDone <- err
	}()

	var resumeErr error
	timedOut := false
	select {
	case resumeErr = <-resumeDone:
	case <-time.After(time.Second):
		timedOut = true
		controller.mu.Lock()
		controller.finishReplacingLocked(controller.replace)
		controller.mu.Unlock()
		resumeErr = awaitError(t, resumeDone, "resume after deadlock cleanup")
	}
	if timedOut {
		t.Fatal("ResumeSession() timed out; deeply wrapped synchronous Close deadlocked")
	}
	if !errors.Is(resumeErr, ErrClosed) {
		t.Fatalf("ResumeSession() error = %v, want ErrClosed", resumeErr)
	}
	if old.CloseCalls() != 1 || candidate.CloseCalls() != 1 {
		t.Fatalf("close calls = old %d, candidate %d; want 1 each", old.CloseCalls(), candidate.CloseCalls())
	}
}

func TestControllerCloseWaitsForResumeWithoutDeadlock(t *testing.T) {
	entered, release := make(chan struct{}), make(chan struct{})
	old := &fakeSession{header: testHeader("old")}
	candidate := &fakeSession{header: testHeader("next")}
	controller := newControllerWithRunnerAndBrowser(t, old, &recordingRunner{}, nil,
		func(context.Context, string) (SessionReplacement, error) {
			close(entered)
			<-release
			return SessionReplacement{Session: candidate, Runner: &recordingRunner{}}, nil
		})
	resumeDone := make(chan error, 1)
	go func() {
		_, err := controller.ResumeSession(context.Background(), candidate.Path())
		resumeDone <- err
	}()
	awaitSignal(t, entered, "resume factory start")

	closeDone := make(chan error, 1)
	go func() { closeDone <- controller.Close() }()
	awaitControllerClosed(t, controller)
	select {
	case err := <-closeDone:
		t.Fatalf("Close() returned before resume factory: %v", err)
	default:
	}
	close(release)
	if err := awaitError(t, resumeDone, "resume"); !errors.Is(err, ErrClosed) {
		t.Fatalf("ResumeSession() error = %v, want ErrClosed", err)
	}
	if err := awaitError(t, closeDone, "close"); err != nil {
		t.Fatalf("Close() error = %v", err)
	}
	if old.CloseCalls() != 1 || candidate.CloseCalls() != 1 {
		t.Fatalf("close calls = old %d, candidate %d", old.CloseCalls(), candidate.CloseCalls())
	}
}

func TestControllerCloseWaitsForResumeCandidateCleanup(t *testing.T) {
	entered, release := make(chan struct{}), make(chan struct{})
	old := &fakeSession{header: testHeader("old")}
	candidate := &fakeSession{header: testHeader("next"), onClose: func() {
		close(entered)
		<-release
	}}
	buildErr := errors.New("build failed after opening candidate")
	controller := newControllerWithRunnerAndBrowser(t, old, &recordingRunner{}, nil,
		func(context.Context, string) (SessionReplacement, error) {
			return SessionReplacement{Session: candidate}, buildErr
		})
	resumeDone := make(chan error, 1)
	go func() {
		_, err := controller.ResumeSession(context.Background(), candidate.Path())
		resumeDone <- err
	}()
	awaitSignal(t, entered, "candidate cleanup start")

	closeDone := make(chan error, 1)
	go func() { closeDone <- controller.Close() }()
	awaitControllerClosed(t, controller)
	select {
	case err := <-closeDone:
		t.Fatalf("Close() returned before candidate cleanup: %v", err)
	case <-time.After(20 * time.Millisecond):
	}
	close(release)
	if err := awaitError(t, resumeDone, "resume cleanup"); err != buildErr {
		t.Fatalf("ResumeSession() error = %v, want exact build error", err)
	}
	if err := awaitError(t, closeDone, "close after cleanup"); err != nil {
		t.Fatal(err)
	}
	if old.CloseCalls() != 1 || candidate.CloseCalls() != 1 {
		t.Fatalf("close calls = old %d, candidate %d", old.CloseCalls(), candidate.CloseCalls())
	}
}

func TestControllerResumeAndCloseAfterOldCloseStartsFinishOwnership(t *testing.T) {
	entered, release := make(chan struct{}), make(chan struct{})
	old := &fakeSession{header: testHeader("old"), onClose: func() {
		close(entered)
		<-release
	}}
	candidate := &fakeSession{header: testHeader("next")}
	controller := newControllerWithRunnerAndBrowser(t, old, &recordingRunner{}, nil,
		func(context.Context, string) (SessionReplacement, error) {
			return SessionReplacement{Session: candidate, Runner: &recordingRunner{}}, nil
		})
	resumeDone := make(chan error, 1)
	go func() {
		_, err := controller.ResumeSession(context.Background(), candidate.Path())
		resumeDone <- err
	}()
	awaitSignal(t, entered, "old close start")
	closeDone := make(chan error, 1)
	go func() { closeDone <- controller.Close() }()
	awaitControllerClosed(t, controller)
	close(release)
	if err := awaitError(t, resumeDone, "resume"); !errors.Is(err, ErrClosed) {
		t.Fatalf("ResumeSession() error = %v, want ErrClosed", err)
	}
	if err := awaitError(t, closeDone, "close"); err != nil {
		t.Fatal(err)
	}
	if old.CloseCalls() != 1 || candidate.CloseCalls() != 1 {
		t.Fatalf("close calls = old %d, candidate %d", old.CloseCalls(), candidate.CloseCalls())
	}
}

func TestControllerPromptCompactReplacementAndCloseLifecycleMatrix(t *testing.T) {
	tests := []struct {
		name  string
		start func(*Controller, context.Context, func(agent.Event)) <-chan error
	}{
		{
			name: "prompt active",
			start: func(controller *Controller, ctx context.Context, emit func(agent.Event)) <-chan error {
				done := make(chan error, 1)
				go func() { done <- controller.Prompt(ctx, "prompt", emit) }()
				return done
			},
		},
		{
			name: "compact active",
			start: func(controller *Controller, ctx context.Context, emit func(agent.Event)) <-chan error {
				done := make(chan error, 1)
				go func() {
					_, err := controller.Compact(ctx, "focus", emit)
					done <- err
				}()
				return done
			},
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			entered := make(chan struct{})
			release := make(chan struct{})
			runner := &lifecycleRunner{
				run: func(context.Context, string, func(agent.Event)) error {
					close(entered)
					<-release
					return nil
				},
				compact: func(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
					close(entered)
					<-release
					return agent.CompactionResult{CheckpointID: "checkpoint"}, nil
				},
			}
			current := &fakeSession{header: testHeader("current")}
			controller := newControllerWithRunnerAndBrowser(t, current, runner, nil,
				func(context.Context, string) (SessionReplacement, error) {
					return SessionReplacement{}, errors.New("must not run")
				})

			firstDone := test.start(controller, context.Background(), nil)
			awaitSignal(t, entered, test.name)
			if err := controller.Prompt(context.Background(), "second", nil); !errors.Is(err, ErrPromptActive) {
				t.Fatalf("Prompt() error = %v, want ErrPromptActive", err)
			}
			if _, err := controller.Compact(context.Background(), "second", nil); !errors.Is(err, ErrPromptActive) {
				t.Fatalf("Compact() error = %v, want ErrPromptActive", err)
			}
			if err := controller.NewSession(); !errors.Is(err, ErrPromptActive) {
				t.Fatalf("NewSession() error = %v, want ErrPromptActive", err)
			}
			if _, err := controller.ResumeSession(context.Background(), "/sessions/next.jsonl"); !errors.Is(err, ErrPromptActive) {
				t.Fatalf("ResumeSession() error = %v, want ErrPromptActive", err)
			}

			closeDone := make(chan error, 1)
			go func() { closeDone <- controller.Close() }()
			awaitControllerClosed(t, controller)
			select {
			case err := <-closeDone:
				t.Fatalf("Close() returned before active operation: %v", err)
			default:
			}
			close(release)
			if err := awaitError(t, firstDone, test.name); err != nil {
				t.Fatal(err)
			}
			if err := awaitError(t, closeDone, "close"); err != nil {
				t.Fatal(err)
			}
			if current.CloseCalls() != 1 {
				t.Fatalf("current close calls = %d, want 1", current.CloseCalls())
			}
		})
	}
}

func TestControllerReplacementRejectsPromptCompactNewAndResume(t *testing.T) {
	for _, replacementKind := range []string{"new", "resume"} {
		t.Run(replacementKind, func(t *testing.T) {
			entered := make(chan struct{})
			release := make(chan struct{})
			current := &fakeSession{header: testHeader("current")}
			candidate := &fakeSession{header: testHeader("candidate")}
			controller, err := New(current, func() (session.Session, error) {
				if replacementKind != "new" {
					return nil, errors.New("must not run")
				}
				close(entered)
				<-release
				return candidate, nil
			}, func(session.Session) Runner { return &recordingRunner{} }, WithSessionBrowser(nil,
				func(context.Context, string) (SessionReplacement, error) {
					if replacementKind != "resume" {
						return SessionReplacement{}, errors.New("must not run")
					}
					close(entered)
					<-release
					return SessionReplacement{Session: candidate, Runner: &recordingRunner{}}, nil
				}))
			if err != nil {
				t.Fatal(err)
			}

			replacementDone := make(chan error, 1)
			if replacementKind == "new" {
				go func() { replacementDone <- controller.NewSession() }()
			} else {
				go func() {
					_, resumeErr := controller.ResumeSession(context.Background(), candidate.Path())
					replacementDone <- resumeErr
				}()
			}
			awaitSignal(t, entered, replacementKind+" replacement")

			if err := controller.Prompt(context.Background(), "prompt", nil); !errors.Is(err, ErrPromptActive) {
				t.Fatalf("Prompt() error = %v, want ErrPromptActive", err)
			}
			if _, err := controller.Compact(context.Background(), "focus", nil); !errors.Is(err, ErrPromptActive) {
				t.Fatalf("Compact() error = %v, want ErrPromptActive", err)
			}
			if err := controller.NewSession(); !errors.Is(err, ErrPromptActive) {
				t.Fatalf("NewSession() error = %v, want ErrPromptActive", err)
			}
			if _, err := controller.ResumeSession(context.Background(), "/sessions/other.jsonl"); !errors.Is(err, ErrPromptActive) {
				t.Fatalf("ResumeSession() error = %v, want ErrPromptActive", err)
			}

			closeDone := make(chan error, 1)
			go func() { closeDone <- controller.Close() }()
			awaitControllerClosed(t, controller)
			select {
			case closeErr := <-closeDone:
				t.Fatalf("Close() returned before replacement: %v", closeErr)
			default:
			}
			close(release)
			if replacementErr := awaitError(t, replacementDone, replacementKind); !errors.Is(replacementErr, ErrClosed) {
				t.Fatalf("replacement error = %v, want ErrClosed", replacementErr)
			}
			if closeErr := awaitError(t, closeDone, "close"); closeErr != nil {
				t.Fatal(closeErr)
			}
			if current.CloseCalls() != 1 || candidate.CloseCalls() != 1 {
				t.Fatalf("close calls = current %d candidate %d, want 1 each", current.CloseCalls(), candidate.CloseCalls())
			}
		})
	}
}

func TestControllerOperationCancellationReleasesLifecycle(t *testing.T) {
	for _, operation := range []string{"prompt", "compact"} {
		t.Run(operation, func(t *testing.T) {
			entered := make(chan struct{})
			runner := &lifecycleRunner{
				run: func(ctx context.Context, _ string, _ func(agent.Event)) error {
					close(entered)
					<-ctx.Done()
					return ctx.Err()
				},
				compact: func(ctx context.Context, _ string, _ func(agent.Event)) (agent.CompactionResult, error) {
					close(entered)
					<-ctx.Done()
					return agent.CompactionResult{}, ctx.Err()
				},
			}
			controller := newTestController(t, runner)
			ctx, cancel := context.WithCancel(context.Background())
			done := make(chan error, 1)
			if operation == "prompt" {
				go func() { done <- controller.Prompt(ctx, "prompt", nil) }()
			} else {
				go func() {
					_, err := controller.Compact(ctx, "focus", nil)
					done <- err
				}()
			}
			awaitSignal(t, entered, operation)
			cancel()
			if err := awaitError(t, done, operation); !errors.Is(err, context.Canceled) {
				t.Fatalf("operation error = %v, want context.Canceled", err)
			}
			if err := controller.NewSession(); err != nil {
				t.Fatalf("NewSession() after cancellation = %v", err)
			}
			if err := controller.Close(); err != nil {
				t.Fatal(err)
			}
		})
	}
}

func TestControllerZeroOwnerCloseFromPromptAndCompactCallbacksDoesNotDeadlock(t *testing.T) {
	for _, operation := range []string{"prompt", "compact"} {
		t.Run(operation, func(t *testing.T) {
			current := &fakeSession{header: testHeader(operation)}
			var controller *Controller
			runner := &lifecycleRunner{
				run: func(_ context.Context, _ string, emit func(agent.Event)) error {
					emit(agent.Event{Type: agent.EventAgentStarted})
					return nil
				},
				compact: func(_ context.Context, _ string, emit func(agent.Event)) (agent.CompactionResult, error) {
					emit(agent.Event{Type: agent.EventCompactionStarted})
					return agent.CompactionResult{CheckpointID: "checkpoint"}, nil
				},
			}
			var err error
			controller, err = New(current, func() (session.Session, error) {
				return &fakeSession{header: testHeader("next")}, nil
			}, func(session.Session) Runner { return runner })
			if err != nil {
				t.Fatal(err)
			}
			controller.ownerIDSource = func() uint64 { return 0 }

			var callbackErr error
			done := make(chan error, 1)
			go func() {
				emit := func(agent.Event) { callbackErr = controller.Close() }
				if operation == "prompt" {
					done <- controller.Prompt(context.Background(), "prompt", emit)
					return
				}
				_, compactErr := controller.Compact(context.Background(), "focus", emit)
				done <- compactErr
			}()

			if operationErr := awaitError(t, done, operation+" with zero owner IDs"); operationErr != nil {
				t.Fatal(operationErr)
			}
			if callbackErr != nil {
				t.Fatalf("callback Close() error = %v", callbackErr)
			}
			if current.CloseCalls() != 1 {
				t.Fatalf("close calls = %d, want 1", current.CloseCalls())
			}
			if err := controller.Close(); err != nil {
				t.Fatal(err)
			}
			if current.CloseCalls() != 1 {
				t.Fatalf("close calls after idempotent Close = %d, want 1", current.CloseCalls())
			}
		})
	}
}

func TestControllerZeroOwnerCloseOutsideCallbackWaits(t *testing.T) {
	entered := make(chan struct{})
	release := make(chan struct{})
	current := &fakeSession{header: testHeader("zero-owner-external")}
	controller, err := New(current, func() (session.Session, error) {
		return nil, errors.New("unused")
	}, func(session.Session) Runner {
		return &lifecycleRunner{run: func(context.Context, string, func(agent.Event)) error {
			close(entered)
			<-release
			return nil
		}}
	})
	if err != nil {
		t.Fatal(err)
	}
	controller.ownerIDSource = func() uint64 { return 0 }

	promptDone := make(chan error, 1)
	go func() { promptDone <- controller.Prompt(context.Background(), "prompt", nil) }()
	awaitSignal(t, entered, "zero-owner prompt")
	closeDone := make(chan error, 1)
	go func() { closeDone <- controller.Close() }()
	select {
	case closeErr := <-closeDone:
		t.Fatalf("zero-owner Close() outside callback returned early: %v", closeErr)
	case <-time.After(20 * time.Millisecond):
	}
	close(release)
	if err := awaitError(t, promptDone, "zero-owner prompt"); err != nil {
		t.Fatal(err)
	}
	if err := awaitError(t, closeDone, "zero-owner external close"); err != nil {
		t.Fatal(err)
	}
	if current.CloseCalls() != 1 {
		t.Fatalf("close calls = %d, want 1", current.CloseCalls())
	}
}

func TestControllerExternalCloseWithDistinctOwnerIDWaits(t *testing.T) {
	entered := make(chan struct{})
	release := make(chan struct{})
	current := &fakeSession{header: testHeader("normal-owner-external")}
	controller, err := New(current, func() (session.Session, error) {
		return nil, errors.New("unused")
	}, func(session.Session) Runner {
		return &lifecycleRunner{run: func(context.Context, string, func(agent.Event)) error {
			close(entered)
			<-release
			return nil
		}}
	})
	if err != nil {
		t.Fatal(err)
	}
	var ownerCalls uint64
	controller.ownerIDSource = func() uint64 {
		ownerCalls++
		if ownerCalls == 1 {
			return 101
		}
		return 202
	}

	promptDone := make(chan error, 1)
	go func() { promptDone <- controller.Prompt(context.Background(), "prompt", nil) }()
	awaitSignal(t, entered, "normal-owner prompt")
	closeDone := make(chan error, 1)
	go func() { closeDone <- controller.Close() }()
	select {
	case closeErr := <-closeDone:
		t.Fatalf("external Close() returned early: %v", closeErr)
	case <-time.After(20 * time.Millisecond):
	}
	close(release)
	if err := awaitError(t, promptDone, "normal-owner prompt"); err != nil {
		t.Fatal(err)
	}
	if err := awaitError(t, closeDone, "normal-owner external close"); err != nil {
		t.Fatal(err)
	}
	if current.CloseCalls() != 1 {
		t.Fatalf("close calls = %d, want 1", current.CloseCalls())
	}
}

func TestControllerZeroOwnerCloseFromReplacementBuildDoesNotDeadlock(t *testing.T) {
	current := &fakeSession{header: testHeader("current")}
	candidate := &fakeSession{header: testHeader("candidate")}
	var controller *Controller
	var callbackErr error
	var err error
	controller, err = New(current, func() (session.Session, error) {
		return nil, errors.New("legacy factory must not run")
	}, func(session.Session) Runner { return &recordingRunner{} },
		WithNewSessionBuilder(func(context.Context, RuntimeInfo) (SessionReplacement, error) {
			callbackErr = controller.Close()
			return SessionReplacement{Session: candidate, Runner: &recordingRunner{}}, nil
		}))
	if err != nil {
		t.Fatal(err)
	}
	controller.ownerIDSource = func() uint64 { return 0 }

	done := make(chan error, 1)
	go func() { done <- controller.NewSession() }()
	if replacementErr := awaitError(t, done, "zero-owner replacement build"); !errors.Is(replacementErr, ErrClosed) {
		t.Fatalf("NewSession() error = %v, want ErrClosed", replacementErr)
	}
	if callbackErr != nil {
		t.Fatalf("callback Close() error = %v", callbackErr)
	}
	if current.CloseCalls() != 1 || candidate.CloseCalls() != 1 {
		t.Fatalf("close calls = current %d candidate %d, want 1 each", current.CloseCalls(), candidate.CloseCalls())
	}
	if err := controller.Close(); err != nil {
		t.Fatal(err)
	}
	if current.CloseCalls() != 1 || candidate.CloseCalls() != 1 {
		t.Fatalf("close calls after idempotent Close = current %d candidate %d, want 1 each", current.CloseCalls(), candidate.CloseCalls())
	}
}

func TestControllerCloseFromPromptAndCompactionCallbacksDoesNotDeadlock(t *testing.T) {
	for _, operation := range []string{"prompt", "compact"} {
		t.Run(operation, func(t *testing.T) {
			current := &fakeSession{header: testHeader(operation)}
			var controller *Controller
			runner := &lifecycleRunner{
				run: func(_ context.Context, _ string, emit func(agent.Event)) error {
					emit(agent.Event{Type: agent.EventAgentStarted})
					return nil
				},
				compact: func(_ context.Context, _ string, emit func(agent.Event)) (agent.CompactionResult, error) {
					emit(agent.Event{Type: agent.EventCompactionStarted})
					return agent.CompactionResult{CheckpointID: "checkpoint"}, nil
				},
			}
			var err error
			controller, err = New(current, func() (session.Session, error) {
				return &fakeSession{header: testHeader("next")}, nil
			}, func(session.Session) Runner { return runner })
			if err != nil {
				t.Fatal(err)
			}

			done := make(chan error, 1)
			emit := func(agent.Event) { done <- controller.Close() }
			operationDone := make(chan error, 1)
			go func() {
				if operation == "prompt" {
					operationDone <- controller.Prompt(context.Background(), "prompt", emit)
					return
				}
				_, compactErr := controller.Compact(context.Background(), "focus", emit)
				operationDone <- compactErr
			}()
			if closeErr := awaitError(t, done, "reentrant close"); closeErr != nil {
				t.Fatal(closeErr)
			}
			if operationErr := awaitError(t, operationDone, operation); operationErr != nil {
				t.Fatal(operationErr)
			}
			if current.CloseCalls() != 1 {
				t.Fatalf("close calls = %d, want 1", current.CloseCalls())
			}
			if err := controller.Close(); err != nil {
				t.Fatal(err)
			}
		})
	}
}

func TestControllerExternalCloseWaitsForReentrantOperationClose(t *testing.T) {
	for _, operation := range []string{"prompt", "compact"} {
		t.Run(operation, func(t *testing.T) {
			reentrantDone := make(chan error, 1)
			emitted := make(chan struct{})
			release := make(chan struct{})
			current := &fakeSession{header: testHeader(operation)}
			var controller *Controller
			runner := &lifecycleRunner{
				run: func(_ context.Context, _ string, emit func(agent.Event)) error {
					emit(agent.Event{Type: agent.EventAgentStarted})
					close(emitted)
					<-release
					return nil
				},
				compact: func(_ context.Context, _ string, emit func(agent.Event)) (agent.CompactionResult, error) {
					emit(agent.Event{Type: agent.EventCompactionStarted})
					close(emitted)
					<-release
					return agent.CompactionResult{}, nil
				},
			}
			var err error
			controller, err = New(current, func() (session.Session, error) { return nil, errors.New("unused") }, func(session.Session) Runner { return runner })
			if err != nil {
				t.Fatal(err)
			}
			emit := func(agent.Event) { reentrantDone <- controller.Close() }
			operationDone := make(chan error, 1)
			go func() {
				if operation == "prompt" {
					operationDone <- controller.Prompt(context.Background(), "prompt", emit)
				} else {
					_, compactErr := controller.Compact(context.Background(), "focus", emit)
					operationDone <- compactErr
				}
			}()
			awaitSignal(t, emitted, operation+" event")
			if closeErr := awaitError(t, reentrantDone, "reentrant close"); closeErr != nil {
				t.Fatal(closeErr)
			}
			externalDone := make(chan error, 1)
			go func() { externalDone <- controller.Close() }()
			select {
			case closeErr := <-externalDone:
				t.Fatalf("external Close() returned before operation finalization: %v", closeErr)
			case <-time.After(20 * time.Millisecond):
			}
			close(release)
			if operationErr := awaitError(t, operationDone, operation); operationErr != nil {
				t.Fatal(operationErr)
			}
			if closeErr := awaitError(t, externalDone, "external close"); closeErr != nil {
				t.Fatal(closeErr)
			}
			if current.CloseCalls() != 1 {
				t.Fatalf("close calls = %d, want 1", current.CloseCalls())
			}
		})
	}
}

func TestControllerCompactForwardsResultEventsAndErrorIdentity(t *testing.T) {
	compactErr := errors.New("compact failed")
	wantResult := agent.CompactionResult{CheckpointID: "checkpoint", TokensBefore: 42}
	wantEvents := []agent.Event{{Type: agent.EventCompactionStarted}, {Type: agent.EventCompactionCompleted}}
	controller := newTestController(t, &lifecycleRunner{compact: func(_ context.Context, focus string, emit func(agent.Event)) (agent.CompactionResult, error) {
		if focus != "focus" {
			t.Fatalf("focus = %q, want focus", focus)
		}
		for _, event := range wantEvents {
			emit(event)
		}
		return wantResult, compactErr
	}})
	var gotEvents []agent.Event
	gotResult, err := controller.Compact(context.Background(), "focus", func(event agent.Event) { gotEvents = append(gotEvents, event) })
	if err != compactErr || gotResult != wantResult {
		t.Fatalf("Compact() = %#v, %v; want %#v, exact error", gotResult, err, wantResult)
	}
	if !reflect.DeepEqual(gotEvents, wantEvents) {
		t.Fatalf("events = %#v, want %#v", gotEvents, wantEvents)
	}
}

func TestControllerNewSessionBuilderUsesRuntimeAfterResumeAndCommitsReadyReplacement(t *testing.T) {
	initial := &fakeSession{header: testHeader("initial")}
	resumed := &fakeSession{header: testHeader("resumed")}
	fresh := &fakeSession{header: testHeader("fresh")}
	initialRunner := &recordingRunner{}
	resumedRunner := &recordingRunner{}
	freshRunner := &recordingRunner{}
	resumedRuntime := RuntimeInfo{Provider: "openai-compatible", Profile: "resumed", Model: "resumed-model", ContextWindow: 65_536}
	var gotRuntime RuntimeInfo
	legacyCreateCalls := 0
	legacyBuildCalls := 0
	controller, err := New(initial, func() (session.Session, error) {
		legacyCreateCalls++
		return nil, errors.New("legacy create must not run")
	}, func(session.Session) Runner {
		legacyBuildCalls++
		return initialRunner
	}, WithRuntimeInfo(RuntimeInfo{Provider: "openai-compatible", Profile: "startup", Model: "startup-model", ContextWindow: 128_000}),
		WithSessionBrowser(nil, func(context.Context, string) (SessionReplacement, error) {
			return SessionReplacement{Session: resumed, Runner: resumedRunner, RuntimeInfo: resumedRuntime}, nil
		}),
		WithNewSessionBuilder(func(_ context.Context, current RuntimeInfo) (SessionReplacement, error) {
			gotRuntime = current
			return SessionReplacement{Session: fresh, Runner: freshRunner, RuntimeInfo: current}, nil
		}))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := controller.ResumeSession(context.Background(), resumed.Path()); err != nil {
		t.Fatal(err)
	}
	if err := controller.NewSession(); err != nil {
		t.Fatal(err)
	}
	if gotRuntime != resumedRuntime {
		t.Fatalf("new-session runtime = %#v, want resumed %#v", gotRuntime, resumedRuntime)
	}
	if legacyCreateCalls != 0 || legacyBuildCalls != 1 {
		t.Fatalf("legacy calls = create %d build %d, want 0 and initial build only", legacyCreateCalls, legacyBuildCalls)
	}
	if got := controller.Info(); got.SessionID != "fresh" || got.Profile != "resumed" || got.Model != "resumed-model" {
		t.Fatalf("Info() after new = %#v", got)
	} else {
		assertInfoContext(t, got, 65_536, 0, false, false)
	}
	if err := controller.Prompt(context.Background(), "fresh", nil); err != nil {
		t.Fatal(err)
	}
	if initialRunner.Calls() != 0 || resumedRunner.Calls() != 0 || freshRunner.Calls() != 1 {
		t.Fatalf("runner calls = initial %d resumed %d fresh %d", initialRunner.Calls(), resumedRunner.Calls(), freshRunner.Calls())
	}
}

func TestControllerClosedMethodIdentitiesIncludeCompactAndResume(t *testing.T) {
	controller := newTestController(t, &recordingRunner{})
	if err := controller.Close(); err != nil {
		t.Fatal(err)
	}
	if err := controller.Prompt(context.Background(), "prompt", nil); !errors.Is(err, ErrClosed) {
		t.Fatalf("Prompt() error = %v, want ErrClosed", err)
	}
	if _, err := controller.Compact(context.Background(), "focus", nil); !errors.Is(err, ErrClosed) {
		t.Fatalf("Compact() error = %v, want ErrClosed", err)
	}
	if err := controller.NewSession(); !errors.Is(err, ErrClosed) {
		t.Fatalf("NewSession() error = %v, want ErrClosed", err)
	}
	if _, err := controller.ResumeSession(context.Background(), "/sessions/next.jsonl"); !errors.Is(err, ErrClosed) {
		t.Fatalf("ResumeSession() error = %v, want ErrClosed", err)
	}
	if err := controller.Close(); err != nil {
		t.Fatalf("second Close() error = %v", err)
	}
}

func TestControllerResumeAndCloseRaceDoesNotLeakOrDoubleClose(t *testing.T) {
	for i := 0; i < 100; i++ {
		old := &fakeSession{header: testHeader(fmt.Sprintf("old-%d", i))}
		candidate := &fakeSession{header: testHeader(fmt.Sprintf("next-%d", i))}
		entered, release := make(chan struct{}), make(chan struct{})
		controller := newControllerWithRunnerAndBrowser(t, old, &recordingRunner{}, nil,
			func(context.Context, string) (SessionReplacement, error) {
				close(entered)
				<-release
				return SessionReplacement{Session: candidate, Runner: &recordingRunner{}}, nil
			})
		resumeDone := make(chan error, 1)
		go func() {
			_, err := controller.ResumeSession(context.Background(), candidate.Path())
			resumeDone <- err
		}()
		awaitSignal(t, entered, "race resume start")
		closeDone := make(chan error, 1)
		go func() { closeDone <- controller.Close() }()
		awaitControllerClosed(t, controller)
		close(release)
		if err := awaitError(t, resumeDone, "race resume"); !errors.Is(err, ErrClosed) {
			t.Fatalf("iteration %d ResumeSession() error = %v, want ErrClosed", i, err)
		}
		if err := awaitError(t, closeDone, "race close"); err != nil {
			t.Fatalf("iteration %d Close() error = %v", i, err)
		}
		if old.CloseCalls() != 1 || candidate.CloseCalls() != 1 {
			t.Fatalf("iteration %d close calls = old %d, candidate %d", i, old.CloseCalls(), candidate.CloseCalls())
		}
	}
}

type runnerFunc func(context.Context, string, func(agent.Event)) error

func (f runnerFunc) Run(ctx context.Context, text string, emit func(agent.Event)) error {
	return f(ctx, text, emit)
}

func (f runnerFunc) Compact(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
	return agent.CompactionResult{Noop: true}, nil
}

func noopRun(context.Context, string, func(agent.Event)) error { return nil }

// lifecycleRunner gives lifecycle tests independent blocking prompt and compact
// entry points while remaining a complete app.Runner test double.
type lifecycleRunner struct {
	run     func(context.Context, string, func(agent.Event)) error
	compact func(context.Context, string, func(agent.Event)) (agent.CompactionResult, error)

	mu         sync.Mutex
	closeCalls int
}

func (r *lifecycleRunner) Run(ctx context.Context, text string, emit func(agent.Event)) error {
	if r.run == nil {
		return nil
	}
	return r.run(ctx, text, emit)
}

func (r *lifecycleRunner) Compact(ctx context.Context, focus string, emit func(agent.Event)) (agent.CompactionResult, error) {
	if r.compact == nil {
		return agent.CompactionResult{Noop: true}, nil
	}
	return r.compact(ctx, focus, emit)
}

func (r *lifecycleRunner) Close() error {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.closeCalls++
	return nil
}

func (r *lifecycleRunner) CloseCalls() int {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.closeCalls
}

func testHeader(id string) session.Header {
	return session.Header{Version: 1, ID: id, Workspace: "/workspace", Provider: "openai-compatible", Profile: "test", Model: "model", CreatedAt: time.Unix(1, 0).UTC()}
}

// fakeMemoryManager is a minimal memory.Manager test double: it records the
// last request passed to each of the methods Controller exposes and returns
// canned results/errors.
type fakeMemoryManager struct {
	getRequest memory.RecordRef
	getResult  memory.Record
	getErr     error

	searchRequest memory.SearchRequest
	searchResult  memory.SearchResult
	searchErr     error

	rememberRequest memory.RememberRequest
	rememberResult  memory.Record
	rememberErr     error

	forgetRequest memory.ForgetRequest
	forgetResult  memory.ForgetResult
	forgetErr     error

	reviewRequest memory.ReviewRequest
	reviewResult  memory.ReviewResult
	reviewErr     error
}

func (m *fakeMemoryManager) Get(_ context.Context, ref memory.RecordRef) (memory.Record, error) {
	m.getRequest = ref
	return m.getResult, m.getErr
}

func (m *fakeMemoryManager) GetByKey(context.Context, memory.RecordKey) (memory.Record, error) {
	return memory.Record{}, nil
}

func (m *fakeMemoryManager) GetTombstone(context.Context, memory.RecordRef) (memory.Tombstone, error) {
	return memory.Tombstone{}, nil
}

func (m *fakeMemoryManager) GetCandidate(context.Context, memory.CandidateRef) (memory.Candidate, error) {
	return memory.Candidate{}, nil
}

func (m *fakeMemoryManager) Search(_ context.Context, request memory.SearchRequest) (memory.SearchResult, error) {
	m.searchRequest = request
	return m.searchResult, m.searchErr
}

func (m *fakeMemoryManager) Remember(_ context.Context, request memory.RememberRequest) (memory.Record, error) {
	m.rememberRequest = request
	return m.rememberResult, m.rememberErr
}

func (m *fakeMemoryManager) Forget(_ context.Context, request memory.ForgetRequest) (memory.ForgetResult, error) {
	m.forgetRequest = request
	return m.forgetResult, m.forgetErr
}

func (m *fakeMemoryManager) Review(_ context.Context, request memory.ReviewRequest) (memory.ReviewResult, error) {
	m.reviewRequest = request
	return m.reviewResult, m.reviewErr
}

type recordingRunner struct {
	mu         sync.Mutex
	calls      int
	closeCalls int
}

func (r *recordingRunner) Run(context.Context, string, func(agent.Event)) error {
	r.mu.Lock()
	r.calls++
	r.mu.Unlock()
	return nil
}

func (r *recordingRunner) Compact(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
	r.mu.Lock()
	r.calls++
	r.mu.Unlock()
	return agent.CompactionResult{}, nil
}

func (r *recordingRunner) Calls() int {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.calls
}

func (r *recordingRunner) Close() error {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.closeCalls++
	return nil
}

func (r *recordingRunner) CloseCalls() int {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.closeCalls
}

type aggregateUsageSession struct {
	session.Session
	usage   model.Usage
	present bool
}

func (s *aggregateUsageSession) AggregateUsage() (model.Usage, bool) {
	return s.usage, s.present
}

type snapshotSession struct {
	session.Session
	snapshot session.Snapshot
}

func (s *snapshotSession) Snapshot() session.Snapshot {
	return s.snapshot
}

type fakeSession struct {
	mu         sync.Mutex
	header     session.Header
	messages   []model.Message
	path       string
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

func (f *fakeSession) AppendCompaction(context.Context, session.CompactionCheckpoint) (session.CompactionMetadata, error) {
	return session.CompactionMetadata{}, nil
}

func (f *fakeSession) LatestCompaction() (session.CompactionMetadata, bool) {
	return session.CompactionMetadata{}, false
}

func (f *fakeSession) Path() string {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.path != "" {
		return f.path
	}
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

func assertInfoContext(t *testing.T, info Info, wantWindow, wantInput int, wantPresent, wantPending bool) {
	t.Helper()
	value := reflect.ValueOf(info)
	window := value.FieldByName("ContextWindow")
	if !window.IsValid() || int(window.Int()) != wantWindow {
		t.Fatalf("Info().ContextWindow = %v, want %d", window, wantWindow)
	}
	input := value.FieldByName("ContextInputTokens")
	if !input.IsValid() || int(input.Int()) != wantInput {
		t.Fatalf("Info().ContextInputTokens = %v, want %d", input, wantInput)
	}
	present := value.FieldByName("ContextInputTokensPresent")
	if !present.IsValid() || present.Bool() != wantPresent {
		t.Fatalf("Info().ContextInputTokensPresent = %v, want %v", present, wantPresent)
	}
	pending := value.FieldByName("ContextInputTokensPending")
	if !pending.IsValid() || pending.Bool() != wantPending {
		t.Fatalf("Info().ContextInputTokensPending = %v, want %v", pending, wantPending)
	}
}

func setRuntimeContextWindow(t *testing.T, runtime *RuntimeInfo, value int) {
	t.Helper()
	field := reflect.ValueOf(runtime).Elem().FieldByName("ContextWindow")
	if !field.IsValid() {
		t.Fatal("RuntimeInfo.ContextWindow is missing")
	}
	field.SetInt(int64(value))
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

func newControllerWithRunnerAndBrowser(t *testing.T, initial session.Session, runner Runner, list SessionLister, resume ResumeFactory) *Controller {
	t.Helper()
	controller, err := New(initial, func() (session.Session, error) {
		return &fakeSession{header: testHeader("new")}, nil
	}, func(session.Session) Runner { return runner },
		WithRuntimeInfo(RuntimeInfo{Provider: "openai-compatible", Profile: "old", Model: "old-model"}),
		WithSessionBrowser(list, resume))
	if err != nil {
		t.Fatal(err)
	}
	return controller
}

//go:noinline
func closeControllerDeeply(controller *Controller, depth int) error {
	if depth == 0 {
		return controller.Close()
	}
	return closeControllerDeeply(controller, depth-1)
}

func pollError(result <-chan error) (error, bool) {
	select {
	case err := <-result:
		return err, true
	case <-time.After(20 * time.Millisecond):
		return nil, false
	}
}

func awaitSignal(t *testing.T, signal <-chan struct{}, operation string) {
	t.Helper()
	select {
	case <-signal:
	case <-time.After(time.Second):
		t.Fatalf("%s timed out", operation)
	}
}

func awaitError(t *testing.T, result <-chan error, operation string) error {
	t.Helper()
	select {
	case err := <-result:
		return err
	case <-time.After(time.Second):
		t.Fatalf("%s timed out", operation)
		return nil
	}
}

func awaitControllerClosed(t *testing.T, controller *Controller) {
	t.Helper()
	deadline := time.Now().Add(time.Second)
	for time.Now().Before(deadline) {
		controller.mu.Lock()
		closed := controller.closed
		controller.mu.Unlock()
		if closed {
			return
		}
		time.Sleep(time.Millisecond)
	}
	t.Fatal("controller did not start closing")
}
