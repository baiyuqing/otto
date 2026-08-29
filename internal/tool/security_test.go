package tool

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestReadRejectsOversizedFile(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "huge.txt")
	if err := os.WriteFile(path, []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.Truncate(path, maxReadFileBytes+1); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewReadTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"path":"huge.txt"}`))
	if !result.IsError || !strings.Contains(result.Content, "too large") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestEditRejectsOversizedFile(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "huge.txt")
	if err := os.WriteFile(path, []byte("target\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.Truncate(path, maxReadFileBytes+1); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"huge.txt","old_text":"target","new_text":"changed"}`))
	if !result.IsError || !strings.Contains(result.Content, "too large") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestReadAcceptsFileAtLimit(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "limit.txt")
	content := strings.Repeat("a", maxReadFileBytes)
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewReadTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"path":"limit.txt"}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
}
