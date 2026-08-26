package tool

import (
	"os"
	"path/filepath"
	"testing"
)

func TestWorkspaceRejectsParentTraversal(t *testing.T) {
	root := t.TempDir()
	workspace, err := NewWorkspace(root)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := workspace.ResolveForWrite("../escape.txt"); err == nil {
		t.Fatal("expected traversal rejection")
	}
}

func TestWorkspaceRejectsSymlinkEscape(t *testing.T) {
	root := t.TempDir()
	outside := t.TempDir()
	if err := os.Symlink(outside, filepath.Join(root, "link")); err != nil {
		t.Fatal(err)
	}
	workspace, err := NewWorkspace(root)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := workspace.ResolveForWrite("link/new.txt"); err == nil {
		t.Fatal("expected symlink escape rejection")
	}
}

func TestWorkspaceResolvesNestedMissingRelativePath(t *testing.T) {
	root := t.TempDir()
	canonicalRoot, err := filepath.EvalSymlinks(root)
	if err != nil {
		t.Fatal(err)
	}
	workspace, err := NewWorkspace(root)
	if err != nil {
		t.Fatal(err)
	}

	got, err := workspace.ResolveForWrite(filepath.Join("nested", "dir", "file.txt"))
	if err != nil {
		t.Fatal(err)
	}
	want := filepath.Join(canonicalRoot, "nested", "dir", "file.txt")
	if got != want {
		t.Fatalf("ResolveForWrite() = %q, want %q", got, want)
	}
}

func TestWorkspaceResolvesAbsolutePathInsideWorkspace(t *testing.T) {
	root := t.TempDir()
	canonicalRoot, err := filepath.EvalSymlinks(root)
	if err != nil {
		t.Fatal(err)
	}
	workspace, err := NewWorkspace(root)
	if err != nil {
		t.Fatal(err)
	}

	want := filepath.Join(canonicalRoot, "nested", "file.txt")
	got, err := workspace.ResolveForWrite(want)
	if err != nil {
		t.Fatal(err)
	}
	if got != want {
		t.Fatalf("ResolveForWrite() = %q, want %q", got, want)
	}
}

func TestWorkspaceRejectsAbsolutePathOutsideWorkspace(t *testing.T) {
	root := t.TempDir()
	outside := t.TempDir()
	workspace, err := NewWorkspace(root)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := workspace.ResolveForWrite(filepath.Join(outside, "file.txt")); err == nil {
		t.Fatal("expected absolute path outside workspace rejection")
	}
}

func TestWorkspaceResolvesSymlinkThatStaysInside(t *testing.T) {
	root := t.TempDir()
	canonicalRoot, err := filepath.EvalSymlinks(root)
	if err != nil {
		t.Fatal(err)
	}
	inside := filepath.Join(root, "inside")
	if err := os.MkdirAll(inside, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(inside, filepath.Join(root, "link")); err != nil {
		t.Fatal(err)
	}
	workspace, err := NewWorkspace(root)
	if err != nil {
		t.Fatal(err)
	}

	got, err := workspace.ResolveForWrite(filepath.Join("link", "new.txt"))
	if err != nil {
		t.Fatal(err)
	}
	want := filepath.Join(canonicalRoot, "inside", "new.txt")
	if got != want {
		t.Fatalf("ResolveForWrite() = %q, want %q", got, want)
	}
}

func TestWorkspaceResolvesRootEquality(t *testing.T) {
	root := t.TempDir()
	canonicalRoot, err := filepath.EvalSymlinks(root)
	if err != nil {
		t.Fatal(err)
	}
	workspace, err := NewWorkspace(root)
	if err != nil {
		t.Fatal(err)
	}

	got, err := workspace.ResolveForWrite(root)
	if err != nil {
		t.Fatal(err)
	}
	if got != canonicalRoot {
		t.Fatalf("ResolveForWrite() = %q, want %q", got, canonicalRoot)
	}
}

func TestWorkspaceResolveExistingFollowsSymlinks(t *testing.T) {
	root := t.TempDir()
	canonicalRoot, err := filepath.EvalSymlinks(root)
	if err != nil {
		t.Fatal(err)
	}
	inside := filepath.Join(root, "inside")
	if err := os.MkdirAll(inside, 0o755); err != nil {
		t.Fatal(err)
	}
	want := filepath.Join(canonicalRoot, "inside", "file.txt")
	if err := os.WriteFile(want, []byte("ok"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(inside, filepath.Join(root, "link")); err != nil {
		t.Fatal(err)
	}
	workspace, err := NewWorkspace(root)
	if err != nil {
		t.Fatal(err)
	}

	got, err := workspace.ResolveExisting(filepath.Join("link", "file.txt"))
	if err != nil {
		t.Fatal(err)
	}
	if got != want {
		t.Fatalf("ResolveExisting() = %q, want %q", got, want)
	}
}
