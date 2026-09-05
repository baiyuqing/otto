package tool

import (
	"context"
	"encoding/json"
	"fmt"

	"github.com/baiyuqing/otto/internal/model"
)

type Tool interface {
	// Definition returns the tool schema owned by the caller. Implementations
	// must return independent mutable maps and slices and must not retain or
	// mutate them after return.
	Definition() model.ToolDefinition
	// Execute may be called concurrently on a shared tool. arguments is borrowed
	// read-only for the duration of the call and must not be retained or mutated.
	// The returned Result, including PersistedContent when non-nil, belongs to
	// the caller after Execute returns.
	Execute(context.Context, json.RawMessage) Result
}

type Registry struct {
	ordered []Tool
	byName  map[string]Tool
}

func NewRegistry(tools ...Tool) (*Registry, error) {
	registry := &Registry{
		ordered: make([]Tool, 0, len(tools)),
		byName:  make(map[string]Tool, len(tools)),
	}
	for _, tool := range tools {
		definition := tool.Definition()
		if _, exists := registry.byName[definition.Name]; exists {
			return nil, fmt.Errorf("duplicate tool: %s", definition.Name)
		}
		registry.ordered = append(registry.ordered, tool)
		registry.byName[definition.Name] = tool
	}
	return registry, nil
}

func (r *Registry) Definitions() []model.ToolDefinition {
	definitions := make([]model.ToolDefinition, 0, len(r.ordered))
	for _, tool := range r.ordered {
		definitions = append(definitions, tool.Definition())
	}
	return definitions
}

// Lookup returns the registered tool with the given name.
func (r *Registry) Lookup(name string) (Tool, bool) {
	tool, ok := r.byName[name]
	return tool, ok
}

// Tools returns the registered tools in registration order.
func (r *Registry) Tools() []Tool {
	return append([]Tool(nil), r.ordered...)
}

func (r *Registry) Execute(ctx context.Context, name string, args json.RawMessage) Result {
	tool, ok := r.byName[name]
	if !ok {
		return Result{Content: fmt.Sprintf("unknown tool: %s", name), IsError: true}
	}
	return tool.Execute(ctx, args)
}
