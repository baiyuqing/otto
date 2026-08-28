package tool

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
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
		Description: "List one level of a workspace directory",
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
	if err := decodeStrictJSON(arguments, &args); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if err := ctx.Err(); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	requestedPath := args.Path
	if requestedPath == "" {
		requestedPath = "."
	}
	directory, err := t.workspace.ResolveExisting(requestedPath)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	info, err := os.Stat(directory)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if !info.IsDir() {
		return Result{Content: fmt.Sprintf("not a directory: %s", args.Path), IsError: true}
	}
	entries, err := os.ReadDir(directory)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
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
	return cappedTextResult(output.String(), t.maxOutputBytes)
}

var _ Tool = (*lsTool)(nil)
