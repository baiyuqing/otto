package tool

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
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

	bash, err := NewBashTool(workspace, fake, shell, environment, 3*time.Second, 1024, redactions)
	if err != nil {
		t.Fatalf("NewBashTool() error = %v", err)
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

func TestBashSandboxedNormalizesBeforeFinalRedactionAndCapsCompleteUTF8(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	var printableASCII strings.Builder
	for value := byte(0x20); value <= 0x7e; value++ {
		printableASCII.WriteByte(value)
	}
	values := []string{"TOKEN", "prefix�", printableASCII.String()}

	t.Run("multibyte marker cap cannot synthesize a secret", func(t *testing.T) {
		fake := &fakeBashExecutor{stdoutChunks: [][]byte{[]byte("prefixTOKEN")}}
		bash := mustSandboxedBashTool(t, workspace, fake, []string{}, len("prefix")+1, values)
		result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "ignored"}))
		if result.IsError || !utf8.ValidString(result.Content) {
			t.Fatalf("Execute() = %#v, want valid UTF-8", result)
		}
		for _, secret := range values {
			if strings.Contains(result.Content, secret) {
				t.Fatalf("Execute() synthesized or exposed %q: %q", secret, result.Content)
			}
		}
		if body := sandboxedBashStdout(t, result.Content); body != "prefix" {
			t.Fatalf("stdout = %q, want complete-rune prefix", body)
		}
		if !strings.Contains(result.Content, "[truncated: 2 bytes omitted]") {
			t.Fatalf("Execute() did not truthfully count the omitted marker: %q", result.Content)
		}
	})

	t.Run("an omitted atomic marker cannot join bytes into another secret", func(t *testing.T) {
		joinValues := append(append([]string{}, values...), "prefixs")
		fake := &fakeBashExecutor{stdoutChunks: [][]byte{[]byte("prefixTOKENsuffix")}}
		bash := mustSandboxedBashTool(t, workspace, fake, []string{}, len("prefix")+1, joinValues)
		result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "ignored"}))
		if result.IsError || !utf8.ValidString(result.Content) {
			t.Fatalf("Execute() = %#v, want valid UTF-8", result)
		}
		for _, secret := range joinValues {
			if strings.Contains(result.Content, secret) {
				t.Fatalf("post-cap output synthesized configured value %q: %q", secret, result.Content)
			}
		}
		if !strings.Contains(result.Content, "[truncated: 8 bytes omitted]") {
			t.Fatalf("post-cap omission count was not truthful: %q", result.Content)
		}
	})

	t.Run("arbitrary invalid child bytes are normalized then redacted", func(t *testing.T) {
		fake := &fakeBashExecutor{stdoutChunks: [][]byte{{'p', 'r', 'e', 'f', 'i', 'x', 0xff}}}
		bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 1024, values)
		result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "ignored"}))
		if result.IsError || !utf8.ValidString(result.Content) {
			t.Fatalf("Execute() = %#v, want valid UTF-8", result)
		}
		if strings.Contains(result.Content, "prefix�") {
			t.Fatalf("normalization synthesized a configured secret: %q", result.Content)
		}
	})
}

func TestBashSandboxedCanonicalizesInvalidRedactionsSeparatelyFromEnvironment(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	invalidFF := string(append([]byte("decoded-prefix"), 0xff))
	invalidOverlong := string(append([]byte("overlong-prefix"), 0xc0, 0xaf))
	canonical := []string{"decoded-prefix�", "overlong-prefix��"}
	environment := []string{"RAW_ENDPOINT=" + invalidFF}

	for _, maxOutput := range []int{1, 1024} {
		t.Run(fmt.Sprintf("cap-%d", maxOutput), func(t *testing.T) {
			fake := &fakeBashExecutor{execute: func(_ context.Context, request sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
				if !reflect.DeepEqual(request.Env, environment) {
					t.Errorf("Executor environment = %q, want unmodified clone %q", request.Env, environment)
				}
				stdout := append([]byte("decoded-prefix"), 0xff)
				stderr := append([]byte("overlong-prefix"), 0xc0, 0xaf)
				for _, value := range stdout {
					if _, err := streams.Stdout.Write([]byte{value}); err != nil {
						return sandbox.ExitStatus{}, err
					}
				}
				for _, value := range stderr {
					if _, err := streams.Stderr.Write([]byte{value}); err != nil {
						return sandbox.ExitStatus{}, err
					}
				}
				return sandbox.ExitStatus{Code: 0}, nil
			}}
			bash := mustSandboxedBashTool(t, workspace, fake, environment, maxOutput, []string{invalidFF, invalidOverlong})
			result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "ignored"}))
			if result.IsError || !utf8.ValidString(result.Content) {
				t.Fatalf("Execute() = %#v, want valid UTF-8", result)
			}
			for _, secret := range canonical {
				if strings.Contains(result.Content, secret) {
					t.Fatalf("Execute() retained canonical configured value %q: %q", secret, result.Content)
				}
			}
			if maxOutput == 1 && (sandboxedBashStdout(t, result.Content) != "*" || !strings.Contains(result.Content, "stderr:\n*")) {
				t.Fatalf("one-byte cap did not retain atomic redactions on both streams: %q", result.Content)
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
		if result.IsError || !utf8.ValidString(result.Content) {
			t.Fatalf("cap %d: Execute() = %#v, want valid UTF-8", cap, result)
		}
		for _, value := range values {
			if strings.Contains(result.Content, value) {
				t.Fatalf("cap %d: Execute() exposed configured value %q: %q", cap, value, result.Content)
			}
		}
		want := marker
		if cap < len(marker) {
			want = ""
		}
		if body := sandboxedBashStdout(t, result.Content); body != want {
			t.Fatalf("cap %d: stdout = %q, want complete marker %q", cap, body, want)
		}
		if cap < len(marker) && !strings.Contains(result.Content, fmt.Sprintf("[truncated: %d bytes omitted]", len(marker))) {
			t.Fatalf("cap %d: truncation was not truthful: %q", cap, result.Content)
		}
	}
}

func TestBashSandboxedConstructsWithEveryUTF8ByteClassInSensitiveValues(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	secret := markerByteClassAdversary(t)
	fake := &fakeBashExecutor{stdoutChunks: [][]byte{[]byte(secret)}}
	bash, err := NewBashTool(workspace, fake, "/bin/sh", []string{}, 3*time.Second, 1024, []string{secret})
	if err != nil {
		t.Fatalf("NewBashTool() rejected a valid UTF-8 redaction set: %v", err)
	}
	result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "ignored"}))
	if result.IsError || !utf8.ValidString(result.Content) || strings.Contains(result.Content, secret) {
		t.Fatalf("Execute() did not safely redact the adversarial value: %#v", result)
	}
}

func TestBashSandboxedExpandsJSONStringEscapeRedactions(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	tests := []struct {
		raw     string
		decoded string
	}{
		{raw: `less\u003cthan`, decoded: "less<than"},
		{raw: `greater\u003Ethan`, decoded: "greater>than"},
		{raw: `amp\u0026ersand`, decoded: "amp&ersand"},
		{raw: `quote\"value`, decoded: `quote"value`},
		{raw: `back\\slash`, decoded: `back\slash`},
		{raw: `slash\/value`, decoded: "slash/value"},
		{raw: `line\nbreak`, decoded: "line\nbreak"},
		{raw: `separator\u2028value`, decoded: "separator\u2028value"},
		{raw: `paragraph\u2029value`, decoded: "paragraph\u2029value"},
	}
	values := make([]string, len(tests))
	chunks := make([][]byte, len(tests))
	for index, test := range tests {
		values[index] = test.raw
		chunks[index] = []byte(test.decoded + "\n")
	}
	fake := &fakeBashExecutor{stdoutChunks: chunks, status: sandbox.ExitStatus{Code: 0}}
	bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 64<<10, values)
	result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "ignored"}))
	if result.IsError {
		t.Fatalf("Execute() = %#v", result)
	}
	for _, test := range tests {
		if strings.Contains(result.Content, test.raw) || strings.Contains(result.Content, test.decoded) {
			t.Fatalf("Execute() retained raw/decoded JSON escape secret (%q, %q): %q", test.raw, test.decoded, result.Content)
		}
	}
}

func TestBashSandboxedExhaustiveMarkerFallbackSuppressesAllResultText(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	allRunes := allNonControlSandboxRunes(t)
	longPrefix := strings.Repeat("a", 1<<20) + "z"
	values := []string{allRunes, longPrefix, string(preferredSandboxRedactionMarker), "X", "ab"}
	fake := &fakeBashExecutor{
		stdoutChunks: [][]byte{[]byte("a"), []byte("X"), []byte("b")},
		stderrChunks: [][]byte{[]byte("aX"), []byte("b")},
	}
	bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 2<<20, values)
	if bash.redactionMarker != "" || bash.dynamicContent {
		t.Fatalf("exhaustive marker fallback = marker %q dynamic %t, want suppression", bash.redactionMarker, bash.dynamicContent)
	}

	result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": "ignored"}))
	if result.Content != "" || fake.CallCount() != 1 {
		t.Fatalf("Execute() = %#v calls=%d, want one execution with empty result", result, fake.CallCount())
	}
	invalid := bash.Execute(context.Background(), json.RawMessage(`{"command":123,"X":"X"}`))
	if invalid.Content != "" || !invalid.IsError || fake.CallCount() != 1 {
		t.Fatalf("invalid Execute() = %#v calls=%d, want fixed-free rejection", invalid, fake.CallCount())
	}
}

func TestBashSandboxedUnrepresentableOutputUsesBoundedOneByteDiscard(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	const half = 4 << 10
	byteA, byteX, byteB := []byte("a"), []byte("X"), []byte("b")
	fake := &fakeBashExecutor{execute: func(_ context.Context, _ sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
		for range half {
			_, _ = streams.Stdout.Write(byteA)
		}
		_, _ = streams.Stdout.Write(byteX)
		for range half {
			_, _ = streams.Stdout.Write(byteB)
		}
		return sandbox.ExitStatus{Code: 0}, nil
	}}
	bash := mustSandboxedBashTool(t, workspace, fake, []string{}, 2*half+1, []string{
		allNonControlSandboxRunes(t), strings.Repeat("a", 1<<20) + "z", "X", "ab",
	})
	arguments := mustJSON(t, map[string]string{"command": "ignored"})
	if allocations := testing.AllocsPerRun(3, func() {
		if result := bash.Execute(context.Background(), arguments); result.Content != "" {
			t.Fatalf("Execute() retained suppressed content: %#v", result)
		}
	}); allocations > 64 {
		t.Fatalf("one-byte suppressed Bash allocations = %.1f, want <= 64", allocations)
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
	bash, err := NewBashTool(workspace, executor, "/bin/sh", environment, 3*time.Second, 4096, []string{})
	if err != nil {
		t.Fatalf("NewBashTool() error = %v", err)
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

func allNonControlSandboxRunes(t *testing.T) string {
	t.Helper()
	var value strings.Builder
	for candidate := rune(1); candidate <= utf8.MaxRune; candidate++ {
		if !utf8.ValidRune(candidate) || unicode.IsControl(candidate) {
			continue
		}
		value.WriteRune(candidate)
	}
	result := value.String()
	if !utf8.ValidString(result) || !strings.ContainsRune(result, preferredSandboxRedactionMarker) {
		t.Fatal("exhaustive marker fixture is invalid or omitted the preferred rune")
	}
	return result
}

func markerByteClassAdversary(t *testing.T) string {
	t.Helper()
	var value []byte
	for character := byte(0x20); character <= 0x7e; character++ {
		value = append(value, character)
	}
	for continuation := byte(0x80); continuation <= 0xbf; continuation++ {
		value = append(value, 0xc2, continuation)
	}
	for lead := byte(0xc3); lead <= 0xdf; lead++ {
		value = append(value, lead, 0x80)
	}
	value = append(value, 0xe0, 0xa0, 0x80)
	for lead := byte(0xe1); lead <= 0xec; lead++ {
		value = append(value, lead, 0x80, 0x80)
	}
	value = append(value, 0xed, 0x80, 0x80)
	for lead := byte(0xee); lead <= 0xef; lead++ {
		value = append(value, lead, 0x80, 0x80)
	}
	value = append(value, 0xf0, 0x90, 0x80, 0x80)
	for lead := byte(0xf1); lead <= 0xf3; lead++ {
		value = append(value, lead, 0x80, 0x80, 0x80)
	}
	value = append(value, 0xf4, 0x80, 0x80, 0x80)
	if !utf8.Valid(value) {
		t.Fatal("marker adversary is not valid UTF-8")
	}
	return string(value)
}

func mustSandboxedBashTool(t *testing.T, workspace *Workspace, executor sandbox.CommandExecutor, environment []string, maxOutputBytes int, redactions []string) *bashTool {
	t.Helper()
	constructed, err := NewBashTool(workspace, executor, "/bin/sh", environment, 3*time.Second, maxOutputBytes, redactions)
	if err != nil {
		t.Fatalf("NewBashTool() error = %v", err)
	}
	bash, ok := constructed.(*bashTool)
	if !ok {
		t.Fatalf("NewBashTool() type = %T, want *bashTool", constructed)
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
	if strings.HasPrefix(stdout, "[truncated:") {
		return ""
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
