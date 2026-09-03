package subagent

import (
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
)

func testHeader() session.Header {
	return session.Header{
		Version:   3,
		ID:        "s1",
		Workspace: "/workspace",
		Provider:  "openai-compatible",
		Model:     "gpt-test",
		CreatedAt: time.Unix(0, 0).UTC(),
	}
}

// testPromptFor is a Config.PromptFor stand-in whose output names the
// child's tools, so tests can assert against it directly.
func testPromptFor(defs []model.ToolDefinition) string {
	return "PARENT PROMPT tools=" + strings.Join(toolDefNames(defs), ",")
}

// newTestConfig builds a Config wired to fp with a no-op redactor (no
// configured secrets) and childTools as the parent tool set. Callers copy
// and override fields (MaxParallel, Redactor, Options, ...) as needed.
func newTestConfig(fp *fakeProvider, tasks *agent.Tasks, childTools ...tool.Tool) Config {
	return Config{
		Provider:       fp,
		Tools:          childTools,
		Redactor:       agent.NewRedactor(nil),
		Options:        agent.Options{},
		PromptFor:      testPromptFor,
		Header:         testHeader(),
		Tasks:          tasks,
		MaxParallel:    4,
		MaxOutputBytes: 16384,
	}
}

func toolByName(t *testing.T, tools []tool.Tool, name string) tool.Tool {
	t.Helper()
	for _, tl := range tools {
		if tl.Definition().Name == name {
			return tl
		}
	}
	t.Fatalf("tool %q not found", name)
	return nil
}

// waitStatus polls until id reaches status or the deadline passes.
func waitStatus(t *testing.T, tasks *agent.Tasks, id string, status agent.TaskStatus) agent.Task {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for {
		task, _ := tasks.Get(id)
		if task.Status == status {
			return task
		}
		if time.Now().After(deadline) {
			t.Fatalf("task %s did not reach status %s in time (last: %+v)", id, status, task)
		}
		time.Sleep(10 * time.Millisecond)
	}
}
