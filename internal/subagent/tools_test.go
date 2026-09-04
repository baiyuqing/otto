package subagent

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/tool"
)

// Test 6: agent{wait:true} blocks until the task finishes, returns its
// completion text, and removes its notification (the same dedupe behavior
// as agent_wait).
func TestAgentToolWaitTrueBlocksAndReturnsCompletion(t *testing.T) {
	fp := newFakeProvider()
	fp.addRoute(matchPrompt("do it"), routeStep{resp: assistantText("all done", model.Usage{InputTokens: 4, OutputTokens: 2})})

	tasks := agent.NewTasks()
	defer tasks.Close()
	cfg := newTestConfig(fp, tasks)
	runner, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}

	agentTool := toolByName(t, runner.Tools(), "agent")
	result := agentTool.Execute(context.Background(), json.RawMessage(`{"prompt":"do it","wait":true}`))
	if result.IsError {
		t.Fatalf("unexpected error: %s", result.Content)
	}

	final, _ := tasks.Get("t1")
	if want := CompletionText(final, cfg.MaxOutputBytes); result.Content != want {
		t.Fatalf("content = %q, want %q", result.Content, want)
	}
	if _, ok := tasks.Notifications().Remove("t1", agent.NotificationTaskFinished); ok {
		t.Fatal("notification should already be removed by agent{wait:true}")
	}
}

// A missing or blank prompt is rejected without starting a task.
func TestAgentToolRejectsBlankPrompt(t *testing.T) {
	fp := newFakeProvider()
	tasks := agent.NewTasks()
	defer tasks.Close()
	cfg := newTestConfig(fp, tasks)
	runner, err := NewRunner(cfg)
	if err != nil {
		t.Fatalf("NewRunner: %v", err)
	}

	agentTool := toolByName(t, runner.Tools(), "agent")
	for _, body := range []string{`{"prompt":"   "}`, `{}`} {
		result := agentTool.Execute(context.Background(), json.RawMessage(body))
		if !result.IsError {
			t.Fatalf("body %s: expected an error result", body)
		}
	}
	if len(tasks.List()) != 0 {
		t.Fatalf("expected no tasks to be created, got %d", len(tasks.List()))
	}
}

// Test 5: agent_wait selection, success, and failure behaviors.
func TestAgentWaitTool(t *testing.T) {
	t.Run("named task waits and removes its notification", func(t *testing.T) {
		fp := newFakeProvider()
		fp.addRoute(matchAny, routeStep{resp: assistantText("done", model.Usage{InputTokens: 1, OutputTokens: 1})})
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

		waitTool := toolByName(t, runner.Tools(), "agent_wait")
		result := waitTool.Execute(context.Background(), json.RawMessage(`{"task_id":"t1"}`))
		if result.IsError {
			t.Fatalf("unexpected error: %s", result.Content)
		}
		final, _ := tasks.Get("t1")
		if want := CompletionText(final, cfg.MaxOutputBytes); result.Content != want {
			t.Fatalf("content = %q, want %q", result.Content, want)
		}
		if _, ok := tasks.Notifications().Remove("t1", agent.NotificationTaskFinished); ok {
			t.Fatal("notification should have been removed by agent_wait")
		}
	})

	t.Run("no task_id waits for every non-final task and joins texts", func(t *testing.T) {
		fp := newFakeProvider()
		release := make(chan struct{})
		fp.setHook(func(ctx context.Context, req provider.Request) {
			select {
			case <-release:
			case <-ctx.Done():
			}
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
		if _, err := runner.Start(StartRequest{Prompt: "a"}); err != nil {
			t.Fatalf("Start: %v", err)
		}
		if _, err := runner.Start(StartRequest{Prompt: "b"}); err != nil {
			t.Fatalf("Start: %v", err)
		}
		waitStatus(t, tasks, "t1", agent.TaskRunning)
		waitStatus(t, tasks, "t2", agent.TaskRunning)

		waitTool := toolByName(t, runner.Tools(), "agent_wait")
		resultCh := make(chan tool.Result, 1)
		go func() {
			resultCh <- waitTool.Execute(context.Background(), json.RawMessage(`{}`))
		}()
		// Give agent_wait time to read the non-final task list before the
		// tasks are allowed to finish.
		time.Sleep(20 * time.Millisecond)
		close(release)

		var result tool.Result
		select {
		case result = <-resultCh:
		case <-time.After(5 * time.Second):
			t.Fatal("agent_wait did not return in time")
		}
		if result.IsError {
			t.Fatalf("unexpected error: %s", result.Content)
		}
		t1, _ := tasks.Get("t1")
		t2, _ := tasks.Get("t2")
		want := CompletionText(t1, cfg.MaxOutputBytes) + "\n\n" + CompletionText(t2, cfg.MaxOutputBytes)
		if result.Content != want {
			t.Fatalf("content = %q, want %q", result.Content, want)
		}
	})

	t.Run("no tasks running returns a plain message, not an error", func(t *testing.T) {
		fp := newFakeProvider()
		tasks := agent.NewTasks()
		defer tasks.Close()
		cfg := newTestConfig(fp, tasks)
		runner, err := NewRunner(cfg)
		if err != nil {
			t.Fatalf("NewRunner: %v", err)
		}

		waitTool := toolByName(t, runner.Tools(), "agent_wait")
		result := waitTool.Execute(context.Background(), json.RawMessage(`{}`))
		if result.IsError {
			t.Fatalf("expected a non-error result, got: %s", result.Content)
		}
		if result.Content != "no tasks are running" {
			t.Fatalf("content = %q, want %q", result.Content, "no tasks are running")
		}
	})

	t.Run("unknown task_id errors", func(t *testing.T) {
		fp := newFakeProvider()
		tasks := agent.NewTasks()
		defer tasks.Close()
		cfg := newTestConfig(fp, tasks)
		runner, err := NewRunner(cfg)
		if err != nil {
			t.Fatalf("NewRunner: %v", err)
		}

		waitTool := toolByName(t, runner.Tools(), "agent_wait")
		result := waitTool.Execute(context.Background(), json.RawMessage(`{"task_id":"t9"}`))
		if !result.IsError {
			t.Fatal("expected an error result")
		}
		if result.Content != "unknown task: t9" {
			t.Fatalf("content = %q, want %q", result.Content, "unknown task: t9")
		}
	})

	t.Run("negative timeout_seconds errors", func(t *testing.T) {
		fp := newFakeProvider()
		tasks := agent.NewTasks()
		defer tasks.Close()
		cfg := newTestConfig(fp, tasks)
		runner, err := NewRunner(cfg)
		if err != nil {
			t.Fatalf("NewRunner: %v", err)
		}

		waitTool := toolByName(t, runner.Tools(), "agent_wait")
		result := waitTool.Execute(context.Background(), json.RawMessage(`{"timeout_seconds":-1}`))
		if !result.IsError {
			t.Fatal("expected an error result")
		}
	})

	t.Run("timeout reports which tasks are still running", func(t *testing.T) {
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

		waitTool := toolByName(t, runner.Tools(), "agent_wait")
		result := waitTool.Execute(context.Background(), json.RawMessage(`{"timeout_seconds":1}`))
		if !result.IsError {
			t.Fatalf("expected a timeout error, got: %s", result.Content)
		}
		if result.Content != "timed out after 1s; still running: t1" {
			t.Fatalf("content = %q, want %q", result.Content, "timed out after 1s; still running: t1")
		}

		tasks.Cancel("t1") // unblock the child so tasks.Close() does not hang
	})

	t.Run("parent context canceled reports which tasks are still running", func(t *testing.T) {
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

		waitTool := toolByName(t, runner.Tools(), "agent_wait")
		callCtx, cancelCall := context.WithCancel(context.Background())
		go func() {
			time.Sleep(20 * time.Millisecond)
			cancelCall()
		}()
		result := waitTool.Execute(callCtx, json.RawMessage(`{}`))
		if !result.IsError {
			t.Fatalf("expected a cancel error, got: %s", result.Content)
		}
		if result.Content != "wait canceled; still running: t1" {
			t.Fatalf("content = %q, want %q", result.Content, "wait canceled; still running: t1")
		}

		tasks.Cancel("t1")
	})
}

// The agent tool's optional model parameter: an explicit value overrides
// the parent's model, an absent or whitespace-only value falls back to it.
// The fake provider's request and the finished task record must agree.
func TestAgentToolModelSelection(t *testing.T) {
	cases := []struct {
		name      string
		body      string
		wantModel string
	}{
		{name: "explicit model overrides parent", body: `{"prompt":"go","model":"cheap-model"}`, wantModel: "cheap-model"},
		{name: "missing model falls back to parent's model", body: `{"prompt":"go"}`, wantModel: "gpt-parent"},
		{name: "whitespace-only model falls back to parent's model", body: `{"prompt":"go","model":"   "}`, wantModel: "gpt-parent"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			fp := newFakeProvider()
			fp.addRoute(matchAny, routeStep{resp: assistantText("done", model.Usage{})})
			tasks := agent.NewTasks()
			defer tasks.Close()
			cfg := newTestConfig(fp, tasks)
			cfg.Options.Model = "gpt-parent"
			runner, err := NewRunner(cfg)
			if err != nil {
				t.Fatalf("NewRunner: %v", err)
			}

			agentTool := toolByName(t, runner.Tools(), "agent")
			result := agentTool.Execute(context.Background(), json.RawMessage(tc.body))
			if result.IsError {
				t.Fatalf("agent tool returned error: %s", result.Content)
			}

			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()
			final, err := tasks.Wait(ctx, "t1")
			if err != nil {
				t.Fatalf("Wait: %v", err)
			}
			if final.Model != tc.wantModel {
				t.Fatalf("Task.Model = %q, want %q", final.Model, tc.wantModel)
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

// Test 11: agent_status output, both the multi-task listing and the
// per-task detail view.
func TestAgentStatusTool(t *testing.T) {
	t.Run("empty registry", func(t *testing.T) {
		fp := newFakeProvider()
		tasks := agent.NewTasks()
		defer tasks.Close()
		cfg := newTestConfig(fp, tasks)
		runner, err := NewRunner(cfg)
		if err != nil {
			t.Fatalf("NewRunner: %v", err)
		}
		statusTool := toolByName(t, runner.Tools(), "agent_status")
		result := statusTool.Execute(context.Background(), json.RawMessage(`{}`))
		if result.IsError {
			t.Fatalf("unexpected error: %s", result.Content)
		}
		if result.Content != "no tasks in this session" {
			t.Fatalf("content = %q, want %q", result.Content, "no tasks in this session")
		}
	})

	t.Run("unknown task_id", func(t *testing.T) {
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

		statusTool := toolByName(t, runner.Tools(), "agent_status")
		result := statusTool.Execute(context.Background(), json.RawMessage(`{"task_id":"t9"}`))
		if !result.IsError {
			t.Fatal("expected an error result")
		}
		if result.Content != "unknown task: t9" {
			t.Fatalf("content = %q, want %q", result.Content, "unknown task: t9")
		}
	})

	t.Run("listing shows running and queued rows", func(t *testing.T) {
		fp := newFakeProvider()
		fp.setHook(func(ctx context.Context, req provider.Request) {
			<-ctx.Done()
		})
		tasks := agent.NewTasks()
		defer tasks.Close()
		cfg := newTestConfig(fp, tasks)
		cfg.MaxParallel = 1
		runner, err := NewRunner(cfg)
		if err != nil {
			t.Fatalf("NewRunner: %v", err)
		}

		if _, err := runner.Start(StartRequest{Prompt: "explore the repo", Description: "explore"}); err != nil {
			t.Fatalf("Start: %v", err)
		}
		waitStatus(t, tasks, "t1", agent.TaskRunning)
		if _, err := runner.Start(StartRequest{Prompt: "review the diff"}); err != nil {
			t.Fatalf("Start: %v", err)
		}

		statusTool := toolByName(t, runner.Tools(), "agent_status")
		result := statusTool.Execute(context.Background(), json.RawMessage(`{}`))
		if result.IsError {
			t.Fatalf("unexpected error: %s", result.Content)
		}

		lines := strings.Split(result.Content, "\n")
		if len(lines) != 2 {
			t.Fatalf("got %d lines, want 2:\n%s", len(lines), result.Content)
		}
		if !strings.Contains(lines[0], "t1") || !strings.Contains(lines[0], "(default)") ||
			!strings.Contains(lines[0], "running") || !strings.Contains(lines[0], "explore") {
			t.Fatalf("t1 line = %q", lines[0])
		}
		if !strings.Contains(lines[1], "t2") || !strings.Contains(lines[1], "(default)") ||
			!strings.Contains(lines[1], "queued") || !strings.Contains(lines[1], "review the diff") {
			t.Fatalf("t2 line = %q", lines[1])
		}

		tasks.Cancel("t1")
		tasks.Cancel("t2")
	})

	t.Run("task_id detail shows history and result on success", func(t *testing.T) {
		fp := newFakeProvider()
		fp.addRoute(matchAny,
			routeStep{resp: assistantToolCall("call-1", "echo", `{"msg":"hi"}`, model.Usage{InputTokens: 1, OutputTokens: 1})},
			routeStep{resp: assistantText("wrap-up", model.Usage{InputTokens: 1, OutputTokens: 1})},
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
		if _, err := tasks.Wait(ctx, "t1"); err != nil {
			t.Fatalf("Wait: %v", err)
		}

		statusTool := toolByName(t, runner.Tools(), "agent_status")
		result := statusTool.Execute(context.Background(), json.RawMessage(`{"task_id":"t1"}`))
		if result.IsError {
			t.Fatalf("unexpected error: %s", result.Content)
		}
		if !strings.Contains(result.Content, "succeeded") {
			t.Fatalf("missing status in detail:\n%s", result.Content)
		}
		if !strings.Contains(result.Content, "echo") {
			t.Fatalf("missing tool call step in detail:\n%s", result.Content)
		}
		if !strings.Contains(result.Content, "assistant: wrap-up") {
			t.Fatalf("missing assistant step in detail:\n%s", result.Content)
		}
		if !strings.HasSuffix(result.Content, "result:\nwrap-up") {
			t.Fatalf("missing result tail in detail:\n%s", result.Content)
		}
	})

	t.Run("task_id detail shows model when set", func(t *testing.T) {
		fp := newFakeProvider()
		fp.addRoute(matchAny, routeStep{resp: assistantText("done", model.Usage{})})
		tasks := agent.NewTasks()
		defer tasks.Close()
		cfg := newTestConfig(fp, tasks)
		runner, err := NewRunner(cfg)
		if err != nil {
			t.Fatalf("NewRunner: %v", err)
		}
		if _, err := runner.Start(StartRequest{Prompt: "go", Model: "gpt-test-model"}); err != nil {
			t.Fatalf("Start: %v", err)
		}

		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		if _, err := tasks.Wait(ctx, "t1"); err != nil {
			t.Fatalf("Wait: %v", err)
		}

		statusTool := toolByName(t, runner.Tools(), "agent_status")
		result := statusTool.Execute(context.Background(), json.RawMessage(`{"task_id":"t1"}`))
		if result.IsError {
			t.Fatalf("unexpected error: %s", result.Content)
		}
		lines := strings.Split(result.Content, "\n")
		if len(lines) < 2 || lines[1] != "model: gpt-test-model" {
			t.Fatalf("expected model line right after the status line, got:\n%s", result.Content)
		}
	})

	t.Run("task_id detail shows error on failure", func(t *testing.T) {
		fp := newFakeProvider()
		fp.addRoute(matchAny, routeStep{err: errors.New("boom")})
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

		statusTool := toolByName(t, runner.Tools(), "agent_status")
		result := statusTool.Execute(context.Background(), json.RawMessage(`{"task_id":"t1"}`))
		if result.IsError {
			t.Fatalf("unexpected error: %s", result.Content)
		}
		if !strings.Contains(result.Content, "error: ") || !strings.Contains(result.Content, "boom") {
			t.Fatalf("missing error tail in detail:\n%s", result.Content)
		}
	})
}
