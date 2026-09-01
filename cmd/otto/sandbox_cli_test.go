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
		return fakeSandboxRuntime(app.SandboxInfo{Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true}, nil, nil)
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
	if want := []string{"read", "grep", "find", "ls", "write", "edit"}; !reflect.DeepEqual(names, want) {
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
			Info:  app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true},
			close: newSandboxRuntimeCloser(func() error { appendOrder("sandbox"); return nil }),
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
				Info:  app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true},
				close: newSandboxRuntimeCloser(func() error { appendOrder("sandbox"); return nil }),
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
				Info:  app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true},
				close: newSandboxRuntimeCloser(func() error { sandboxCloses.Add(1); return nil }),
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
					Info: app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true},
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
		close: newSandboxRuntimeCloser(nil),
	}
}

type recordingSandboxExecutor struct {
	calls    atomic.Int32
	mu       sync.Mutex
	requests []sandbox.Request
	execute  func(context.Context, sandbox.Request, sandbox.Streams) (sandbox.ExitStatus, error)
}

func (e *recordingSandboxExecutor) Execute(ctx context.Context, request sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
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
