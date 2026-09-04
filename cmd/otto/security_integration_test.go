package main

import (
	"context"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/sandbox"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
)

func TestWorkspaceInstructionsRejectExternalLinks(t *testing.T) {
	for _, name := range []string{"AGENTS.md", "CLAUDE.md"} {
		t.Run(name, func(t *testing.T) {
			workspace := t.TempDir()
			outside := filepath.Join(t.TempDir(), "private.txt")
			const secret = "audit-only-outside-content"
			if err := os.WriteFile(outside, []byte(secret), 0o600); err != nil {
				t.Fatal(err)
			}
			if err := os.Symlink(outside, filepath.Join(workspace, name)); err != nil {
				t.Fatal(err)
			}
			if name == "AGENTS.md" {
				writeWorkspaceFile(t, workspace, "CLAUDE.md", "safe fallback")
			}
			got := testWorkspaceContextFor(t, workspace, time.Now(), nil, nil)
			if strings.Contains(got, secret) {
				t.Fatal("workspace instructions included an external file")
			}
			if name == "AGENTS.md" && !strings.Contains(got, "safe fallback") {
				t.Fatal("rejected AGENTS.md prevented safe CLAUDE.md fallback")
			}
		})
	}
}

func TestWorkspaceContextKeepsInitialDirectory(t *testing.T) {
	parent := t.TempDir()
	path := filepath.Join(parent, "workspace")
	if err := os.Mkdir(path, 0o700); err != nil {
		t.Fatal(err)
	}
	writeWorkspaceFile(t, path, "AGENTS.md", "original instructions")
	workspace, err := tool.NewWorkspace(path)
	if err != nil {
		t.Fatal(err)
	}
	defer workspace.Close()
	if err := os.Rename(path, filepath.Join(parent, "moved")); err != nil {
		t.Fatal(err)
	}
	outside := t.TempDir()
	writeWorkspaceFile(t, outside, "AGENTS.md", "outside secret")
	if err := os.Symlink(outside, path); err != nil {
		t.Fatal(err)
	}
	got := workspaceContextFor(path, time.Now(), nil, nil, workspace)
	if strings.Contains(got, "outside secret") || !strings.Contains(got, "original instructions") {
		t.Fatal("startup did not keep the initial workspace directory")
	}
}

func TestGitStatusDoesNotRunConfiguredFSMonitor(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	environment := []string{"PATH=/usr/bin:/bin", "HOME=" + t.TempDir(), "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL=/dev/null"}
	run := func(args ...string) {
		t.Helper()
		command := exec.Command("git", args...)
		command.Dir = builder.workspacePath
		command.Env = environment
		if output, err := command.CombinedOutput(); err != nil {
			t.Fatalf("git %v: %v: %s", args, err, output)
		}
	}
	run("init", "-q")
	run("-c", "user.name=Audit", "-c", "user.email=audit@example.invalid", "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-qm", "fixture")
	marker := filepath.Join(builder.workspacePath, "hook-ran")
	hook := filepath.Join(builder.workspacePath, "monitor.sh")
	if err := os.WriteFile(hook, []byte("#!/bin/sh\ntouch hook-ran\nprintf 'token\\0'\n"), 0o700); err != nil {
		t.Fatal(err)
	}
	run("config", "core.fsmonitor", hook)
	if got := gitStatusLine(builder.workspacePath, builder.commandExecutor, environment); !strings.HasPrefix(got, "git: ") {
		t.Fatalf("Git status unavailable: %q", got)
	}
	if _, err := os.Stat(marker); !os.IsNotExist(err) {
		t.Fatalf("configured fsmonitor ran: %v", err)
	}
}

func TestRuntimeBuilderUsesSandboxForWorkspaceContext(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	executor := &integrationContextExecutor{}
	builder.commandExecutor = executor
	builder.sandboxEnvironment = []string{"PATH=/usr/bin:/bin", "HOME=/private/sandbox-home"}
	builder.sandboxInfo = app.SandboxInfo{Mode: app.SandboxSeatbelt, BashAvailable: true}
	runtime, err := builder.resolveSession(session.RuntimeMetadata{Profile: "default", Provider: "openai-compatible", Model: "gpt-4.1"})
	if err != nil {
		t.Fatal(err)
	}
	current := session.NewMemory(session.Header{Version: session.CurrentVersion})
	runner, err := builder.buildRunner(context.Background(), current, runtime)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = closeRuntimeRunner(runner) })
	if len(executor.requests) != 2 {
		t.Fatalf("startup made %d sandbox calls, want two Git queries", len(executor.requests))
	}
	for _, request := range executor.requests {
		if request.Dir != builder.workspacePath || !reflect.DeepEqual(request.Env, builder.sandboxEnvironment) {
			t.Fatal("startup query did not use the workspace and sandbox environment")
		}
	}
}

type integrationContextExecutor struct{ requests []sandbox.Request }

func (e *integrationContextExecutor) Execute(_ context.Context, request sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
	e.requests = append(e.requests, request.Clone())
	_, _ = io.WriteString(streams.Stdout, "security-branch\n")
	return sandbox.ExitStatus{}, nil
}
