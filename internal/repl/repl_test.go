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
	"github.com/baiyuqing/otto/internal/tool"
)

func TestREPLRunsPromptAndRendersEvents(t *testing.T) {
	input := strings.NewReader("inspect files\n/exit\n")
	var output bytes.Buffer
	runner := &fakeRunner{run: func(_ context.Context, prompt string, emit func(agent.Event)) error {
		if prompt != "inspect files" {
			t.Fatalf("prompt = %q", prompt)
		}
		emit(agent.Event{Type: agent.EventTextDelta, Text: "done"})
		emit(agent.Event{Type: agent.EventToolCallStarted, ToolName: "read", ToolCallID: "call-1"})
		emit(agent.Event{Type: agent.EventToolCallFinished, ToolName: "read", ToolCallID: "call-1", ToolResult: tool.Result{Content: "read README.md\nfull output must not render"}})
		return nil
	}}
	r := New(input, &output, &output, runner, Info{SessionID: "session-1", SessionPath: "/tmp/session.jsonl", Provider: "openai-compatible", Model: "test"})
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

func TestREPLCommandsAndInputHandling(t *testing.T) {
	var stdout, stderr bytes.Buffer
	var prompts []string
	runner := &fakeRunner{run: func(_ context.Context, prompt string, _ func(agent.Event)) error {
		prompts = append(prompts, prompt)
		return nil
	}}
	input := strings.NewReader("\n/help\n/session\n/unknown\nhello\n/exit\n")
	r := New(input, &stdout, &stderr, runner, Info{SessionID: "s-1", SessionPath: "/sessions/s-1.jsonl", Provider: "openai-compatible", Model: "m-1"})
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
	if len(prompts) != 1 || prompts[0] != "hello" {
		t.Fatalf("runner prompts = %q, want [hello]", prompts)
	}
	for _, expected := range []string{"/help", "/exit", "/new", "/session", "s-1", "/sessions/s-1.jsonl", "openai-compatible", "m-1"} {
		if !strings.Contains(stdout.String(), expected) {
			t.Fatalf("stdout missing %q: %s", expected, stdout.String())
		}
	}
	if !strings.Contains(stderr.String(), "unknown command: /unknown") {
		t.Fatalf("stderr = %q", stderr.String())
	}
}

func TestREPLReturnsNilAtEOF(t *testing.T) {
	var output bytes.Buffer
	r := New(strings.NewReader(""), &output, &output, &fakeRunner{}, Info{})
	if err := r.Run(context.Background()); err != nil {
		t.Fatalf("Run() at EOF = %v", err)
	}
	if got := output.String(); got != "> " {
		t.Fatalf("output = %q, want prompt", got)
	}
}

func TestREPLReturnsNewSessionSentinel(t *testing.T) {
	var output bytes.Buffer
	r := New(strings.NewReader("/new\n"), &output, &output, &fakeRunner{}, Info{})
	if err := r.Run(context.Background()); !errors.Is(err, ErrNewSession) {
		t.Fatalf("Run() = %v, want %v", err, ErrNewSession)
	}
}

func TestREPLWritesProviderErrorsToStderrAndContinues(t *testing.T) {
	providerErr := errors.New("provider unavailable")
	var stdout, stderr bytes.Buffer
	calls := 0
	runner := &fakeRunner{run: func(_ context.Context, _ string, emit func(agent.Event)) error {
		calls++
		if calls == 1 {
			emit(agent.Event{Type: agent.EventAgentError, Err: providerErr})
			return providerErr
		}
		emit(agent.Event{Type: agent.EventTextDelta, Text: "recovered"})
		return nil
	}}
	r := New(strings.NewReader("first\nsecond\n/exit\n"), &stdout, &stderr, runner, Info{})
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

func TestREPLAcceptsInputUpToOneMiB(t *testing.T) {
	prompt := strings.Repeat("x", 1<<20)
	var output bytes.Buffer
	runner := &fakeRunner{run: func(_ context.Context, got string, _ func(agent.Event)) error {
		if got != prompt {
			t.Fatalf("prompt length = %d, want %d", len(got), len(prompt))
		}
		return nil
	}}
	r := New(strings.NewReader(prompt+"\n/exit\n"), &output, &output, runner, Info{})
	if err := r.Run(context.Background()); err != nil {
		t.Fatal(err)
	}
}

func TestREPLRejectsInputOverOneMiB(t *testing.T) {
	var output bytes.Buffer
	input := strings.NewReader(strings.Repeat("x", (1<<20)+1) + "\n")
	r := New(input, &output, &output, &fakeRunner{}, Info{})
	if err := r.Run(context.Background()); err == nil || !strings.Contains(err.Error(), "token too long") {
		t.Fatalf("Run() = %v, want scanner token-too-long error", err)
	}
}

func TestREPLSeparatesTurns(t *testing.T) {
	var output bytes.Buffer
	runner := &fakeRunner{run: func(_ context.Context, prompt string, emit func(agent.Event)) error {
		emit(agent.Event{Type: agent.EventTextDelta, Text: prompt})
		return nil
	}}
	r := New(strings.NewReader("one\ntwo\n/exit\n"), &output, &output, runner, Info{})
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
	runner := &fakeRunner{run: func(ctx context.Context, _ string, emit func(agent.Event)) error {
		close(started)
		<-ctx.Done()
		emit(agent.Event{Type: agent.EventAgentError, Err: ctx.Err()})
		close(finished)
		return ctx.Err()
	}}
	var stdout, stderr bytes.Buffer
	r := New(strings.NewReader("wait\n/exit\n"), &stdout, &stderr, runner, Info{})
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
	r := New(reader, &output, &output, &fakeRunner{}, Info{})
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

type fakeRunner struct {
	run func(context.Context, string, func(agent.Event)) error
}

func (f *fakeRunner) Run(ctx context.Context, prompt string, emit func(agent.Event)) error {
	if f.run == nil {
		return nil
	}
	return f.run(ctx, prompt, emit)
}
