package tool

import (
	"context"
	"encoding/json"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"strings"

	"github.com/baiyuqing/otto/internal/model"
)

const (
	defaultFindLimit = 1000
	maximumFindLimit = 10000
)

var errFindLimitReached = errors.New("find result limit reached")

type findTool struct {
	workspace      *Workspace
	maxOutputBytes int
}

type findArgs struct {
	Pattern string `json:"pattern"`
	Path    string `json:"path,omitempty"`
	Limit   *int   `json:"limit,omitempty"`
}

func NewFindTool(workspace *Workspace, maxOutputBytes int) Tool {
	return &findTool{workspace: workspace, maxOutputBytes: maxOutputBytes}
}

func (t *findTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{
		Name:        "find",
		Description: "Find workspace files by glob pattern (read-only)",
		Parameters: map[string]any{
			"type":                 "object",
			"additionalProperties": false,
			"properties": map[string]any{
				"pattern": map[string]any{"type": "string", "description": "Relative glob pattern; supports recursive ** segments"},
				"path":    map[string]any{"type": "string", "description": "Workspace-relative directory or file to search; defaults to ."},
				"limit":   map[string]any{"type": "integer", "minimum": 1, "maximum": maximumFindLimit, "description": "Maximum matching files; defaults to 1000"},
			},
			"required": []string{"pattern"},
		},
	}
}

func (t *findTool) Execute(ctx context.Context, arguments json.RawMessage) Result {
	var args findArgs
	if err := DecodeStrictJSON(arguments, &args, "pattern"); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if args.Pattern == "" {
		return Result{Content: "pattern must not be empty", IsError: true}
	}
	globSegments, err := validatedGlobSegments(args.Pattern)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	limit, err := resolveSearchLimit(args.Limit, defaultFindLimit, maximumFindLimit)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if err := ctx.Err(); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	searchPath := args.Path
	if searchPath == "" {
		searchPath = "."
	}
	root, err := t.workspace.existingRelative(searchPath)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	insideGit, err := searchRootInsideGit(t.workspace, searchPath, root)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if insideGit {
		return Result{}
	}

	matches := make([]string, 0, min(limit, 128))
	truncated := false
	err = fs.WalkDir(t.workspace.rootFS.FS(), root, func(filePath string, entry fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		if err := ctx.Err(); err != nil {
			return err
		}
		if filePath != root && entry.Name() == ".git" {
			if entry.IsDir() {
				return filepath.SkipDir
			}
			return nil
		}
		if entry.IsDir() {
			return nil
		}
		if entry.Type()&os.ModeSymlink != 0 {
			return nil
		}
		info, err := entry.Info()
		if err != nil {
			return err
		}
		if !info.Mode().IsRegular() {
			return nil
		}
		candidate, err := searchRelativePath(root, filePath)
		if err != nil {
			return err
		}
		if !matchGlobSegments(globSegments, candidate) {
			return nil
		}
		if len(matches) >= limit {
			truncated = true
			return errFindLimitReached
		}
		matches = append(matches, filepath.ToSlash(filePath))
		return nil
	})
	if err != nil && !errors.Is(err, errFindLimitReached) {
		return Result{Content: err.Error(), IsError: true}
	}

	var output strings.Builder
	for _, match := range matches {
		output.WriteString(match)
		output.WriteByte('\n')
	}
	if truncated {
		output.WriteString("[truncated: result limit reached]\n")
	}
	return CappedTextResult(output.String(), t.maxOutputBytes)
}

var _ Tool = (*findTool)(nil)
