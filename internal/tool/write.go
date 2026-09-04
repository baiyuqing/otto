package tool

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"

	"github.com/baiyuqing/otto/internal/model"
)

type writeTool struct {
	workspace *Workspace
}

type writeArgs struct {
	Path    string `json:"path"`
	Content string `json:"content"`
}

func NewWriteTool(workspace *Workspace) Tool {
	return &writeTool{workspace: workspace}
}

func (t *writeTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{
		Name:        "write",
		Description: "Create or replace a file in the workspace",
		Parameters: map[string]any{
			"type":                 "object",
			"additionalProperties": false,
			"properties": map[string]any{
				"path": map[string]any{
					"type":        "string",
					"description": "Workspace-relative file path to write",
				},
				"content": map[string]any{
					"type":        "string",
					"description": "Full file content",
				},
			},
			"required": []string{"path", "content"},
		},
	}
}

func (t *writeTool) Execute(_ context.Context, arguments json.RawMessage) Result {
	var args writeArgs
	if err := DecodeStrictJSON(arguments, &args, "path", "content"); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if args.Path == "" {
		return Result{Content: "missing required argument: path", IsError: true}
	}

	path, err := t.workspace.ResolveForWrite(args.Path)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if err := writeFileAtomic(path, []byte(args.Content)); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	return Result{Content: fmt.Sprintf("wrote %s (%d bytes)", args.Path, len(args.Content))}
}

func writeFileAtomic(path string, content []byte) (err error) {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}

	mode := os.FileMode(0o644)
	if info, statErr := os.Stat(path); statErr == nil {
		if info.IsDir() {
			return fmt.Errorf("path is a directory: %s", path)
		}
		mode = info.Mode().Perm()
	} else if !os.IsNotExist(statErr) {
		return statErr
	}

	tmp, err := os.CreateTemp(filepath.Dir(path), ".otto-*")
	if err != nil {
		return err
	}
	tmpPath := tmp.Name()
	cleanup := true
	defer func() {
		if !cleanup {
			return
		}
		_ = tmp.Close()
		_ = os.Remove(tmpPath)
	}()

	if err := tmp.Chmod(mode); err != nil {
		return err
	}
	if _, err := tmp.Write(content); err != nil {
		return err
	}
	if err := tmp.Sync(); err != nil {
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	if err := os.Rename(tmpPath, path); err != nil {
		return err
	}
	cleanup = false
	return nil
}
