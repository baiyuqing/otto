package tool

import (
	"context"
	"encoding/json"
	"testing"

	"github.com/baiyuqing/otto/internal/model"
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

type fakeTool struct {
	name string
}

func (f fakeTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{Name: f.name, Parameters: map[string]any{"type": "object"}}
}

func (f fakeTool) Execute(context.Context, json.RawMessage) Result {
	return Result{Content: "ok"}
}
