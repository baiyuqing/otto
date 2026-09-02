package main

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	goruntime "runtime"
	"sort"
	"strings"
	"time"
)

const (
	// maxWorkspaceContextFileBytes caps how much of AGENTS.md/CLAUDE.md is
	// embedded in the system prompt.
	maxWorkspaceContextFileBytes = 8 << 10
	// maxWorkspaceListingEntries caps the one-level workspace listing.
	maxWorkspaceListingEntries = 200
	// gitStatusTimeout bounds the git subprocess calls so a slow or hanging
	// git never blocks startup.
	gitStatusTimeout = 2 * time.Second
)

// workspaceContextFor builds the dynamic "## Environment" section appended
// to the static system prompt: cwd, platform/date, git branch and dirty
// count, a one-level workspace listing, and AGENTS.md/CLAUDE.md content when
// present. It embeds file content from the user's workspace, so callers must
// redact the result before sending it to a provider.
func workspaceContextFor(workspacePath string, now time.Time) string {
	var b strings.Builder
	b.WriteString("\n\n## Environment\n")
	fmt.Fprintf(&b, "cwd: %s\n", workspacePath)
	fmt.Fprintf(&b, "platform: %s, date: %s\n", goruntime.GOOS, now.Format("2006-01-02"))
	if line := gitStatusLine(workspacePath); line != "" {
		b.WriteString(line)
		b.WriteString("\n")
	}
	b.WriteString(workspaceListing(workspacePath))
	for _, name := range []string{"AGENTS.md", "CLAUDE.md"} {
		if content, ok := readWorkspaceDocFile(workspacePath, name); ok {
			fmt.Fprintf(&b, "\n## %s\n%s\n", name, content)
		}
	}
	return b.String()
}

// gitStatusLine reports "git: <branch>, <N> modified", or "" if the
// workspace is not a git repository, git is unavailable, or either command
// fails or times out.
func gitStatusLine(workspacePath string) string {
	ctx, cancel := context.WithTimeout(context.Background(), gitStatusTimeout)
	defer cancel()
	branch, err := runGit(ctx, workspacePath, "rev-parse", "--abbrev-ref", "HEAD")
	if err != nil {
		return ""
	}
	status, err := runGit(ctx, workspacePath, "status", "--porcelain")
	if err != nil {
		return ""
	}
	modified := 0
	for _, line := range strings.Split(status, "\n") {
		if strings.TrimSpace(line) != "" {
			modified++
		}
	}
	return fmt.Sprintf("git: %s, %d modified", strings.TrimSpace(branch), modified)
}

func runGit(ctx context.Context, dir string, args ...string) (string, error) {
	cmd := exec.CommandContext(ctx, "git", args...)
	cmd.Dir = dir
	var out bytes.Buffer
	cmd.Stdout = &out
	cmd.Stderr = io.Discard
	if err := cmd.Run(); err != nil {
		return "", err
	}
	return out.String(), nil
}

// workspaceListing returns a sorted, one-level listing of workspacePath
// (directories marked with a trailing "/", ".git" skipped), capped at
// maxWorkspaceListingEntries. Returns "" if the directory can't be read.
func workspaceListing(workspacePath string) string {
	entries, err := os.ReadDir(workspacePath)
	if err != nil {
		return ""
	}
	names := make([]string, 0, len(entries))
	for _, entry := range entries {
		if entry.Name() == ".git" {
			continue
		}
		name := entry.Name()
		if entry.IsDir() {
			name += "/"
		}
		names = append(names, name)
	}
	if len(names) == 0 {
		return ""
	}
	sort.Strings(names)
	total := len(names)
	truncated := total > maxWorkspaceListingEntries
	if truncated {
		names = names[:maxWorkspaceListingEntries]
	}
	var b strings.Builder
	for _, name := range names {
		b.WriteString(name)
		b.WriteString("\n")
	}
	if truncated {
		fmt.Fprintf(&b, "... (%d entries, truncated)\n", total)
	}
	return b.String()
}

// readWorkspaceDocFile reads workspacePath/name, cut at
// maxWorkspaceContextFileBytes with a trailing marker when truncated.
// Returns ok=false if the file doesn't exist or can't be read.
func readWorkspaceDocFile(workspacePath, name string) (string, bool) {
	data, err := os.ReadFile(filepath.Join(workspacePath, name))
	if err != nil {
		return "", false
	}
	if len(data) > maxWorkspaceContextFileBytes {
		total := len(data)
		return fmt.Sprintf("%s\n[truncated: %d bytes, showing first %d]", data[:maxWorkspaceContextFileBytes], total, maxWorkspaceContextFileBytes), true
	}
	return string(data), true
}
