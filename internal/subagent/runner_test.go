package subagent

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"reflect"
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
	runner, _, err := NewRunner(cfg)
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
	runner, _, err := NewRunner(cfg)
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
	runner, _, err := NewRunner(cfg)
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

		runner, _, err := NewRunner(cfg)
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

		runner, _, err := NewRunner(cfg)
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
	runner, _, err := NewRunner(cfg)
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
	runner, _, err := NewRunner(cfg)
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
	runner, _, err := NewRunner(cfg)
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
	runner, _, err := NewRunner(cfg)
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
	runner, _, err := NewRunner(cfg)
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
	runner, _, err := NewRunner(cfg)
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

// An unknown Agent name is rejected before any task is created.
func TestStartUnknownAgentRejectedBeforeTaskCreated(t *testing.T) {
	fp := newFakeProvider()
	tasks := agent.NewTasks()
	defer tasks.Close()
	cfg := newTestConfig(fp, tasks)
	runner, _, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}

	if _, err := runner.Start(StartRequest{Prompt: "go", Agent: "nope"}); err == nil {
		t.Fatal("expected an error")
	} else if want := `unknown agent "nope"`; err.Error() != want {
		t.Fatalf("err = %q, want %q", err.Error(), want)
	}
	if len(tasks.List()) != 0 {
		t.Fatalf("expected no task to be created, got %d", len(tasks.List()))
	}
}

// A named agent's task record carries its definition name.
func TestStartNamedAgentSetsTaskAgent(t *testing.T) {
	fp := newFakeProvider()
	fp.addRoute(matchAny, routeStep{resp: assistantText("done", model.Usage{})})
	tasks := agent.NewTasks()
	defer tasks.Close()
	cfg := newTestConfig(fp, tasks)
	cfg.Catalog = Catalog{definitions: []Definition{{Name: "reviewer"}}}
	runner, _, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}

	task, err := runner.Start(StartRequest{Prompt: "go", Agent: "reviewer"})
	if err != nil {
		t.Fatalf("Start: %v", err)
	}
	if task.Agent != "reviewer" {
		t.Fatalf("task.Agent = %q, want %q", task.Agent, "reviewer")
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	final, err := tasks.Wait(ctx, task.ID)
	if err != nil {
		t.Fatalf("Wait: %v", err)
	}
	if final.Agent != "reviewer" {
		t.Fatalf("final.Agent = %q, want %q", final.Agent, "reviewer")
	}
}

// Model precedence: request.Model > definition.Model > Options.Model.
func TestStartModelPrecedence(t *testing.T) {
	cases := []struct {
		name      string
		callModel string
		defModel  string
		sessModel string
		wantModel string
	}{
		{name: "call overrides definition and session", callModel: "call-model", defModel: "def-model", sessModel: "sess-model", wantModel: "call-model"},
		{name: "definition overrides session", callModel: "", defModel: "def-model", sessModel: "sess-model", wantModel: "def-model"},
		{name: "session is the fallback", callModel: "", defModel: "", sessModel: "sess-model", wantModel: "sess-model"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			fp := newFakeProvider()
			fp.addRoute(matchAny, routeStep{resp: assistantText("done", model.Usage{})})
			tasks := agent.NewTasks()
			defer tasks.Close()
			cfg := newTestConfig(fp, tasks)
			cfg.Options.Model = tc.sessModel
			cfg.Catalog = Catalog{definitions: []Definition{{Name: "reviewer", Model: tc.defModel}}}
			runner, _, err := NewRunner(cfg)
			if err != nil {
				t.Fatalf("NewRunner: %v", err)
			}

			if _, err := runner.Start(StartRequest{Prompt: "go", Agent: "reviewer", Model: tc.callModel}); err != nil {
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
			if reqs[0].Model != tc.wantModel {
				t.Fatalf("provider request Model = %q, want %q", reqs[0].Model, tc.wantModel)
			}
		})
	}
}

// A definition's Tools allowlist limits its registry to exactly those
// default child tools, in the parent's tool order.
func TestNewRunnerToolsAllowlistParentOrder(t *testing.T) {
	fp := newFakeProvider()
	tasks := agent.NewTasks()
	defer tasks.Close()

	parentTools := []tool.Tool{
		newStubTool("grep", tool.Result{}),
		newStubTool("read", tool.Result{}),
		newStubTool("write", tool.Result{}),
	}
	cfg := newTestConfig(fp, tasks, parentTools...)
	cfg.Catalog = Catalog{definitions: []Definition{{Name: "reviewer", Tools: []string{"read", "grep"}}}}
	runner, warnings, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}
	if len(warnings) != 0 {
		t.Fatalf("unexpected warnings: %v", warnings)
	}

	got := toolDefNames(runner.registries["reviewer"].Definitions())
	want := []string{"grep", "read"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("registry tools = %v, want %v", got, want)
	}
}

// A definition's Tools entry that names no default child tool - including
// an excluded tool like "agent" or "remember" - produces a warning and is
// left out of that definition's registry.
func TestNewRunnerUnknownToolWarning(t *testing.T) {
	cases := []struct {
		name    string
		badTool string
	}{
		{name: "tool name that does not exist", badTool: "bogus"},
		{name: "excluded control tool", badTool: "agent"},
		{name: "excluded memory tool", badTool: "remember"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			fp := newFakeProvider()
			tasks := agent.NewTasks()
			defer tasks.Close()
			parentTools := []tool.Tool{newStubTool("read", tool.Result{})}
			cfg := newTestConfig(fp, tasks, parentTools...)
			cfg.Catalog = Catalog{definitions: []Definition{{Name: "reviewer", Tools: []string{"read", tc.badTool}}}}
			runner, warnings, err := NewRunner(cfg)
			if err != nil {
				t.Fatalf("NewRunner: %v", err)
			}

			wantWarning := fmt.Sprintf(`agent reviewer: unknown tool %q ignored`, tc.badTool)
			found := false
			for _, w := range warnings {
				if w == wantWarning {
					found = true
				}
			}
			if !found {
				t.Fatalf("warnings = %v, want to contain %q", warnings, wantWarning)
			}

			got := toolDefNames(runner.registries["reviewer"].Definitions())
			want := []string{"read"}
			if !reflect.DeepEqual(got, want) {
				t.Fatalf("registry tools = %v, want %v", got, want)
			}
		})
	}
}

// A non-empty definition body replaces the generic instruction under the
// "## Sub-agent role" heading.
func TestStartSystemPromptUsesDefinitionBody(t *testing.T) {
	fp := newFakeProvider()
	fp.addRoute(matchAny, routeStep{resp: assistantText("done", model.Usage{})})
	tasks := agent.NewTasks()
	defer tasks.Close()
	cfg := newTestConfig(fp, tasks)
	cfg.Catalog = Catalog{definitions: []Definition{{Name: "reviewer", Body: "You are a reviewer."}}}
	runner, _, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}
	if _, err := runner.Start(StartRequest{Prompt: "go", Agent: "reviewer"}); err != nil {
		t.Fatalf("Start: %v", err)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if _, err := tasks.Wait(ctx, "t1"); err != nil {
		t.Fatalf("Wait: %v", err)
	}

	reqs := fp.requests()
	prompt := reqs[0].SystemPrompt
	if !strings.Contains(prompt, "## Sub-agent role\nYou are a reviewer.") {
		t.Fatalf("system prompt missing definition body:\n%s", prompt)
	}
	if strings.Contains(prompt, genericSubagentInstruction) {
		t.Fatalf("system prompt should not contain the generic instruction when a body is set:\n%s", prompt)
	}
}

// An empty definition body falls back to the generic instruction, same as
// having no definition at all.
func TestStartSystemPromptEmptyBodyUsesGenericInstruction(t *testing.T) {
	fp := newFakeProvider()
	fp.addRoute(matchAny, routeStep{resp: assistantText("done", model.Usage{})})
	tasks := agent.NewTasks()
	defer tasks.Close()
	cfg := newTestConfig(fp, tasks)
	cfg.Catalog = Catalog{definitions: []Definition{{Name: "reviewer"}}}
	runner, _, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}
	if _, err := runner.Start(StartRequest{Prompt: "go", Agent: "reviewer"}); err != nil {
		t.Fatalf("Start: %v", err)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if _, err := tasks.Wait(ctx, "t1"); err != nil {
		t.Fatalf("Wait: %v", err)
	}

	reqs := fp.requests()
	if !strings.Contains(reqs[0].SystemPrompt, genericSubagentInstruction) {
		t.Fatalf("system prompt missing generic instruction:\n%s", reqs[0].SystemPrompt)
	}
}

// context "inherit" seeds the child's memory with InheritSnapshot of
// Config.ParentSession(), so the child's first provider request carries
// that snapshot followed by the new user prompt message.
func TestStartContextInheritSnapshotInFirstRequest(t *testing.T) {
	userMsg := func(id, text string) model.Message {
		return model.Message{ID: id, Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: text}}}
	}
	assistantTextMsg := func(id, text string) model.Message {
		return model.Message{ID: id, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: text}}}
	}
	assistantToolCallMsg := func(id, toolName string) model.Message {
		return model.Message{ID: id, Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolName: toolName, ToolCallID: "call-1"}}}
	}
	toolResultMsg := func(id string) model.Message {
		return model.Message{ID: id, Role: model.RoleTool, Blocks: []model.Block{{Type: model.BlockToolResult, ToolCallID: "call-0"}}}
	}

	parent := []model.Message{
		userMsg("u1", "first"),
		assistantTextMsg("a1", "reply one"),
		userMsg("u2", "second"),
		assistantToolCallMsg("a2", "agent"),
		toolResultMsg("tr1"), // a sibling call's result, appended after the pending agent call
	}
	wantSnapshot := append([]model.Message{}, parent[:3]...)

	fp := newFakeProvider()
	fp.addRoute(matchAny, routeStep{resp: assistantText("done", model.Usage{})})
	tasks := agent.NewTasks()
	defer tasks.Close()
	cfg := newTestConfig(fp, tasks)
	cfg.ParentSession = func() []model.Message { return parent }
	runner, _, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}

	task, err := runner.Start(StartRequest{Prompt: "delegated task", Context: "inherit"})
	if err != nil {
		t.Fatalf("Start: %v", err)
	}
	if task.Context != "inherit" {
		t.Fatalf("task.Context = %q, want %q", task.Context, "inherit")
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if _, err := tasks.Wait(ctx, task.ID); err != nil {
		t.Fatalf("Wait: %v", err)
	}

	reqs := fp.requests()
	if len(reqs) == 0 {
		t.Fatal("expected at least one provider request")
	}
	got := reqs[0].Messages
	if len(got) != len(wantSnapshot)+1 {
		t.Fatalf("Messages len = %d, want %d:\n%+v", len(got), len(wantSnapshot)+1, got)
	}
	if !reflect.DeepEqual(got[:len(wantSnapshot)], wantSnapshot) {
		t.Fatalf("inherited prefix = %+v, want %+v", got[:len(wantSnapshot)], wantSnapshot)
	}
	last := got[len(wantSnapshot)]
	if last.Role != model.RoleUser || last.Text() != "delegated task" {
		t.Fatalf("last message = %+v, want a user message with text %q", last, "delegated task")
	}
}

// context "fresh" (the default) never consults ParentSession, and the
// child's first provider request carries only the prompt.
func TestStartContextFreshOnlyPrompt(t *testing.T) {
	fp := newFakeProvider()
	fp.addRoute(matchAny, routeStep{resp: assistantText("done", model.Usage{})})
	tasks := agent.NewTasks()
	defer tasks.Close()
	cfg := newTestConfig(fp, tasks)
	cfg.ParentSession = func() []model.Message {
		t.Fatal("ParentSession must not be consulted for context fresh")
		return nil
	}
	runner, _, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}

	task, err := runner.Start(StartRequest{Prompt: "go", Context: "fresh"})
	if err != nil {
		t.Fatalf("Start: %v", err)
	}
	if task.Context != "fresh" {
		t.Fatalf("task.Context = %q, want %q", task.Context, "fresh")
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if _, err := tasks.Wait(ctx, task.ID); err != nil {
		t.Fatalf("Wait: %v", err)
	}

	reqs := fp.requests()
	if len(reqs) == 0 {
		t.Fatal("expected at least one provider request")
	}
	if len(reqs[0].Messages) != 1 {
		t.Fatalf("Messages = %+v, want exactly the prompt message", reqs[0].Messages)
	}
	if reqs[0].Messages[0].Text() != "go" {
		t.Fatalf("Messages[0].Text() = %q, want %q", reqs[0].Messages[0].Text(), "go")
	}
}

// A definition's context: inherit is honoured when the call omits Context,
// and an explicit call Context "fresh" overrides it.
func TestStartContextDefinitionInheritHonouredAndCallOverride(t *testing.T) {
	parent := []model.Message{
		{ID: "u1", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "hello"}}},
		{ID: "a1", Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolName: "agent", ToolCallID: "call-1"}}},
	}
	wantSnapshotLen := 1

	cases := []struct {
		name           string
		requestContext string
		wantContext    string
		wantMessageLen int
	}{
		{name: "definition inherit honoured", requestContext: "", wantContext: "inherit", wantMessageLen: wantSnapshotLen + 1},
		{name: "call fresh overrides definition inherit", requestContext: "fresh", wantContext: "fresh", wantMessageLen: 1},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			fp := newFakeProvider()
			fp.addRoute(matchAny, routeStep{resp: assistantText("done", model.Usage{})})
			tasks := agent.NewTasks()
			defer tasks.Close()
			cfg := newTestConfig(fp, tasks)
			cfg.ParentSession = func() []model.Message { return parent }
			cfg.Catalog = Catalog{definitions: []Definition{{Name: "reviewer", Context: "inherit"}}}
			runner, _, err := NewRunner(cfg)
			if err != nil {
				t.Fatalf("NewRunner: %v", err)
			}

			task, err := runner.Start(StartRequest{Prompt: "go", Agent: "reviewer", Context: tc.requestContext})
			if err != nil {
				t.Fatalf("Start: %v", err)
			}
			if task.Context != tc.wantContext {
				t.Fatalf("task.Context = %q, want %q", task.Context, tc.wantContext)
			}

			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()
			if _, err := tasks.Wait(ctx, task.ID); err != nil {
				t.Fatalf("Wait: %v", err)
			}

			reqs := fp.requests()
			if len(reqs) == 0 {
				t.Fatal("expected at least one provider request")
			}
			if len(reqs[0].Messages) != tc.wantMessageLen {
				t.Fatalf("Messages len = %d, want %d", len(reqs[0].Messages), tc.wantMessageLen)
			}
		})
	}
}

// context "inherit" with no Config.ParentSession configured is rejected
// before any task is created.
func TestStartContextInheritWithoutParentSessionErrors(t *testing.T) {
	fp := newFakeProvider()
	tasks := agent.NewTasks()
	defer tasks.Close()
	cfg := newTestConfig(fp, tasks) // ParentSession left nil
	runner, _, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}

	if _, err := runner.Start(StartRequest{Prompt: "go", Context: "inherit"}); err == nil {
		t.Fatal("expected an error")
	} else if want := "context inherit is not available in this session"; err.Error() != want {
		t.Fatalf("err = %q, want %q", err.Error(), want)
	}
	if len(tasks.List()) != 0 {
		t.Fatalf("expected no task to be created, got %d", len(tasks.List()))
	}
}

// A Context value other than "fresh" or "inherit" is rejected before any
// task is created.
func TestStartInvalidContextErrors(t *testing.T) {
	fp := newFakeProvider()
	tasks := agent.NewTasks()
	defer tasks.Close()
	cfg := newTestConfig(fp, tasks)
	runner, _, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}

	if _, err := runner.Start(StartRequest{Prompt: "go", Context: "bogus"}); err == nil {
		t.Fatal("expected an error")
	} else if want := `context must be "fresh" or "inherit"`; err.Error() != want {
		t.Fatalf("err = %q, want %q", err.Error(), want)
	}
	if len(tasks.List()) != 0 {
		t.Fatalf("expected no task to be created, got %d", len(tasks.List()))
	}
}
