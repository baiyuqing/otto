package main

import (
	"bytes"
	"context"
	"fmt"
	"io"
	goruntime "runtime"
	"sort"
	"strings"
	"time"

	"github.com/baiyuqing/otto/internal/sandbox"
	"github.com/baiyuqing/otto/internal/tool"
)

const (
	// maxWorkspaceContextFileBytes caps how much of the workspace instruction
	// file is embedded in the system prompt.
	maxWorkspaceContextFileBytes = 8 << 10
	// maxWorkspaceListingEntries caps the one-level workspace listing.
	maxWorkspaceListingEntries = 200
	// gitStatusTimeout bounds the git subprocess calls so a slow or hanging
	// git never blocks startup.
	gitStatusTimeout = 2 * time.Second
)

// workspaceContextFor builds the dynamic "## Environment" section appended
// to the static system prompt: cwd, platform/date, git branch and dirty
// count, a one-level workspace listing, and the workspace instruction file
// (AGENTS.md, else CLAUDE.md) when present. It embeds file content from the
// user's workspace, so callers must redact the result before sending it to a
// provider.
func workspaceContextFor(workspacePath string, now time.Time, executor sandbox.CommandExecutor, environment []string, workspace *tool.Workspace) string {
	var b strings.Builder
	b.WriteString("\n\n## Environment\n")
	fmt.Fprintf(&b, "cwd: %s\n", workspacePath)
	fmt.Fprintf(&b, "platform: %s, date: %s\n", goruntime.GOOS, now.Format("2006-01-02"))
	if line := gitStatusLine(workspacePath, executor, environment); line != "" {
		b.WriteString(line)
		b.WriteString("\n")
	}
	b.WriteString(workspaceListing(workspace))
	// Only one instruction file is embedded. Both are re-sent on every request
	// of a session, so a second file costs its full size per request; measured
	// against this repo that was ~1100 tokens per request for a file the model
	// never asked to read. AGENTS.md wins when both exist: it is the canonical
	// rulebook, and CLAUDE.md is Claude Code's own config, which typically
	// references AGENTS.md rather than adding to it.
	for _, name := range []string{"AGENTS.md", "CLAUDE.md"} {
		if content, ok := readWorkspaceDocFile(workspace, name); ok {
			fmt.Fprintf(&b, "\n## Workspace instructions\n<workspace-instructions file=%q>\n%s\n</workspace-instructions>\n",
				name, neutralizeInstructionFence(content))
			break
		}
	}
	return b.String()
}

// instructionFencePrefix is the delimiter the workspace instruction file is
// wrapped in. The file belongs to whoever wrote the repository, so without a
// fence its text is indistinguishable from the surrounding system prompt and
// can forge Otto's own sections.
const instructionFencePrefix = "<workspace-instructions"

// neutralizeInstructionFence makes the fence delimiter unforgeable by breaking
// any occurrence of it inside the file, including the closing tag. Unlike
// renderMemoryContext, which escapes record text wholesale, only the delimiter
// is touched: an instruction file is markdown full of backticked shell
// snippets and comparisons that full escaping would turn into entities the
// model then has to read through.
//
// Matching walks the original bytes rather than a lowercased copy, because
// lowercasing can change byte length (U+0130 becomes two runes) and shift
// every offset after it.
func neutralizeInstructionFence(content string) string {
	var b strings.Builder
	for index := 0; index < len(content); {
		matched := fenceMatchLen(content[index:])
		if matched == 0 {
			b.WriteByte(content[index])
			index++
			continue
		}
		// Keep the file's own bytes, minus the "<" that makes it a tag.
		b.WriteString("<_")
		b.WriteString(content[index+1 : index+matched])
		index += matched
	}
	return b.String()
}

// fenceMatchLen reports the byte length of a fence tag opening at the start of
// text, or 0 when there is none. Both forms are ASCII, so a case-insensitive
// match over a fixed byte window consumes exactly that many bytes.
func fenceMatchLen(text string) int {
	for _, form := range []string{"</workspace-instructions", instructionFencePrefix} {
		if len(text) >= len(form) && strings.EqualFold(text[:len(form)], form) {
			return len(form)
		}
	}
	return 0
}

// gitStatusLine reports "git: <branch>, <N> modified", or "" if the
// workspace is not a git repository, Git is unavailable, the sandbox executor
// is unavailable, or either command fails or times out.
func gitStatusLine(workspacePath string, executor sandbox.CommandExecutor, environment []string) string {
	if isNilSandboxRuntimeValue(executor) || environment == nil {
		return ""
	}
	ctx, cancel := context.WithTimeout(context.Background(), gitStatusTimeout)
	defer cancel()
	branch, err := runGit(ctx, executor, workspacePath, environment, "rev-parse", "--abbrev-ref", "HEAD")
	if err != nil || strings.TrimSpace(branch) == "" {
		return ""
	}
	status, err := runGit(ctx, executor, workspacePath, environment, "status", "--porcelain")
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

func runGit(ctx context.Context, executor sandbox.CommandExecutor, dir string, environment []string, args ...string) (string, error) {
	argv := append([]string{"git", "-c", "core.fsmonitor=false"}, args...)
	var out bytes.Buffer
	status, err := executor.Execute(ctx, sandbox.Request{
		Argv: argv,
		Dir:  dir,
		Env:  append([]string(nil), environment...),
	}, sandbox.Streams{Stdout: &out, Stderr: io.Discard})
	if err != nil {
		return "", err
	}
	if status.Code != 0 || status.Signaled {
		return "", fmt.Errorf("git exited with status %d", status.Code)
	}
	return out.String(), nil
}

// workspaceListing returns a sorted, one-level listing of the workspace
// (directories marked with a trailing "/", ".git" skipped), capped at
// maxWorkspaceListingEntries. Returns "" if the directory can't be read.
func workspaceListing(workspace *tool.Workspace) string {
	directory, err := workspace.Open(".")
	if err != nil {
		return ""
	}
	defer directory.Close()
	entries, err := directory.ReadDir(-1)
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

// readWorkspaceDocFile reads a file through the initial workspace handle, cut at
// maxWorkspaceContextFileBytes with a trailing marker when truncated.
// Returns ok=false if the file doesn't exist or can't be read.
func readWorkspaceDocFile(workspace *tool.Workspace, name string) (string, bool) {
	file, err := workspace.Open(name)
	if err != nil {
		return "", false
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil || !info.Mode().IsRegular() {
		return "", false
	}
	data, err := io.ReadAll(io.LimitReader(file, maxWorkspaceContextFileBytes+1))
	if err != nil {
		return "", false
	}
	if len(data) > maxWorkspaceContextFileBytes {
		total := max(info.Size(), int64(len(data)))
		return fmt.Sprintf("%s\n[truncated: %d bytes, showing first %d]", data[:maxWorkspaceContextFileBytes], total, maxWorkspaceContextFileBytes), true
	}
	return string(data), true
}
