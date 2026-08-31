package tool

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"sync"
	"testing"
	"time"
	"unicode"
	"unicode/utf8"
	"unsafe"

	"github.com/baiyuqing/otto/internal/sandbox"
	"github.com/baiyuqing/otto/internal/sandbox/direct"
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

func TestBashSandboxedDelegatesExactRequestAndClonesInputs(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	fake := &fakeBashExecutor{
		stdoutChunks: [][]byte{[]byte("split-*"), []byte("secret")},
		stderrChunks: [][]byte{[]byte("problem")},
		status:       sandbox.ExitStatus{Code: 7},
	}

	shellStorage := []byte("/bin/sh")
	environmentStorage := []byte("FIRST=original")
	redactionStorage := []byte("split-*secret")
	shell := unsafe.String(unsafe.SliceData(shellStorage), len(shellStorage))
	environment := []string{
		unsafe.String(unsafe.SliceData(environmentStorage), len(environmentStorage)),
		"SECOND=preserved",
	}
	redactions := []string{unsafe.String(unsafe.SliceData(redactionStorage), len(redactionStorage))}

	bash, err := NewSandboxedBashTool(workspace, fake, shell, environment, 3*time.Second, 1024, redactions)
	if err != nil {
		t.Fatalf("NewSandboxedBashTool() error = %v", err)
	}

	copy(shellStorage, "/bad/sh")
	copy(environmentStorage, "FIRST=modified")
	for i := range redactionStorage {
		redactionStorage[i] = 'x'
	}
	environment[1] = "SECOND=caller-mutated"
	redactions[0] = "caller-mutated-secret"

	first := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "first command"}))
	if first.IsError {
		t.Fatalf("first Execute() = %#v", first)
	}
	firstMarker := sandboxedBashStdout(t, first.Content)
	if utf8.RuneCountInString(firstMarker) != 1 || firstMarker == "*" || strings.Contains(first.Content, "split-*secret") {
		t.Fatalf("first Execute() did not retain collision-safe cloned redaction state: %q", first.Content)
	}

	fake.mutateRetainedRequest(0)
	second := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "second command"}))
	if second.IsError {
		t.Fatalf("second Execute() = %#v", second)
	}
	if secondMarker := sandboxedBashStdout(t, second.Content); secondMarker != firstMarker || strings.Contains(second.Content, "split-*secret") {
		t.Fatalf("second Execute() reused mutable caller or prior-call redactor state: %q", second.Content)
	}

	requests := fake.Requests()
	wantRequests := []sandbox.Request{
		{Argv: []string{"/bin/sh", "-lc", "first command"}, Dir: workspace.root, Env: []string{"FIRST=original", "SECOND=preserved"}},
		{Argv: []string{"/bin/sh", "-lc", "second command"}, Dir: workspace.root, Env: []string{"FIRST=original", "SECOND=preserved"}},
	}
	if !reflect.DeepEqual(requests, wantRequests) {
		t.Fatalf("captured requests = %#v, want %#v", requests, wantRequests)
	}
	for i, request := range requests {
		if request.Env == nil {
			t.Fatalf("request %d environment is nil", i)
		}
	}

	streams := fake.Streams()
	if len(streams) != 2 {
		t.Fatalf("captured streams = %d, want 2", len(streams))
	}
	if streams[0].Stdout == streams[0].Stderr || streams[1].Stdout == streams[1].Stderr {
		t.Fatal("stdout and stderr writers were not independent")
	}
	if streams[0].Stdout == streams[1].Stdout || streams[0].Stderr == streams[1].Stderr {
		t.Fatal("separate calls reused stream writers")
	}
}

func TestBashSandboxedReportsIndependentCapsExitCodeAndSignal(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())

	t.Run("independent stream caps and nonzero exit", func(t *testing.T) {
		fake := &fakeBashExecutor{
			stdoutChunks: [][]byte{[]byte("1234567890abcdef")},
			stderrChunks: [][]byte{[]byte("abcdefghijklmnop")},
			status:       sandbox.ExitStatus{Code: 7, Signal: "must-be-ignored"},
		}
		bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 12, nil)

		result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "ignored"}))
		want := "stdout:\n1234567890ab\n[truncated: 4 bytes omitted]\nstderr:\nabcdefghijkl\n[truncated: 4 bytes omitted]\nexit_code: 7"
		if result.IsError || result.Content != want {
			t.Fatalf("Execute() = %#v, want content %q", result, want)
		}
		if strings.Contains(result.Content, "must-be-ignored") {
			t.Fatalf("nonsignaled status exposed Signal: %q", result.Content)
		}
	})

	t.Run("signaled exit", func(t *testing.T) {
		fake := &fakeBashExecutor{status: sandbox.ExitStatus{Code: -1, Signaled: true, Signal: "killed"}}
		bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 1024, nil)

		result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "ignored"}))
		want := "stdout:\n\nstderr:\n\nexit_code: -1; signal: killed"
		if result.IsError || result.Content != want {
			t.Fatalf("Execute() = %#v, want content %q", result, want)
		}
	})

	t.Run("signal is defense-in-depth redacted", func(t *testing.T) {
		const secret = "signal-secret-value"
		fake := &fakeBashExecutor{status: sandbox.ExitStatus{Code: -1, Signaled: true, Signal: "killed-" + secret}}
		bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 1024, []string{secret})

		result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "ignored"}))
		if result.IsError || strings.Contains(result.Content, secret) || !strings.Contains(result.Content, "signal: killed-*") {
			t.Fatalf("Execute() exposed signal redaction value: %#v", result)
		}
	})
}

func TestBashSandboxedRedactsSplitOverlappingSecretsBeforeCaps(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	longSecret := "credential-" + strings.Repeat("z", 32)
	overlapFirst := "overlap-secret"
	overlapSecond := "secret-tail"
	fake := &fakeBashExecutor{
		stdoutChunks: [][]byte{
			[]byte(longSecret[:8]),
			[]byte(longSecret[8:]),
			[]byte(" | overlap-"),
			[]byte("secret-tail | "),
			[]byte(strings.Repeat("x", 48)),
			[]byte(longSecret),
		},
		stderrChunks: [][]byte{
			[]byte("overlap-"),
			[]byte("secret-tail | "),
			[]byte(strings.Repeat("y", 48)),
			[]byte(longSecret),
		},
		status: sandbox.ExitStatus{Code: 0},
	}
	bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 32, []string{longSecret, overlapFirst, overlapSecond})

	result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "ignored"}))
	if result.IsError {
		t.Fatalf("Execute() = %#v", result)
	}
	for _, forbidden := range []string{longSecret, longSecret[:32], overlapFirst, overlapSecond} {
		if strings.Contains(result.Content, forbidden) {
			t.Fatalf("Execute() leaked %q: %q", forbidden, result.Content)
		}
	}
	if !strings.Contains(result.Content, "stdout:\n*") || !strings.Contains(result.Content, "stderr:\n*") {
		t.Fatalf("split redaction markers missing: %q", result.Content)
	}
	if got := strings.Count(result.Content, "[truncated:"); got != 2 {
		t.Fatalf("truncation notices = %d, want 2: %q", got, result.Content)
	}
}

func TestBashSandboxedUsesCollisionSafeSingleRuneMarker(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	tests := []struct {
		name       string
		stdout     string
		values     []string
		prefix     string
		suffix     string
		wantMarker string
	}{
		{
			name:       "stable preferred marker",
			stdout:     "TOKEN",
			values:     []string{"TOKEN"},
			wantMarker: "*",
		},
		{
			name:   "preferred marker is itself a secret",
			stdout: "*",
			values: []string{"*", "[REDACTED]"},
		},
		{
			name:   "preferred marker occurs in a secret and would synthesize another",
			stdout: "leftTOKENright",
			values: []string{"TOKEN", "left*right", "left!right", "[REDACTED]"},
			prefix: "left",
			suffix: "right",
		},
		{
			name:   "legacy marker is itself a secret",
			stdout: "[REDACTED]",
			values: []string{"[REDACTED]"},
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			fake := &fakeBashExecutor{
				stdoutChunks: [][]byte{[]byte(test.stdout)},
				status: sandbox.ExitStatus{
					Code:     -1,
					Signaled: true,
					Signal:   "signal-" + test.values[0],
				},
			}
			bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 1024, test.values)

			result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "ignored"}))
			if result.IsError {
				t.Fatalf("Execute() = %#v", result)
			}
			for _, value := range test.values {
				if strings.Contains(result.Content, value) {
					t.Fatalf("Execute() synthesized or exposed configured value %q: %q", value, result.Content)
				}
			}

			body := sandboxedBashStdout(t, result.Content)
			if !strings.HasPrefix(body, test.prefix) || !strings.HasSuffix(body, test.suffix) || len(body) < len(test.prefix)+len(test.suffix) {
				t.Fatalf("stdout body = %q, want surrounding fragments %q and %q", body, test.prefix, test.suffix)
			}
			marker := body[len(test.prefix) : len(body)-len(test.suffix)]
			r, size := utf8.DecodeRuneInString(marker)
			if marker == "" || size != len(marker) || r == utf8.RuneError && size == 1 || unicode.IsControl(r) {
				t.Fatalf("replacement marker = %q, want one valid non-control rune", marker)
			}
			if test.wantMarker != "" && marker != test.wantMarker {
				t.Fatalf("replacement marker = %q, want stable default %q", marker, test.wantMarker)
			}
			for _, value := range test.values {
				for _, markerByte := range []byte(marker) {
					if bytes.IndexByte([]byte(value), markerByte) >= 0 {
						t.Fatalf("marker byte %#x occurs in configured value %q", markerByte, value)
					}
				}
			}
		})
	}
}

func TestBashSandboxedCollisionSafeMarkerSurvivesEveryByteCap(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	var printableASCII strings.Builder
	for value := byte(0x20); value <= 0x7e; value++ {
		printableASCII.WriteByte(value)
	}
	values := []string{"TOKEN", printableASCII.String()}

	resultForCap := func(cap int) Result {
		t.Helper()
		fake := &fakeBashExecutor{
			stdoutChunks: [][]byte{[]byte("TOKEN")},
			status:       sandbox.ExitStatus{Code: -1, Signaled: true, Signal: "signal-TOKEN"},
		}
		bash := mustSandboxedBashTool(t, workspace, fake, []string{}, cap, values)
		return bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "ignored"}))
	}

	full := resultForCap(1024)
	if full.IsError {
		t.Fatalf("Execute() = %#v", full)
	}
	marker := sandboxedBashStdout(t, full.Content)
	r, size := utf8.DecodeRuneInString(marker)
	if size != len(marker) || size <= 1 || unicode.IsControl(r) {
		t.Fatalf("fallback marker = %q, want one multibyte non-control rune", marker)
	}
	for _, markerByte := range []byte(marker) {
		for _, value := range values {
			if bytes.IndexByte([]byte(value), markerByte) >= 0 {
				t.Fatalf("fallback marker byte %#x occurs in configured value", markerByte)
			}
		}
	}

	for cap := 1; cap <= len(marker)+1; cap++ {
		result := resultForCap(cap)
		if result.IsError {
			t.Fatalf("cap %d: Execute() = %#v", cap, result)
		}
		for _, value := range values {
			if strings.Contains(result.Content, value) {
				t.Fatalf("cap %d: Execute() exposed configured value %q: %q", cap, value, result.Content)
			}
		}
		wantLength := cap
		if wantLength > len(marker) {
			wantLength = len(marker)
		}
		if body := sandboxedBashStdout(t, result.Content); body != marker[:wantLength] {
			t.Fatalf("cap %d: stdout = %q, want marker prefix %q", cap, body, marker[:wantLength])
		}
	}
}

func TestBashSandboxedHoldsEarlierFragmentBeforeLaterMatchAndCap(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	const longSecret = "credential-zzSHORT-rest"
	values := []string{longSecret, "SHORT"}

	for split := 0; split <= len(longSecret); split++ {
		fake := &fakeBashExecutor{
			stdoutChunks: [][]byte{[]byte(longSecret[:split]), []byte(longSecret[split:])},
			status:       sandbox.ExitStatus{Code: 0},
		}
		bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 12, values)
		result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "ignored"}))
		if result.IsError {
			t.Fatalf("split %d: Execute() = %#v", split, result)
		}
		if body := sandboxedBashStdout(t, result.Content); body != "*" {
			t.Fatalf("split %d: stdout = %q, want only the redaction marker", split, body)
		}
		if strings.Contains(result.Content, "credential-") {
			t.Fatalf("split %d: pre-truncation credential bytes escaped: %q", split, result.Content)
		}
		for _, value := range values {
			if strings.Contains(result.Content, value) {
				t.Fatalf("split %d: configured value %q escaped: %q", split, value, result.Content)
			}
		}
	}
}

func TestBashSandboxedRedactsStrictJSONErrorsWithoutDelegating(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	tests := []struct {
		name      string
		secret    string
		arguments json.RawMessage
		wantText  string
	}{
		{
			name:      "unknown field name and value",
			secret:    "private-field",
			arguments: json.RawMessage(`{"command":"true","private-field":"private-field"}`),
			wantText:  "json: unknown field",
		},
		{
			name:      "JSON value kind",
			secret:    "number",
			arguments: json.RawMessage(`{"command":123}`),
			wantText:  "invalid JSON:",
		},
		{
			name:      "malformed text fragment",
			secret:    "X",
			arguments: json.RawMessage(`{"command":"true"}X`),
			wantText:  "invalid JSON:",
		},
		{
			name:      "required argument name",
			secret:    "command",
			arguments: json.RawMessage(`{}`),
			wantText:  "missing required argument:",
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			fake := &fakeBashExecutor{}
			bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 1024, []string{test.secret})
			result := bash.Execute(context.Background(), test.arguments)
			if !result.IsError || !strings.Contains(result.Content, test.wantText) {
				t.Fatalf("Execute() = %#v, want fixed argument semantics containing %q", result, test.wantText)
			}
			if strings.Contains(result.Content, test.secret) {
				t.Fatalf("Execute() exposed strict-JSON value %q: %q", test.secret, result.Content)
			}
			if calls := fake.CallCount(); calls != 0 {
				t.Fatalf("strict-JSON error delegated %d calls", calls)
			}
		})
	}
}

func TestBashSandboxedRejectsArgumentsAndPreCancellationWithoutExecution(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	fake := &fakeBashExecutor{}
	bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 1024, nil)

	for _, test := range []struct {
		name      string
		arguments json.RawMessage
		want      string
	}{
		{name: "invalid JSON", arguments: json.RawMessage(`{"command":"unterminated"`), want: "invalid JSON: unexpected EOF"},
		{name: "unknown field", arguments: json.RawMessage(`{"command":"true","extra":true}`), want: `json: unknown field "extra"`},
		{name: "missing command", arguments: json.RawMessage(`{}`), want: "missing required argument: command"},
		{name: "blank command", arguments: json.RawMessage(`{"command":" \t "}`), want: "missing required argument: command"},
	} {
		t.Run(test.name, func(t *testing.T) {
			result := bash.Execute(context.Background(), test.arguments)
			if !result.IsError || result.Content != test.want {
				t.Fatalf("Execute() = %#v, want error %q", result, test.want)
			}
		})
	}
	if calls := fake.CallCount(); calls != 0 {
		t.Fatalf("invalid arguments delegated %d calls", calls)
	}

	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	result := bash.Execute(ctx, mustJSON(t, map[string]string{"command": "must not run"}))
	want := "stdout:\n\nstderr:\n\nstatus: cancelled"
	if result.IsError || result.Content != want {
		t.Fatalf("pre-cancelled Execute() = %#v, want content %q", result, want)
	}
	if calls := fake.CallCount(); calls != 0 {
		t.Fatalf("pre-cancelled context delegated %d calls", calls)
	}
}

func TestBashSandboxedDistinguishesCancellationAndTimeout(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())

	t.Run("caller cancellation", func(t *testing.T) {
		started := make(chan struct{})
		fake := &fakeBashExecutor{execute: func(ctx context.Context, _ sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
			close(started)
			<-ctx.Done()
			if _, err := io.WriteString(streams.Stdout, "partial output"); err != nil {
				return sandbox.ExitStatus{}, err
			}
			return sandbox.ExitStatus{Code: -1, Signaled: true, Signal: "killed"}, context.Canceled
		}}
		bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 1024, []string{"longer-than-partial-output-secret"})
		ctx, cancel := context.WithCancel(context.Background())
		resultCh := executeBashAsync(bash, ctx, mustJSON(t, map[string]string{"command": "wait"}))
		<-started
		cancel()

		result := <-resultCh
		want := "stdout:\npartial output\nstderr:\n\nstatus: cancelled; signal: killed"
		if result.IsError || result.Content != want {
			t.Fatalf("Execute() = %#v, want content %q", result, want)
		}
	})

	t.Run("adapter timeout cause", func(t *testing.T) {
		fake := &fakeBashExecutor{execute: func(ctx context.Context, _ sandbox.Request, _ sandbox.Streams) (sandbox.ExitStatus, error) {
			<-ctx.Done()
			return sandbox.ExitStatus{Code: -1, Signaled: true, Signal: "killed"}, context.Canceled
		}}
		bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 1024, nil)
		triggerReady := make(chan func())
		causeSeen := make(chan error, 1)
		bash.deadlineContext = func(parent context.Context, _ time.Duration, timeoutCause error) (context.Context, context.CancelFunc) {
			child, cancelCause := context.WithCancelCause(parent)
			causeSeen <- timeoutCause
			triggerReady <- func() { cancelCause(timeoutCause) }
			return child, func() { cancelCause(context.Canceled) }
		}

		resultCh := executeBashAsync(bash, context.Background(), mustJSON(t, map[string]string{"command": "wait"}))
		trigger := <-triggerReady
		trigger()
		result := <-resultCh
		cause := <-causeSeen
		if cause == nil || cause == context.Canceled || cause == context.DeadlineExceeded {
			t.Fatalf("timeout cause = %v, want unique cause", cause)
		}
		want := "stdout:\n\nstderr:\n\nstatus: timed out after 3s; signal: killed"
		if result.IsError || result.Content != want {
			t.Fatalf("Execute() = %#v, want content %q", result, want)
		}
	})

	t.Run("fired timeout survives successful cleanup", func(t *testing.T) {
		fake := &fakeBashExecutor{execute: func(ctx context.Context, _ sandbox.Request, _ sandbox.Streams) (sandbox.ExitStatus, error) {
			<-ctx.Done()
			return sandbox.ExitStatus{Code: -1, Signaled: true, Signal: "killed"}, nil
		}}
		bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 1024, nil)
		triggerReady := make(chan func())
		bash.deadlineContext = func(parent context.Context, _ time.Duration, timeoutCause error) (context.Context, context.CancelFunc) {
			child, cancelCause := context.WithCancelCause(parent)
			triggerReady <- func() { cancelCause(timeoutCause) }
			return child, func() { cancelCause(context.Canceled) }
		}

		resultCh := executeBashAsync(bash, context.Background(), mustJSON(t, map[string]string{"command": "wait"}))
		trigger := <-triggerReady
		trigger()
		result := <-resultCh
		if result.IsError || !strings.Contains(result.Content, "status: timed out after 3s; signal: killed") {
			t.Fatalf("Execute() = %#v", result)
		}
	})

	t.Run("parent cancellation wins timeout race", func(t *testing.T) {
		observedCancellation := make(chan struct{})
		release := make(chan struct{})
		fake := &fakeBashExecutor{execute: func(ctx context.Context, _ sandbox.Request, _ sandbox.Streams) (sandbox.ExitStatus, error) {
			<-ctx.Done()
			close(observedCancellation)
			<-release
			return sandbox.ExitStatus{Code: -1, Signaled: true, Signal: "killed"}, context.Canceled
		}}
		bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 1024, nil)
		triggerReady := make(chan func())
		bash.deadlineContext = func(parent context.Context, _ time.Duration, timeoutCause error) (context.Context, context.CancelFunc) {
			child, cancelCause := context.WithCancelCause(parent)
			triggerReady <- func() { cancelCause(timeoutCause) }
			return child, func() { cancelCause(context.Canceled) }
		}
		parent, cancelParent := context.WithCancel(context.Background())
		resultCh := executeBashAsync(bash, parent, mustJSON(t, map[string]string{"command": "wait"}))
		trigger := <-triggerReady
		trigger()
		<-observedCancellation
		cancelParent()
		close(release)

		result := <-resultCh
		if result.IsError || !strings.Contains(result.Content, "status: cancelled; signal: killed") || strings.Contains(result.Content, "timed out") {
			t.Fatalf("Execute() did not prefer parent cancellation: %#v", result)
		}
	})

	t.Run("successful return cancels deadline before inspecting it", func(t *testing.T) {
		fake := &fakeBashExecutor{status: sandbox.ExitStatus{Code: 0}}
		bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 1024, nil)
		cancelCalled := make(chan struct{})
		bash.deadlineContext = func(parent context.Context, _ time.Duration, timeoutCause error) (context.Context, context.CancelFunc) {
			child, cancelCause := context.WithCancelCause(parent)
			wrapped := &cancelOnFirstErrContext{
				Context: child,
				cancel:  func() { cancelCause(timeoutCause) },
			}
			return wrapped, func() {
				cancelCause(context.Canceled)
				close(cancelCalled)
			}
		}

		result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "true"}))
		<-cancelCalled
		if result.IsError || !strings.Contains(result.Content, "exit_code: 0") || strings.Contains(result.Content, "timed out") {
			t.Fatalf("Execute() = %#v", result)
		}
	})
}

func TestBashSandboxedInfrastructureErrorsAreFixedAndDiscardDiagnostics(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	const secret = "infrastructure-secret-value"

	for _, test := range []struct {
		name string
		err  error
	}{
		{name: "launch", err: sandbox.ErrChildLaunch},
		{name: "wait", err: sandbox.ErrChildWait},
		{name: "terminate", err: sandbox.ErrChildTerminate},
		{name: "closed", err: sandbox.ErrClosed},
		{name: "unavailable", err: &sandbox.UnavailableError{Reason: sandbox.ReasonRuntimeFailure}},
		{name: "invalid boundary", err: sandbox.ErrInvalidRequest},
		{name: "environment", err: sandbox.ErrEnvironmentUnsafe},
		{name: "unsupported policy", err: sandbox.ErrUnsupportedPolicy},
		{name: "raw", err: errors.New("raw launch detail " + secret)},
		{name: "wrapped cancellation", err: fmt.Errorf("raw wrapper %s: %w", secret, context.Canceled)},
		{name: "joined cancellation and infrastructure", err: errors.Join(context.Canceled, sandbox.ErrChildTerminate, errors.New("raw "+secret))},
	} {
		t.Run(test.name, func(t *testing.T) {
			fake := &fakeBashExecutor{
				stdoutChunks: [][]byte{[]byte("diagnostic-"), []byte(secret)},
				stderrChunks: [][]byte{[]byte("raw stderr " + secret)},
				err:          test.err,
			}
			bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 8, []string{secret})
			result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "ignored"}))
			if !result.IsError || result.Content != "sandbox execution unavailable" {
				t.Fatalf("Execute() = %#v", result)
			}
			if strings.Contains(result.Content, secret) || strings.Contains(result.Content, "raw") || strings.Contains(result.Content, "diagnostic") {
				t.Fatalf("Execute() exposed diagnostics: %#v", result)
			}
		})
	}

	t.Run("capture failure is fixed and safe", func(t *testing.T) {
		fake := &fakeBashExecutor{execute: func(_ context.Context, _ sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
			writer, ok := streams.Stdout.(*exactRedactingWriter)
			if !ok {
				return sandbox.ExitStatus{}, errors.New("unexpected writer type")
			}
			writer.err = errors.New("raw capture failure " + secret)
			return sandbox.ExitStatus{Code: 0}, nil
		}}
		bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 1024, []string{secret})
		result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "ignored"}))
		if !result.IsError || result.Content != "sandbox execution unavailable" {
			t.Fatalf("Execute() = %#v", result)
		}
	})

	t.Run("infrastructure wins ended context", func(t *testing.T) {
		started := make(chan struct{})
		fake := &fakeBashExecutor{execute: func(ctx context.Context, _ sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
			close(started)
			<-ctx.Done()
			_, _ = io.WriteString(streams.Stdout, secret)
			return sandbox.ExitStatus{Code: -1, Signaled: true, Signal: "killed"}, errors.Join(context.Canceled, sandbox.ErrChildTerminate)
		}}
		bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 1024, []string{secret})
		ctx, cancel := context.WithCancel(context.Background())
		resultCh := executeBashAsync(bash, ctx, mustJSON(t, map[string]string{"command": "wait"}))
		<-started
		cancel()
		result := <-resultCh
		if !result.IsError || result.Content != "sandbox execution unavailable" {
			t.Fatalf("Execute() = %#v", result)
		}
	})
}

func TestBashSandboxedConcurrentCallsKeepOutputAndRedactionIndependent(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	arrived := make(chan string, 2)
	release := make(chan struct{})
	fake := &fakeBashExecutor{execute: func(_ context.Context, request sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
		name := request.Argv[2]
		if _, err := io.WriteString(streams.Stdout, name+":shared-"); err != nil {
			return sandbox.ExitStatus{}, err
		}
		arrived <- name
		<-release
		if _, err := io.WriteString(streams.Stdout, "secret"); err != nil {
			return sandbox.ExitStatus{}, err
		}
		if _, err := io.WriteString(streams.Stderr, "stderr-"+name); err != nil {
			return sandbox.ExitStatus{}, err
		}
		return sandbox.ExitStatus{Code: 0}, nil
	}}
	bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 1024, []string{"shared-secret"})

	type namedResult struct {
		name   string
		result Result
	}
	results := make(chan namedResult, 2)
	for _, name := range []string{"first", "second"} {
		name := name
		go func() {
			results <- namedResult{name: name, result: bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": name}))}
		}()
	}
	seen := map[string]bool{<-arrived: true, <-arrived: true}
	if !seen["first"] || !seen["second"] {
		t.Fatalf("arrivals = %#v", seen)
	}
	close(release)

	for range 2 {
		got := <-results
		other := "first"
		if got.name == "first" {
			other = "second"
		}
		if got.result.IsError || !strings.Contains(got.result.Content, got.name+":*") || !strings.Contains(got.result.Content, "stderr-"+got.name) {
			t.Fatalf("%s Execute() = %#v", got.name, got.result)
		}
		if strings.Contains(got.result.Content, other+":") || strings.Contains(got.result.Content, "stderr-"+other) || strings.Contains(got.result.Content, "shared-secret") {
			t.Fatalf("%s result mixed concurrent state: %q", got.name, got.result.Content)
		}
	}
}

func TestBashSandboxedDirectIntegration(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	driver := direct.New()
	executor, err := sandbox.NewExecutor(driver, sandbox.Policy{
		Filesystem: sandbox.FilesystemUnconfined,
		Network:    sandbox.NetworkAllow,
	}, workspace.root)
	if err != nil {
		t.Fatalf("sandbox.NewExecutor() error = %v", err)
	}
	t.Cleanup(func() {
		if err := executor.Close(); err != nil {
			t.Errorf("Executor.Close() error = %v", err)
		}
	})

	environment := []string{
		"PATH=/usr/bin:/bin",
		"HOME=",
		"ENV=",
		"LC_ALL=C",
		"OTTO_DIRECT_VALUE=deterministic",
	}
	bash, err := NewSandboxedBashTool(workspace, executor, "/bin/sh", environment, 3*time.Second, 4096, []string{})
	if err != nil {
		t.Fatalf("NewSandboxedBashTool() error = %v", err)
	}
	result := bash.Execute(context.Background(), mustJSON(t, map[string]string{
		"command": `printf 'cwd='; /bin/pwd; printf 'env=%s\n' "$OTTO_DIRECT_VALUE"; printf 'problem\n' >&2; exit 7`,
	}))
	if result.IsError {
		t.Fatalf("Execute() = %#v", result)
	}
	for _, expected := range []string{
		"stdout:\ncwd=" + workspace.root,
		"env=deterministic",
		"stderr:\nproblem",
		"exit_code: 7",
	} {
		if !strings.Contains(result.Content, expected) {
			t.Fatalf("Execute() missing %q: %q", expected, result.Content)
		}
	}
}

func mustSandboxedBashTool(t *testing.T, workspace *Workspace, executor sandbox.CommandExecutor, environment []string, maxOutputBytes int, redactions []string) *sandboxedBashTool {
	t.Helper()
	constructed, err := NewSandboxedBashTool(workspace, executor, "/bin/sh", environment, 3*time.Second, maxOutputBytes, redactions)
	if err != nil {
		t.Fatalf("NewSandboxedBashTool() error = %v", err)
	}
	bash, ok := constructed.(*sandboxedBashTool)
	if !ok {
		t.Fatalf("NewSandboxedBashTool() type = %T, want *sandboxedBashTool", constructed)
	}
	return bash
}

func executeBashAsync(bash Tool, ctx context.Context, arguments json.RawMessage) <-chan Result {
	result := make(chan Result, 1)
	go func() {
		result <- bash.Execute(ctx, arguments)
	}()
	return result
}

type cancelOnFirstErrContext struct {
	context.Context
	once   sync.Once
	cancel func()
}

func (c *cancelOnFirstErrContext) Err() error {
	c.once.Do(c.cancel)
	return c.Context.Err()
}

type fakeBashExecutor struct {
	mu sync.Mutex

	requests         []sandbox.Request
	retainedRequests []sandbox.Request
	streams          []sandbox.Streams
	stdoutChunks     [][]byte
	stderrChunks     [][]byte
	status           sandbox.ExitStatus
	err              error
	execute          func(context.Context, sandbox.Request, sandbox.Streams) (sandbox.ExitStatus, error)
}

func (f *fakeBashExecutor) Execute(ctx context.Context, request sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
	f.mu.Lock()
	f.requests = append(f.requests, request.Clone())
	f.retainedRequests = append(f.retainedRequests, request)
	f.streams = append(f.streams, streams)
	execute := f.execute
	stdoutChunks := cloneByteChunks(f.stdoutChunks)
	stderrChunks := cloneByteChunks(f.stderrChunks)
	status := f.status
	err := f.err
	f.mu.Unlock()

	if execute != nil {
		return execute(ctx, request, streams)
	}
	if writeErr := writeFakeChunks(streams.Stdout, stdoutChunks); writeErr != nil {
		return status, writeErr
	}
	if writeErr := writeFakeChunks(streams.Stderr, stderrChunks); writeErr != nil {
		return status, writeErr
	}
	return status, err
}

func (f *fakeBashExecutor) Requests() []sandbox.Request {
	f.mu.Lock()
	defer f.mu.Unlock()
	requests := make([]sandbox.Request, len(f.requests))
	for i := range f.requests {
		requests[i] = f.requests[i].Clone()
	}
	return requests
}

func (f *fakeBashExecutor) Streams() []sandbox.Streams {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]sandbox.Streams(nil), f.streams...)
}

func (f *fakeBashExecutor) CallCount() int {
	f.mu.Lock()
	defer f.mu.Unlock()
	return len(f.requests)
}

func (f *fakeBashExecutor) mutateRetainedRequest(index int) {
	f.mu.Lock()
	defer f.mu.Unlock()
	request := &f.retainedRequests[index]
	request.Argv[0] = "executor-mutated-shell"
	request.Env[0] = "FIRST=executor-mutated"
}

func cloneByteChunks(chunks [][]byte) [][]byte {
	cloned := make([][]byte, len(chunks))
	for i := range chunks {
		cloned[i] = append([]byte(nil), chunks[i]...)
	}
	return cloned
}

func writeFakeChunks(destination io.Writer, chunks [][]byte) error {
	for _, chunk := range chunks {
		n, err := destination.Write(chunk)
		if err != nil {
			return err
		}
		if n != len(chunk) {
			return io.ErrShortWrite
		}
	}
	return nil
}

func sandboxedBashStdout(t *testing.T, content string) string {
	t.Helper()
	const prefix = "stdout:\n"
	if !strings.HasPrefix(content, prefix) {
		t.Fatalf("sandboxed Bash content lacks stdout prefix: %q", content)
	}
	stdout, _, ok := strings.Cut(strings.TrimPrefix(content, prefix), "\nstderr:\n")
	if !ok {
		t.Fatalf("sandboxed Bash content lacks stderr delimiter: %q", content)
	}
	if captured, _, truncated := strings.Cut(stdout, "\n[truncated:"); truncated {
		return captured
	}
	return stdout
}

func mustJSON(t *testing.T, value any) json.RawMessage {
	t.Helper()
	encoded, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	return encoded
}
