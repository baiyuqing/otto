package tool

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/sandbox"
)

func TestRegistryRejectsDuplicateNames(t *testing.T) {
	fake := fakeTool{name: "read"}
	if _, err := NewRegistry(fake, fake); err == nil {
		t.Fatal("expected duplicate tool error")
	}
}

func TestRegistryDefinitionsPreserveInputOrder(t *testing.T) {
	registry, err := NewRegistry(
		fakeTool{name: "first"},
		fakeTool{name: "second"},
	)
	if err != nil {
		t.Fatal(err)
	}

	defs := registry.Definitions()
	if len(defs) != 2 {
		t.Fatalf("Definitions() len = %d, want %d", len(defs), 2)
	}
	if defs[0].Name != "first" || defs[1].Name != "second" {
		t.Fatalf("Definitions() order = %q, %q", defs[0].Name, defs[1].Name)
	}
}

func TestRegistryReturnsUnknownToolError(t *testing.T) {
	registry, err := NewRegistry(fakeTool{name: "read"})
	if err != nil {
		t.Fatal(err)
	}

	result := registry.Execute(context.Background(), "missing", nil)
	if !result.IsError {
		t.Fatal("expected unknown tool result to be marked as error")
	}
	if result.Content != "unknown tool: missing" {
		t.Fatalf("Execute() content = %q, want %q", result.Content, "unknown tool: missing")
	}
}

func TestResultCollectorRetainsLimitAndCountsDiscarded(t *testing.T) {
	collector := newCappedByteCollector(4)
	if n, err := collector.Write([]byte("abcdef")); err != nil || n != 6 {
		t.Fatalf("Write() = (%d, %v), want (6, nil)", n, err)
	}
	if got := string(collector.Bytes()); got != "abcd" {
		t.Fatalf("Bytes() = %q, want %q", got, "abcd")
	}
	if got := collector.Discarded(); got != 2 {
		t.Fatalf("Discarded() = %d, want %d", got, 2)
	}
}

func TestBashSandboxedConstructorRejectsEveryInvalidBoundaryWithOneSafeError(t *testing.T) {
	root := t.TempDir()
	workspace := mustWorkspace(t, root)
	validExecutor := &fakeBashExecutor{}
	var typedNilExecutor *fakeBashExecutor

	noncanonicalRoot := root + string(os.PathSeparator) + "."
	invalidNoncanonicalWorkspace := &Workspace{root: noncanonicalRoot, lexicalRoot: noncanonicalRoot}
	invalidEmptyWorkspace := &Workspace{}
	removedRoot := filepath.Join(t.TempDir(), "removed")
	if err := os.Mkdir(removedRoot, 0o700); err != nil {
		t.Fatal(err)
	}
	removedWorkspace := mustWorkspace(t, removedRoot)
	if err := os.Remove(removedRoot); err != nil {
		t.Fatal(err)
	}

	for _, test := range []struct {
		name           string
		workspace      *Workspace
		executor       sandbox.CommandExecutor
		shell          string
		environment    []string
		timeout        time.Duration
		maxOutputBytes int
	}{
		{name: "nil executor", workspace: workspace, shell: "/bin/sh", environment: []string{}, timeout: time.Second, maxOutputBytes: 1},
		{name: "typed nil executor", workspace: workspace, executor: typedNilExecutor, shell: "/bin/sh", environment: []string{}, timeout: time.Second, maxOutputBytes: 1},
		{name: "nil workspace", executor: validExecutor, shell: "/bin/sh", environment: []string{}, timeout: time.Second, maxOutputBytes: 1},
		{name: "empty workspace state", workspace: invalidEmptyWorkspace, executor: validExecutor, shell: "/bin/sh", environment: []string{}, timeout: time.Second, maxOutputBytes: 1},
		{name: "noncanonical workspace state", workspace: invalidNoncanonicalWorkspace, executor: validExecutor, shell: "/bin/sh", environment: []string{}, timeout: time.Second, maxOutputBytes: 1},
		{name: "missing workspace state", workspace: removedWorkspace, executor: validExecutor, shell: "/bin/sh", environment: []string{}, timeout: time.Second, maxOutputBytes: 1},
		{name: "empty shell", workspace: workspace, executor: validExecutor, environment: []string{}, timeout: time.Second, maxOutputBytes: 1},
		{name: "whitespace shell", workspace: workspace, executor: validExecutor, shell: " \t\n", environment: []string{}, timeout: time.Second, maxOutputBytes: 1},
		{name: "NUL shell", workspace: workspace, executor: validExecutor, shell: "/bin/\x00sh", environment: []string{}, timeout: time.Second, maxOutputBytes: 1},
		{name: "nil environment", workspace: workspace, executor: validExecutor, shell: "/bin/sh", timeout: time.Second, maxOutputBytes: 1},
		{name: "zero timeout", workspace: workspace, executor: validExecutor, shell: "/bin/sh", environment: []string{}, maxOutputBytes: 1},
		{name: "negative timeout", workspace: workspace, executor: validExecutor, shell: "/bin/sh", environment: []string{}, timeout: -time.Second, maxOutputBytes: 1},
		{name: "zero output cap", workspace: workspace, executor: validExecutor, shell: "/bin/sh", environment: []string{}, timeout: time.Second},
		{name: "negative output cap", workspace: workspace, executor: validExecutor, shell: "/bin/sh", environment: []string{}, timeout: time.Second, maxOutputBytes: -1},
	} {
		t.Run(test.name, func(t *testing.T) {
			constructed, err := NewBashTool(test.workspace, test.executor, test.shell, test.environment, test.timeout, test.maxOutputBytes, []string{"must-not-appear"})
			if constructed != nil {
				t.Fatalf("NewBashTool() tool = %T, want nil", constructed)
			}
			if err == nil || err.Error() != "invalid sandboxed bash configuration" {
				t.Fatalf("NewBashTool() error = %v, want fixed safe error", err)
			}
		})
	}
}

func TestBashSandboxedConstructorHandlesFormerByteMarkerExhaustion(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	allBytes := make([]byte, 256)
	for i := range allBytes {
		allBytes[i] = byte(i)
	}

	constructed, err := NewBashTool(
		workspace,
		&fakeBashExecutor{},
		"/bin/sh",
		[]string{},
		time.Second,
		1024,
		[]string{string(allBytes)},
	)
	if err != nil || constructed == nil {
		t.Fatalf("NewBashTool() = (%T, %v), want safe construction", constructed, err)
	}
}

func TestBashSandboxedConstructorRegistersWithExplicitEmptyEnvironment(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	fake := &fakeBashExecutor{status: sandbox.ExitStatus{Code: 0}}
	constructed, err := NewBashTool(workspace, fake, "/bin/sh", []string{}, time.Second, 1024, nil)
	if err != nil {
		t.Fatalf("NewBashTool() error = %v", err)
	}
	registry, err := NewRegistry(constructed)
	if err != nil {
		t.Fatalf("NewRegistry() error = %v", err)
	}
	definitions := registry.Definitions()
	if len(definitions) != 1 || definitions[0].Name != "bash" {
		t.Fatalf("Definitions() = %#v", definitions)
	}
	result := registry.Execute(context.Background(), "bash", json.RawMessage(`{"command":"true"}`))
	if result.IsError {
		t.Fatalf("registry Execute() = %#v", result)
	}
	requests := fake.Requests()
	if len(requests) != 1 || requests[0].Env == nil || len(requests[0].Env) != 0 {
		t.Fatalf("delegated environment = %#v, want explicit non-nil empty", requests)
	}
}

type fakeTool struct {
	name string
}

func (f fakeTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{Name: f.name, Parameters: map[string]any{"type": "object"}}
}

func (f fakeTool) Execute(context.Context, json.RawMessage) Result {
	return Result{Content: "ok"}
}
