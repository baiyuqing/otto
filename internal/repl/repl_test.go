package repl

import (
	"bytes"
	"context"
	"errors"
	"io"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
)

func TestREPLRunsPromptAndRendersEvents(t *testing.T) {
	input := strings.NewReader("inspect files\n/exit\n")
	var output bytes.Buffer
	backend := &fakeBackend{
		info: app.Info{SessionID: "session-1", SessionPath: "/tmp/session.jsonl", Provider: "openai-compatible", Model: "test"},
		prompt: func(_ context.Context, prompt string, emit func(agent.Event)) error {
			if prompt != "inspect files" {
				t.Fatalf("prompt = %q", prompt)
			}
			emit(agent.Event{Type: agent.EventTextDelta, Text: "done"})
			emit(agent.Event{Type: agent.EventToolCallStarted, ToolName: "read", ToolCallID: "call-1"})
			emit(agent.Event{Type: agent.EventToolCallFinished, ToolName: "read", ToolCallID: "call-1", ToolResult: tool.Result{Content: "read README.md\nfull output must not render"}})
			return nil
		},
	}
	r := New(input, &output, &output, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	rendered := output.String()
	for _, expected := range []string{"done", "[tool] read (call-1)", "[tool result] read README.md", "session-1"} {
		if !strings.Contains(rendered, expected) {
			t.Fatalf("output missing %q: %s", expected, rendered)
		}
	}
	if strings.Contains(rendered, "full output must not render") {
		t.Fatalf("tool result dumped more than its summary line: %s", rendered)
	}
}

func TestRunOnceRunsSinglePromptWithoutBannerOrPrompts(t *testing.T) {
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{
		info: app.Info{SessionID: "session-1"},
		prompt: func(_ context.Context, prompt string, emit func(agent.Event)) error {
			if prompt != "line one\nline two" {
				t.Fatalf("prompt = %q", prompt)
			}
			emit(agent.Event{Type: agent.EventTextDelta, Text: "done"})
			return nil
		},
	}
	r := New(strings.NewReader(""), &stdout, &stderr, backend)
	if err := r.RunOnce(context.Background(), "line one\nline two"); err != nil {
		t.Fatal(err)
	}
	rendered := stdout.String()
	if !strings.Contains(rendered, "done") || strings.Contains(rendered, "> ") || strings.Contains(rendered, "session-1") {
		t.Fatalf("stdout = %q", rendered)
	}
}

func TestRunOnceReturnsPromptErrorAfterRenderingItOnce(t *testing.T) {
	providerErr := errors.New("provider unavailable")
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{prompt: func(_ context.Context, _ string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventAgentError, Err: providerErr})
		return providerErr
	}}
	r := New(strings.NewReader(""), &stdout, &stderr, backend)
	if err := r.RunOnce(context.Background(), "boom"); !errors.Is(err, providerErr) {
		t.Fatalf("err = %v, want provider error", err)
	}
	if strings.Count(stderr.String(), providerErr.Error()) != 1 {
		t.Fatalf("stderr should contain one provider error: %q", stderr.String())
	}
}

func TestREPLCommandsAndInputHandling(t *testing.T) {
	var stdout, stderr bytes.Buffer
	var prompts []string
	backend := &fakeBackend{
		info: app.Info{SessionID: "s-1", SessionPath: "/sessions/s-1.jsonl", Provider: "openai-compatible", Model: "m-1"},
		prompt: func(_ context.Context, prompt string, _ func(agent.Event)) error {
			prompts = append(prompts, prompt)
			return nil
		},
	}
	input := strings.NewReader("\n/help\n/session\n/unknown\nhello\n/exit\n")
	r := New(input, &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if len(prompts) != 1 || prompts[0] != "hello" {
		t.Fatalf("runner prompts = %q, want [hello]", prompts)
	}
	for _, expected := range []string{"/help", "/exit", "/new", "/session", "/compact [focus]", "s-1", "/sessions/s-1.jsonl", "openai-compatible", "m-1"} {
		if !strings.Contains(stdout.String(), expected) {
			t.Fatalf("stdout missing %q: %s", expected, stdout.String())
		}
	}
	if !strings.Contains(stderr.String(), "unknown command: /unknown") {
		t.Fatalf("stderr = %q", stderr.String())
	}
}

func TestREPLCompactCommandPassesFocusAndPrintsConciseResult(t *testing.T) {
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{}
	backend.compact = func(_ context.Context, focus string, _ func(agent.Event)) (agent.CompactionResult, error) {
		if focus != "focus on auth" {
			t.Fatalf("focus = %q", focus)
		}
		return agent.CompactionResult{
			CheckpointID:         "deadbeef",
			TokensBefore:         258000,
			EstimatedTokensAfter: 23000,
		}, nil
	}
	r := New(strings.NewReader("/compact \t focus on auth  \n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "[context] compacted 258k → 23k tokens") {
		t.Fatalf("stdout = %q", stdout.String())
	}
}

func TestREPLCompactCommandPrefersEventAndDeduplicatesCheckpointID(t *testing.T) {
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{}
	backend.compact = func(_ context.Context, _ string, emit func(agent.Event)) (agent.CompactionResult, error) {
		emit(agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{
			CheckpointID:         "deadbeef",
			TokensBefore:         258000,
			EstimatedTokensAfter: 22000,
		}})
		return agent.CompactionResult{
			CheckpointID:         "deadbeef",
			TokensBefore:         258000,
			EstimatedTokensAfter: 23000,
		}, nil
	}
	r := New(strings.NewReader("/compact\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if got := strings.Count(stdout.String(), "[context]"); got != 1 {
		t.Fatalf("stdout = %q", stdout.String())
	}
	if !strings.Contains(stdout.String(), "[context] compacted 258k → 22k tokens") {
		t.Fatalf("stdout = %q", stdout.String())
	}
	if strings.Contains(stdout.String(), "23k") {
		t.Fatalf("stdout should prefer event result: %q", stdout.String())
	}
}

func TestREPLCompactCommandParentCancellationReturnsContextErrorWithoutExtraPrompt(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	started := make(chan struct{})
	backend := &fakeBackend{}
	backend.compact = func(turnCtx context.Context, _ string, _ func(agent.Event)) (agent.CompactionResult, error) {
		close(started)
		<-turnCtx.Done()
		return agent.CompactionResult{}, turnCtx.Err()
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/compact\n"), &stdout, &stderr, backend)
	errCh := make(chan error, 1)
	go func() { errCh <- r.Run(ctx) }()
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("compact command did not start")
	}
	cancel()
	if err := <-errCh; !errors.Is(err, context.Canceled) {
		t.Fatalf("Run() = %v, want context.Canceled", err)
	}
	if got := strings.Count(stdout.String(), "> "); got != 1 {
		t.Fatalf("stdout = %q", stdout.String())
	}
}

func TestREPLCompactCommandRendersDistinctCheckpointIDsOnceEach(t *testing.T) {
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{}
	backend.compact = func(_ context.Context, _ string, emit func(agent.Event)) (agent.CompactionResult, error) {
		emit(agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{
			CheckpointID:         "deadbeef",
			TokensBefore:         1500,
			EstimatedTokensAfter: 500,
		}})
		return agent.CompactionResult{
			CheckpointID:         "feedface",
			TokensBefore:         1500,
			EstimatedTokensAfter: 400,
		}, nil
	}
	r := New(strings.NewReader("/compact\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if got := strings.Count(stdout.String(), "[context]"); got != 2 {
		t.Fatalf("stdout = %q", stdout.String())
	}
	for _, expected := range []string{"[context] compacted 1k → 500 tokens", "[context] compacted 1k → 400 tokens"} {
		if !strings.Contains(stdout.String(), expected) {
			t.Fatalf("stdout missing %q: %q", expected, stdout.String())
		}
	}
}

func TestREPLCompactCommandNoopEmptyIDDoesNotSuppressNonemptyResult(t *testing.T) {
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{}
	backend.compact = func(_ context.Context, _ string, emit func(agent.Event)) (agent.CompactionResult, error) {
		emit(agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{Noop: true}})
		return agent.CompactionResult{
			CheckpointID:         "deadbeef",
			TokensBefore:         5000,
			EstimatedTokensAfter: 1200,
		}, nil
	}
	r := New(strings.NewReader("/compact\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if got := strings.Count(stdout.String(), "[context]"); got != 2 {
		t.Fatalf("stdout = %q", stdout.String())
	}
	if !strings.Contains(stdout.String(), "[context] no-op") || !strings.Contains(stdout.String(), "[context] compacted 5k → 1k tokens") {
		t.Fatalf("stdout = %q", stdout.String())
	}
}

func TestREPLCompactCommandNoOpRendersConcisely(t *testing.T) {
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{}
	backend.compact = func(_ context.Context, _ string, _ func(agent.Event)) (agent.CompactionResult, error) {
		return agent.CompactionResult{Noop: true}, nil
	}
	r := New(strings.NewReader("/compact\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "[context] no-op") {
		t.Fatalf("stdout = %q", stdout.String())
	}
}

func TestREPLCompactlyRemainsUnknownCommand(t *testing.T) {
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{}
	backend.compact = func(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
		t.Fatal("compact backend should not run")
		return agent.CompactionResult{}, nil
	}
	r := New(strings.NewReader("/compactly\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stderr.String(), "unknown command: /compactly") {
		t.Fatalf("stderr = %q", stderr.String())
	}
}

func TestREPLAutomaticCompactionSuccessAndWarningDuringPrompt(t *testing.T) {
	warningErr := errors.New("automatic context compaction failed below the hard input limit; continuing with the original request")
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{prompt: func(_ context.Context, _ string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventCompactionWarning, Err: warningErr})
		emit(agent.Event{Type: agent.EventCompactionCompleted, Compaction: &agent.CompactionEvent{
			CheckpointID:         "abc123",
			TokensBefore:         64000,
			EstimatedTokensAfter: 12000,
		}})
		emit(agent.Event{Type: agent.EventTextDelta, Text: "done"})
		return nil
	}}
	r := New(strings.NewReader("inspect\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stdout.String(), "[context] compacted 64k → 12k tokens") {
		t.Fatalf("stdout = %q", stdout.String())
	}
	if !strings.Contains(stderr.String(), warningErr.Error()) {
		t.Fatalf("stderr = %q", stderr.String())
	}
}

func TestREPLCompactCommandInterruptReturnsToPrompt(t *testing.T) {
	started := make(chan struct{})
	finished := make(chan struct{})
	backend := &fakeBackend{}
	backend.compact = func(ctx context.Context, _ string, _ func(agent.Event)) (agent.CompactionResult, error) {
		close(started)
		<-ctx.Done()
		close(finished)
		return agent.CompactionResult{}, ctx.Err()
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/compact\n/exit\n"), &stdout, &stderr, backend)
	errCh := make(chan error, 1)
	go func() { errCh <- r.Run(context.Background()) }()
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("compact command did not start")
	}
	if !r.Interrupt() {
		t.Fatal("Interrupt() returned false during compact command")
	}
	select {
	case <-finished:
	case <-time.After(time.Second):
		t.Fatal("compact command was not canceled")
	}
	if err := <-errCh; err != nil {
		t.Fatalf("Run() = %v, want return to REPL then /exit", err)
	}
	if r.Interrupt() {
		t.Fatal("Interrupt() returned true while idle")
	}
}

func TestREPLCompactCommandReturnsFatalPersistenceError(t *testing.T) {
	fatalErr := errors.Join(session.ErrFatalPersistence, errors.New("disk full"))
	backend := &fakeBackend{}
	backend.compact = func(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
		return agent.CompactionResult{}, fatalErr
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/compact\n/exit\n"), &stdout, &stderr, backend)
	err := r.Run(context.Background())
	if !errors.Is(err, session.ErrFatalPersistence) || !IsCommandError(err, "/compact") {
		t.Fatalf("Run() = %v, want fatal compact command error", err)
	}
}

func TestREPLReturnsNilAtEOF(t *testing.T) {
	var output bytes.Buffer
	r := New(strings.NewReader(""), &output, &output, &fakeBackend{})
	if err := r.Run(context.Background()); err != nil {
		t.Fatalf("Run() at EOF = %v", err)
	}
	want := logo + "Sandbox: bash disabled · sandbox unavailable\n> "
	if got := output.String(); got != want {
		t.Fatalf("output = %q, want %q", got, want)
	}
}

func TestREPLNewSessionUsesBackendAndContinuesInput(t *testing.T) {
	backend := &fakeBackend{info: app.Info{SessionID: "old"}}
	backend.newSession = func() error {
		backend.info.SessionID = "new"
		return nil
	}
	input := strings.NewReader("/new\n/session\n/exit\n")
	var output bytes.Buffer
	console := New(input, &output, &output, backend)
	if err := console.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if backend.newCalls != 1 || !strings.Contains(output.String(), "ID: new") {
		t.Fatalf("newCalls=%d output=%q", backend.newCalls, output.String())
	}
}

func TestREPLWritesProviderErrorsToStderrAndContinues(t *testing.T) {
	providerErr := errors.New("provider unavailable")
	var stdout, stderr bytes.Buffer
	calls := 0
	backend := &fakeBackend{prompt: func(_ context.Context, _ string, emit func(agent.Event)) error {
		calls++
		if calls == 1 {
			emit(agent.Event{Type: agent.EventAgentError, Err: providerErr})
			return providerErr
		}
		emit(agent.Event{Type: agent.EventTextDelta, Text: "recovered"})
		return nil
	}}
	r := New(strings.NewReader("first\nsecond\n/exit\n"), &stdout, &stderr, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if strings.Count(stderr.String(), providerErr.Error()) != 1 {
		t.Fatalf("stderr should contain one provider error: %q", stderr.String())
	}
	if !strings.Contains(stdout.String(), "recovered") || calls != 2 {
		t.Fatalf("stdout = %q, calls = %d", stdout.String(), calls)
	}
}

func TestREPLTerminatesAfterFatalPersistenceError(t *testing.T) {
	fatalErr := errors.Join(session.ErrFatalPersistence, errors.New("injected append failure"))
	var calls int
	var stdout, stderr bytes.Buffer
	backend := &fakeBackend{prompt: func(_ context.Context, _ string, emit func(agent.Event)) error {
		calls++
		emit(agent.Event{Type: agent.EventAgentError, Err: fatalErr})
		return fatalErr
	}}
	r := New(strings.NewReader("first\nsecond\n/exit\n"), &stdout, &stderr, backend)
	err := r.Run(context.Background())
	if !errors.Is(err, session.ErrFatalPersistence) {
		t.Fatalf("Run() error = %v, want fatal persistence", err)
	}
	if calls != 1 {
		t.Fatalf("runner calls = %d, want no later prompt processing", calls)
	}
}

func TestREPLAcceptsInputUpToOneMiB(t *testing.T) {
	prompt := strings.Repeat("x", 1<<20)
	var output bytes.Buffer
	backend := &fakeBackend{prompt: func(_ context.Context, got string, _ func(agent.Event)) error {
		if got != prompt {
			t.Fatalf("prompt length = %d, want %d", len(got), len(prompt))
		}
		return nil
	}}
	r := New(strings.NewReader(prompt+"\n/exit\n"), &output, &output, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
}

func TestREPLRejectsInputOverOneMiB(t *testing.T) {
	var output bytes.Buffer
	input := strings.NewReader(strings.Repeat("x", (1<<20)+1) + "\n")
	r := New(input, &output, &output, &fakeBackend{})
	if err := r.Run(context.Background()); err == nil || !strings.Contains(err.Error(), "token too long") {
		t.Fatalf("Run() = %v, want scanner token-too-long error", err)
	}
}

func TestREPLSeparatesTurns(t *testing.T) {
	var output bytes.Buffer
	backend := &fakeBackend{prompt: func(_ context.Context, prompt string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventTextDelta, Text: prompt})
		return nil
	}}
	r := New(strings.NewReader("one\ntwo\n/exit\n"), &output, &output, backend)
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(output.String(), "one\n> two\n> ") {
		t.Fatalf("turns are not separated: %q", output.String())
	}
}

func TestREPLInterruptCancelsOnlyActiveTurn(t *testing.T) {
	started := make(chan struct{})
	finished := make(chan struct{})
	backend := &fakeBackend{prompt: func(ctx context.Context, _ string, emit func(agent.Event)) error {
		close(started)
		<-ctx.Done()
		emit(agent.Event{Type: agent.EventAgentError, Err: ctx.Err()})
		close(finished)
		return ctx.Err()
	}}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("wait\n/exit\n"), &stdout, &stderr, backend)
	errCh := make(chan error, 1)
	go func() { errCh <- r.Run(context.Background()) }()
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("runner did not start")
	}
	if !r.Interrupt() {
		t.Fatal("Interrupt() returned false during active turn")
	}
	select {
	case <-finished:
	case <-time.After(time.Second):
		t.Fatal("active turn was not canceled")
	}
	if err := <-errCh; err != nil {
		t.Fatalf("Run() = %v, want return to REPL then /exit", err)
	}
	if r.Interrupt() {
		t.Fatal("Interrupt() returned true while idle")
	}
}

func TestREPLParentCancellationStopsIdleScan(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	reader, writer := io.Pipe()
	defer reader.Close()
	defer writer.Close()
	var output bytes.Buffer
	r := New(reader, &output, &output, &fakeBackend{})
	errCh := make(chan error, 1)
	go func() { errCh <- r.Run(ctx) }()
	cancel()
	select {
	case err := <-errCh:
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("Run() = %v, want context cancellation", err)
		}
	case <-time.After(time.Second):
		t.Fatal("idle scan did not stop after parent cancellation")
	}
}

func TestREPLSandboxStatusAppearsAtStartupAndInSessionCommand(t *testing.T) {
	info := app.Info{
		SessionID: "sandbox-session", Provider: "openai-compatible", Model: "model",
		Sandbox: app.SandboxInfo{
			Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkDenied, BashAvailable: true, Reason: app.SandboxReasonNone,
		},
	}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("/session\n/exit\n"), &stdout, &stderr, &fakeBackend{info: info})
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}

	const status = "Sandbox: seatbelt · workspace-write · network denied"
	rendered := stdout.String()
	if got := strings.Count(rendered, status); got != 2 {
		t.Fatalf("Sandbox status count = %d, want startup and /session output in %q", got, rendered)
	}
	if sessionIndex, sandboxIndex := strings.Index(rendered, "Session: sandbox-session\n"), strings.Index(rendered, status); sessionIndex < 0 || sandboxIndex < sessionIndex {
		t.Fatalf("startup Sandbox status is not after session line: %q", rendered)
	}
	if strings.Contains(rendered, "Sandbox reason:") {
		t.Fatalf("available Sandbox rendered a reason: %q", rendered)
	}
	if stderr.Len() != 0 {
		t.Fatalf("stderr = %q, want empty", stderr.String())
	}
}

func TestREPLSandboxUnavailableReasonIsFixedAndControlSafe(t *testing.T) {
	payload := "raw\x1b]52;c;owned\a\nSandbox reason: forged"
	info := app.Info{
		SessionID: "sandbox-session",
		Sandbox: app.SandboxInfo{
			Mode:          app.SandboxUnavailable,
			Network:       app.SandboxNetwork(payload),
			BashAvailable: false,
			Reason:        app.SandboxReason(payload),
		},
	}
	var output bytes.Buffer
	r := New(strings.NewReader("/session\n/exit\n"), &output, &output, &fakeBackend{info: info})
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}

	rendered := output.String()
	if !strings.Contains(rendered, "Sandbox: bash disabled · sandbox unavailable") {
		t.Fatalf("output missing safe unavailable summary: %q", rendered)
	}
	if !strings.Contains(rendered, "Sandbox reason: runtime-failure") {
		t.Fatalf("output missing safe unavailable reason: %q", rendered)
	}
	if strings.Contains(rendered, payload) || strings.ContainsAny(rendered, "\x1b\a") {
		t.Fatalf("output leaked control-bearing Sandbox state: %q", rendered)
	}
}

func TestRunOnceTreatsSessionAsOrdinaryPromptWithoutSandboxPresentation(t *testing.T) {
	var stdout, stderr bytes.Buffer
	calls := 0
	backend := &fakeBackend{
		info: app.Info{Sandbox: app.SandboxInfo{
			Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true, Reason: app.SandboxReasonNone,
		}},
		prompt: func(_ context.Context, prompt string, emit func(agent.Event)) error {
			calls++
			if prompt != "/session" {
				t.Fatalf("prompt = %q, want /session", prompt)
			}
			emit(agent.Event{Type: agent.EventTextDelta, Text: "provider response"})
			return nil
		},
	}
	r := New(strings.NewReader(""), &stdout, &stderr, backend)
	if err := r.RunOnce(context.Background(), "/session"); err != nil {
		t.Fatal(err)
	}
	if calls != 1 || stdout.String() != "provider response\n" {
		t.Fatalf("calls = %d, stdout = %q", calls, stdout.String())
	}
	if strings.Contains(stdout.String(), "Sandbox:") || strings.Contains(stdout.String(), logo) || stderr.Len() != 0 {
		t.Fatalf("RunOnce rendered interactive presentation: stdout=%q stderr=%q", stdout.String(), stderr.String())
	}
}

type fakeBackend struct {
	info       app.Info
	newCalls   int
	newSession func() error
	prompt     func(context.Context, string, func(agent.Event)) error
	compact    func(context.Context, string, func(agent.Event)) (agent.CompactionResult, error)
}

func (f *fakeBackend) Prompt(ctx context.Context, prompt string, emit func(agent.Event)) error {
	if f.prompt == nil {
		return nil
	}
	return f.prompt(ctx, prompt, emit)
}

func (f *fakeBackend) Compact(ctx context.Context, focus string, emit func(agent.Event)) (agent.CompactionResult, error) {
	if f.compact == nil {
		return agent.CompactionResult{Noop: true}, nil
	}
	return f.compact(ctx, focus, emit)
}

func (f *fakeBackend) NewSession() error {
	f.newCalls++
	if f.newSession == nil {
		return nil
	}
	return f.newSession()
}

func (f *fakeBackend) Info() app.Info {
	return f.info
}

func (f *fakeBackend) History() []model.Message {
	return nil
}
