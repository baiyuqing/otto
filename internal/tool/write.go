package tool

import (
	"context"
	"crypto/rand"
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

	path, err := t.workspace.writeRelative(args.Path)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if err := writeFileAtomic(t.workspace, path, []byte(args.Content)); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	return Result{Content: fmt.Sprintf("wrote %s (%d bytes)", args.Path, len(args.Content))}
}

func writeFileAtomic(workspace *Workspace, path string, content []byte) (err error) {
	directory := filepath.Dir(path)
	if err := workspace.rootFS.MkdirAll(directory, 0o755); err != nil {
		return err
	}

	mode := os.FileMode(0o644)
	if info, statErr := workspace.rootFS.Stat(path); statErr == nil {
		if info.IsDir() {
			return fmt.Errorf("path is a directory: %s", path)
		}
		mode = info.Mode().Perm()
	} else if !os.IsNotExist(statErr) {
		return statErr
	}

	tmpPath, tmp, err := createWorkspaceTemp(workspace, directory, mode)
	if err != nil {
		return err
	}
	cleanup := true
	defer func() {
		if !cleanup {
			return
		}
		_ = tmp.Close()
		_ = workspace.rootFS.Remove(tmpPath)
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
	if err := workspace.rootFS.Rename(tmpPath, path); err != nil {
		return err
	}
	cleanup = false
	return nil
}

func createWorkspaceTemp(workspace *Workspace, directory string, mode os.FileMode) (string, *os.File, error) {
	for range 100 {
		var suffix [8]byte
		if _, err := rand.Read(suffix[:]); err != nil {
			return "", nil, err
		}
		name := filepath.Join(directory, fmt.Sprintf(".otto-%x", suffix))
		file, err := workspace.rootFS.OpenFile(name, os.O_WRONLY|os.O_CREATE|os.O_EXCL, mode)
		if err == nil {
			return name, file, nil
		}
		if !os.IsExist(err) {
			return "", nil, err
		}
	}
	return "", nil, fmt.Errorf("could not create temporary file")
}
