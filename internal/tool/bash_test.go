package tool

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestBashRunsInWorkspaceAndReportsExitCode(t *testing.T) {
	root := t.TempDir()
	workspace := mustWorkspace(t, root)
	bash := NewBashTool(workspace, "/bin/sh", 5*time.Second, 51200)

	result := bash.Execute(context.Background(), json.RawMessage(`{"command":"pwd; echo problem >&2; exit 7"}`))
	if result.IsError {
		t.Fatalf("tool execution infrastructure failed: %s", result.Content)
	}
	if !strings.Contains(result.Content, root) || !strings.Contains(result.Content, "problem") || !strings.Contains(result.Content, "exit_code: 7") {
		t.Fatalf("unexpected result: %s", result.Content)
	}
}

func TestBashSucceedsOnZeroExit(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	bash := NewBashTool(workspace, "/bin/sh", 5*time.Second, 51200)

	result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "echo ready"}))
	if result.IsError {
		t.Fatalf("unexpected error: %s", result.Content)
	}
	if !strings.Contains(result.Content, "stdout:") || !strings.Contains(result.Content, "ready") || !strings.Contains(result.Content, "exit_code: 0") {
		t.Fatalf("unexpected result: %s", result.Content)
	}
}

func TestBashRejectsInvalidArguments(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	bash := NewBashTool(workspace, "/bin/sh", 5*time.Second, 51200)

	invalid := bash.Execute(context.Background(), json.RawMessage(`{"command":"echo hi"`))
	if !invalid.IsError || !strings.Contains(invalid.Content, "invalid JSON") {
		t.Fatalf("unexpected invalid-json result: %#v", invalid)
	}

	unknown := bash.Execute(context.Background(), json.RawMessage(`{"command":"echo hi","extra":true}`))
	if !unknown.IsError || !strings.Contains(unknown.Content, "unknown field") {
		t.Fatalf("unexpected unknown-field result: %#v", unknown)
	}

	missing := bash.Execute(context.Background(), json.RawMessage(`{}`))
	if !missing.IsError || !strings.Contains(missing.Content, "command") {
		t.Fatalf("unexpected missing-command result: %#v", missing)
	}
}

func TestBashRejectsEmptyCommand(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	bash := NewBashTool(workspace, "/bin/sh", 5*time.Second, 51200)

	result := bash.Execute(context.Background(), json.RawMessage(`{"command":""}`))
	if !result.IsError || !strings.Contains(result.Content, "command") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestBashInheritsEnvironment(t *testing.T) {
	t.Setenv("OTTO_BASH_TEST_VALUE", "from-env")
	workspace := mustWorkspace(t, t.TempDir())
	bash := NewBashTool(workspace, "/bin/sh", 5*time.Second, 51200)

	result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "printf %s \"$OTTO_BASH_TEST_VALUE\""}))
	if result.IsError {
		t.Fatalf("unexpected error: %s", result.Content)
	}
	if !strings.Contains(result.Content, "from-env") {
		t.Fatalf("unexpected result: %s", result.Content)
	}
}

func TestBashRemovesCredentialEnvironmentAndRedactsResolvedValue(t *testing.T) {
	resolvedCredential := "resolved-" + strings.Repeat("x", 24)
	t.Setenv("OTTO_API_KEY", "fallback-value")
	t.Setenv("OTTO_TEST_PROFILE_KEY", resolvedCredential)
	t.Setenv("OTTO_BASH_TEST_VALUE", "inherited-value")
	t.Setenv("OTTO_TEST_SECRET_PART_1", resolvedCredential[:len(resolvedCredential)/2])
	t.Setenv("OTTO_TEST_SECRET_PART_2", resolvedCredential[len(resolvedCredential)/2:])

	workspace := mustWorkspace(t, t.TempDir())
	bash := NewBashTool(workspace, "/bin/sh", 5*time.Second, 51200, BashSecurity{
		RemoveEnv:    []string{"OTTO_API_KEY", "OTTO_TEST_PROFILE_KEY"},
		RedactValues: []string{resolvedCredential},
	})
	result := bash.Execute(context.Background(), mustJSON(t, map[string]string{
		"command": `printf 'fallback=%s profile=%s inherited=%s reconstructed=%s%s' "$OTTO_API_KEY" "$OTTO_TEST_PROFILE_KEY" "$OTTO_BASH_TEST_VALUE" "$OTTO_TEST_SECRET_PART_1" "$OTTO_TEST_SECRET_PART_2"`,
	}))
	if result.IsError {
		t.Fatalf("unexpected error: %s", result.Content)
	}
	if strings.Contains(result.Content, resolvedCredential) || strings.Contains(result.Content, "fallback-value") {
		t.Fatalf("credential leaked in bash result: %q", result.Content)
	}
	for _, expected := range []string{"fallback= profile= inherited=inherited-value", "reconstructed=[REDACTED]"} {
		if !strings.Contains(result.Content, expected) {
			t.Fatalf("bash result missing %q: %q", expected, result.Content)
		}
	}
}

func TestBashRedactsCredentialBeforeOutputTruncation(t *testing.T) {
	credential := "credential-" + strings.Repeat("z", 32)
	workspace := mustWorkspace(t, t.TempDir())
	bash := NewBashTool(workspace, "/bin/sh", 5*time.Second, 12, BashSecurity{RedactValues: []string{credential}})

	result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "printf %s " + credential}))
	if result.IsError {
		t.Fatalf("unexpected error: %s", result.Content)
	}
	if strings.Contains(result.Content, credential) || strings.Contains(result.Content, credential[:12]) {
		t.Fatalf("credential or truncated prefix leaked: %q", result.Content)
	}
	if !strings.Contains(result.Content, "[REDACTED]") {
		t.Fatalf("redaction marker missing: %q", result.Content)
	}
}

func TestBashReportsStdoutAndStderrTruncationSeparately(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	bash := NewBashTool(workspace, "/bin/sh", 5*time.Second, 12)

	command := "printf 1234567890abcdef; printf abcdefghijklmnop >&2"
	result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": command}))
	if result.IsError {
		t.Fatalf("unexpected error: %s", result.Content)
	}
	if !strings.Contains(result.Content, "stdout:") || !strings.Contains(result.Content, "1234567890ab") {
		t.Fatalf("unexpected stdout section: %s", result.Content)
	}
	if !strings.Contains(result.Content, "stderr:") || !strings.Contains(result.Content, "abcdefghijkl") {
		t.Fatalf("unexpected stderr section: %s", result.Content)
	}
	if count := strings.Count(result.Content, "truncated"); count < 2 {
		t.Fatalf("expected truncation notices for both streams, got %d in %s", count, result.Content)
	}
}

func TestBashCallerCancellationReportsCancelledStatus(t *testing.T) {
	root := t.TempDir()
	workspace := mustWorkspace(t, root)
	marker := filepath.Join(root, "cancel-child-survived")
	command := fmt.Sprintf("(sleep 1; touch %q) & wait", marker)
	bash := NewBashTool(workspace, "/bin/sh", 5*time.Second, 51200)

	ctx, cancel := context.WithCancel(context.Background())
	time.AfterFunc(50*time.Millisecond, cancel)

	result := bash.Execute(ctx, mustJSON(t, map[string]string{"command": command}))
	if result.IsError || !strings.Contains(result.Content, "status: cancelled") || !strings.Contains(result.Content, "signal: killed") {
		t.Fatalf("unexpected result: %#v", result)
	}

	time.Sleep(1200 * time.Millisecond)
	if _, err := os.Stat(marker); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("child process survived: %v", err)
	}
}

func TestBashPreCancelledContextReportsCancelledStatus(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	bash := NewBashTool(workspace, "/bin/sh", 5*time.Second, 51200)

	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	result := bash.Execute(ctx, mustJSON(t, map[string]string{"command": "echo hi"}))
	if result.IsError || !strings.Contains(result.Content, "status: cancelled") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestBashTimeoutReportsTimedOutStatus(t *testing.T) {
	root := t.TempDir()
	workspace := mustWorkspace(t, root)
	marker := filepath.Join(root, "child-survived")
	command := fmt.Sprintf("(sleep 1; touch %q) & wait", marker)
	bash := NewBashTool(workspace, "/bin/sh", 50*time.Millisecond, 51200)

	result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": command}))
	if result.IsError || !strings.Contains(result.Content, "status: timed out") || !strings.Contains(result.Content, "signal: killed") {
		t.Fatalf("unexpected result: %s", result.Content)
	}

	time.Sleep(1200 * time.Millisecond)
	if _, err := os.Stat(marker); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("child process survived: %v", err)
	}
}

func TestBashReportsConfiguredShellFailure(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	bash := NewBashTool(workspace, "/definitely/missing-shell", 5*time.Second, 51200)

	result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "echo hi"}))
	if !result.IsError || !strings.Contains(result.Content, "missing-shell") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func mustJSON(t *testing.T, value any) json.RawMessage {
	t.Helper()
	encoded, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	return encoded
}
