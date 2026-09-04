package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"reflect"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/memory"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/sandbox"
	"github.com/baiyuqing/otto/internal/session"
)

func TestRunSandboxFlagParsing(t *testing.T) {
	for _, value := range []string{"auto", "seatbelt", "off"} {
		t.Run(value, func(t *testing.T) {
			options, help, err := parseFlags([]string{"--sandbox", value}, io.Discard, io.Discard)
			if err != nil || help {
				t.Fatalf("parseFlags() = help %t error %v", help, err)
			}
			if options.sandbox != value || !options.sandboxSet {
				t.Fatalf("sandbox options = %q set=%t, want %q true", options.sandbox, options.sandboxSet, value)
			}
		})
	}

	var stderr bytes.Buffer
	if _, _, err := parseFlags([]string{"--sandbox", "docker"}, io.Discard, &stderr); err == nil {
		t.Fatal("parseFlags() accepted unsupported docker")
	}
	if got := stderr.String(); !strings.Contains(got, "--sandbox must be one of auto, seatbelt, off") || strings.Contains(got, "Docker") {
		t.Fatalf("stderr = %q", got)
	}
}

func TestRunSandboxCLIOverridesTOML(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeSandboxCLIConfig(t, "seatbelt", "allow", "http://127.0.0.1:1")
	deps := deterministicRunDependencies(t)
	var opened sandboxOpenOptions
	deps.openSandbox = func(_ context.Context, options sandboxOpenOptions) sandboxRuntime {
		opened = options
		return fakeSandboxRuntime(app.SandboxInfo{Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true}, &recordingSandboxExecutor{}, []string{})
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace, "--sandbox", "off"}, strings.NewReader("/exit\n"), &stdout, &stderr, testEnviron(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "provider-value",
	}), deps)
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if opened.Settings.Driver != sandbox.DriverOff || opened.Settings.Network != sandbox.NetworkAllow {
		t.Fatalf("opened settings = %#v, want CLI off with TOML network", opened.Settings)
	}
	if got, want := stderr.String(), "warning: sandbox is off; bash runs unsandboxed as your macOS user\n"; got != want {
		t.Fatalf("stderr = %q, want %q", got, want)
	}
}

func TestRunHelpDescribesSandboxModes(t *testing.T) {
	var stdout, stderr bytes.Buffer
	code := runForTest(t, context.Background(), []string{"--help"}, strings.NewReader(""), &stdout, &stderr, testEnviron(nil))
	if code != 0 || stderr.Len() != 0 {
		t.Fatalf("code = %d stderr = %q", code, stderr.String())
	}
	for _, text := range []string{"--sandbox", "auto", "Seatbelt", "off", "unsafe", "unsandboxed", "macOS user"} {
		if !strings.Contains(stdout.String(), text) {
			t.Fatalf("help missing %q: %s", text, stdout.String())
		}
	}
}

func TestRunEnumeratesEnvironmentExactlyOnceAndClonesSnapshot(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	entries := []string{"HOME=" + home, "SHELL=/bin/sh", "TEST_KEY=provider-value", "UNRELATED=preserved"}
	var enumerateCalls atomic.Int32
	enumerate := environmentEnumerator(func() []string {
		enumerateCalls.Add(1)
		return entries
	})
	deps := deterministicRunDependencies(t)
	var captured []string
	deps.openSandbox = func(_ context.Context, options sandboxOpenOptions) sandboxRuntime {
		entries[0] = "HOME=" + filepath.Join(t.TempDir(), "mutated")
		captured = append([]string(nil), options.HostEntries...)
		return fakeSandboxRuntime(app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true}, &recordingSandboxExecutor{}, []string{})
	}
	deps.resolveUserHome = func() (string, error) {
		return "", errors.New("HOME fallback must not run")
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader("/exit\n"), &stdout, &stderr, enumerate, deps)
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if enumerateCalls.Load() != 1 {
		t.Fatalf("environment enumerations = %d, want 1", enumerateCalls.Load())
	}
	if !containsString(captured, "HOME="+home) || containsString(captured, entries[0]) {
		t.Fatalf("captured HostEntries = %#v, want immutable original HOME", captured)
	}
}

func TestRunUsesSingleOSUserHomeFallbackWithoutReenumerating(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	liveHomeBefore, liveHomePresentBefore := os.LookupEnv("HOME")
	configPath := filepath.Join(home, ".config", "otto", "config.toml")
	if err := os.MkdirAll(filepath.Dir(configPath), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(configPath, []byte(`default_profile = "test"
[profiles.test]
provider = "openai-compatible"
base_url = "http://127.0.0.1:1"
model = "test-model"
api_key_env = "TEST_KEY"
`), 0o600); err != nil {
		t.Fatal(err)
	}
	var enumerations, fallbackCalls atomic.Int32
	deps := deterministicRunDependencies(t)
	deps.resolveUserHome = func() (string, error) {
		fallbackCalls.Add(1)
		return home, nil
	}
	fixture := newSandboxRuntimeFixture(t)
	deps.openSandbox = func(ctx context.Context, options sandboxOpenOptions) sandboxRuntime {
		return openSandboxRuntimeWithDependencies(ctx, options, fixture.dependencies())
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"--cwd", workspace}, strings.NewReader("/exit\n"), &stdout, &stderr, func() []string {
		enumerations.Add(1)
		return []string{"SHELL=/bin/sh", "TEST_KEY=provider-value"}
	}, deps)
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if enumerations.Load() != 1 || fallbackCalls.Load() != 1 {
		t.Fatalf("enumerations/fallback calls = %d/%d, want 1/1", enumerations.Load(), fallbackCalls.Load())
	}
	if got, want := fixture.openOptions.CacheBase, filepath.Join(home, "Library", "Caches"); got != want {
		t.Fatalf("Seatbelt CacheBase = %q, want fallback-derived %q", got, want)
	}
	liveHomeAfter, liveHomePresentAfter := os.LookupEnv("HOME")
	if liveHomeAfter != liveHomeBefore || liveHomePresentAfter != liveHomePresentBefore {
		t.Fatalf("run mutated live HOME: before=(%q,%t) after=(%q,%t)", liveHomeBefore, liveHomePresentBefore, liveHomeAfter, liveHomePresentAfter)
	}
}

func TestRunLoadsCustomCredentialBoundaryBeforeDynamicStartupErrors(t *testing.T) {
	tests := []struct {
		name      string
		paths     func(*testing.T) (secret string, workspace string, args []string)
		wantClass string
	}{
		{
			name: "cwd",
			paths: func(t *testing.T) (string, string, []string) {
				secret := filepath.Join(mustCanonicalDirectory(t, t.TempDir()), "CUSTOMVALUE", "missing-cwd")
				return secret, secret, []string{"--cwd", secret}
			},
			wantClass: "resolve cwd",
		},
		{
			name: "approve file",
			paths: func(t *testing.T) (string, string, []string) {
				secret := filepath.Join(mustCanonicalDirectory(t, t.TempDir()), "CUSTOMVALUE", "missing-approve")
				workspace := t.TempDir()
				return secret, workspace, []string{"--cwd", workspace, "--approve", "@" + secret}
			},
			wantClass: "read approve prompt",
		},
		{
			name: "continue listing",
			paths: func(t *testing.T) (string, string, []string) {
				secret := filepath.Join(mustCanonicalDirectory(t, t.TempDir()), "CUSTOMVALUE")
				if err := os.Mkdir(secret, 0o700); err != nil {
					t.Fatal(err)
				}
				return secret, secret, []string{"--cwd", secret, "--continue"}
			},
			wantClass: "no session found for workspace",
		},
		{
			name: "resume path",
			paths: func(t *testing.T) (string, string, []string) {
				secret := filepath.Join(mustCanonicalDirectory(t, t.TempDir()), "CUSTOMVALUE", "missing-session.jsonl")
				workspace := t.TempDir()
				return secret, workspace, []string{"--cwd", workspace, "--resume", secret}
			},
			wantClass: "open session file",
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			secret, _, args := test.paths(t)
			home := mustCanonicalDirectory(t, t.TempDir())
			if err := os.MkdirAll(filepath.Join(home, ".otto", "sessions"), 0o700); err != nil {
				t.Fatal(err)
			}
			configPath := filepath.Join(t.TempDir(), "config.toml")
			inactiveEndpoint := "https://example.test/v1?target=" + url.QueryEscape(secret)
			configText := fmt.Sprintf(`default_profile = "active"
[profiles.active]
provider = "openai-compatible"
base_url = "http://127.0.0.1:1"
model = "test-model"
api_key_env = "CUSTOMVALUE"
[profiles.inactive]
provider = "openai-compatible"
base_url = %q
model = "inactive-model"
api_key_env = "INACTIVE_CUSTOMVALUE"
`, inactiveEndpoint)
			if err := os.WriteFile(configPath, []byte(configText), 0o600); err != nil {
				t.Fatal(err)
			}
			args = append([]string{"--config", configPath}, args...)
			var stdout, stderr bytes.Buffer
			code := runWithDependencies(context.Background(), args, strings.NewReader(""), &stdout, &stderr, testEnviron(map[string]string{
				"HOME": home, "SHELL": "/bin/sh", "CUSTOMVALUE": "active-provider-value", "INACTIVE_CUSTOMVALUE": secret,
			}), deterministicRunDependencies(t))
			if code != 1 || !strings.Contains(stderr.String(), test.wantClass) {
				t.Fatalf("code = %d stderr = %q, want fixed class %q", code, stderr.String(), test.wantClass)
			}
			for _, forbidden := range []string{"CUSTOMVALUE", "INACTIVE_CUSTOMVALUE", secret, inactiveEndpoint} {
				if strings.Contains(stderr.String(), forbidden) {
					t.Fatalf("startup error retained custom credential name/value/endpoint %q: %q", forbidden, stderr.String())
				}
			}
		})
	}
}

func TestRunOversizedCustomCredentialFailsClosedBeforeApproveFileError(t *testing.T) {
	home := t.TempDir()
	configPath := filepath.Join(t.TempDir(), "config.toml")
	if err := os.WriteFile(configPath, []byte(`default_profile = "active"
[profiles.active]
provider = "openai-compatible"
base_url = "http://127.0.0.1:1"
model = "test-model"
api_key_env = "CUSTOMVALUE"
`), 0o600); err != nil {
		t.Fatal(err)
	}
	omitted := strings.Repeat("oversized-custom-credential-", 42_000)
	if len("CUSTOMVALUE="+omitted) <= maxLookupEnvironmentEntryBytes {
		t.Fatal("fixture did not exceed the lookup entry ceiling")
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{
		"--config", configPath, "--approve", "@" + omitted,
	}, strings.NewReader(""), &stdout, &stderr, func() []string {
		return []string{"HOME=" + home, "SHELL=/bin/sh", "CUSTOMVALUE=" + omitted}
	}, deterministicRunDependencies(t))
	if code != 1 {
		t.Fatalf("code = %d stderr bytes = %d", code, stderr.Len())
	}
	if strings.Contains(stderr.String(), omitted[:256]) || stderr.Len() > 1024 {
		t.Fatalf("pre-sandbox approve error retained oversized custom credential (%d bytes)", stderr.Len())
	}
}

func TestRunFlagParserErrorsUseFixedClass(t *testing.T) {
	for _, args := range [][]string{
		{"--shell-timeout", "flag-parser-sensitive-value"},
		{"--flag-parser-sensitive-value", "1"},
	} {
		var stdout, stderr bytes.Buffer
		code := runWithDependencies(context.Background(), args, strings.NewReader(""), &stdout, &stderr, func() []string {
			t.Fatal("environment must not be captured after an unsafe flag parser error")
			return nil
		}, deterministicRunDependencies(t))
		if code != 2 || stderr.String() != "otto: invalid command-line arguments\n" {
			t.Fatalf("args = %q code = %d stderr = %q, want fixed parse class", args, code, stderr.String())
		}
		if strings.Contains(stderr.String(), "flag-parser-sensitive-value") {
			t.Fatalf("flag parser retained raw value: %q", stderr.String())
		}
	}
}

func TestParseFlagsDiscardsRawParserDiagnostics(t *testing.T) {
	const sensitive = "direct-flag-parser-sensitive-value"
	for _, args := range [][]string{
		{"--shell-timeout", sensitive},
		{"--" + sensitive, "1"},
	} {
		var stderr bytes.Buffer
		if _, _, err := parseFlags(args, io.Discard, &stderr); err == nil {
			t.Fatalf("parseFlags(%q) succeeded", args)
		}
		if stderr.Len() != 0 {
			t.Fatalf("parseFlags(%q) wrote raw parser diagnostics: %q", args, stderr.String())
		}
	}
}

func TestRunConfigErrorsUseFixedPreBoundaryClass(t *testing.T) {
	secretRoot := filepath.Join(t.TempDir(), "config-path-sensitive-value")
	missing := filepath.Join(secretRoot, "missing.toml")
	malformed := filepath.Join(t.TempDir(), "config-decode-sensitive-value.toml")
	if err := os.WriteFile(malformed, []byte("config-toml-sensitive-value = ["), 0o600); err != nil {
		t.Fatal(err)
	}
	for _, path := range []string{missing, malformed} {
		var stdout, stderr bytes.Buffer
		code := runWithDependencies(context.Background(), []string{"--config", path}, strings.NewReader(""), &stdout, &stderr, testEnviron(map[string]string{
			"HOME": t.TempDir(), "SHELL": "/bin/sh",
		}), deterministicRunDependencies(t))
		if code != 1 || stderr.String() != "otto: load config: configuration is invalid or unavailable\n" {
			t.Fatalf("path = %q code = %d stderr = %q, want fixed config class", path, code, stderr.String())
		}
		for _, forbidden := range []string{"config-path-sensitive-value", "config-decode-sensitive-value", "config-toml-sensitive-value"} {
			if strings.Contains(stderr.String(), forbidden) {
				t.Fatalf("config error retained pre-boundary content %q: %q", forbidden, stderr.String())
			}
		}
	}
}

func TestRunRedactsProcessSecretsFromErrorsBeforeSandboxConstruction(t *testing.T) {
	secretPath := filepath.Join(t.TempDir(), "missing-aws-startup-secret")
	entries := []string{"HOME=" + t.TempDir(), "AWS_SECRET_ACCESS_KEY=" + secretPath}
	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"--cwd", secretPath}, strings.NewReader(""), &stdout, &stderr, func() []string {
		return entries
	}, deterministicRunDependencies(t))
	if code != 1 {
		t.Fatalf("code = %d stderr = %q", code, stderr.String())
	}
	if strings.Contains(stderr.String(), secretPath) || !strings.Contains(stderr.String(), "otto: resolve cwd:") {
		t.Fatalf("startup boundary leaked process secret: %q", stderr.String())
	}
}

func TestEnvironmentLookupBoundsMapAndSkipsMalformedEntries(t *testing.T) {
	if maxLookupEnvironmentEntries <= (1<<20)/8 || maxLookupEnvironmentBytes <= 1<<20 {
		t.Fatalf("production lookup ceilings must remain above Darwin process-environment limits")
	}
	entries := []string{
		"BROKEN",
		string([]byte{'I', 'N', 'V', 'A', 'L', 'I', 'D', '=', 0xff}),
		"TOO_BIG=" + strings.Repeat("x", maxLookupEnvironmentEntryBytes),
		"HOME=/safe/home",
		"TEST_KEY=provider-value",
	}
	lookup, err := newEnvironmentLookupWithLimits(entries, 2, 64)
	if err != nil || lookup.value("HOME") != "/safe/home" || lookup.value("TEST_KEY") != "provider-value" {
		t.Fatalf("bounded lookup lost unrelated valid values: lookup=%#v error=%v", lookup, err)
	}
	if lookup.value("INVALID") != "" {
		t.Fatal("bounded lookup retained a malformed entry")
	}

	if lookup, err := newEnvironmentLookupWithLimits(append(entries, "THIRD=value"), 2, 64); lookup != nil || !errors.Is(err, errEnvironmentSnapshotTooLarge) {
		t.Fatalf("entry-bound lookup = (%#v, %v), want fixed failure", lookup, err)
	}
	if lookup, err := newEnvironmentLookupWithLimits([]string{"ONE=1234", "TWO=5678"}, 4, 10); lookup != nil || !errors.Is(err, errEnvironmentSnapshotTooLarge) {
		t.Fatalf("byte-bound lookup = (%#v, %v), want fixed failure", lookup, err)
	}
}

func TestRunRejectsOversizedEnvironmentLookupWithoutLosingBoundedInputs(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	entries := []string{"BROKEN", "HOME=" + home, "SHELL=/bin/sh", "TEST_KEY=provider-value"}
	for index := range 9 {
		name := fmt.Sprintf("OVERSIZED_%02d", index)
		entries = append(entries, name+"="+strings.Repeat("x", (1<<20)-len(name)-2))
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader("/exit\n"), &stdout, &stderr, func() []string {
		return entries
	}, deterministicRunDependencies(t))
	if code != 1 || stderr.String() != "otto: process environment snapshot is too large\n" {
		t.Fatalf("code = %d stderr = %q, want bounded startup failure", code, stderr.String())
	}
}

func TestRunMalformedEnvironmentDisablesBashWithoutHidingProviderValues(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	deps := deterministicRunDependencies(t)
	fixture := newSandboxRuntimeFixture(t)
	deps.openSandbox = func(ctx context.Context, options sandboxOpenOptions) sandboxRuntime {
		fixture.settings = options.Settings
		fixture.workspace = options.Workspace
		fixture.shell = options.Shell
		fixture.home = options.Home
		fixture.hostEntries = options.HostEntries
		fixture.providerNames = options.ProviderNames
		return openSandboxRuntimeWithDependencies(ctx, options, fixture.dependencies())
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader("/exit\n"), &stdout, &stderr, func() []string {
		return []string{"BROKEN", "HOME=" + home, "SHELL=/bin/sh", "TEST_KEY=provider-value"}
	}, deps)
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	want := "warning: bash is unavailable because the configured sandbox could not be established (reason: environment-rejected); file tools remain available\n"
	if stderr.String() != want {
		t.Fatalf("stderr = %q, want %q", stderr.String(), want)
	}
}

func TestRunKnownIncompleteBoundaryRejectsResumeAndContinueBeforeSessionAccess(t *testing.T) {
	for _, selector := range []string{"resume", "continue"} {
		t.Run(selector, func(t *testing.T) {
			home := t.TempDir()
			workspace := t.TempDir()
			root := filepath.Join(home, ".otto", "sessions")
			path := createCLISession(t, root, workspace, "known-incomplete")
			before := bytes.TrimSuffix(mustReadFile(t, path), []byte{'\n'})
			if err := os.WriteFile(path, before, 0o600); err != nil {
				t.Fatal(err)
			}
			configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
			entries := []string{"HOME=" + home, "SHELL=/bin/sh", "TEST_KEY=provider-value"}
			for index := range 513 {
				entries = append(entries, fmt.Sprintf("VALUE_%03d_TOKEN=environment-sensitive-value-%03d", index, index))
			}

			deps := deterministicRunDependencies(t)
			var prepareCalls, prepareListedCalls, newCalls, sandboxCalls atomic.Int32
			deps.prepareSession = func(context.Context, string, string) (preparedSession, error) {
				prepareCalls.Add(1)
				return nil, errors.New("unexpected prepare")
			}
			deps.prepareListedSession = func(context.Context, string, string, string) (preparedSession, error) {
				prepareListedCalls.Add(1)
				return nil, errors.New("unexpected listed prepare")
			}
			deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) {
				newCalls.Add(1)
				return nil, errors.New("unexpected create")
			}
			deps.openSandbox = func(context.Context, sandboxOpenOptions) sandboxRuntime {
				sandboxCalls.Add(1)
				return fakeSandboxRuntime(app.SandboxInfo{}, nil, []string{})
			}
			args := []string{"--config", configPath, "--cwd", workspace}
			if selector == "resume" {
				args = append(args, "--resume", path)
			} else {
				args = append(args, "--continue")
			}
			var stdout, stderr bytes.Buffer
			code := runWithDependencies(context.Background(), args, strings.NewReader(""), &stdout, &stderr, func() []string { return entries }, deps)
			if code != 1 || stderr.String() != "otto: session operation is unavailable\n" {
				t.Fatalf("code=%d stderr=%q", code, stderr.String())
			}
			if prepareCalls.Load() != 0 || prepareListedCalls.Load() != 0 || newCalls.Load() != 0 || sandboxCalls.Load() != 0 {
				t.Fatalf("callbacks = prepare %d listed %d new %d sandbox %d", prepareCalls.Load(), prepareListedCalls.Load(), newCalls.Load(), sandboxCalls.Load())
			}
			if after := mustReadFile(t, path); !bytes.Equal(after, before) {
				t.Fatal("known-incomplete startup mutated session bytes")
			}
		})
	}
}

func TestRunKnownIncompleteBoundaryRejectsArchiveBeforeSessionAccess(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	root := filepath.Join(home, ".otto", "sessions")
	path := createCLISession(t, root, workspace, "known-incomplete-archive")
	before := mustReadFile(t, path)
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	entries := []string{"HOME=" + home, "SHELL=/bin/sh", "TEST_KEY=provider-value"}
	for index := range 513 {
		entries = append(entries, fmt.Sprintf("VALUE_%03d_TOKEN=environment-sensitive-value-%03d", index, index))
	}

	deps := deterministicRunDependencies(t)
	var sandboxCalls atomic.Int32
	deps.openSandbox = func(context.Context, sandboxOpenOptions) sandboxRuntime {
		sandboxCalls.Add(1)
		return fakeSandboxRuntime(app.SandboxInfo{}, nil, []string{})
	}
	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace, "--archive", path}, strings.NewReader(""), &stdout, &stderr, func() []string { return entries }, deps)
	if code != 1 || stderr.String() != "otto: session operation is unavailable\n" {
		t.Fatalf("code=%d stderr=%q", code, stderr.String())
	}
	if sandboxCalls.Load() != 0 {
		t.Fatalf("sandbox opened %d times, want 0", sandboxCalls.Load())
	}
	if after := mustReadFile(t, path); !bytes.Equal(after, before) {
		t.Fatal("known-incomplete archive mutated session bytes")
	}
	if _, err := os.Stat(filepath.Join(filepath.Dir(path), "archive", filepath.Base(path))); !os.IsNotExist(err) {
		t.Fatalf("archive target exists after rejected archive: %v", err)
	}
}

func TestRunLateIncompleteBoundaryClosesPreparedResumeWithoutRepair(t *testing.T) {
	for _, fixture := range []string{"missing delimiter", "truncated tail", "dangling tool call"} {
		t.Run(fixture, func(t *testing.T) {
			home := t.TempDir()
			workspace := t.TempDir()
			root := filepath.Join(home, ".otto", "sessions")
			store, err := session.Create(root, session.Header{
				Version: session.CurrentVersion, ID: "late-incomplete", Workspace: workspace,
				Provider: "openai-compatible", Profile: "test", Model: "test-model", CreatedAt: time.Now().UTC(),
			})
			if err != nil {
				t.Fatal(err)
			}
			path := store.Path()
			if fixture == "dangling tool call" {
				err = store.Append(context.Background(), model.Message{
					ID: "assistant-dangling", Role: model.RoleAssistant, CreatedAt: time.Now().UTC(), FinishReason: model.FinishToolCalls,
					Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-dangling", ToolName: "read", Arguments: json.RawMessage(`{"path":"README.md"}`)}},
				})
				if err != nil {
					t.Fatal(err)
				}
			}
			if err := store.Close(); err != nil {
				t.Fatal(err)
			}
			before := mustReadFile(t, path)
			switch fixture {
			case "missing delimiter":
				before = bytes.TrimSuffix(before, []byte{'\n'})
			case "truncated tail":
				before = append(before, []byte(`{"type":"message"`)...)
			}
			if err := os.WriteFile(path, before, 0o600); err != nil {
				t.Fatal(err)
			}

			configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
			deps := deterministicRunDependencies(t)
			var activateCalls atomic.Int32
			var wrapped *fakePreparedSession
			deps.prepareSession = func(ctx context.Context, candidate, expectedWorkspace string) (preparedSession, error) {
				prepared, prepareErr := prepareSession(ctx, candidate, expectedWorkspace)
				if prepareErr != nil {
					return nil, prepareErr
				}
				wrapped = &fakePreparedSession{
					info: prepared.Info(),
					activate: func(ctx context.Context) (session.Session, []session.Warning, error) {
						activateCalls.Add(1)
						return prepared.Activate(ctx)
					},
					close: prepared.Close,
				}
				return wrapped, nil
			}
			deps.openSandbox = func(context.Context, sandboxOpenOptions) sandboxRuntime {
				runtime := fakeSandboxRuntime(app.SandboxInfo{Mode: app.SandboxSeatbelt, BashAvailable: true}, &recordingSandboxExecutor{}, []string{})
				runtime.RedactionsComplete = false
				return runtime
			}

			var stdout, stderr bytes.Buffer
			code := runWithDependencies(context.Background(), []string{
				"--config", configPath, "--cwd", workspace, "--resume", path, "--approve", "safe",
			}, strings.NewReader(""), &stdout, &stderr, testEnviron(map[string]string{
				"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "provider-value",
			}), deps)
			if code != 1 || !strings.HasSuffix(stderr.String(), "otto: session operation is unavailable\n") {
				t.Fatalf("code=%d stderr=%q", code, stderr.String())
			}
			if wrapped == nil || activateCalls.Load() != 0 || wrapped.closeCalls.Load() != 1 {
				t.Fatalf("prepared state = %#v activate=%d", wrapped, activateCalls.Load())
			}
			if after := mustReadFile(t, path); !bytes.Equal(after, before) {
				t.Fatalf("late-incomplete startup repaired session: before=%d after=%d", len(before), len(after))
			}
		})
	}
}

func TestRunIncompleteInitialSessionIsLazyAndFrontendStateIsSuppressed(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	var providerCalls atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		providerCalls.Add(1)
		writeSSE(w, `{"choices":[{"delta":{"content":"dynamic"},"finish_reason":"stop"}]}`)
	}))
	defer server.Close()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)
	deps := deterministicRunDependencies(t)
	deps.detectTerminal = func(io.Reader, io.Writer) bool { return true }
	var newCalls, memoryOpenCalls, memoryScopeCalls atomic.Int32
	deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) {
		newCalls.Add(1)
		return nil, errors.New("unexpected persistent session creation")
	}
	deps.openMemoryService = func(context.Context, config.MemoryRuntime, []string, io.Writer) (memory.Service, memory.Scope, bool, error) {
		memoryOpenCalls.Add(1)
		return nil, memory.Scope{}, false, errors.New("unexpected memory open")
	}
	deps.workspaceMemoryScope = func(config.MemoryRuntime, string) (memory.Scope, error) {
		memoryScopeCalls.Add(1)
		return memory.Scope{}, nil
	}
	deps.openSandbox = func(context.Context, sandboxOpenOptions) sandboxRuntime {
		runtime := fakeSandboxRuntime(app.SandboxInfo{Mode: app.SandboxSeatbelt, BashAvailable: true}, &recordingSandboxExecutor{}, []string{})
		runtime.RedactionsComplete = false
		return runtime
	}
	deps.runTUI = func(ctx context.Context, _ io.Reader, _ io.Writer, backend app.Backend) error {
		info := backend.Info()
		if info.SessionID != "" || info.SessionPath != "" || info.Workspace != "" || info.Provider != "" || info.Profile != "" || info.Model != "" || info.ContextWindow != 0 || info.UsagePresent {
			return fmt.Errorf("Info exposed incomplete state: %#v", info)
		}
		if history := backend.History(); len(history) != 0 {
			return fmt.Errorf("History exposed incomplete state: %#v", history)
		}
		browser, ok := backend.(app.SessionBrowser)
		if !ok {
			return errors.New("backend omitted SessionBrowser contract")
		}
		if _, err := browser.ListSessions(ctx, 20); !errors.Is(err, app.ErrPersistenceDisabled) {
			return fmt.Errorf("ListSessions error = %v", err)
		}
		if _, err := browser.ResumeSession(ctx, filepath.Join(workspace, "secret.jsonl")); !errors.Is(err, app.ErrPersistenceDisabled) {
			return fmt.Errorf("ResumeSession error = %v", err)
		}
		if err := backend.NewSession(); !errors.Is(err, app.ErrPersistenceDisabled) {
			return fmt.Errorf("NewSession error = %v", err)
		}
		var events []agent.Event
		if err := backend.Prompt(ctx, "dynamic prompt", func(event agent.Event) { events = append(events, event) }); err != nil {
			return err
		}
		if len(events) != 2 || events[0].Type != agent.EventAgentStarted || events[1].Type != agent.EventAgentFinished {
			return fmt.Errorf("events = %#v", events)
		}
		return nil
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{
		"--config", configPath, "--cwd", workspace, "--ui", "tui",
	}, strings.NewReader(""), &stdout, &stderr, testEnviron(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "provider-value",
	}), deps)
	if code != 0 {
		t.Fatalf("code=%d stderr=%q", code, stderr.String())
	}
	if providerCalls.Load() != 0 || newCalls.Load() != 0 || memoryOpenCalls.Load() != 0 || memoryScopeCalls.Load() != 0 {
		t.Fatalf("dynamic callbacks = provider %d new %d memory-open %d memory-scope %d", providerCalls.Load(), newCalls.Load(), memoryOpenCalls.Load(), memoryScopeCalls.Load())
	}
	if _, err := os.Stat(filepath.Join(home, ".otto", "memory", "memory.db")); !os.IsNotExist(err) {
		t.Fatalf("late-incomplete startup created memory db: %v", err)
	}
	paths, err := filepath.Glob(filepath.Join(home, ".otto", "sessions", "*", "*.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	if len(paths) != 0 {
		t.Fatalf("lazy incomplete startup created sessions: %v", paths)
	}
}

func TestRunMalformedChatGPTCredentialsFallBackToLifecycleOnlyStartup(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := filepath.Join(t.TempDir(), "otto.toml")
	if err := os.WriteFile(configPath, []byte(`default_profile = "chatgpt"
[profiles.chatgpt]
provider = "chatgpt"
model = "gpt-5"
`), 0o600); err != nil {
		t.Fatal(err)
	}
	authPath := filepath.Join(home, ".otto", "auth", "chatgpt.json")
	if err := os.MkdirAll(filepath.Dir(authPath), 0o700); err != nil {
		t.Fatal(err)
	}
	const leaked = "broken-auth-secret"
	if err := os.WriteFile(authPath, []byte(`{"access_token":"`+leaked), 0o600); err != nil {
		t.Fatal(err)
	}
	deps := deterministicRunDependencies(t)
	deps.detectTerminal = func(io.Reader, io.Writer) bool { return true }
	var memoryOpenCalls atomic.Int32
	deps.openMemoryService = func(context.Context, config.MemoryRuntime, []string, io.Writer) (memory.Service, memory.Scope, bool, error) {
		memoryOpenCalls.Add(1)
		return nil, memory.Scope{}, false, errors.New("unexpected memory open")
	}
	deps.workspaceMemoryScope = func(config.MemoryRuntime, string) (memory.Scope, error) {
		return memory.Scope{}, nil
	}
	deps.runTUI = func(ctx context.Context, _ io.Reader, _ io.Writer, backend app.Backend) error {
		if info := backend.Info(); info.Provider != "" || info.Profile != "" || info.Model != "" || info.SessionID != "" || info.SessionPath != "" {
			return fmt.Errorf("Info exposed malformed auth state: %#v", info)
		}
		var events []agent.Event
		if err := backend.Prompt(ctx, "prompt", func(event agent.Event) { events = append(events, event) }); err != nil {
			return err
		}
		if len(events) != 2 || events[0].Type != agent.EventAgentStarted || events[1].Type != agent.EventAgentFinished {
			return fmt.Errorf("events = %#v", events)
		}
		return nil
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace, "--ui", "tui"}, strings.NewReader(""), &stdout, &stderr, testEnviron(map[string]string{"HOME": home, "SHELL": "/bin/sh"}), deps)
	if code != 0 {
		t.Fatalf("code=%d stderr=%q", code, stderr.String())
	}
	if memoryOpenCalls.Load() != 0 {
		t.Fatalf("memory opened %d times, want 0", memoryOpenCalls.Load())
	}
	if strings.Contains(stderr.String(), leaked) || strings.Contains(stderr.String(), authPath) {
		t.Fatalf("stderr leaked malformed auth detail: %q", stderr.String())
	}
}

func TestRunIncompleteEnvironmentRedactionFailsClosedAcrossProviderEventsAndPi(t *testing.T) {
	tests := []struct {
		name    string
		entries func() ([]string, string)
	}{
		{
			name: "513 sensitive values",
			entries: func() ([]string, string) {
				entries := make([]string, 0, 516)
				entries = append(entries, "HOME="+t.TempDir(), "SHELL=/bin/sh", "TEST_KEY=provider-value")
				for index := range 513 {
					entries = append(entries, fmt.Sprintf("VALUE_%03d_TOKEN=environment-sensitive-value-%03d", index, index))
				}
				return entries, "environment-sensitive-value-512"
			},
		},
		{
			name: "over one MiB",
			entries: func() ([]string, string) {
				omitted := strings.Repeat("oversized-environment-value-", 42_000)
				return []string{"HOME=" + t.TempDir(), "SHELL=/bin/sh", "TEST_KEY=provider-value", "LARGE_TOKEN=" + omitted}, omitted
			},
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			entries, omitted := test.entries()
			workspace := t.TempDir()
			var requestMu sync.Mutex
			var requestBody string
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				body, err := io.ReadAll(r.Body)
				if err != nil {
					t.Errorf("read request: %v", err)
				}
				requestMu.Lock()
				requestBody = string(body)
				requestMu.Unlock()
				writeSSE(w, `{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`)
			}))
			defer server.Close()
			configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)

			fixture := newSandboxRuntimeFixture(t)
			deps := deterministicRunDependencies(t)
			deps.openSandbox = func(ctx context.Context, options sandboxOpenOptions) sandboxRuntime {
				return openSandboxRuntimeWithDependencies(ctx, options, fixture.dependencies())
			}
			var stdout, stderr bytes.Buffer
			code := runWithDependencies(context.Background(), []string{
				"--config", configPath, "--cwd", workspace, "--approve", omitted,
			}, strings.NewReader(""), &stdout, &stderr, func() []string { return entries }, deps)
			if code != 0 {
				t.Fatalf("code = %d, stderr = %q", code, stderr.String())
			}
			wantWarning := "warning: bash is unavailable because the configured sandbox could not be established (reason: environment-rejected); file tools remain available\n"
			if stderr.String() != wantWarning {
				t.Fatalf("stderr = %q, want fixed environment warning", stderr.String())
			}
			paths, err := filepath.Glob(filepath.Join(strings.TrimPrefix(entries[0], "HOME="), ".otto", "sessions", "*", "*.jsonl"))
			if err != nil {
				t.Fatal(err)
			}
			if len(paths) != 0 {
				t.Fatalf("incomplete run persisted session paths: %v", paths)
			}
			requestMu.Lock()
			body := requestBody
			requestMu.Unlock()
			for boundary, content := range map[string]string{
				"provider request": body,
				"events":           stdout.String(),
				"stderr":           stderr.String(),
			} {
				if strings.Contains(content, omitted) {
					t.Fatalf("%s retained omitted environment value (content bytes %d)", boundary, len(content))
				}
			}
			if body != "" {
				t.Fatalf("incomplete boundary made a provider request: %q", body)
			}
			if fixture.seatbeltCalls.Load() != 0 || fixture.directCalls.Load() != 0 || fixture.executorCalls.Load() != 0 {
				t.Fatalf("incomplete environment constructed a host child path: seatbelt/direct/executor = %d/%d/%d", fixture.seatbeltCalls.Load(), fixture.directCalls.Load(), fixture.executorCalls.Load())
			}
		})
	}
}

func TestRunIncompleteEnvironmentDoesNotPersistOmittedRuntimeModel(t *testing.T) {
	const omitted = "environment-sensitive-value-512"
	home := t.TempDir()
	workspace := t.TempDir()
	entries := []string{"HOME=" + home, "SHELL=/bin/sh", "TEST_KEY=provider-value"}
	for index := range 513 {
		entries = append(entries, fmt.Sprintf("VALUE_%03d_TOKEN=environment-sensitive-value-%03d", index, index))
	}
	entries = append(entries, "OTTO_MODEL="+omitted)

	var requestMu sync.Mutex
	var requestBody string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(r.Body)
		if err != nil {
			t.Errorf("read request: %v", err)
		}
		requestMu.Lock()
		requestBody = string(body)
		requestMu.Unlock()
		writeSSE(w, `{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`)
	}))
	defer server.Close()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)
	fixture := newSandboxRuntimeFixture(t)
	deps := deterministicRunDependencies(t)
	deps.openSandbox = func(ctx context.Context, options sandboxOpenOptions) sandboxRuntime {
		return openSandboxRuntimeWithDependencies(ctx, options, fixture.dependencies())
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{
		"--config", configPath, "--cwd", workspace, "--approve", "safe prompt",
	}, strings.NewReader(""), &stdout, &stderr, func() []string { return entries }, deps)
	if code != 0 {
		t.Fatalf("code = %d stderr = %q", code, stderr.String())
	}
	requestMu.Lock()
	body := requestBody
	requestMu.Unlock()
	if strings.Contains(body, omitted) || strings.Contains(stdout.String(), omitted) || strings.Contains(stderr.String(), omitted) {
		t.Fatal("incomplete boundary retained the omitted runtime model")
	}
	paths, err := filepath.Glob(filepath.Join(home, ".otto", "sessions", "*", "*.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	if len(paths) != 0 {
		for _, path := range paths {
			if persisted, readErr := os.ReadFile(path); readErr == nil && strings.Contains(string(persisted), omitted) {
				t.Fatalf("Pi JSONL retained omitted runtime model: %q", persisted)
			}
		}
		t.Fatalf("incomplete run persisted session paths: %v", paths)
	}
}

func TestRunSandboxOffUsesRealDirectExecutor(t *testing.T) {
	var calls atomic.Int32
	var followUp string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if calls.Add(1) == 1 {
			writeSSE(w, `{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-bash","type":"function","function":{"name":"bash","arguments":"{\"command\":\"printf explicit-off\"}"}}]},"finish_reason":"tool_calls"}]}`)
			return
		}
		body, err := io.ReadAll(r.Body)
		if err != nil {
			t.Errorf("read follow-up request: %v", err)
		}
		followUp = string(body)
		writeSSE(w, `{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`)
	}))
	defer server.Close()

	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)
	var stdout, stderr bytes.Buffer
	code := run(context.Background(), []string{
		"--config", configPath, "--cwd", workspace, "--sandbox", "off", "--approve", "run direct",
	}, strings.NewReader(""), &stdout, &stderr, testEnviron(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "provider-value", "UNRELATED": "preserved",
	}))
	if code != 0 || calls.Load() != 2 {
		t.Fatalf("code/provider calls = %d/%d, stderr = %q", code, calls.Load(), stderr.String())
	}
	if !strings.Contains(followUp, "explicit-off") {
		t.Fatalf("follow-up request = %q, want Direct Bash output", followUp)
	}
	if want := "warning: sandbox is off; bash runs unsandboxed as your macOS user\n"; stderr.String() != want {
		t.Fatalf("stderr = %q, want %q", stderr.String(), want)
	}
}

func TestRunSandboxWarningsAreFixedAcrossFrontends(t *testing.T) {
	for _, test := range []struct {
		name     string
		args     []string
		input    string
		terminal bool
		info     app.SandboxInfo
		warning  string
	}{
		{
			name: "successful Seatbelt REPL is quiet", input: "/exit\n",
			info: app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true},
		},
		{
			name: "unavailable REPL", input: "/exit\n",
			info:    app.SandboxInfo{Mode: app.SandboxUnavailable, BashAvailable: false, Reason: app.SandboxReasonSelfTestFailed},
			warning: "warning: bash is unavailable because the configured sandbox could not be established (reason: self-test-failed); file tools remain available\n",
		},
		{
			name: "off REPL", args: []string{"--sandbox", "off"}, input: "/exit\n",
			info:    app.SandboxInfo{Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true},
			warning: "warning: sandbox is off; bash runs unsandboxed as your macOS user\n",
		},
		{
			name: "off headless", args: []string{"--sandbox", "off", "--approve", "prompt"},
			info:    app.SandboxInfo{Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true},
			warning: "warning: sandbox is off; bash runs unsandboxed as your macOS user\n",
		},
		{
			name: "off TUI", args: []string{"--sandbox", "off", "--ui", "tui"}, terminal: true,
			info:    app.SandboxInfo{Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true},
			warning: "warning: sandbox is off; bash runs unsandboxed as your macOS user\n",
		},
	} {
		t.Run(test.name, func(t *testing.T) {
			home := t.TempDir()
			workspace := t.TempDir()
			configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
			deps := deterministicRunDependencies(t)
			deps.detectTerminal = func(io.Reader, io.Writer) bool { return test.terminal }
			deps.runTUI = func(context.Context, io.Reader, io.Writer, app.Backend) error { return nil }
			deps.openSandbox = func(context.Context, sandboxOpenOptions) sandboxRuntime {
				return fakeSandboxRuntime(test.info, &recordingSandboxExecutor{}, []string{})
			}
			deps.newRunner = func(session.Session) app.Runner {
				return commandRunnerFunc(func(context.Context, string, func(agent.Event)) error { return nil })
			}
			args := append([]string{"--config", configPath, "--cwd", workspace}, test.args...)
			var stdout, stderr bytes.Buffer
			code := runWithDependencies(context.Background(), args, strings.NewReader(test.input), &stdout, &stderr, testEnviron(map[string]string{
				"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "provider-value",
			}), deps)
			if code != 0 {
				t.Fatalf("code = %d, stderr = %q", code, stderr.String())
			}
			if stderr.String() != test.warning {
				t.Fatalf("stderr = %q, want %q", stderr.String(), test.warning)
			}
		})
	}
}

func TestRunUnavailableSandboxRedactsHostSecretsAcrossEveryBoundary(t *testing.T) {
	for _, test := range []struct {
		name          string
		configureOpen func(*sandboxRuntimeFixture)
		shell         func(*testing.T) string
		malformed     bool
		proxy         string
		incomplete    bool
		wantRequests  int32
	}{
		{
			name: "invalid shell", wantRequests: 2,
			shell: func(t *testing.T) string {
				return filepath.Join(t.TempDir(), "missing-shell")
			},
		},
		{
			name: "Seatbelt opener failure", wantRequests: 2,
			configureOpen: func(fixture *sandboxRuntimeFixture) {
				fixture.openErr = &sandbox.UnavailableError{Reason: sandbox.ReasonSeatbeltMissing}
			},
		},
		{name: "malformed entry after valid secrets", malformed: true, incomplete: true, wantRequests: 0},
		{name: "malformed proxy authority", proxy: "http:///raw%20user:raw%2Fpass@example.test/path", incomplete: true, wantRequests: 0},
	} {
		t.Run(test.name, func(t *testing.T) {
			const (
				awsSecret       = "host-aws-secret-value"
				rawUserinfo     = "raw%20user:raw%2Fpass"
				decodedUserinfo = "raw user:raw/pass"
			)
			secretValues := []string{
				awsSecret,
				rawUserinfo,
				"raw%20user",
				"raw%2Fpass",
				decodedUserinfo,
				"raw user",
				"raw/pass",
			}
			home := t.TempDir()
			workspace := t.TempDir()
			if err := os.WriteFile(filepath.Join(workspace, "secrets.txt"), []byte(strings.Join(secretValues, "\n")), 0o600); err != nil {
				t.Fatal(err)
			}

			var requestBodies []string
			var requestMu sync.Mutex
			var requests atomic.Int32
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				body, err := io.ReadAll(r.Body)
				if err != nil {
					t.Errorf("read request: %v", err)
				}
				requestMu.Lock()
				requestBodies = append(requestBodies, string(body))
				requestMu.Unlock()
				if requests.Add(1) == 1 {
					payload := fmt.Sprintf(`{"choices":[{"delta":{"content":%q,"tool_calls":[{"index":0,"id":"call-read","type":"function","function":{"name":"read","arguments":%q}}]},"finish_reason":"tool_calls"}]}`,
						"provider event "+strings.Join(secretValues, " | "), `{"path":"secrets.txt"}`)
					writeSSE(w, payload)
					return
				}
				writeSSE(w, fmt.Sprintf(`{"choices":[{"delta":{"content":%q},"finish_reason":"stop"}]}`, "final "+awsSecret))
			}))
			defer server.Close()

			configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)
			fixture := newSandboxRuntimeFixture(t)
			if test.configureOpen != nil {
				test.configureOpen(fixture)
			}
			deps := deterministicRunDependencies(t)
			deps.openSandbox = func(ctx context.Context, options sandboxOpenOptions) sandboxRuntime {
				return openSandboxRuntimeWithDependencies(ctx, options, fixture.dependencies())
			}
			shell := "/bin/sh"
			if test.shell != nil {
				shell = test.shell(t)
			}
			proxy := test.proxy
			if proxy == "" {
				proxy = "http://" + rawUserinfo + "@[::1]:8443/path"
			}
			entries := []string{
				"AWS_SECRET_ACCESS_KEY=" + awsSecret,
				"HTTPS_PROXY=" + proxy,
			}
			if test.malformed {
				entries = append(entries, "BROKEN")
			}
			entries = append(entries, "HOME="+home, "SHELL="+shell, "TEST_KEY=provider-value")

			var stdout, stderr bytes.Buffer
			code := runWithDependencies(context.Background(), []string{
				"--config", configPath, "--cwd", workspace, "--approve", "read the file",
			}, strings.NewReader(""), &stdout, &stderr, func() []string { return entries }, deps)
			if code != 0 || requests.Load() != test.wantRequests {
				t.Fatalf("code/provider requests = %d/%d, want %d requests; stderr = %q", code, requests.Load(), test.wantRequests, stderr.String())
			}
			var persisted []byte
			if test.incomplete {
				paths, globErr := filepath.Glob(filepath.Join(home, ".otto", "sessions", "*", "*.jsonl"))
				if globErr != nil {
					t.Fatal(globErr)
				}
				if len(paths) != 0 {
					t.Fatalf("incomplete malformed environment persisted sessions: %v", paths)
				}
			} else {
				var readErr error
				persisted, readErr = os.ReadFile(onlySessionPath(t, home))
				if readErr != nil {
					t.Fatal(readErr)
				}
			}
			requestMu.Lock()
			bodies := append([]string(nil), requestBodies...)
			requestMu.Unlock()
			if len(bodies) != int(test.wantRequests) {
				t.Fatalf("provider request bodies = %d, want %d", len(bodies), test.wantRequests)
			}
			boundaries := map[string]string{
				"provider events and local output": stdout.String(),
				"stderr":                           stderr.String(),
			}
			if len(persisted) != 0 {
				boundaries["Pi JSONL"] = string(persisted)
			}
			if len(bodies) > 1 {
				boundaries["provider follow-up"] = bodies[1]
			}
			for boundary, content := range boundaries {
				for _, secret := range secretValues {
					if strings.Contains(content, secret) {
						t.Fatalf("%s leaked a host secret: %q", boundary, content)
					}
				}
			}
			if fixture.executorCalls.Load() != 0 || fixture.directCalls.Load() != 0 {
				t.Fatalf("unavailable path constructed execution: executor/direct = %d/%d", fixture.executorCalls.Load(), fixture.directCalls.Load())
			}
		})
	}
}

func TestRunInvalidShellContinuesWithExactlySixFileTools(t *testing.T) {
	var names []string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var request struct {
			Tools []struct {
				Function struct {
					Name string `json:"name"`
				} `json:"function"`
			} `json:"tools"`
		}
		if err := json.NewDecoder(r.Body).Decode(&request); err != nil {
			t.Errorf("decode request: %v", err)
		}
		for _, item := range request.Tools {
			names = append(names, item.Function.Name)
		}
		writeSSE(w, `{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`)
	}))
	defer server.Close()

	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)
	deps := deterministicRunDependencies(t)
	fixture := newSandboxRuntimeFixture(t)
	deps.openSandbox = func(ctx context.Context, options sandboxOpenOptions) sandboxRuntime {
		return openSandboxRuntimeWithDependencies(ctx, options, fixture.dependencies())
	}
	missingShell := filepath.Join(t.TempDir(), "missing-shell")
	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace, "--approve", "inspect"}, strings.NewReader(""), &stdout, &stderr, testEnviron(map[string]string{
		"HOME": home, "SHELL": missingShell, "TEST_KEY": "provider-value",
	}), deps)
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if want := []string{"read", "grep", "find", "ls", "write", "edit", "memory_search", "remember", "forget", "agent", "agent_wait", "agent_status"}; !reflect.DeepEqual(names, want) {
		t.Fatalf("tool names = %#v, want %#v", names, want)
	}
	wantWarning := "warning: bash is unavailable because the configured sandbox could not be established (reason: invalid-shell); file tools remain available\n"
	if stderr.String() != wantWarning {
		t.Fatalf("stderr = %q, want %q", stderr.String(), wantWarning)
	}
}

func TestRunUnavailableSandboxRejectsUnsolicitedBashWithoutExecution(t *testing.T) {
	var requests atomic.Int32
	var followUp string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(r.Body)
		if err != nil {
			t.Errorf("read request: %v", err)
		}
		if requests.Add(1) == 1 {
			writeSSE(w, `{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-bash","type":"function","function":{"name":"bash","arguments":"{\"command\":\"touch must-not-exist\"}"}}]},"finish_reason":"tool_calls"}]}`)
			return
		}
		followUp = string(body)
		writeSSE(w, `{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`)
	}))
	defer server.Close()

	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)
	executor := &recordingSandboxExecutor{}
	deps := deterministicRunDependencies(t)
	deps.openSandbox = func(context.Context, sandboxOpenOptions) sandboxRuntime {
		return fakeSandboxRuntime(app.SandboxInfo{Mode: app.SandboxUnavailable, BashAvailable: false, Reason: app.SandboxReasonSelfTestFailed}, nil, nil)
	}
	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace, "--approve", "try bash"}, strings.NewReader(""), &stdout, &stderr, testEnviron(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "provider-value",
	}), deps)
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if executor.calls.Load() != 0 {
		t.Fatalf("executor calls = %d, want 0", executor.calls.Load())
	}
	if !strings.Contains(followUp, "unknown tool: bash") || strings.Contains(followUp, "sandbox execution") {
		t.Fatalf("follow-up request = %q, want bounded unknown-tool result", followUp)
	}
	if _, err := os.Stat(filepath.Join(workspace, "must-not-exist")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("unsolicited bash started host child: %v", err)
	}
}

func TestRunStartupInterruptionOwnsPartialSandboxCleanup(t *testing.T) {
	for _, test := range []struct {
		name        string
		preCanceled bool
	}{
		{name: "injected SIGINT"},
		{name: "already canceled context", preCanceled: true},
	} {
		t.Run(test.name, func(t *testing.T) {
			home := t.TempDir()
			workspace := t.TempDir()
			configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
			interrupts := make(chan os.Signal, 1)
			subscribed := make(chan struct{})
			openStarted := make(chan struct{})
			abortOpen := make(chan struct{})
			var stopCalls, sandboxCloses, sessionCalls atomic.Int32

			deps := deterministicRunDependencies(t)
			deps.subscribeInterrupts = func() interruptSubscription {
				close(subscribed)
				return interruptSubscription{signals: interrupts, stop: func() { stopCalls.Add(1) }}
			}
			deps.openSandbox = func(ctx context.Context, _ sandboxOpenOptions) sandboxRuntime {
				close(openStarted)
				select {
				case <-ctx.Done():
				case <-abortOpen:
				}
				return sandboxRuntime{
					Executor:           &recordingSandboxExecutor{},
					Environment:        []string{},
					Info:               app.SandboxInfo{Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true},
					RedactionsComplete: true,
					close: newSandboxRuntimeCloser(func() error {
						sandboxCloses.Add(1)
						return nil
					}),
				}
			}
			deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) {
				sessionCalls.Add(1)
				return session.NewMemory(session.Header{
					Version: session.CurrentVersion, ID: "must-not-construct", Workspace: workspace,
					Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Now().UTC(),
				}), nil
			}
			deps.newRunner = func(session.Session) app.Runner {
				return commandRunnerFunc(func(context.Context, string, func(agent.Event)) error { return nil })
			}

			parent, cancelParent := context.WithCancel(context.Background())
			if test.preCanceled {
				cancelParent()
			}
			defer cancelParent()
			var stdout, stderr bytes.Buffer
			codeDone := make(chan int, 1)
			go func() {
				codeDone <- runWithDependencies(parent, []string{
					"--config", configPath, "--cwd", workspace, "--approve", "must not run",
				}, strings.NewReader(""), &stdout, &stderr, testEnviron(map[string]string{
					"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "provider-value",
				}), deps)
			}()

			awaitSandboxRuntimeEvent(t, openStarted, "Sandbox open barrier")
			if !test.preCanceled {
				select {
				case <-subscribed:
				case <-time.After(2 * time.Second):
					close(abortOpen)
					<-codeDone
					t.Fatal("interrupt subscription was not established before Sandbox open")
				}
				interrupts <- os.Interrupt
			}
			select {
			case code := <-codeDone:
				if code != 130 {
					t.Fatalf("code = %d, stderr = %q, want 130", code, stderr.String())
				}
			case <-time.After(2 * time.Second):
				close(abortOpen)
				t.Fatal("startup cancellation did not release Sandbox open")
			}
			if sandboxCloses.Load() != 1 || sessionCalls.Load() != 0 || stopCalls.Load() != 1 {
				t.Fatalf("closes/session/stop = %d/%d/%d, want 1/0/1", sandboxCloses.Load(), sessionCalls.Load(), stopCalls.Load())
			}
			if stdout.Len() != 0 || stderr.Len() != 0 {
				t.Fatalf("startup cancellation rendered misleading output: stdout=%q stderr=%q", stdout.String(), stderr.String())
			}
		})
	}
}

func TestRunClosesControllerBeforeSandboxAfterActiveBash(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		writeSSE(w, `{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-bash","type":"function","function":{"name":"bash","arguments":"{\"command\":\"touch host-child-must-not-start\"}"}}]},"finish_reason":"tool_calls"}]}`)
	}))
	defer server.Close()

	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)
	var mu sync.Mutex
	var order []string
	appendOrder := func(value string) {
		mu.Lock()
		order = append(order, value)
		mu.Unlock()
	}
	started := make(chan struct{})
	executor := &recordingSandboxExecutor{execute: func(ctx context.Context, _ sandbox.Request, _ sandbox.Streams) (sandbox.ExitStatus, error) {
		close(started)
		<-ctx.Done()
		appendOrder("bash")
		return sandbox.ExitStatus{Code: -1, Signaled: true, Signal: "killed"}, context.Canceled
	}}
	deps := deterministicRunDependencies(t)
	store := &orderingSession{Session: session.NewMemory(session.Header{
		Version: session.CurrentVersion, ID: "active-bash", Workspace: workspace,
		Provider: "openai-compatible", Profile: "test", Model: "test-model", CreatedAt: time.Now().UTC(),
	}), onClose: func() { appendOrder("controller") }}
	deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) { return store, nil }
	deps.detectTerminal = func(io.Reader, io.Writer) bool { return true }
	deps.runTUI = func(ctx context.Context, _ io.Reader, _ io.Writer, backend app.Backend) error {
		go func() { _ = backend.Prompt(ctx, "run bash", nil) }()
		select {
		case <-started:
			return errors.New("frontend stopped")
		case <-ctx.Done():
			return ctx.Err()
		}
	}
	deps.openSandbox = func(context.Context, sandboxOpenOptions) sandboxRuntime {
		return sandboxRuntime{
			Executor: executor, Environment: []string{"PATH=/usr/bin:/bin"},
			Info:               app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true},
			RedactionsComplete: true,
			close:              newSandboxRuntimeCloser(func() error { appendOrder("sandbox"); return nil }),
		}
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace, "--ui", "tui"}, strings.NewReader(""), &stdout, &stderr, testEnviron(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "provider-value",
	}), deps)
	if code != 1 || !strings.Contains(stderr.String(), "frontend stopped") {
		t.Fatalf("code = %d stderr = %q", code, stderr.String())
	}
	mu.Lock()
	got := append([]string(nil), order...)
	mu.Unlock()
	if !reflect.DeepEqual(got, []string{"bash", "controller", "sandbox"}) {
		t.Fatalf("close order = %#v, want bash/controller/sandbox", got)
	}
	if _, err := os.Stat(filepath.Join(workspace, "host-child-must-not-start")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("fake sandbox path started a host child: %v", err)
	}
}

func TestRunClosesControllerBeforeSandboxAndClosesSandboxOnStartupFailure(t *testing.T) {
	t.Run("normal active controller ordering", func(t *testing.T) {
		home := t.TempDir()
		workspace := t.TempDir()
		configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
		var mu sync.Mutex
		var order []string
		appendOrder := func(value string) {
			mu.Lock()
			order = append(order, value)
			mu.Unlock()
		}
		started := make(chan struct{})
		deps := deterministicRunDependencies(t)
		store := &orderingSession{Session: session.NewMemory(session.Header{Version: session.CurrentVersion, ID: "ordering", Workspace: workspace, Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Now().UTC()}), onClose: func() { appendOrder("controller") }}
		deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) { return store, nil }
		deps.newRunner = func(session.Session) app.Runner {
			return commandRunnerFunc(func(ctx context.Context, _ string, _ func(agent.Event)) error {
				close(started)
				<-ctx.Done()
				appendOrder("runner")
				return ctx.Err()
			})
		}
		deps.detectTerminal = func(io.Reader, io.Writer) bool { return true }
		deps.runTUI = func(ctx context.Context, _ io.Reader, _ io.Writer, backend app.Backend) error {
			go func() { _ = backend.Prompt(ctx, "active", nil) }()
			<-started
			return errors.New("frontend stopped")
		}
		deps.openSandbox = func(context.Context, sandboxOpenOptions) sandboxRuntime {
			return sandboxRuntime{
				Executor: &recordingSandboxExecutor{}, Environment: []string{},
				Info:               app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true},
				RedactionsComplete: true,
				close:              newSandboxRuntimeCloser(func() error { appendOrder("sandbox"); return nil }),
			}
		}

		var stdout, stderr bytes.Buffer
		code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace, "--ui", "tui"}, strings.NewReader(""), &stdout, &stderr, testEnviron(map[string]string{
			"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "provider-value",
		}), deps)
		if code != 1 || !strings.Contains(stderr.String(), "frontend stopped") {
			t.Fatalf("code = %d stderr = %q", code, stderr.String())
		}
		mu.Lock()
		got := append([]string(nil), order...)
		mu.Unlock()
		if !reflect.DeepEqual(got, []string{"runner", "controller", "sandbox"}) {
			t.Fatalf("close order = %#v, want runner/controller/sandbox", got)
		}
	})

	t.Run("startup failure", func(t *testing.T) {
		home := t.TempDir()
		workspace := t.TempDir()
		configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
		var sandboxCloses atomic.Int32
		deps := deterministicRunDependencies(t)
		deps.openSandbox = func(context.Context, sandboxOpenOptions) sandboxRuntime {
			return sandboxRuntime{
				Executor: &recordingSandboxExecutor{}, Environment: []string{},
				Info:               app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true},
				RedactionsComplete: true,
				close:              newSandboxRuntimeCloser(func() error { sandboxCloses.Add(1); return nil }),
			}
		}
		deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) {
			return nil, errors.New("startup session failure")
		}
		var stdout, stderr bytes.Buffer
		code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader("/exit\n"), &stdout, &stderr, testEnviron(map[string]string{
			"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "provider-value",
		}), deps)
		if code != 1 || sandboxCloses.Load() != 1 {
			t.Fatalf("code = %d closes = %d stderr = %q", code, sandboxCloses.Load(), stderr.String())
		}
	})
}

func TestRunSandboxCloseErrorIsFixedAndControllerErrorStaysPrimary(t *testing.T) {
	for _, test := range []struct {
		name       string
		sessionErr error
		want       string
	}{
		{name: "sandbox close error", want: "otto: close sandbox: sandbox runtime cleanup failed\n"},
		{name: "controller error primary", sessionErr: errors.New("controller close failed"), want: "otto: close session: controller close failed\n"},
	} {
		t.Run(test.name, func(t *testing.T) {
			home := t.TempDir()
			workspace := t.TempDir()
			configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
			deps := deterministicRunDependencies(t)
			var sandboxCloses atomic.Int32
			deps.openSandbox = func(context.Context, sandboxOpenOptions) sandboxRuntime {
				return sandboxRuntime{
					Executor: &recordingSandboxExecutor{}, Environment: []string{},
					Info:               app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true},
					RedactionsComplete: true,
					close: newSandboxRuntimeCloser(func() error {
						sandboxCloses.Add(1)
						return errors.New("raw sandbox state and profile")
					}),
				}
			}
			store := &trackingSession{Session: session.NewMemory(session.Header{Version: session.CurrentVersion, ID: "close", Workspace: workspace, Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Now().UTC()}), closeErr: test.sessionErr}
			deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) { return store, nil }
			var stdout, stderr bytes.Buffer
			code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader("/exit\n"), &stdout, &stderr, testEnviron(map[string]string{
				"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "provider-value",
			}), deps)
			if code != 1 || stderr.String() != test.want || sandboxCloses.Load() != 1 {
				t.Fatalf("code = %d stderr = %q closes = %d, want %q and one close", code, stderr.String(), sandboxCloses.Load(), test.want)
			}
			if strings.Contains(stderr.String(), "raw sandbox") {
				t.Fatalf("stderr leaked raw sandbox close state: %q", stderr.String())
			}
		})
	}
}

func TestRunDoesNotUseOTTOSandboxAsConfiguration(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	deps := deterministicRunDependencies(t)
	var driver sandbox.DriverMode
	deps.openSandbox = func(_ context.Context, options sandboxOpenOptions) sandboxRuntime {
		driver = options.Settings.Driver
		return fakeSandboxRuntime(app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true}, &recordingSandboxExecutor{}, []string{})
	}
	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader("/exit\n"), &stdout, &stderr, testEnviron(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "provider-value", "OTTO_SANDBOX": "off",
	}), deps)
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if driver != sandbox.DriverAuto {
		t.Fatalf("Sandbox Driver = %q, want auto despite OTTO_SANDBOX", driver)
	}
}

func TestRunSandboxProviderNamesIncludeSelectedAndEveryProfile(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := filepath.Join(t.TempDir(), "config.toml")
	if err := os.WriteFile(configPath, []byte(`default_profile = "active"
[profiles.active]
provider = "openai-compatible"
base_url = "http://127.0.0.1:1"
model = "active-model"
api_key_env = "ACTIVE_KEY"
[profiles.inactive]
provider = "openai-compatible"
base_url = "http://127.0.0.1:2"
model = "inactive-model"
api_key_env = "INACTIVE_KEY"
`), 0o600); err != nil {
		t.Fatal(err)
	}
	deps := deterministicRunDependencies(t)
	var names []string
	deps.openSandbox = func(_ context.Context, options sandboxOpenOptions) sandboxRuntime {
		names = append([]string(nil), options.ProviderNames...)
		return fakeSandboxRuntime(app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true}, &recordingSandboxExecutor{}, []string{})
	}
	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader("/exit\n"), &stdout, &stderr, testEnviron(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "ACTIVE_KEY": "active-value", "INACTIVE_KEY": "inactive-value", "OTTO_API_KEY": "fallback-value",
	}), deps)
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	sort.Strings(names)
	if want := []string{"ACTIVE_KEY", "INACTIVE_KEY", "OTTO_API_KEY"}; !reflect.DeepEqual(names, want) {
		t.Fatalf("ProviderNames = %#v, want %#v", names, want)
	}
}

func runForTest(t *testing.T, ctx context.Context, args []string, stdin io.Reader, stdout, stderr io.Writer, enumerate environmentEnumerator) int {
	t.Helper()
	return runWithDependencies(ctx, args, stdin, stdout, stderr, enumerate, deterministicRunDependencies(t))
}

func deterministicRunDependencies(t *testing.T) runDependencies {
	t.Helper()
	deps := defaultRunDependencies()
	deps.subscribeInterrupts = func() interruptSubscription { return interruptSubscription{stop: func() {}} }
	deps.openSandbox = func(context.Context, sandboxOpenOptions) sandboxRuntime {
		return fakeSandboxRuntime(
			app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true},
			&recordingSandboxExecutor{}, []string{},
		)
	}
	deps.resolveUserHome = func() (string, error) { return t.TempDir(), nil }
	return deps
}

func fakeSandboxRuntime(info app.SandboxInfo, executor sandbox.CommandExecutor, environment []string) sandboxRuntime {
	if environment == nil {
		environment = []string{}
	}
	return sandboxRuntime{
		Executor: executor, Environment: append([]string{}, environment...), Info: info,
		RedactionsComplete: true,
		close:              newSandboxRuntimeCloser(nil),
	}
}

type recordingSandboxExecutor struct {
	calls    atomic.Int32
	mu       sync.Mutex
	requests []sandbox.Request
	execute  func(context.Context, sandbox.Request, sandbox.Streams) (sandbox.ExitStatus, error)
}

func (e *recordingSandboxExecutor) Execute(ctx context.Context, request sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
	// These fixtures model shell tool calls in non-repository workspaces.
	// Startup Git queries have their own recording executor coverage.
	if len(request.Argv) > 0 && request.Argv[0] == "git" {
		return sandbox.ExitStatus{Code: 128}, nil
	}
	e.calls.Add(1)
	e.mu.Lock()
	e.requests = append(e.requests, request.Clone())
	execute := e.execute
	e.mu.Unlock()
	if execute != nil {
		return execute(ctx, request, streams)
	}
	return sandbox.ExitStatus{Code: 0}, nil
}

type orderingSession struct {
	session.Session
	onClose func()
}

func (s *orderingSession) UpdateRuntime(ctx context.Context, metadata session.RuntimeMetadata) error {
	updater, ok := s.Session.(session.RuntimeUpdater)
	if !ok {
		return errors.New("session does not support runtime provenance updates")
	}
	return updater.UpdateRuntime(ctx, metadata)
}

func (s *orderingSession) Close() error {
	if s.onClose != nil {
		s.onClose()
	}
	return s.Session.Close()
}

func writeSandboxCLIConfig(t *testing.T, driver, network, baseURL string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "config.toml")
	content := fmt.Sprintf(`default_profile = "test"
[sandbox]
driver = %q
network = %q

[profiles.test]
provider = "openai-compatible"
base_url = %q
model = "test-model"
api_key_env = "TEST_KEY"
`, driver, network, baseURL)
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}
