package main

import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestRunArchiveArchivesSessionAndExits(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	root := filepath.Join(home, ".otto", "sessions")
	path := createCLISession(t, root, workspace, "archive-me")
	before, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}

	var stdout, stderr bytes.Buffer
	code := run(context.Background(), []string{"--archive", path, "--cwd", workspace}, strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{"HOME": home}))
	if code != 0 {
		t.Fatalf("code = %d, stdout = %q, stderr = %q", code, stdout.String(), stderr.String())
	}
	wantPath := filepath.Join(filepath.Dir(path), "archive", "archive-me.jsonl")
	if !strings.Contains(stdout.String(), "Archived:") || !strings.Contains(stdout.String(), wantPath) {
		t.Fatalf("stdout = %q, want archived path %q", stdout.String(), wantPath)
	}
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatalf("source still exists after archive: %v", err)
	}
	after, err := os.ReadFile(wantPath)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(after, before) {
		t.Fatal("archived file bytes changed")
	}
}

func TestRunArchiveRejectsInvalidSession(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	root := filepath.Join(home, ".otto", "sessions")
	path := writeOldOttoV1Session(t, root, workspace, "old")

	var stdout, stderr bytes.Buffer
	code := run(context.Background(), []string{"--archive", path, "--cwd", workspace}, strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{"HOME": home}))
	if code != 1 {
		t.Fatalf("code = %d, stdout = %q, stderr = %q", code, stdout.String(), stderr.String())
	}
	if !strings.Contains(stderr.String(), "archive session") {
		t.Fatalf("stderr = %q", stderr.String())
	}
	if _, err := os.Stat(path); err != nil {
		t.Fatalf("source was removed on failure: %v", err)
	}
}

func TestRunArchiveRejectsWorkspaceMismatch(t *testing.T) {
	home := t.TempDir()
	workspaceA := t.TempDir()
	workspaceB := t.TempDir()
	root := filepath.Join(home, ".otto", "sessions")
	path := createCLISession(t, root, workspaceB, "other")

	var stdout, stderr bytes.Buffer
	code := run(context.Background(), []string{"--archive", path, "--cwd", workspaceA}, strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{"HOME": home}))
	if code != 1 {
		t.Fatalf("code = %d, stdout = %q, stderr = %q", code, stdout.String(), stderr.String())
	}
	if !strings.Contains(stderr.String(), "archive session") {
		t.Fatalf("stderr = %q", stderr.String())
	}
	if _, err := os.Stat(path); err != nil {
		t.Fatalf("source was removed on failure: %v", err)
	}
}

func TestRunArchiveFlagConflicts(t *testing.T) {
	for _, test := range []struct {
		name string
		args []string
		want string
	}{
		{name: "continue", args: []string{"--archive", "x.jsonl", "--continue"}, want: "--archive cannot be used with --continue"},
		{name: "resume", args: []string{"--archive", "x.jsonl", "--resume", "y.jsonl"}, want: "--archive cannot be used with --resume"},
		{name: "no-session", args: []string{"--archive", "x.jsonl", "--no-session"}, want: "--archive cannot be used with --no-session"},
		{name: "approve", args: []string{"--archive", "x.jsonl", "--approve", "hello"}, want: "--archive cannot be used with --approve"},
	} {
		t.Run(test.name, func(t *testing.T) {
			var stdout, stderr bytes.Buffer
			code := run(context.Background(), test.args, strings.NewReader(""), &stdout, &stderr, func(string) string { return "" })
			if code != 2 {
				t.Fatalf("code = %d, stderr = %q", code, stderr.String())
			}
			if !strings.Contains(stderr.String(), test.want) {
				t.Fatalf("stderr = %q, want substring %q", stderr.String(), test.want)
			}
		})
	}
}
