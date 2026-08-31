package tool

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
	"github.com/baiyuqing/otto/internal/model"
)

const (
	memorySearchLimit       = 20
	memorySearchTokenBudget = 4000
)

type memorySearchTool struct {
	reader         memory.Reader
	scopes         []memory.Scope
	maxOutputBytes int
}

type memorySearchArgs struct {
	Query string `json:"query"`
}

func NewMemorySearchTool(reader memory.Reader, scopes []memory.Scope, maxOutputBytes int) Tool {
	return &memorySearchTool{reader: reader, scopes: scopes, maxOutputBytes: maxOutputBytes}
}

func (t *memorySearchTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{
		Name:        "memory_search",
		Description: "Search remembered facts and preferences for the current user and workspace",
		Parameters: map[string]any{
			"type":                 "object",
			"additionalProperties": false,
			"properties": map[string]any{
				"query": map[string]any{
					"type":        "string",
					"description": "Search terms; empty lists recently active records",
				},
			},
		},
	}
}

func (t *memorySearchTool) Execute(ctx context.Context, arguments json.RawMessage) Result {
	var args memorySearchArgs
	if err := decodeStrictJSON(arguments, &args); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if err := ctx.Err(); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	result, err := t.reader.Search(ctx, memory.SearchRequest{
		Query:       args.Query,
		Scopes:      t.scopes,
		Limit:       memorySearchLimit,
		TokenBudget: memorySearchTokenBudget,
		Now:         time.Now().UTC(),
	})
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if len(result.Records) == 0 {
		return Result{Content: "no matching records", PersistedContent: "0 records"}
	}

	var content strings.Builder
	ids := make([]string, len(result.Records))
	for i, record := range result.Records {
		ids[i] = record.ID
		fmt.Fprintf(&content, "id=%s scope=%s/%s kind=%s key=%s revision=%d text=%s\n",
			record.ID, record.Scope.Namespace, record.Scope.ID, record.Kind, record.Key, record.Revision, record.Text)
	}
	rendered := cappedTextResult(content.String(), t.maxOutputBytes)
	rendered.PersistedContent = fmt.Sprintf("%d records: %s", len(result.Records), strings.Join(ids, ", "))
	return rendered
}

var _ Tool = (*memorySearchTool)(nil)
