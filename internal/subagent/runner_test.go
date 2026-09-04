package subagent

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/tool"
)

// Test 1: agent starts a task and returns immediately; the pushed
// notification matches CompletionText of the finished task.
func TestAgentStartsTaskAndPushesMatchingNotification(t *testing.T) {
	fp := newFakeProvider()
	fp.addRoute(matchPrompt("do the thing"),
		routeStep{resp: assistantText("all done", model.Usage{InputTokens: 100, OutputTokens: 20})})

	tasks := agent.NewTasks()
	defer tasks.Close()
	cfg := newTestConfig(fp, tasks)
	runner, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}

	agentTool := toolByName(t, runner.Tools(), "agent")
	result := agentTool.Execute(context.Background(), json.RawMessage(`{"prompt":"do the thing"}`))
	if result.IsError {
		t.Fatalf("agent tool returned error: %s", result.Content)
	}
	if result.Content != "task t1 (default) started" {
		t.Fatalf("Execute content = %q, want %q", result.Content, "task t1 (default) started")
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	final, err := tasks.Wait(ctx, "t1")
	if err != nil {
		t.Fatalf("Wait: %v", err)
	}
	if final.Status != agent.TaskSucceeded {
		t.Fatalf("status = %s, want succeeded", final.Status)
	}
	if final.Result != "all done" {
		t.Fatalf("result = %q, want %q", final.Result, "all done")
	}

	notification, ok := tasks.Notifications().Remove("t1", agent.NotificationTaskFinished)
	if !ok {
		t.Fatal("expected a task_finished notification")
	}
	if want := CompletionText(final, cfg.MaxOutputBytes); notification.Text != want {
		t.Fatalf("notification text = %q, want %q", notification.Text, want)
	}
}

// Test 2: the child registry excludes every name in ExcludedChildTools,
// preserving the order of the remaining tools.
func TestChildRegistryExcludesControlAndMemoryTools(t *testing.T) {
	fp := newFakeProvider()
	tasks := agent.NewTasks()
	defer tasks.Close()

	var parentTools []tool.Tool
	parentTools = append(parentTools, newStubTool("read", tool.Result{}), newStubTool("write", tool.Result{}))
	for _, name := range ExcludedChildTools {
		parentTools = append(parentTools, newStubTool(name, tool.Result{}))
	}
	parentTools = append(parentTools, newStubTool("grep", tool.Result{}))

	cfg := newTestConfig(fp, tasks, parentTools...)
	runner, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}

	got := strings.Join(toolDefNames(runner.childRegistry.Definitions()), ",")
	want := "read,write,grep"
	if got != want {
		t.Fatalf("child registry tools = %q, want %q", got, want)
	}
}

// Test 3: the child system prompt starts with PromptFor's output and
// contains the sub-agent role heading and the generic instruction.
func TestChildSystemPromptShape(t *testing.T) {
	fp := newFakeProvider()
	fp.addRoute(matchAny, routeStep{resp: assistantText("done", model.Usage{})})

	tasks := agent.NewTasks()
	defer tasks.Close()
	cfg := newTestConfig(fp, tasks)
	runner, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}
	if _, err := runner.Start(StartRequest{Prompt: "go"}); err != nil {
		t.Fatalf("Start: %v", err)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if _, err := tasks.Wait(ctx, "t1"); err != nil {
		t.Fatalf("Wait: %v", err)
	}

	reqs := fp.requests()
	if len(reqs) == 0 {
		t.Fatal("expected at least one provider request")
	}
	prompt := reqs[0].SystemPrompt
	wantPrefix := testPromptFor(runner.childRegistry.Definitions())
	if !strings.HasPrefix(prompt, wantPrefix) {
		t.Fatalf("system prompt does not start with PromptFor output:\n%s", prompt)
	}
	if !strings.Contains(prompt, "## Sub-agent role") {
		t.Fatalf("system prompt missing role heading:\n%s", prompt)
	}
	if !strings.Contains(prompt, genericSubagentInstruction) {
		t.Fatalf("system prompt missing generic instruction:\n%s", prompt)
	}
}

// Test 4: running a sub-agent task through a redactor holding a real secret
// leaves that redactor able to redact the secret afterward. A naive
// implementation that fed the raw (unredacted) Model into agent.New would
// trip the boundary-mutation guard and permanently disable it. This holds
// whether the secret arrives as the parent's configured model or as a
// per-call model argument.
func TestSharedRedactorNotMutatedBySubagentRun(t *testing.T) {
	secret := "sk-supersecret123"

	t.Run("parent model contains the secret", func(t *testing.T) {
		fp := newFakeProvider()
		fp.addRoute(matchAny, routeStep{resp: assistantText("done", model.Usage{})})

		tasks := agent.NewTasks()
		defer tasks.Close()

		redactor := agent.NewRedactor([]string{secret})

		cfg := newTestConfig(fp, tasks)
		cfg.Redactor = redactor
		cfg.Options.Model = secret

		runner, err := NewRunner(cfg)
		if err != nil {
			t.Fatalf("NewRunner: %v", err)
		}
		if _, err := runner.Start(StartRequest{Prompt: "go"}); err != nil {
			t.Fatalf("Start: %v", err)
		}

		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		final, err := tasks.Wait(ctx, "t1")
		if err != nil {
			t.Fatalf("Wait: %v", err)
		}
		if final.Status != agent.TaskSucceeded {
			t.Fatalf("status = %s, want succeeded", final.Status)
		}

		if got := redactor.RedactString(secret); got == secret {
			t.Fatal("redactor no longer redacts its configured secret; shared redactor was corrupted")
		}
	})

	t.Run("per-call model contains the secret", func(t *testing.T) {
		fp := newFakeProvider()
		fp.addRoute(matchAny, routeStep{resp: assistantText("done", model.Usage{})})

		tasks := agent.NewTasks()
		defer tasks.Close()

		redactor := agent.NewRedactor([]string{secret})

		cfg := newTestConfig(fp, tasks)
		cfg.Redactor = redactor
		cfg.Options.Model = "gpt-parent"

		runner, err := NewRunner(cfg)
		if err != nil {
			t.Fatalf("NewRunner: %v", err)
		}
		if _, err := runner.Start(StartRequest{Prompt: "go", Model: secret}); err != nil {
			t.Fatalf("Start: %v", err)
		}

		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		final, err := tasks.Wait(ctx, "t1")
		if err != nil {
			t.Fatalf("Wait: %v", err)
		}
		if final.Status != agent.TaskSucceeded {
			t.Fatalf("status = %s, want succeeded", final.Status)
		}

		wantRedacted := redactor.RedactString(secret)
		reqs := fp.requests()
		if len(reqs) == 0 {
			t.Fatal("expected at least one provider request")
		}
		if reqs[0].Model != wantRedacted {
			t.Fatalf("child provider request Model = %q, want redacted form %q", reqs[0].Model, wantRedacted)
		}

		if got := redactor.RedactString(secret); got == secret {
			t.Fatal("redactor no longer redacts its configured secret; shared redactor was corrupted")
		}
	})
}

// Test 7: MaxParallel = 2 with 3 started tasks runs at most 2 children
// concurrently, leaving the third queued until a slot frees.
func TestMaxParallelLimitsConcurrency(t *testing.T) {
	fp := newFakeProvider()

	var mu sync.Mutex
	inFlight, maxInFlight := 0, 0
	release := make(chan struct{})
	fp.setHook(func(ctx context.Context, req provider.Request) {
		mu.Lock()
		inFlight++
		if inFlight > maxInFlight {
			maxInFlight = inFlight
		}
		mu.Unlock()
		select {
		case <-release:
		case <-ctx.Done():
		}
		mu.Lock()
		inFlight--
		mu.Unlock()
	})
	fp.addRoute(matchAny, routeStep{resp: assistantText("done", model.Usage{})})

	tasks := agent.NewTasks()
	defer tasks.Close()
	cfg := newTestConfig(fp, tasks)
	cfg.MaxParallel = 2
	runner, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}

	// Start t1 and t2 and wait until each has actually acquired a semaphore
	// slot (Status flips to running only after that) before starting t3.
	// Starting all 3 up front would race 3 goroutines against a 2-slot
	// semaphore with no ordering guarantee tied to Start() call order.
	for i := 0; i < 2; i++ {
		if _, err := runner.Start(StartRequest{Prompt: fmt.Sprintf("task %d", i)}); err != nil {
			t.Fatalf("Start: %v", err)
		}
	}
	waitStatus(t, tasks, "t1", agent.TaskRunning)
	waitStatus(t, tasks, "t2", agent.TaskRunning)

	if _, err := runner.Start(StartRequest{Prompt: "task 2"}); err != nil {
		t.Fatalf("Start: %v", err)
	}
	t3, _ := tasks.Get("t3")
	if t3.Status != agent.TaskQueued {
		t.Fatalf("t3 status = %s, want queued while t1/t2 hold the only 2 slots", t3.Status)
	}

	close(release)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	for _, id := range []string{"t1", "t2", "t3"} {
		final, err := tasks.Wait(ctx, id)
		if err != nil {
			t.Fatalf("Wait(%s): %v", id, err)
		}
		if final.Status != agent.TaskSucceeded {
			t.Fatalf("%s status = %s, want succeeded", id, final.Status)
		}
	}

	mu.Lock()
	defer mu.Unlock()
	if maxInFlight > 2 {
		t.Fatalf("observed %d concurrent provider calls, want at most 2", maxInFlight)
	}
}

// Test 8a: canceling a running task's context makes the provider call
// return ctx.Err(), and the task record ends up canceled.
func TestCancelRunningTask(t *testing.T) {
	fp := newFakeProvider()
	fp.setHook(func(ctx context.Context, req provider.Request) {
		<-ctx.Done()
	})

	tasks := agent.NewTasks()
	defer tasks.Close()
	cfg := newTestConfig(fp, tasks)
	runner, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}
	if _, err := runner.Start(StartRequest{Prompt: "go"}); err != nil {
		t.Fatalf("Start: %v", err)
	}

	waitStatus(t, tasks, "t1", agent.TaskRunning)

	if err := tasks.Cancel("t1"); err != nil {
		t.Fatalf("Cancel: %v", err)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	final, err := tasks.Wait(ctx, "t1")
	if err != nil {
		t.Fatalf("Wait: %v", err)
	}
	if final.Status != agent.TaskCanceled {
		t.Fatalf("status = %s, want canceled", final.Status)
	}
}

// Test 8b: canceling a task that is still queued (MaxParallel already
// exhausted by another running task) marks it canceled without ever
// starting it, and StartedAt stays zero.
func TestCancelQueuedTask(t *testing.T) {
	fp := newFakeProvider()
	block := make(chan struct{})
	fp.setHook(func(ctx context.Context, req provider.Request) {
		select {
		case <-block:
		case <-ctx.Done():
		}
	})
	fp.addRoute(matchAny, routeStep{resp: assistantText("done", model.Usage{})})

	tasks := agent.NewTasks()
	defer tasks.Close()
	cfg := newTestConfig(fp, tasks)
	cfg.MaxParallel = 1
	runner, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}

	if _, err := runner.Start(StartRequest{Prompt: "first"}); err != nil {
		t.Fatalf("Start: %v", err)
	}
	waitStatus(t, tasks, "t1", agent.TaskRunning)

	if _, err := runner.Start(StartRequest{Prompt: "second"}); err != nil {
		t.Fatalf("Start: %v", err)
	}
	t2, _ := tasks.Get("t2")
	if t2.Status != agent.TaskQueued {
		t.Fatalf("t2 status = %s, want queued", t2.Status)
	}

	if err := tasks.Cancel("t2"); err != nil {
		t.Fatalf("Cancel: %v", err)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	final, err := tasks.Wait(ctx, "t2")
	if err != nil {
		t.Fatalf("Wait(t2): %v", err)
	}
	if final.Status != agent.TaskCanceled {
		t.Fatalf("status = %s, want canceled", final.Status)
	}
	if !final.StartedAt.IsZero() {
		t.Fatalf("StartedAt = %v, want zero for a task canceled while queued", final.StartedAt)
	}

	close(block)
	if _, err := tasks.Wait(ctx, "t1"); err != nil {
		t.Fatalf("Wait(t1): %v", err)
	}
}

// Test 9: a provider error fails the task and records the error text.
func TestProviderErrorFailsTask(t *testing.T) {
	fp := newFakeProvider()
	wantErr := errors.New("provider exploded")
	fp.addRoute(matchAny, routeStep{err: wantErr})

	tasks := agent.NewTasks()
	defer tasks.Close()
	cfg := newTestConfig(fp, tasks)
	runner, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}
	if _, err := runner.Start(StartRequest{Prompt: "go"}); err != nil {
		t.Fatalf("Start: %v", err)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	final, err := tasks.Wait(ctx, "t1")
	if err != nil {
		t.Fatalf("Wait: %v", err)
	}
	if final.Status != agent.TaskFailed {
		t.Fatalf("status = %s, want failed", final.Status)
	}
	if !strings.Contains(final.Error, wantErr.Error()) {
		t.Fatalf("Error = %q, want to contain %q", final.Error, wantErr.Error())
	}
}

// Test 10: across 2 provider round-trips (a tool call, then a final
// answer), the task record accumulates Steps, ToolCalls, a LastTool
// preview, the latest LastText, and total Usage.
func TestTaskRecordFieldsUpdateAcrossProviderSteps(t *testing.T) {
	fp := newFakeProvider()
	fp.addRoute(matchAny,
		routeStep{resp: assistantToolCall("call-1", "echo", `{"msg":"hi"}`, model.Usage{InputTokens: 10, OutputTokens: 5})},
		routeStep{resp: assistantText("final report", model.Usage{InputTokens: 20, OutputTokens: 8})},
	)

	echo := newStubTool("echo", tool.Result{Content: "echoed"})

	tasks := agent.NewTasks()
	defer tasks.Close()
	cfg := newTestConfig(fp, tasks, echo)
	runner, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}
	if _, err := runner.Start(StartRequest{Prompt: "go"}); err != nil {
		t.Fatalf("Start: %v", err)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	final, err := tasks.Wait(ctx, "t1")
	if err != nil {
		t.Fatalf("Wait: %v", err)
	}
	if final.Status != agent.TaskSucceeded {
		t.Fatalf("status = %s, want succeeded", final.Status)
	}
	if final.Steps != 2 {
		t.Fatalf("Steps = %d, want 2", final.Steps)
	}
	if final.ToolCalls != 1 {
		t.Fatalf("ToolCalls = %d, want 1", final.ToolCalls)
	}
	if !strings.Contains(final.LastTool, "echo") {
		t.Fatalf("LastTool = %q, want it to mention echo", final.LastTool)
	}
	if final.LastText != "final report" {
		t.Fatalf("LastText = %q, want %q", final.LastText, "final report")
	}
	if final.Usage.InputTokens != 30 || final.Usage.OutputTokens != 13 {
		t.Fatalf("Usage = %+v, want input=30 output=13", final.Usage)
	}
	if echo.callCount() != 1 {
		t.Fatalf("echo called %d times, want 1", echo.callCount())
	}
}

// Tasks.Add publishes a task's history closure before Start finishes
// constructing that task's memory. A concurrent Tasks.History call in that
// window must not race with the write that makes the memory available.
func TestHistoryIsSafeWhileStartPublishesTask(t *testing.T) {
	fp := newFakeProvider()
	fp.setHook(func(ctx context.Context, req provider.Request) {
		<-ctx.Done()
	})

	tasks := agent.NewTasks()
	defer tasks.Close()
	cfg := newTestConfig(fp, tasks)
	runner, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}

	stop := make(chan struct{})
	done := make(chan struct{})
	go func() {
		defer close(done)
		for {
			select {
			case <-stop:
				return
			default:
			}
			for _, task := range tasks.List() {
				tasks.History(task.ID) // result ignored; only the concurrent access matters
			}
		}
	}()

	for i := 0; i < 200; i++ {
		if _, err := runner.Start(StartRequest{Prompt: fmt.Sprintf("task %d", i)}); err != nil {
			t.Fatalf("Start: %v", err)
		}
	}

	close(stop)
	<-done

	for _, task := range tasks.List() {
		tasks.Cancel(task.ID)
	}
}
