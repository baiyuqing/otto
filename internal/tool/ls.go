package tool

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"sort"
	"strings"

	"github.com/baiyuqing/otto/internal/model"
)

type lsTool struct {
	workspace      *Workspace
	maxOutputBytes int
}

type lsArgs struct {
	Path string `json:"path,omitempty"`
}

func NewLSTool(workspace *Workspace, maxOutputBytes int) Tool {
	return &lsTool{workspace: workspace, maxOutputBytes: maxOutputBytes}
}

func (t *lsTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{
		Name:        "ls",
		Description: "List one level of a workspace directory (read-only)",
		Parameters: map[string]any{
			"type":                 "object",
			"additionalProperties": false,
			"properties": map[string]any{
				"path": map[string]any{"type": "string", "description": "Workspace-relative directory to list; defaults to ."},
			},
		},
	}
}

func (t *lsTool) Execute(ctx context.Context, arguments json.RawMessage) Result {
	var args lsArgs
	if err := DecodeStrictJSON(arguments, &args); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if err := ctx.Err(); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	requestedPath := args.Path
	if requestedPath == "" {
		requestedPath = "."
	}
	directory, err := t.workspace.existingRelative(requestedPath)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	file, err := t.workspace.openRelative(directory)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if !info.IsDir() {
		return Result{Content: fmt.Sprintf("not a directory: %s", args.Path), IsError: true}
	}
	entries, err := file.ReadDir(-1)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	sort.Slice(entries, func(i, j int) bool { return entries[i].Name() < entries[j].Name() })
	var output strings.Builder
	for _, entry := range entries {
		if err := ctx.Err(); err != nil {
			return Result{Content: err.Error(), IsError: true}
		}
		output.WriteString(entry.Name())
		switch {
		case entry.Type()&os.ModeSymlink != 0:
			output.WriteByte('@')
		case entry.IsDir():
			output.WriteByte('/')
		}
		output.WriteByte('\n')
	}
	return CappedTextResult(output.String(), t.maxOutputBytes)
}

var _ Tool = (*lsTool)(nil)
