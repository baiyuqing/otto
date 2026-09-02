package main

import (
	"os"
	"os/exec"
	"path/filepath"
	goruntime "runtime"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
)

func TestWorkspaceContextForIncludesEnvironmentHeaderAndCwd(t *testing.T) {
	dir := t.TempDir()
	now := time.Date(2026, 9, 2, 12, 0, 0, 0, time.UTC)
	got := workspaceContextFor(dir, now)
	if !strings.Contains(got, "\n\n## Environment\n") {
		t.Fatalf("workspaceContextFor() = %q, want an Environment header", got)
	}
	if !strings.Contains(got, "cwd: "+dir+"\n") {
		t.Fatalf("workspaceContextFor() = %q, want cwd line for %q", got, dir)
	}
	if !strings.Contains(got, "platform: "+goruntime.GOOS+", date: 2026-09-02\n") {
		t.Fatalf("workspaceContextFor() = %q, want platform/date line", got)
	}
}

func TestWorkspaceContextForPrefersAgentsOverClaudeWhenBothPresent(t *testing.T) {
	dir := t.TempDir()
	writeWorkspaceFile(t, dir, "AGENTS.md", "agents rules")
	writeWorkspaceFile(t, dir, "CLAUDE.md", "claude rules")
	got := workspaceContextFor(dir, time.Now())
	if !strings.Contains(got, "\n## AGENTS.md\nagents rules\n") {
		t.Fatalf("workspaceContextFor() = %q, want AGENTS.md section", got)
	}
	if strings.Contains(got, "## CLAUDE.md") {
		t.Fatalf("workspaceContextFor() = %q, want no CLAUDE.md section when AGENTS.md exists", got)
	}
}

func TestWorkspaceContextForFallsBackToClaudeWhenAgentsMissing(t *testing.T) {
	dir := t.TempDir()
	writeWorkspaceFile(t, dir, "CLAUDE.md", "claude rules")
	got := workspaceContextFor(dir, time.Now())
	if !strings.Contains(got, "\n## CLAUDE.md\nclaude rules\n") {
		t.Fatalf("workspaceContextFor() = %q, want CLAUDE.md section", got)
	}
}

func TestWorkspaceContextForIncludesOnlyAgentsWhenClaudeMissing(t *testing.T) {
	dir := t.TempDir()
	writeWorkspaceFile(t, dir, "AGENTS.md", "agents rules")
	got := workspaceContextFor(dir, time.Now())
	if !strings.Contains(got, "## AGENTS.md") {
		t.Fatalf("workspaceContextFor() = %q, want AGENTS.md section", got)
	}
	if strings.Contains(got, "## CLAUDE.md") {
		t.Fatalf("workspaceContextFor() = %q, want no CLAUDE.md section", got)
	}
}

func TestWorkspaceContextForOmitsBothDocFilesWhenNeitherPresent(t *testing.T) {
	dir := t.TempDir()
	got := workspaceContextFor(dir, time.Now())
	if strings.Contains(got, "## AGENTS.md") || strings.Contains(got, "## CLAUDE.md") {
		t.Fatalf("workspaceContextFor() = %q, want no doc sections", got)
	}
}

func TestWorkspaceContextForTruncatesDocFileAtCap(t *testing.T) {
	dir := t.TempDir()
	oversized := strings.Repeat("x", maxWorkspaceContextFileBytes+100)
	writeWorkspaceFile(t, dir, "AGENTS.md", oversized)
	got := workspaceContextFor(dir, time.Now())
	wantMarker := "[truncated: " + strconv.Itoa(len(oversized)) + " bytes, showing first " + strconv.Itoa(maxWorkspaceContextFileBytes) + "]"
	if !strings.Contains(got, wantMarker) {
		t.Fatalf("workspaceContextFor() = %q, want truncation marker %q", got, wantMarker)
	}
	if strings.Contains(got, strings.Repeat("x", maxWorkspaceContextFileBytes+1)) {
		t.Fatalf("workspaceContextFor() included more than the byte cap")
	}
}

func TestWorkspaceListingSkipsGitDirAndMarksDirectories(t *testing.T) {
	dir := t.TempDir()
	if err := os.Mkdir(filepath.Join(dir, ".git"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(filepath.Join(dir, "internal"), 0o755); err != nil {
		t.Fatal(err)
	}
	writeWorkspaceFile(t, dir, "go.mod", "module example\n")
	got := workspaceListing(dir)
	if strings.Contains(got, ".git") {
		t.Fatalf("workspaceListing() = %q, want .git skipped", got)
	}
	if !strings.Contains(got, "internal/\n") {
		t.Fatalf("workspaceListing() = %q, want directories marked with trailing /", got)
	}
	if !strings.Contains(got, "go.mod\n") {
		t.Fatalf("workspaceListing() = %q, want plain file entries", got)
	}
}

func TestGitStatusLineOmittedWhenNotARepository(t *testing.T) {
	if _, err := exec.LookPath("git"); err != nil {
		t.Skip("git not installed")
	}
	dir := t.TempDir()
	if got := gitStatusLine(dir); got != "" {
		t.Fatalf("gitStatusLine() = %q, want empty for a non-repository directory", got)
	}
}

func TestWorkspaceContextRedactedByBoundaryRedactor(t *testing.T) {
	dir := t.TempDir()
	const secret = "sk-configured-secret-value"
	writeWorkspaceFile(t, dir, "AGENTS.md", "token: "+secret+"\n")
	dynamic := workspaceContextFor(dir, time.Now())
	if !strings.Contains(dynamic, secret) {
		t.Fatalf("test setup invalid: raw content did not contain the secret")
	}
	redactor := agent.NewRedactor([]string{secret})
	redacted := redactor.RedactString(dynamic)
	if strings.Contains(redacted, secret) {
		t.Fatalf("redacted workspace context leaked configured secret: %q", redacted)
	}
}

func writeWorkspaceFile(t *testing.T, dir, name, content string) {
	t.Helper()
	if err := os.WriteFile(filepath.Join(dir, name), []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}
