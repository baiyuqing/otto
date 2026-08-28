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
	if count == 0 {
		return Result{Content: fmt.Sprintf("edit failed: old_text was not found in %s", args.Path), IsError: true}
	}
	if count > 1 {
		return Result{Content: fmt.Sprintf("edit failed: old_text matched %d locations in %s; include more surrounding context to make it unique", count, args.Path), IsError: true}
	}

	replaced := strings.Replace(text, args.OldText, args.NewText, 1)
	if err := writeFileAtomic(path, []byte(replaced)); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	return Result{Content: fmt.Sprintf("edited %s\n%s", args.Path, editDiff(text, replaced))}
}

const (
	diffContextLines = 3
	maxDiffBytes     = 4096
)

// editDiff renders a unified-style hunk for a single-match replacement.
// The edit is one contiguous region, so trimming common prefix and suffix
// lines between the two file versions yields the exact changed range.
func editDiff(before, after string) string {
	if before == after {
		return "(no textual changes)"
	}
	oldLines := strings.Split(before, "\n")
	newLines := strings.Split(after, "\n")

	prefix := 0
	for prefix < len(oldLines) && prefix < len(newLines) && oldLines[prefix] == newLines[prefix] {
		prefix++
	}
	suffix := 0
	for suffix < len(oldLines)-prefix && suffix < len(newLines)-prefix &&
		oldLines[len(oldLines)-1-suffix] == newLines[len(newLines)-1-suffix] {
		suffix++
	}

	contextStart := prefix - diffContextLines
	if contextStart < 0 {
		contextStart = 0
	}
	contextEnd := len(oldLines) - suffix + diffContextLines
	if contextEnd > len(oldLines) {
		contextEnd = len(oldLines)
	}

	oldCount := contextEnd - contextStart
	newCount := oldCount - (len(oldLines) - suffix - prefix) + (len(newLines) - suffix - prefix)

	var builder strings.Builder
	fmt.Fprintf(&builder, "@@ -%d,%d +%d,%d @@\n", contextStart+1, oldCount, contextStart+1, newCount)
	for _, line := range oldLines[contextStart:prefix] {
		builder.WriteString(" " + line + "\n")
	}
	for _, line := range oldLines[prefix : len(oldLines)-suffix] {
		builder.WriteString("-" + line + "\n")
	}
	for _, line := range newLines[prefix : len(newLines)-suffix] {
		builder.WriteString("+" + line + "\n")
	}
	for _, line := range oldLines[len(oldLines)-suffix : contextEnd] {
		builder.WriteString(" " + line + "\n")
	}

	diff := strings.TrimSuffix(builder.String(), "\n")
	if len(diff) > maxDiffBytes {
		cut := strings.LastIndexByte(diff[:maxDiffBytes], '\n')
		if cut < 0 {
			cut = maxDiffBytes
		}
		diff = diff[:cut] + "\n... (diff truncated)"
	}
	return diff
}
