package tool

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
	"time"
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

func TestReadKeepsOriginalWorkspaceAfterPathIsReplaced(t *testing.T) {
	parent := t.TempDir()
	root := filepath.Join(parent, "workspace")
	if err := os.Mkdir(root, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(root, "note.txt"), []byte("inside"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)

	moved := filepath.Join(parent, "moved")
	if err := os.Rename(root, moved); err != nil {
		t.Fatal(err)
	}
	outside := t.TempDir()
	if err := os.WriteFile(filepath.Join(outside, "note.txt"), []byte("outside secret"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(outside, root); err != nil {
		t.Fatal(err)
	}

	result := NewReadTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"path":"note.txt"}`))
	if result.IsError || result.Content != "inside" {
		t.Fatalf("Read() = %#v, want original workspace content", result)
	}
}

func TestWriteKeepsOriginalWorkspaceAfterPathIsReplaced(t *testing.T) {
	parent := t.TempDir()
	root := filepath.Join(parent, "workspace")
	if err := os.Mkdir(root, 0o755); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)

	moved := filepath.Join(parent, "moved")
	if err := os.Rename(root, moved); err != nil {
		t.Fatal(err)
	}
	outside := t.TempDir()
	if err := os.Symlink(outside, root); err != nil {
		t.Fatal(err)
	}

	result := NewWriteTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"note.txt","content":"inside"}`))
	if result.IsError {
		t.Fatalf("Write() = %#v", result)
	}
	inside, err := os.ReadFile(filepath.Join(moved, "note.txt"))
	if err != nil || string(inside) != "inside" {
		t.Fatalf("original workspace content = %q, %v", inside, err)
	}
	if _, err := os.Stat(filepath.Join(outside, "note.txt")); !os.IsNotExist(err) {
		t.Fatalf("outside file exists: %v", err)
	}
}

func TestReadRejectsFIFOWithoutBlocking(t *testing.T) {
	root := t.TempDir()
	if err := syscall.Mkfifo(filepath.Join(root, "pipe"), 0o600); err != nil {
		t.Fatal(err)
	}
	read := NewReadTool(mustWorkspace(t, root), 51200)
	done := make(chan Result, 1)
	go func() {
		done <- read.Execute(context.Background(), json.RawMessage(`{"path":"pipe"}`))
	}()
	var result Result
	select {
	case result = <-done:
	case <-time.After(2 * time.Second):
		writer, err := os.OpenFile(filepath.Join(root, "pipe"), os.O_WRONLY|syscall.O_NONBLOCK, 0)
		if err == nil {
			_ = writer.Close()
		}
		select {
		case result = <-done:
		case <-time.After(time.Second):
			t.Fatal("Read blocked opening FIFO")
		}
		t.Fatal("Read blocked opening FIFO")
	}
	if !result.IsError || !strings.Contains(result.Content, "not a regular file") {
		t.Fatalf("Read() = %#v, want regular-file rejection", result)
	}
}

func TestWriteAcceptsNewAbsolutePathInsideWorkspace(t *testing.T) {
	root := t.TempDir()
	workspace := mustWorkspace(t, root)
	path := filepath.Join(root, "new.txt")
	payload, err := json.Marshal(map[string]string{"path": path, "content": "inside"})
	if err != nil {
		t.Fatal(err)
	}
	result := NewWriteTool(workspace).Execute(context.Background(), payload)
	if result.IsError {
		t.Fatalf("Write() = %#v", result)
	}
}

func TestWriteRejectsSymlinkParentTraversal(t *testing.T) {
	root := t.TempDir()
	outside := t.TempDir()
	if err := os.Symlink(outside, filepath.Join(root, "link")); err != nil {
		t.Fatal(err)
	}
	result := NewWriteTool(mustWorkspace(t, root)).Execute(context.Background(), json.RawMessage(`{"path":"link/../note.txt","content":"nope"}`))
	if !result.IsError {
		t.Fatalf("Write() = %#v, want escape rejection", result)
	}
	if _, err := os.Stat(filepath.Join(root, "note.txt")); !os.IsNotExist(err) {
		t.Fatalf("workspace file exists: %v", err)
	}
}

func TestWriteAndEditFollowInternalFinalSymlinks(t *testing.T) {
	root := t.TempDir()
	target := filepath.Join(root, "target.txt")
	if err := os.WriteFile(target, []byte("old"), 0o640); err != nil {
		t.Fatal(err)
	}
	for _, link := range []string{"relative-link", "absolute-link"} {
		targetPath := "target.txt"
		if link == "absolute-link" {
			targetPath = target
		}
		if err := os.Symlink(targetPath, filepath.Join(root, link)); err != nil {
			t.Fatal(err)
		}
	}
	workspace := mustWorkspace(t, root)

	for _, path := range []string{"relative-link", "absolute-link"} {
		result := NewWriteTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"`+path+`","content":"write"}`))
		if result.IsError {
			t.Fatalf("Write(%q) = %#v", path, result)
		}
		result = NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"`+path+`","old_text":"write","new_text":"edit"}`))
		if result.IsError {
			t.Fatalf("Edit(%q) = %#v", path, result)
		}
		info, err := os.Lstat(filepath.Join(root, path))
		if err != nil || info.Mode()&os.ModeSymlink == 0 {
			t.Fatalf("link %q was replaced: %v", path, err)
		}
	}
	data, err := os.ReadFile(target)
	if err != nil || string(data) != "edit" {
		t.Fatalf("target = %q, %v", data, err)
	}
}
