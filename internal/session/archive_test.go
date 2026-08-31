package session

import (
	"bytes"
	"context"
	"errors"
	"os"
	"path/filepath"
	"testing"
)

func TestArchiveMovesActiveSessionPreservingBytesAndMode(t *testing.T) {
	root := t.TempDir()
	workspace := t.TempDir()
	path := createListSession(t, root, workspace, "archive-me")
	before := readFile(t, path)
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	mode := info.Mode().Perm()

	result, err := Archive(context.Background(), root, workspace, path)
	if err != nil {
		t.Fatal(err)
	}
	if result.ID != "archive-me" {
		t.Fatalf("result.ID = %q, want archive-me", result.ID)
	}
	if filepath.Dir(result.Path) != filepath.Join(filepath.Dir(path), "archive") {
		t.Fatalf("result.Path = %q, want under archive/", result.Path)
	}
	if _, err := os.Stat(path); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("source still exists after archive: %v", err)
	}
	if !bytes.Equal(readFile(t, result.Path), before) {
		t.Fatal("archived file bytes changed")
	}
	archivedInfo, err := os.Stat(result.Path)
	if err != nil {
		t.Fatal(err)
	}
	if archivedInfo.Mode().Perm() != mode || archivedInfo.Mode().Perm() != 0o600 {
		t.Fatalf("archived mode = %v, want %v", archivedInfo.Mode().Perm(), mode)
	}
}

func TestArchiveRemovesSessionFromListButKeepsItResumable(t *testing.T) {
	root := t.TempDir()
	workspace := t.TempDir()
	archived := createListSession(t, root, workspace, "archived")
	active := createListSession(t, root, workspace, "active")

	result, err := List(context.Background(), root, workspace, "", 20)
	if err != nil {
		t.Fatal(err)
	}
	if len(result.Sessions) != 2 {
		t.Fatalf("before archive sessions = %#v", result.Sessions)
	}

	archivedResult, err := Archive(context.Background(), root, workspace, archived)
	if err != nil {
		t.Fatal(err)
	}
	result, err = List(context.Background(), root, workspace, active, 20)
	if err != nil {
		t.Fatal(err)
	}
	if len(result.Sessions) != 1 || result.Sessions[0].ID != "active" || !result.Sessions[0].Current {
		t.Fatalf("after archive sessions = %#v", result.Sessions)
	}

	if _, _, err := Inspect(context.Background(), archivedResult.Path); err != nil {
		t.Fatalf("inspect archived session: %v", err)
	}
	prepared, err := Prepare(context.Background(), archivedResult.Path)
	if err != nil {
		t.Fatalf("prepare archived session: %v", err)
	}
	defer prepared.Close()
}

func TestArchiveRejectsAlreadyArchivedPath(t *testing.T) {
	root := t.TempDir()
	workspace := t.TempDir()
	path := createListSession(t, root, workspace, "already")
	archived, err := Archive(context.Background(), root, workspace, path)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := Archive(context.Background(), root, workspace, archived.Path); !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("err = %v, want ErrInvalidSession", err)
	}
}

func TestArchiveRejectsSessionFromAnotherWorkspace(t *testing.T) {
	root := t.TempDir()
	workspaceA := t.TempDir()
	workspaceB := t.TempDir()
	path := createListSession(t, root, workspaceB, "other")
	if _, err := Archive(context.Background(), root, workspaceA, path); !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("err = %v, want ErrInvalidSession", err)
	}
}

func TestArchiveRejectsRecordedWorkspaceMismatch(t *testing.T) {
	root := t.TempDir()
	workspace := t.TempDir()
	directory := listSessionDirectory(t, root, workspace)
	path := filepath.Join(directory, "misrecorded.jsonl")
	createPiSessionAtPath(t, path, filepath.Join(t.TempDir(), "other"), "misrecorded")
	if _, err := Archive(context.Background(), root, workspace, path); !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("err = %v, want ErrInvalidSession", err)
	}
	before := readFile(t, path)
	if !bytes.Equal(readFile(t, path), before) {
		t.Fatal("rejected session file was mutated")
	}
}

func TestArchiveRejectsSymlinkedSource(t *testing.T) {
	root := t.TempDir()
	workspace := t.TempDir()
	real := createListSession(t, root, workspace, "real")
	directory := listSessionDirectory(t, root, workspace)
	link := filepath.Join(directory, "link.jsonl")
	if err := os.Symlink(real, link); err != nil {
		t.Fatal(err)
	}
	if _, err := Archive(context.Background(), root, workspace, link); !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("err = %v, want ErrInvalidSession", err)
	}
}

func TestArchiveRejectsNonRegularSource(t *testing.T) {
	root := t.TempDir()
	workspace := t.TempDir()
	directory := listSessionDirectory(t, root, workspace)
	path := filepath.Join(directory, "dir.jsonl")
	if err := os.MkdirAll(path, 0o700); err != nil {
		t.Fatal(err)
	}
	if _, err := Archive(context.Background(), root, workspace, path); !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("err = %v, want ErrInvalidSession", err)
	}
}

func TestArchiveRejectsInvalidPiSession(t *testing.T) {
	root := t.TempDir()
	workspace := t.TempDir()
	directory := listSessionDirectory(t, root, workspace)
	path := filepath.Join(directory, "invalid.jsonl")
	writeListRecords(t, path,
		`{"type":"session","version":3,"id":"invalid","timestamp":"2026-08-27T12:00:00Z","cwd":"`+workspace+`"}`,
		`{"type":"custom","id":"aaaaaaaa","parentId":null,"timestamp":"2026-08-27T12:00:01Z","customType":"otto.runtime","data":{"profile":"default","provider":"openai-compatible","model":"test-model"}}`,
		`{"type":"message","id":"aaaaaaaa","parentId":null,"timestamp":"2026-08-27T12:00:02Z","message":{"role":"user","content":[{"type":"text","text":"hi"}],"timestamp":1787817600000}}`,
	)
	before := readFile(t, path)
	if _, err := Archive(context.Background(), root, workspace, path); !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("err = %v, want ErrInvalidSession", err)
	}
	if !bytes.Equal(readFile(t, path), before) {
		t.Fatal("rejected session file was mutated")
	}
}

func TestArchiveRefusesExistingDestination(t *testing.T) {
	root := t.TempDir()
	workspace := t.TempDir()
	path := createListSession(t, root, workspace, "collide")
	directory := listSessionDirectory(t, root, workspace)
	archiveDir := filepath.Join(directory, "archive")
	if err := os.MkdirAll(archiveDir, 0o700); err != nil {
		t.Fatal(err)
	}
	existing := filepath.Join(archiveDir, "collide.jsonl")
	writeListRecords(t, existing, `{"type":"session","version":3,"id":"collide","timestamp":"2026-08-27T12:00:00Z","cwd":"`+workspace+`"}`)
	if _, err := Archive(context.Background(), root, workspace, path); !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("err = %v, want ErrInvalidSession", err)
	}
	if _, err := os.Stat(path); err != nil {
		t.Fatalf("source was moved despite destination collision: %v", err)
	}
	if !bytes.Equal(readFile(t, existing), readFile(t, existing)) {
		t.Fatal("existing destination was mutated")
	}
}

func TestArchiveRejectsSymlinkedArchiveDirectory(t *testing.T) {
	root := t.TempDir()
	workspace := t.TempDir()
	path := createListSession(t, root, workspace, "symlink-archive")
	directory := listSessionDirectory(t, root, workspace)
	if err := os.Symlink(filepath.Join(t.TempDir(), "elsewhere"), filepath.Join(directory, "archive")); err != nil {
		t.Fatal(err)
	}
	if _, err := Archive(context.Background(), root, workspace, path); !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("err = %v, want ErrInvalidSession", err)
	}
	if _, err := os.Stat(path); err != nil {
		t.Fatalf("source was moved despite symlinked archive directory: %v", err)
	}
}

func TestArchiveReusesExistingArchiveDirectory(t *testing.T) {
	root := t.TempDir()
	workspace := t.TempDir()
	directory := listSessionDirectory(t, root, workspace)
	archiveDir := filepath.Join(directory, "archive")
	if err := os.MkdirAll(archiveDir, 0o700); err != nil {
		t.Fatal(err)
	}
	path := createListSession(t, root, workspace, "reuse")
	if _, err := Archive(context.Background(), root, workspace, path); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(archiveDir, "reuse.jsonl")); err != nil {
		t.Fatalf("archived session missing: %v", err)
	}
}

func TestArchiveRespectsContextCancellation(t *testing.T) {
	root := t.TempDir()
	workspace := t.TempDir()
	path := createListSession(t, root, workspace, "cancel")
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := Archive(ctx, root, workspace, path); !errors.Is(err, context.Canceled) {
		t.Fatalf("err = %v, want context.Canceled", err)
	}
}
