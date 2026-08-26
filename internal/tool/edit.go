package tool

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/baiyuqing/otto/internal/model"
)

type editTool struct {
	workspace *Workspace
}

type editArgs struct {
	Path    string `json:"path"`
	OldText string `json:"old_text"`
	NewText string `json:"new_text"`
}

func NewEditTool(workspace *Workspace) Tool {
	return &editTool{workspace: workspace}
}

func (t *editTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{
		Name:        "edit",
		Description: "Replace exactly one matching text fragment in a workspace file",
		Parameters: map[string]any{
			"type":                 "object",
			"additionalProperties": false,
			"properties": map[string]any{
				"path": map[string]any{
					"type":        "string",
					"description": "Workspace-relative file path to edit",
				},
				"old_text": map[string]any{
					"type":        "string",
					"description": "Exact existing text to replace",
				},
				"new_text": map[string]any{
					"type":        "string",
					"description": "Replacement text",
				},
			},
			"required": []string{"path", "old_text", "new_text"},
		},
	}
}

func (t *editTool) Execute(_ context.Context, arguments json.RawMessage) Result {
	var args editArgs
	if err := decodeStrictJSON(arguments, &args, "path", "old_text", "new_text"); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if args.Path == "" {
		return Result{Content: "missing required argument: path", IsError: true}
	}
	if args.OldText == "" {
		return Result{Content: "missing required argument: old_text", IsError: true}
	}

	path, err := t.workspace.ResolveExisting(args.Path)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	text, err := readValidatedTextFile(path)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}

	count := strings.Count(text, args.OldText)
	if count != 1 {
		return Result{Content: fmt.Sprintf("expected exactly one match for %q in %s, found %d occurrences", args.OldText, args.Path, count), IsError: true}
	}

	replaced := strings.Replace(text, args.OldText, args.NewText, 1)
	if err := writeFileAtomic(path, []byte(replaced)); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	return Result{Content: fmt.Sprintf("edited %s (%d bytes replaced with %d bytes)", args.Path, len(args.OldText), len(args.NewText))}
}
