package tool

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"unicode/utf8"
)

func TestReadRejectsInvalidJSON(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	result := NewReadTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt"`))
	if !result.IsError || !strings.Contains(result.Content, "invalid JSON") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestReadRejectsUnknownFieldAndTrailingTokens(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("hello\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	read := NewReadTool(workspace, 51200)

	unknown := read.Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","extra":true}`))
	if !unknown.IsError || !strings.Contains(unknown.Content, "unknown field") {
		t.Fatalf("unexpected unknown-field result: %#v", unknown)
	}

	trailing := read.Execute(context.Background(), json.RawMessage(`{"path":"sample.txt"} true`))
	if !trailing.IsError || !strings.Contains(trailing.Content, "trailing") {
		t.Fatalf("unexpected trailing-token result: %#v", trailing)
	}
}

func TestReadRejectsMissingRequiredPath(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	result := NewReadTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{}`))
	if !result.IsError || !strings.Contains(result.Content, "path") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestReadRejectsBinaryFile(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "binary"), []byte{'a', 0, 'b'}, 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewReadTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"path":"binary"}`))
	if !result.IsError || !strings.Contains(result.Content, "binary") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestReadRejectsInvalidUTF8(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "invalid.txt"), []byte{0xff, 0xfe}, 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewReadTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"path":"invalid.txt"}`))
	if !result.IsError || !strings.Contains(result.Content, "UTF-8") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestReadSupportsOffsetAndLimit(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("one\ntwo\nthree\nfour\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewReadTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","offset":2,"limit":2}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	if got, want := result.Content, "two\nthree\n"; got != want {
		t.Fatalf("content = %q, want %q", got, want)
	}
}

func TestReadReportsOutputTruncation(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("abcdefghi"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewReadTool(workspace, 5).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt"}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	if !strings.HasPrefix(result.Content, "abcde") || !strings.Contains(result.Content, "truncated") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestReadTruncationRemainsValidUTF8(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("é"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewReadTool(workspace, 1).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt"}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	if !utf8.ValidString(result.Content) {
		t.Fatalf("result content is not valid UTF-8: %q", result.Content)
	}
	if !strings.Contains(result.Content, "truncated") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestReadRejectsTraversal(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(filepath.Dir(root), "escape.txt"), []byte("nope"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewReadTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"path":"../escape.txt"}`))
	if !result.IsError || !strings.Contains(result.Content, "escapes workspace") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestWriteRejectsInvalidJSONUnknownFieldAndTrailingTokens(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	write := NewWriteTool(workspace)

	invalid := write.Execute(context.Background(), json.RawMessage(`{"path":"file.txt","content":"x"`))
	if !invalid.IsError || !strings.Contains(invalid.Content, "invalid JSON") {
		t.Fatalf("unexpected invalid-json result: %#v", invalid)
	}

	unknown := write.Execute(context.Background(), json.RawMessage(`{"path":"file.txt","content":"x","extra":true}`))
	if !unknown.IsError || !strings.Contains(unknown.Content, "unknown field") {
		t.Fatalf("unexpected unknown-field result: %#v", unknown)
	}

	trailing := write.Execute(context.Background(), json.RawMessage(`{"path":"file.txt","content":"x"} []`))
	if !trailing.IsError || !strings.Contains(trailing.Content, "trailing") {
		t.Fatalf("unexpected trailing-token result: %#v", trailing)
	}
}

func TestWriteRejectsMissingRequiredPath(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	result := NewWriteTool(workspace).Execute(context.Background(), json.RawMessage(`{"content":"hello"}`))
	if !result.IsError || !strings.Contains(result.Content, "path") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestWriteIsAtomicAndCreatesParents(t *testing.T) {
	root := t.TempDir()
	workspace := mustWorkspace(t, root)
	result := NewWriteTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"nested/file.txt","content":"hello"}`))
	if result.IsError {
		t.Fatal(result.Content)
	}
	data, err := os.ReadFile(filepath.Join(root, "nested/file.txt"))
	if err != nil || string(data) != "hello" {
		t.Fatalf("data=%q err=%v", data, err)
	}
}

func TestWritePreservesExistingPermissions(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "file.txt")
	if err := os.WriteFile(path, []byte("before"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(path, 0o600); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewWriteTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"file.txt","content":"after"}`))
	if result.IsError {
		t.Fatal(result.Content)
	}
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := info.Mode().Perm(); got != 0o600 {
		t.Fatalf("mode = %#o, want %#o", got, 0o600)
	}
}

func TestWriteLeavesNoTemporaryFilesAfterSuccess(t *testing.T) {
	root := t.TempDir()
	workspace := mustWorkspace(t, root)
	result := NewWriteTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"file.txt","content":"hello"}`))
	if result.IsError {
		t.Fatal(result.Content)
	}
	entries, err := os.ReadDir(root)
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 1 || entries[0].Name() != "file.txt" {
		t.Fatalf("unexpected directory entries: %v", entryNames(entries))
	}
}

func TestWriteRejectsSymlinkEscape(t *testing.T) {
	root := t.TempDir()
	outside := t.TempDir()
	if err := os.Symlink(outside, filepath.Join(root, "link")); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewWriteTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"link/file.txt","content":"hello"}`))
	if !result.IsError || !strings.Contains(result.Content, "escapes workspace") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestEditRejectsInvalidJSONUnknownFieldAndTrailingTokens(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("same\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	edit := NewEditTool(workspace)

	invalid := edit.Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"same","new_text":"new"`))
	if !invalid.IsError || !strings.Contains(invalid.Content, "invalid JSON") {
		t.Fatalf("unexpected invalid-json result: %#v", invalid)
	}

	unknown := edit.Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"same","new_text":"new","extra":true}`))
	if !unknown.IsError || !strings.Contains(unknown.Content, "unknown field") {
		t.Fatalf("unexpected unknown-field result: %#v", unknown)
	}

	trailing := edit.Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"same","new_text":"new"} 1`))
	if !trailing.IsError || !strings.Contains(trailing.Content, "trailing") {
		t.Fatalf("unexpected trailing-token result: %#v", trailing)
	}
}

func TestEditRejectsMissingRequiredArguments(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("same\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)

	missingPath := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{"old_text":"same","new_text":"new"}`))
	if !missingPath.IsError || !strings.Contains(missingPath.Content, "path") {
		t.Fatalf("unexpected missing-path result: %#v", missingPath)
	}

	missingOldText := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","new_text":"new"}`))
	if !missingOldText.IsError || !strings.Contains(missingOldText.Content, "old_text") {
		t.Fatalf("unexpected missing-old_text result: %#v", missingOldText)
	}
}

func TestEditRejectsAmbiguousMatch(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "sample.txt")
	if err := os.WriteFile(path, []byte("same\nsame\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	edit := NewEditTool(workspace)
	result := edit.Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"same","new_text":"new"}`))
	if !result.IsError || !strings.Contains(result.Content, "2 occurrences") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestEditRejectsAbsentMatch(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("same\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"missing","new_text":"new"}`))
	if !result.IsError || !strings.Contains(result.Content, "0 occurrences") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestEditSucceedsWithExactSingleMatch(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "sample.txt")
	if err := os.WriteFile(path, []byte("hello world\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"world","new_text":"there"}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := string(data), "hello there\n"; got != want {
		t.Fatalf("content = %q, want %q", got, want)
	}
	if !strings.Contains(result.Content, "sample.txt") || !strings.Contains(result.Content, "5") {
		t.Fatalf("unexpected result: %#v", result)
	}
	if strings.Contains(result.Content, "hello there") || strings.Contains(result.Content, "hello world") {
		t.Fatalf("result leaked file content: %#v", result)
	}
}

func TestEditRejectsTraversal(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(filepath.Dir(root), "escape.txt"), []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"../escape.txt","old_text":"x","new_text":"y"}`))
	if !result.IsError || !strings.Contains(result.Content, "escapes workspace") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func mustWorkspace(t *testing.T, root string) *Workspace {
	t.Helper()
	workspace, err := NewWorkspace(root)
	if err != nil {
		t.Fatal(err)
	}
	return workspace
}

func entryNames(entries []os.DirEntry) []string {
	names := make([]string, 0, len(entries))
	for _, entry := range entries {
		names = append(names, entry.Name())
	}
	return names
}
