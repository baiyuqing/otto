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
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

func TestRunHelpDoesNotRequireCredentials(t *testing.T) {
	var stdout, stderr bytes.Buffer
	code := run(context.Background(), []string{"--help"}, strings.NewReader(""), &stdout, &stderr, func(string) string { return "" })
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	for _, text := range []string{"--config", "--cwd", "--ui", "--continue", "--resume", "--no-session", "WARNING", "unsandboxed", "anything accessible to your macOS user"} {
		if !strings.Contains(stdout.String(), text) {
			t.Fatalf("help missing %s: %s", text, stdout.String())
		}
	}
	if stderr.Len() != 0 {
		t.Fatalf("help wrote to stderr: %q", stderr.String())
	}
}

func TestRunSelectsFrontendFromResolvedUIMode(t *testing.T) {
	for _, test := range []struct {
		name       string
		configUI   string
		envUI      string
		cliUI      string
		isTerminal bool
		wantTUI    bool
	}{
		{name: "auto uses repl for redirected IO", isTerminal: false, wantTUI: false},
		{name: "auto uses tui for terminal IO", isTerminal: true, wantTUI: true},
		{name: "config selects tui", configUI: "tui", isTerminal: true, wantTUI: true},
		{name: "env overrides config", configUI: "repl", envUI: "tui", isTerminal: true, wantTUI: true},
		{name: "explicit auto overrides env", envUI: "tui", cliUI: "auto", isTerminal: false, wantTUI: false},
		{name: "cli overrides env and config", configUI: "tui", envUI: "tui", cliUI: "repl", isTerminal: true, wantTUI: false},
	} {
		t.Run(test.name, func(t *testing.T) {
			home := t.TempDir()
			workspace := t.TempDir()
			configPath := writeCLIConfigWithUI(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1", test.configUI)

			deps := defaultRunDependencies()
			deps.subscribeInterrupts = func() interruptSubscription {
				return interruptSubscription{stop: func() {}}
			}
			deps.detectTerminal = func(io.Reader, io.Writer) bool {
				return test.isTerminal
			}

			var tuiCalls atomic.Int32
			var sessionCalls atomic.Int32
			var stores []*trackingSession
			deps.newSession = func(_ bool, _ string, workspace string, runtime config.Runtime) (session.Session, error) {
				sessionCalls.Add(1)
				store := &trackingSession{Session: session.NewMemory(session.Header{
					Version: 1, ID: fmt.Sprintf("session-%d", sessionCalls.Load()), Workspace: workspace,
					Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model, CreatedAt: time.Now().UTC(),
				})}
				stores = append(stores, store)
				return store, nil
			}
			deps.runTUI = func(_ context.Context, _ io.Reader, _ io.Writer, backend app.Backend) error {
				tuiCalls.Add(1)
				if info := backend.Info(); info.SessionID == "" {
					t.Fatal("tui backend session info is empty")
				}
				return nil
			}

			args := []string{"--config", configPath, "--cwd", workspace}
			if test.cliUI != "" {
				args = append(args, "--ui", test.cliUI)
			}
			env := map[string]string{"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret"}
			if test.envUI != "" {
				env["OTTO_UI"] = test.envUI
			}

			var stdout, stderr bytes.Buffer
			code := runWithDependencies(context.Background(), args, strings.NewReader("/exit\n"), &stdout, &stderr, testGetenv(env), deps)
			if code != 0 {
				t.Fatalf("code = %d, stdout = %q, stderr = %q", code, stdout.String(), stderr.String())
			}
			if got := tuiCalls.Load(); test.wantTUI && got != 1 {
				t.Fatalf("tui calls = %d, want 1", got)
			} else if !test.wantTUI && got != 0 {
				t.Fatalf("tui calls = %d, want 0", got)
			}
			if !test.wantTUI && !strings.Contains(stdout.String(), "Session: ") {
				t.Fatalf("stdout = %q, want repl session banner", stdout.String())
			}
			if sessionCalls.Load() != 1 || len(stores) != 1 {
				t.Fatalf("session calls = %d stores = %d, want 1", sessionCalls.Load(), len(stores))
			}
			if stores[0].closeCalls.Load() != 1 {
				t.Fatalf("store close calls = %d, want 1", stores[0].closeCalls.Load())
			}
		})
	}
}

func TestRunTUIProgramErrorReturnsOne(t *testing.T) {
	programErr := errors.New("injected program failure")
	for _, test := range []struct {
		name     string
		closeErr error
		want     string
	}{
		{name: "program error", want: "otto: TUI: injected program failure\n"},
		{name: "close error takes precedence", closeErr: errors.New("injected close failure"), want: "otto: close session: injected close failure\n"},
	} {
		t.Run(test.name, func(t *testing.T) {
			home := t.TempDir()
			workspace := t.TempDir()
			configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")

			deps := defaultRunDependencies()
			deps.subscribeInterrupts = func() interruptSubscription {
				return interruptSubscription{stop: func() {}}
			}
			deps.detectTerminal = func(io.Reader, io.Writer) bool { return true }
			store := &trackingSession{Session: session.NewMemory(session.Header{
				Version: 1, ID: "tui-error", Workspace: workspace, Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Now().UTC(),
			}), closeErr: test.closeErr}
			deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) {
				return store, nil
			}
			deps.runTUI = func(context.Context, io.Reader, io.Writer, app.Backend) error {
				return programErr
			}

			var stdout, stderr bytes.Buffer
			code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{
				"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
			}), deps)
			if code != 1 {
				t.Fatalf("code = %d, stderr = %q, want 1", code, stderr.String())
			}
			if got := stderr.String(); got != test.want {
				t.Fatalf("stderr = %q, want %q", got, test.want)
			}
			if got := store.closeCalls.Load(); got != 1 {
				t.Fatalf("store close calls = %d, want 1", got)
			}
		})
	}
}

func TestRunTUIFatalPersistenceReturnsOneAndPrintsDiagnostic(t *testing.T) {
	fatalErr := errors.Join(session.ErrFatalPersistence, errors.New("injected disk failure"), context.Canceled)
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")

	deps := defaultRunDependencies()
	deps.subscribeInterrupts = func() interruptSubscription {
		return interruptSubscription{stop: func() {}}
	}
	deps.detectTerminal = func(io.Reader, io.Writer) bool { return true }
	store := &trackingSession{Session: session.NewMemory(session.Header{
		Version: 1, ID: "fatal-tui", Workspace: workspace, Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Now().UTC(),
	})}
	deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) {
		return store, nil
	}
	terminalRestored := false
	deps.runTUI = func(context.Context, io.Reader, io.Writer, app.Backend) error {
		terminalRestored = true
		return fatalErr
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}), deps)
	if code != 1 {
		t.Fatalf("code = %d, stderr = %q, want 1", code, stderr.String())
	}
	if !terminalRestored {
		t.Fatal("TUI returned before its terminal-restoration point")
	}
	if got, want := stderr.String(), "otto: TUI: fatal session persistence failure\ninjected disk failure\ncontext canceled\n"; got != want {
		t.Fatalf("stderr = %q, want %q", got, want)
	}
	if got := store.closeCalls.Load(); got != 1 {
		t.Fatalf("store close calls = %d, want 1", got)
	}
}

func TestRunTUIProgramErrorCancelsActivePromptBeforeClosing(t *testing.T) {
	programErr := errors.New("injected program failure")
	runnerStarted := make(chan struct{})
	runnerCanceled := make(chan struct{})

	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	store := &trackingSession{Session: session.NewMemory(session.Header{
		Version: 1, ID: "active-tui", Workspace: workspace, Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Now().UTC(),
	})}
	deps := defaultRunDependencies()
	deps.subscribeInterrupts = func() interruptSubscription {
		return interruptSubscription{stop: func() {}}
	}
	deps.detectTerminal = func(io.Reader, io.Writer) bool { return true }
	deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) {
		return store, nil
	}
	deps.newRunner = func(session.Session) app.Runner {
		return commandRunnerFunc(func(ctx context.Context, _ string, _ func(agent.Event)) error {
			close(runnerStarted)
			<-ctx.Done()
			close(runnerCanceled)
			return ctx.Err()
		})
	}
	promptDone := make(chan error, 1)
	deps.runTUI = func(ctx context.Context, _ io.Reader, _ io.Writer, backend app.Backend) error {
		go func() {
			promptDone <- backend.Prompt(ctx, "wait for cancellation", nil)
		}()
		select {
		case <-runnerStarted:
			return programErr
		case <-ctx.Done():
			return ctx.Err()
		}
	}

	processCtx, cancelProcess := context.WithCancel(context.Background())
	defer cancelProcess()
	var stdout, stderr bytes.Buffer
	codeDone := make(chan int, 1)
	go func() {
		codeDone <- runWithDependencies(processCtx, []string{"--config", configPath, "--cwd", workspace}, strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{
			"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
		}), deps)
	}()

	select {
	case code := <-codeDone:
		if code != 1 {
			t.Fatalf("code = %d, stderr = %q, want 1", code, stderr.String())
		}
	case <-time.After(2 * time.Second):
		cancelProcess()
		select {
		case <-codeDone:
		case <-time.After(time.Second):
		}
		t.Fatal("run did not cancel the active prompt before closing the controller")
	}
	if got, want := stderr.String(), "otto: TUI: injected program failure\n"; got != want {
		t.Fatalf("stderr = %q, want %q", got, want)
	}
	select {
	case <-runnerCanceled:
	case <-time.After(time.Second):
		t.Fatal("active runner context was not canceled")
	}
	select {
	case err := <-promptDone:
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("Prompt() error = %v, want context cancellation", err)
		}
	case <-time.After(time.Second):
		t.Fatal("active prompt did not terminate")
	}
	if got := store.closeCalls.Load(); got != 1 {
		t.Fatalf("store close calls = %d, want 1", got)
	}
}

func TestRunForcedTUINeedsTerminalBeforeOpeningSession(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")

	deps := defaultRunDependencies()
	deps.subscribeInterrupts = func() interruptSubscription {
		return interruptSubscription{stop: func() {}}
	}
	deps.detectTerminal = func(io.Reader, io.Writer) bool { return false }
	var newSessionCalls atomic.Int32
	var prepareSessionCalls atomic.Int32
	var tuiCalls atomic.Int32
	deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) {
		newSessionCalls.Add(1)
		return nil, errors.New("new session should not be called")
	}
	deps.prepareSession = func(context.Context, string, string) (preparedSession, error) {
		prepareSessionCalls.Add(1)
		return nil, errors.New("prepare session should not be called")
	}
	deps.runTUI = func(context.Context, io.Reader, io.Writer, app.Backend) error {
		tuiCalls.Add(1)
		return nil
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace, "--ui", "tui"}, strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}), deps)
	if code != 1 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if got, want := stderr.String(), "otto: --ui tui requires terminal stdin and stdout; use --ui repl for redirected input\n"; got != want {
		t.Fatalf("stderr = %q, want %q", got, want)
	}
	if newSessionCalls.Load() != 0 || prepareSessionCalls.Load() != 0 || tuiCalls.Load() != 0 {
		t.Fatalf("new session calls = %d prepare session calls = %d tui calls = %d, want all zero", newSessionCalls.Load(), prepareSessionCalls.Load(), tuiCalls.Load())
	}
}

func TestRunReportsResolutionErrors(t *testing.T) {
	workspace := t.TempDir()
	home := t.TempDir()
	valid := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	unsupported := writeCLIConfig(t, "other-provider", "TEST_KEY", "http://127.0.0.1:1")
	missingKey := writeCLIConfig(t, "openai-compatible", "MISSING_KEY", "http://127.0.0.1:1")
	env := testGetenv(map[string]string{"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret"})

	tests := []struct {
		name string
		args []string
		want string
	}{
		{name: "missing API key", args: []string{"--config", missingKey, "--cwd", workspace}, want: "missing api key"},
		{name: "invalid profile", args: []string{"--config", valid, "--profile", "missing", "--cwd", workspace}, want: `profile "missing" not found`},
		{name: "unsupported provider", args: []string{"--config", unsupported, "--cwd", workspace}, want: "unsupported provider"},
		{name: "missing resume", args: []string{"--config", valid, "--cwd", workspace, "--resume", filepath.Join(t.TempDir(), "missing.jsonl")}, want: "open session file"},
		{name: "conflicting continue and resume", args: []string{"--continue", "--resume", "anything"}, want: "cannot be used together"},
		{name: "invalid max turns", args: []string{"--max-turns", "0"}, want: "max-turns must be greater than zero"},
		{name: "invalid shell timeout", args: []string{"--shell-timeout", "never"}, want: "invalid value"},
		{name: "invalid ui mode", args: []string{"--ui", "popup"}, want: "must be one of auto, tui, repl"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			var stdout, stderr bytes.Buffer
			code := run(context.Background(), test.args, strings.NewReader("/exit\n"), &stdout, &stderr, env)
			if code == 0 || !strings.Contains(stderr.String(), test.want) {
				t.Fatalf("code = %d, stderr = %q, want %q", code, stderr.String(), test.want)
			}
		})
	}
}

func TestRunRejectsInvalidBaseURLBeforeOpeningSession(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "https://example.test/v1?tenant=x")
	resumePath := createCLISession(t, filepath.Join(home, ".otto", "sessions"), workspace, "resume")
	deps := defaultRunDependencies()
	var prepareCalls, activateCalls atomic.Int32
	deps.prepareSession = func(ctx context.Context, path, workspace string) (preparedSession, error) {
		prepareCalls.Add(1)
		prepared, err := prepareSession(ctx, path, workspace)
		if err != nil {
			return nil, err
		}
		return &fakePreparedSession{
			info: prepared.Info(),
			activate: func(context.Context) (session.Session, []session.Warning, error) {
				activateCalls.Add(1)
				return nil, nil, errors.New("session must not activate")
			},
			close: prepared.Close,
		}, nil
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace, "--resume", resumePath}, strings.NewReader("/exit\n"), &stdout, &stderr, testGetenv(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}), deps)
	if code == 0 || !strings.Contains(stderr.String(), "base_url") {
		t.Fatalf("code = %d, stderr = %q, want base_url error", code, stderr.String())
	}
	if prepareCalls.Load() != 1 || activateCalls.Load() != 0 {
		t.Fatalf("prepare calls = %d activation calls = %d, want 1 and 0", prepareCalls.Load(), activateCalls.Load())
	}
}

func TestRunNoSessionAndExplicitConfig(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	var stdout, stderr bytes.Buffer
	code := run(context.Background(), []string{"--config", configPath, "--cwd", workspace, "--no-session"}, strings.NewReader("/session\n/exit\n"), &stdout, &stderr, testGetenv(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}))
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if !strings.Contains(stdout.String(), "Path: \n") {
		t.Fatalf("memory session path should be empty: %q", stdout.String())
	}
	if _, err := os.Stat(filepath.Join(home, ".otto", "sessions")); !os.IsNotExist(err) {
		t.Fatalf("--no-session created persistent root: %v", err)
	}
}

func TestRunNoSessionDoesNotWireSessionBrowser(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	deps := defaultRunDependencies()
	deps.subscribeInterrupts = func() interruptSubscription {
		return interruptSubscription{stop: func() {}}
	}
	deps.detectTerminal = func(io.Reader, io.Writer) bool { return true }
	deps.runTUI = func(_ context.Context, _ io.Reader, _ io.Writer, backend app.Backend) error {
		browser, ok := backend.(app.SessionBrowser)
		if !ok {
			t.Fatal("backend does not expose SessionBrowser")
		}
		if _, err := browser.ListSessions(context.Background(), 20); !errors.Is(err, app.ErrPersistenceDisabled) {
			t.Fatalf("ListSessions() error = %v, want ErrPersistenceDisabled", err)
		}
		if _, err := browser.ResumeSession(context.Background(), filepath.Join(workspace, "resume.jsonl")); !errors.Is(err, app.ErrPersistenceDisabled) {
			t.Fatalf("ResumeSession() error = %v, want ErrPersistenceDisabled", err)
		}
		return nil
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace, "--no-session", "--ui", "tui"}, strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}), deps)
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
}

func TestRunPrintsStartupWarningsBeforeTUI(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	resumePath := createCLISession(t, filepath.Join(home, ".otto", "sessions"), workspace, "warning-session")
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	deps := defaultRunDependencies()
	deps.subscribeInterrupts = func() interruptSubscription {
		return interruptSubscription{stop: func() {}}
	}
	deps.detectTerminal = func(io.Reader, io.Writer) bool { return true }
	var stdout, stderr bytes.Buffer
	deps.prepareSession = func(ctx context.Context, path, workspace string) (preparedSession, error) {
		prepared, err := prepareSession(ctx, path, workspace)
		if err != nil {
			return nil, err
		}
		return &fakePreparedSession{
			info: prepared.Info(),
			activate: func(ctx context.Context) (session.Session, []session.Warning, error) {
				store, warnings, err := prepared.Activate(ctx)
				return store, append(warnings, session.Warning{Message: "startup warning"}), err
			},
			close: prepared.Close,
		}, nil
	}
	deps.runTUI = func(_ context.Context, _ io.Reader, _ io.Writer, _ app.Backend) error {
		if !strings.Contains(stderr.String(), "warning: startup warning\n") {
			t.Fatalf("stderr before TUI = %q", stderr.String())
		}
		return nil
	}

	code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace, "--resume", resumePath, "--ui", "tui"}, strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}), deps)
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
}

func TestRunCanonicalizesCWDBeforeCreatingSession(t *testing.T) {
	home := t.TempDir()
	parent := t.TempDir()
	workspace := filepath.Join(parent, "workspace")
	if err := os.Mkdir(workspace, 0o755); err != nil {
		t.Fatal(err)
	}
	link := filepath.Join(parent, "link")
	if err := os.Symlink(workspace, link); err != nil {
		t.Fatal(err)
	}
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	var stdout, stderr bytes.Buffer
	code := run(context.Background(), []string{"--config", configPath, "--cwd", link}, strings.NewReader("/exit\n"), &stdout, &stderr, testGetenv(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}))
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	path := onlySessionPath(t, home)
	store, _, err := session.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	canonical, err := filepath.EvalSymlinks(workspace)
	if err != nil {
		t.Fatal(err)
	}
	if got := store.Header().Workspace; got != canonical {
		t.Fatalf("workspace = %q, want %q", got, canonical)
	}
}

func TestRunContinueSkipsOldOttoSessions(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		writeSSE(w, `{"choices":[{"delta":{"content":"unused"},"finish_reason":"stop"}]}`)
	}))
	defer server.Close()

	workspace := t.TempDir()
	home := t.TempDir()
	root := filepath.Join(home, ".otto", "sessions")
	olderValidPath := createCLISession(t, root, workspace, "older-valid-v3")
	newestValidPath := createCLISession(t, root, workspace, "newest-valid-v3")
	oldV1Path := writeOldOttoV1Session(t, root, workspace, "newer-old-otto-v1")
	corruptPath := writeCorruptPiSession(t, root, workspace, "newest-corrupt")
	oldV1Before, err := os.ReadFile(oldV1Path)
	if err != nil {
		t.Fatal(err)
	}
	corruptBefore, err := os.ReadFile(corruptPath)
	if err != nil {
		t.Fatal(err)
	}
	now := time.Now()
	setCLISessionMTime(t, olderValidPath, now.Add(-4*time.Hour))
	setCLISessionMTime(t, newestValidPath, now.Add(-3*time.Hour))
	setCLISessionMTime(t, oldV1Path, now.Add(-2*time.Hour))
	setCLISessionMTime(t, corruptPath, now.Add(-time.Hour))

	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)
	var stdout, stderr bytes.Buffer
	code := run(context.Background(), []string{"--config", configPath, "--cwd", workspace, "--continue"}, strings.NewReader("/session\n/exit\n"), &stdout, &stderr, testGetenv(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "offline-test-key",
	}))
	if code != 0 || !strings.Contains(stdout.String(), "ID: newest-valid-v3") {
		t.Fatalf("code = %d, stdout = %q, stderr = %q", code, stdout.String(), stderr.String())
	}
	if strings.Contains(stdout.String(), "newer-old-otto-v1") || strings.Contains(stdout.String(), "newest-corrupt") {
		t.Fatalf("--continue selected an invalid newer file: %q", stdout.String())
	}
	for _, fixture := range []struct {
		name   string
		path   string
		before []byte
	}{
		{name: "old Otto v1", path: oldV1Path, before: oldV1Before},
		{name: "corrupt Pi v3", path: corruptPath, before: corruptBefore},
	} {
		after, readErr := os.ReadFile(fixture.path)
		if readErr != nil {
			t.Fatalf("read %s after --continue: %v", fixture.name, readErr)
		}
		if !bytes.Equal(after, fixture.before) {
			t.Fatalf("%s session was modified by --continue", fixture.name)
		}
	}
}

func TestRunResumeAndContinueSelectSessions(t *testing.T) {
	workspace := t.TempDir()
	home := t.TempDir()
	root := filepath.Join(home, ".otto", "sessions")
	oldPath := createCLISession(t, root, workspace, "old-session")
	newPath := createCLISession(t, root, workspace, "new-session")
	oldV1Path := writeOldOttoV1Session(t, root, workspace, "old-v1")
	corruptPath := writeCorruptPiSession(t, root, workspace, "corrupt")
	now := time.Now()
	setCLISessionMTime(t, oldPath, now.Add(-2*time.Hour))
	setCLISessionMTime(t, newPath, now.Add(-time.Hour))
	setCLISessionMTime(t, oldV1Path, now.Add(-30*time.Minute))
	setCLISessionMTime(t, corruptPath, now)
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	env := testGetenv(map[string]string{"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret"})

	for _, test := range []struct {
		name string
		args []string
		want string
	}{
		{name: "resume", args: []string{"--resume", oldPath}, want: "ID: old-session"},
		{name: "continue skips invalid newer sessions", args: []string{"--continue"}, want: "ID: new-session"},
	} {
		t.Run(test.name, func(t *testing.T) {
			var stdout, stderr bytes.Buffer
			args := append([]string{"--config", configPath, "--cwd", workspace}, test.args...)
			code := run(context.Background(), args, strings.NewReader("/session\n/exit\n"), &stdout, &stderr, env)
			if code != 0 || !strings.Contains(stdout.String(), test.want) {
				t.Fatalf("code = %d, stdout = %q, stderr = %q", code, stdout.String(), stderr.String())
			}
		})
	}
}

func TestRunResumeExplicitPathRejectsInvalidPiSessions(t *testing.T) {
	workspace := t.TempDir()
	home := t.TempDir()
	root := filepath.Join(home, ".otto", "sessions")
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	env := testGetenv(map[string]string{"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret"})

	for _, test := range []struct {
		name string
		path string
		want string
	}{
		{name: "old Otto v1", path: writeOldOttoV1Session(t, root, workspace, "old-v1"), want: "unsupported session format"},
		{name: "corrupt", path: writeCorruptPiSession(t, root, workspace, "corrupt"), want: "invalid session"},
	} {
		t.Run(test.name, func(t *testing.T) {
			var stdout, stderr bytes.Buffer
			code := run(context.Background(), []string{"--config", configPath, "--cwd", workspace, "--resume", test.path}, strings.NewReader("/exit\n"), &stdout, &stderr, env)
			if code == 0 || !strings.Contains(stderr.String(), test.want) {
				t.Fatalf("code = %d, stdout = %q, stderr = %q", code, stdout.String(), stderr.String())
			}
		})
	}
}

func TestRunResumeRejectsAtomicReplacementOfPreparedSessionWithoutMutation(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	root := filepath.Join(home, ".otto", "sessions")
	resumePath := createCLISession(t, root, workspace, "prepared-startup")
	replacementPath := createCLISession(t, root, workspace, "path-replacement")
	replacementBefore, err := os.ReadFile(replacementPath)
	if err != nil {
		t.Fatal(err)
	}
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")

	deps := defaultRunDependencies()
	deps.subscribeInterrupts = func() interruptSubscription {
		return interruptSubscription{stop: func() {}}
	}
	deps.detectTerminal = func(io.Reader, io.Writer) bool { return true }
	deps.prepareSession = func(ctx context.Context, path, workspace string) (preparedSession, error) {
		prepared, err := prepareSession(ctx, path, workspace)
		if err != nil {
			return nil, err
		}
		return &fakePreparedSession{
			info: prepared.Info(),
			activate: func(ctx context.Context) (session.Session, []session.Warning, error) {
				if err := os.Rename(replacementPath, resumePath); err != nil {
					return nil, nil, err
				}
				return prepared.Activate(ctx)
			},
			close: prepared.Close,
		}, nil
	}
	var tuiCalls atomic.Int32
	deps.runTUI = func(context.Context, io.Reader, io.Writer, app.Backend) error {
		tuiCalls.Add(1)
		return nil
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{
		"--config", configPath, "--cwd", workspace, "--resume", resumePath, "--ui", "tui",
	}, strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}), deps)
	if code != 1 || !strings.Contains(stderr.String(), "identity changed") {
		t.Fatalf("code = %d, stderr = %q, want prepared identity error", code, stderr.String())
	}
	if tuiCalls.Load() != 0 {
		t.Fatalf("TUI calls = %d, want 0", tuiCalls.Load())
	}
	after, err := os.ReadFile(resumePath)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(after, replacementBefore) {
		t.Fatal("failed startup activation mutated the replacement path")
	}
}

func TestRunResumeTUIInfoUsesResolvedRuntimeOverrides(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	resumePath := createCLISession(t, filepath.Join(home, ".otto", "sessions"), workspace, "resumed-session")
	configPath := filepath.Join(t.TempDir(), "otto.toml")
	content := `default_profile = "active"
[profiles.active]
provider = "openai-compatible"
base_url = "http://127.0.0.1:1"
model = "profile-model"
api_key_env = "ACTIVE_KEY"
[profiles.test]
provider = "openai-compatible"
base_url = "http://127.0.0.1:2"
model = "persisted-model"
api_key_env = "TEST_KEY"
`
	if err := os.WriteFile(configPath, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}

	deps := defaultRunDependencies()
	deps.subscribeInterrupts = func() interruptSubscription {
		return interruptSubscription{stop: func() {}}
	}
	deps.detectTerminal = func(io.Reader, io.Writer) bool { return true }
	var got app.Info
	deps.runTUI = func(_ context.Context, _ io.Reader, _ io.Writer, backend app.Backend) error {
		got = backend.Info()
		return nil
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{
		"--config", configPath,
		"--cwd", workspace,
		"--resume", resumePath,
		"--profile", "active",
		"--model", "override-model",
		"--ui", "tui",
	}, strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "ACTIVE_KEY": "active-secret", "TEST_KEY": "persisted-secret",
	}), deps)
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	canonicalResumePath, err := filepath.EvalSymlinks(resumePath)
	if err != nil {
		t.Fatal(err)
	}
	if got.SessionID != "resumed-session" || got.SessionPath != canonicalResumePath || got.Workspace != workspace {
		t.Fatalf("dynamic session info = %#v", got)
	}
	if got.Provider != "openai-compatible" || got.Profile != "active" || got.Model != "override-model" {
		t.Fatalf("runtime info = %#v, want resolved overrides", got)
	}
}

func TestRunResumeAndContinuePersistEffectiveRuntimeOverrides(t *testing.T) {
	for _, selector := range []string{"resume", "continue"} {
		t.Run(selector, func(t *testing.T) {
			var requestModel, authorization string
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				authorization = r.Header.Get("Authorization")
				var request struct {
					Model string `json:"model"`
				}
				if err := json.NewDecoder(r.Body).Decode(&request); err != nil {
					t.Errorf("decode request: %v", err)
				}
				requestModel = request.Model
				writeSSE(w, `{"choices":[{"delta":{"content":"effective answer"},"finish_reason":"stop"}]}`)
			}))
			defer server.Close()

			home := t.TempDir()
			workspace := t.TempDir()
			root := filepath.Join(home, ".otto", "sessions")
			store, err := session.Create(root, session.Header{
				Version: session.CurrentVersion, ID: selector + "-runtime", Workspace: workspace,
				Provider: "openai-compatible", Profile: "stored", Model: "stored-model", CreatedAt: time.Now().UTC(),
			})
			if err != nil {
				t.Fatal(err)
			}
			path := store.Path()
			if err := store.Close(); err != nil {
				t.Fatal(err)
			}

			configPath := filepath.Join(t.TempDir(), "otto.toml")
			configText := fmt.Sprintf(`default_profile = "stored"
[profiles.stored]
provider = "openai-compatible"
base_url = %q
model = "stored-profile-model"
api_key_env = "STORED_KEY"
[profiles.override]
provider = "openai-compatible"
base_url = %q
model = "override-profile-model"
api_key_env = "OVERRIDE_KEY"
`, server.URL, server.URL)
			if err := os.WriteFile(configPath, []byte(configText), 0o600); err != nil {
				t.Fatal(err)
			}
			args := []string{
				"--config", configPath, "--cwd", workspace,
				"--profile", "override", "--provider", "openai-compatible", "--model", "effective-model",
			}
			if selector == "resume" {
				args = append(args, "--resume", path)
			} else {
				args = append(args, "--continue")
			}
			var stdout, stderr bytes.Buffer
			code := run(context.Background(), args, strings.NewReader("use effective runtime\n/exit\n"), &stdout, &stderr, testGetenv(map[string]string{
				"HOME": home, "SHELL": "/bin/sh", "STORED_KEY": "stored-secret", "OVERRIDE_KEY": "override-secret",
			}))
			if code != 0 {
				t.Fatalf("code = %d, stderr = %q", code, stderr.String())
			}
			if requestModel != "effective-model" || authorization != "Bearer override-secret" {
				t.Fatalf("request model/auth = %q / %q", requestModel, authorization)
			}

			reopened, warnings, err := session.Open(path)
			if err != nil {
				t.Fatal(err)
			}
			if len(warnings) != 0 {
				_ = reopened.Close()
				t.Fatalf("warnings = %#v", warnings)
			}
			header := reopened.Header()
			if err := reopened.Close(); err != nil {
				t.Fatal(err)
			}
			if header.Profile != "override" || header.Provider != "openai-compatible" || header.Model != "effective-model" {
				t.Fatalf("reopened Header() = %#v", header)
			}

			lines := bytes.Split(bytes.TrimSpace(mustReadFile(t, path)), []byte{'\n'})
			var assistantProvider, assistantModel string
			for _, line := range lines {
				var record struct {
					Type    string `json:"type"`
					Message struct {
						Role     string `json:"role"`
						Provider string `json:"provider"`
						Model    string `json:"model"`
					} `json:"message"`
				}
				if err := json.Unmarshal(line, &record); err != nil {
					t.Fatal(err)
				}
				if record.Type == "message" && record.Message.Role == "assistant" {
					assistantProvider, assistantModel = record.Message.Provider, record.Message.Model
				}
			}
			if assistantProvider != "openai-compatible" || assistantModel != "effective-model" {
				t.Fatalf("persisted assistant provider/model = %q/%q", assistantProvider, assistantModel)
			}
		})
	}
}

func TestRunResumeUsesPersistedProfileForEndpointAndKey(t *testing.T) {
	var defaultRequests, resumedRequests atomic.Int32
	defaultServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		defaultRequests.Add(1)
		writeSSE(w, `{"choices":[{"delta":{"content":"wrong profile"},"finish_reason":"stop"}]}`)
	}))
	defer defaultServer.Close()
	resumedServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		resumedRequests.Add(1)
		if r.Header.Get("Authorization") != "Bearer resumed-secret" {
			t.Errorf("authorization did not use resumed profile key")
		}
		writeSSE(w, `{"choices":[{"delta":{"content":"resumed profile"},"finish_reason":"stop"}]}`)
	}))
	defer resumedServer.Close()

	home := t.TempDir()
	workspace := t.TempDir()
	resumePath := createCLISession(t, filepath.Join(home, ".otto", "sessions"), workspace, "resumed-session")
	configPath := filepath.Join(t.TempDir(), "otto.toml")
	content := fmt.Sprintf(`default_profile = "default"
[profiles.default]
provider = "openai-compatible"
base_url = %q
model = "default-model"
api_key_env = "DEFAULT_KEY"
[profiles.test]
provider = "openai-compatible"
base_url = %q
model = "resumed-model"
api_key_env = "RESUMED_KEY"
`, defaultServer.URL, resumedServer.URL)
	if err := os.WriteFile(configPath, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}

	var stdout, stderr bytes.Buffer
	code := run(context.Background(), []string{"--config", configPath, "--cwd", workspace, "--resume", resumePath}, strings.NewReader("hello\n/exit\n"), &stdout, &stderr, testGetenv(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "DEFAULT_KEY": "default-secret", "RESUMED_KEY": "resumed-secret",
	}))
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if defaultRequests.Load() != 0 || resumedRequests.Load() != 1 || !strings.Contains(stdout.String(), "resumed profile") {
		t.Fatalf("default requests = %d, resumed requests = %d, stdout = %q", defaultRequests.Load(), resumedRequests.Load(), stdout.String())
	}
}

func TestRunNewAfterResumeResetsStartupRuntimeInfoRunnerAndHeader(t *testing.T) {
	var defaultRequests, resumedRequests atomic.Int32
	defaultServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		defaultRequests.Add(1)
		var request struct {
			Model string `json:"model"`
		}
		if err := json.NewDecoder(r.Body).Decode(&request); err != nil {
			t.Errorf("decode default request: %v", err)
		}
		if request.Model != "startup-model" || r.Header.Get("Authorization") != "Bearer startup-secret" {
			t.Errorf("default request model/auth = %q / %q", request.Model, r.Header.Get("Authorization"))
		}
		writeSSE(w, `{"choices":[{"delta":{"content":"startup runner"},"finish_reason":"stop"}]}`)
	}))
	defer defaultServer.Close()
	resumedServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		resumedRequests.Add(1)
		writeSSE(w, `{"choices":[{"delta":{"content":"resumed runner"},"finish_reason":"stop"}]}`)
	}))
	defer resumedServer.Close()

	home := t.TempDir()
	workspace := t.TempDir()
	resumedStore, err := session.Create(filepath.Join(home, ".otto", "sessions"), session.Header{
		Version: session.CurrentVersion, ID: "resumed", Workspace: workspace, Provider: "openai-compatible",
		Profile: "resumed", Model: "resumed-model", CreatedAt: time.Now().UTC(),
	})
	if err != nil {
		t.Fatal(err)
	}
	resumePath := resumedStore.Path()
	if err := resumedStore.Close(); err != nil {
		t.Fatal(err)
	}
	configPath := filepath.Join(t.TempDir(), "otto.toml")
	configContent := fmt.Sprintf(`default_profile = "startup"
[profiles.startup]
provider = "openai-compatible"
base_url = %q
model = "startup-model"
api_key_env = "STARTUP_KEY"
[profiles.resumed]
provider = "openai-compatible"
base_url = %q
model = "profile-resumed-model"
api_key_env = "RESUMED_KEY"
`, defaultServer.URL, resumedServer.URL)
	if err := os.WriteFile(configPath, []byte(configContent), 0o600); err != nil {
		t.Fatal(err)
	}

	deps := defaultRunDependencies()
	deps.subscribeInterrupts = func() interruptSubscription {
		return interruptSubscription{stop: func() {}}
	}
	deps.detectTerminal = func(io.Reader, io.Writer) bool { return true }
	deps.runTUI = func(ctx context.Context, _ io.Reader, _ io.Writer, backend app.Backend) error {
		browser, ok := backend.(app.SessionBrowser)
		if !ok {
			t.Fatal("backend does not expose SessionBrowser")
		}
		if _, err := browser.ResumeSession(ctx, resumePath); err != nil {
			return err
		}
		if got := backend.Info(); got.SessionID != "resumed" || got.Profile != "resumed" || got.Model != "resumed-model" {
			t.Fatalf("info after resume = %#v", got)
		}
		if err := backend.NewSession(); err != nil {
			return err
		}
		got := backend.Info()
		if got.SessionID == "resumed" || got.Provider != "openai-compatible" || got.Profile != "startup" || got.Model != "startup-model" {
			t.Fatalf("info after new = %#v", got)
		}
		return backend.Prompt(ctx, "use startup runtime", nil)
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{
		"--config", configPath, "--cwd", workspace, "--ui", "tui",
	}, strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "STARTUP_KEY": "startup-secret", "RESUMED_KEY": "resumed-secret",
	}), deps)
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if defaultRequests.Load() != 1 || resumedRequests.Load() != 0 {
		t.Fatalf("requests = startup %d resumed %d, want 1 and 0", defaultRequests.Load(), resumedRequests.Load())
	}
}

func TestRunNewReplacesSessionWithoutRestarting(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	var stdout, stderr bytes.Buffer
	code := run(context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader("/new\n/session\n/exit\nmust not run\n"), &stdout, &stderr, testGetenv(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}))
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if !strings.Contains(stdout.String(), "ID: ") {
		t.Fatalf("/session after /new was not executed: stdout = %q", stdout.String())
	}
	paths, err := filepath.Glob(filepath.Join(home, ".otto", "sessions", "*", "*.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	if len(paths) != 2 {
		t.Fatalf("session files = %v, want two", paths)
	}
	for _, path := range paths {
		store, _, err := session.Open(path)
		if err != nil {
			t.Fatal(err)
		}
		if messages := store.Messages(); len(messages) != 0 {
			_ = store.Close()
			t.Fatalf("/exit after /new did not stop later input: messages = %#v", messages)
		}
		if err := store.Close(); err != nil {
			t.Fatal(err)
		}
	}
}

func TestRunNewSessionCreationFailureReturnsDirectError(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	deps := defaultRunDependencies()
	deps.subscribeInterrupts = func() interruptSubscription {
		return interruptSubscription{stop: func() {}}
	}
	var calls int
	deps.newSession = func(_ bool, _ string, workspace string, runtime config.Runtime) (session.Session, error) {
		calls++
		if calls == 2 {
			return nil, errors.New("create replacement failed")
		}
		return session.NewMemory(session.Header{
			Version: 1, ID: fmt.Sprintf("session-%d", calls), Workspace: workspace,
			Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model, CreatedAt: time.Now().UTC(),
		}), nil
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader("/new\n"), &stdout, &stderr, testGetenv(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}), deps)
	if code != 1 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if got, want := stderr.String(), "otto: create replacement failed\n"; got != want {
		t.Fatalf("stderr = %q, want %q", got, want)
	}
	if strings.Contains(stderr.String(), "REPL:") {
		t.Fatalf("stderr unexpectedly wrapped REPL error: %q", stderr.String())
	}
}

func TestRunAppliesMaxTurnsAndShellTimeout(t *testing.T) {
	var requests atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		requests.Add(1)
		writeSSE(w, `{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"sleep 0.2\"}"}}]},"finish_reason":"tool_calls"}]}`)
	}))
	defer server.Close()

	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)
	var stdout, stderr bytes.Buffer
	code := run(context.Background(), []string{"--config", configPath, "--cwd", workspace, "--max-turns", "1", "--shell-timeout", "10ms"}, strings.NewReader("run slowly\n/exit\n"), &stdout, &stderr, testGetenv(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}))
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if requests.Load() != 1 || !strings.Contains(stderr.String(), "agent max turns exceeded") {
		t.Fatalf("requests = %d, stderr = %q", requests.Load(), stderr.String())
	}
	content, err := os.ReadFile(onlySessionPath(t, home))
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(content), "timed out after 10ms") {
		t.Fatalf("session missing timed-out tool result: %s", content)
	}
}

func TestRunEndToEndToolCallSmoke(t *testing.T) {
	const expectedSystemPrompt = "You are Otto, a concise coding agent. Inspect the workspace before changing it. Use read, write, edit, and bash when needed. File tools are restricted to the workspace, but bash is unsandboxed. Prefer exact, minimal changes. Report what changed and what verification ran."
	var requestCount int
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		requestCount++
		var payload struct {
			Messages []struct {
				Role    string `json:"role"`
				Content string `json:"content"`
			} `json:"messages"`
			Tools []struct {
				Function struct {
					Name string `json:"name"`
				} `json:"function"`
			} `json:"tools"`
		}
		if err := json.NewDecoder(r.Body).Decode(&payload); err != nil {
			t.Errorf("decode request: %v", err)
		}
		if len(payload.Messages) == 0 || payload.Messages[0].Role != "system" || payload.Messages[0].Content != expectedSystemPrompt {
			t.Errorf("system message = %#v", payload.Messages)
		}
		if requestCount == 1 {
			var names []string
			for _, item := range payload.Tools {
				names = append(names, item.Function.Name)
			}
			if !reflect.DeepEqual(names, []string{"read", "write", "edit", "bash"}) {
				t.Errorf("tool names = %v", names)
			}
			writeSSE(w, `{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-write","type":"function","function":{"name":"write","arguments":"{\"path\":\"created.txt\",\"content\":\"hello\"}"}}]},"finish_reason":"tool_calls"}]}`)
			return
		}
		writeSSE(w, `{"choices":[{"delta":{"content":"complete"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":1}}`)
	}))
	defer server.Close()

	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)
	var stdout, stderr bytes.Buffer
	code := run(context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader("create it\n/exit\n"), &stdout, &stderr, testGetenv(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "smoke-secret",
	}))
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if requestCount != 2 {
		t.Fatalf("requests = %d, want 2", requestCount)
	}
	created, err := os.ReadFile(filepath.Join(workspace, "created.txt"))
	if err != nil || string(created) != "hello" {
		t.Fatalf("created file = %q, error = %v", created, err)
	}
	if !strings.Contains(stdout.String(), "[tool] write (call-write)") || !strings.Contains(stdout.String(), "[tool result] wrote created.txt (5 bytes)") || !strings.Contains(stdout.String(), "complete") {
		t.Fatalf("unexpected compact output: %q", stdout.String())
	}

	store, _, err := session.Open(onlySessionPath(t, home))
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	messages := store.Messages()
	var roles []model.Role
	for _, message := range messages {
		roles = append(roles, message.Role)
	}
	if !reflect.DeepEqual(roles, []model.Role{model.RoleUser, model.RoleAssistant, model.RoleTool, model.RoleAssistant}) {
		t.Fatalf("session roles = %v", roles)
	}
	if got := messages[len(messages)-1].Text(); got != "complete" {
		t.Fatalf("final assistant text = %q", got)
	}
	persisted, err := os.ReadFile(store.Path())
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(persisted), "smoke-secret") {
		t.Fatal("API key leaked into session")
	}
}

func TestRunRedactsSuccessfulProviderCredentialEchoAcrossSSEDeltasAndToolArguments(t *testing.T) {
	credential := fmt.Sprintf("provider-echo-secret-%d", time.Now().UnixNano())
	authorization := "Bearer " + credential
	home := t.TempDir()
	workspace := t.TempDir()

	var requestBodies []string
	var requestCount int
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(r.Body)
		if err != nil {
			t.Errorf("read request: %v", err)
		}
		requestBodies = append(requestBodies, string(body))
		requestCount++
		if got := r.Header.Get("Authorization"); got != authorization {
			t.Errorf("Authorization = %q, want resolved credential", got)
		}

		w.Header().Set("Content-Type", "text/event-stream")
		if requestCount == 1 {
			textSplit := len(authorization) - 3
			arguments := fmt.Sprintf(`{"path":"credential.txt","content":%q}`, authorization)
			argumentSplit := strings.Index(arguments, credential) + len(credential)/2
			chunks := []string{
				fmt.Sprintf(`{"choices":[{"delta":{"content":%q}}]}`, "authorization="+authorization[:textSplit]),
				fmt.Sprintf(`{"choices":[{"delta":{"content":%q}}]}`, authorization[textSplit:]),
				fmt.Sprintf(`{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-write","type":"function","function":{"name":"write","arguments":%q}}]}}]}`, arguments[:argumentSplit]),
				fmt.Sprintf(`{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":%q}}]},"finish_reason":"tool_calls"}]}`, arguments[argumentSplit:]),
			}
			for _, chunk := range chunks {
				_, _ = fmt.Fprintf(w, "data: %s\n\n", chunk)
			}
			_, _ = io.WriteString(w, "data: [DONE]\n\n")
			return
		}
		writeSSE(w, `{"choices":[{"delta":{"content":"credential remained private"},"finish_reason":"stop"}]}`)
	}))
	defer server.Close()

	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)
	var stdout, stderr bytes.Buffer
	code := run(context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader("check provider output\n/exit\n"), &stdout, &stderr, testGetenv(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": credential,
	}))
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if requestCount != 2 {
		t.Fatalf("provider requests = %d, want 2", requestCount)
	}

	persisted, err := os.ReadFile(onlySessionPath(t, home))
	if err != nil {
		t.Fatal(err)
	}
	executed, err := os.ReadFile(filepath.Join(workspace, "credential.txt"))
	if err != nil {
		t.Fatal(err)
	}
	locations := map[string]string{
		"stdout events":            stdout.String(),
		"stderr":                   stderr.String(),
		"executed write arguments": string(executed),
		"provider follow-up":       requestBodies[1],
		"session JSONL":            string(persisted),
	}
	for location, content := range locations {
		if strings.Contains(content, credential) || strings.Contains(content, authorization) {
			t.Fatalf("%s leaked successful provider credential echo: %q", location, content)
		}
	}
	for _, location := range []string{"stdout events", "executed write arguments", "provider follow-up", "session JSONL"} {
		if !strings.Contains(locations[location], "█") {
			t.Fatalf("%s did not retain a redaction marker: %q", location, locations[location])
		}
	}
}

func TestRunKeepsEveryProfileCredentialOutOfBashEventsSessionAndProviderHistory(t *testing.T) {
	resolvedCredential := fmt.Sprintf("resolved-%d", time.Now().UnixNano())
	inactiveCredential := fmt.Sprintf("inactive-%d", time.Now().UnixNano())
	fallbackCredential := fmt.Sprintf("fallback-%d", time.Now().UnixNano())
	const (
		profileKeyEnv  = "OTTO_E2E_PROFILE_KEY"
		inactiveKeyEnv = "OTTO_E2E_INACTIVE_KEY"
	)
	t.Setenv(profileKeyEnv, resolvedCredential)
	t.Setenv(inactiveKeyEnv, inactiveCredential)
	t.Setenv("OTTO_API_KEY", fallbackCredential)
	t.Setenv("OTTO_E2E_UNRELATED", "preserved-environment")

	home := t.TempDir()
	workspace := t.TempDir()
	midpoint := len(resolvedCredential) / 2
	if err := os.WriteFile(filepath.Join(workspace, ".credential-part-1"), []byte(resolvedCredential[:midpoint]), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(workspace, ".credential-part-2"), []byte(resolvedCredential[midpoint:]), 0o600); err != nil {
		t.Fatal(err)
	}

	var mu sync.Mutex
	var requestBodies []string
	requestCount := 0
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(r.Body)
		if err != nil {
			t.Errorf("read request: %v", err)
		}
		if got := r.Header.Get("Authorization"); got != "Bearer "+resolvedCredential {
			t.Errorf("Authorization = %q, want resolved credential", got)
		}
		mu.Lock()
		requestBodies = append(requestBodies, string(body))
		requestCount++
		current := requestCount
		mu.Unlock()
		if current == 1 {
			command := "env | grep -E '^OTTO_E2E_(.*_KEY|UNRELATED)=' || true; printf 'reconstructed='; cat .credential-part-1 .credential-part-2"
			writeSSE(w, fmt.Sprintf(`{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-bash","type":"function","function":{"name":"bash","arguments":%q}}]},"finish_reason":"tool_calls"}]}`, fmt.Sprintf(`{"command":%q}`, command)))
			return
		}
		writeSSE(w, `{"choices":[{"delta":{"content":"credentials stayed private"},"finish_reason":"stop"}]}`)
	}))
	defer server.Close()

	configPath := filepath.Join(t.TempDir(), "otto.toml")
	configContent := fmt.Sprintf(`default_profile = "active"
[profiles.active]
provider = "openai-compatible"
base_url = %q
model = "test-model"
api_key_env = %q
[profiles.inactive]
provider = "openai-compatible"
base_url = "https://inactive.example/v1"
model = "inactive-model"
api_key_env = %q
`, server.URL, profileKeyEnv, inactiveKeyEnv)
	if err := os.WriteFile(configPath, []byte(configContent), 0o600); err != nil {
		t.Fatal(err)
	}
	var stdout, stderr bytes.Buffer
	code := run(context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader("check credentials\n/exit\n"), &stdout, &stderr, testGetenv(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", profileKeyEnv: resolvedCredential, inactiveKeyEnv: inactiveCredential, "OTTO_API_KEY": fallbackCredential,
	}))
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}

	persisted, err := os.ReadFile(onlySessionPath(t, home))
	if err != nil {
		t.Fatal(err)
	}
	mu.Lock()
	bodies := append([]string(nil), requestBodies...)
	mu.Unlock()
	if len(bodies) != 2 {
		t.Fatalf("provider requests = %d, want 2", len(bodies))
	}
	for location, content := range map[string]string{
		"stdout": stdout.String(), "stderr": stderr.String(), "JSONL": string(persisted), "provider request 1": bodies[0], "provider request 2": bodies[1],
	} {
		for _, forbidden := range []string{resolvedCredential, inactiveCredential, fallbackCredential, profileKeyEnv, inactiveKeyEnv, "OTTO_API_KEY"} {
			if strings.Contains(content, forbidden) {
				t.Fatalf("%s leaked protected credential data %q", location, forbidden)
			}
		}
	}
	redactedReconstruction := strings.Contains(string(persisted), "reconstructed=[REDACTED]") || strings.Contains(string(persisted), "reconstructed=█")
	if !redactedReconstruction || !strings.Contains(string(persisted), "OTTO_E2E_UNRELATED=preserved-environment") {
		t.Fatalf("persisted bash event/result did not redact credential while preserving unrelated environment: %s", persisted)
	}
}

func TestRunInjectedSignalCancelsOnlyActiveTurn(t *testing.T) {
	requestStarted := make(chan struct{})
	releaseRequest := make(chan struct{})
	server := httptest.NewServer(http.HandlerFunc(func(_ http.ResponseWriter, r *http.Request) {
		close(requestStarted)
		select {
		case <-r.Context().Done():
		case <-releaseRequest:
		}
	}))
	defer server.Close()
	defer close(releaseRequest)

	interrupts := make(chan os.Signal, 1)
	subscribed := make(chan struct{})
	var stopCalls atomic.Int32
	deps := defaultRunDependencies()
	deps.subscribeInterrupts = func() interruptSubscription {
		close(subscribed)
		return interruptSubscription{
			signals: interrupts,
			stop:    func() { stopCalls.Add(1) },
		}
	}
	var stores []*trackingSession
	deps.newSession = func(_ bool, _ string, workspace string, runtime config.Runtime) (session.Session, error) {
		store := &trackingSession{Session: session.NewMemory(session.Header{
			Version: 1, ID: fmt.Sprintf("session-%d", len(stores)+1), Workspace: workspace,
			Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model, CreatedAt: time.Now().UTC(),
		})}
		stores = append(stores, store)
		return store, nil
	}

	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)
	var stdout, stderr bytes.Buffer
	codeCh := make(chan int, 1)
	go func() {
		codeCh <- runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader("wait\n/exit\n"), &stdout, &stderr, testGetenv(map[string]string{
			"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
		}), deps)
	}()

	select {
	case <-subscribed:
	case <-time.After(time.Second):
		t.Fatal("interrupt subscription did not start")
	}
	select {
	case <-requestStarted:
	case <-time.After(time.Second):
		t.Fatal("active provider turn did not start")
	}
	interrupts <- os.Interrupt
	select {
	case code := <-codeCh:
		if code != 0 {
			t.Fatalf("code = %d, stderr = %q", code, stderr.String())
		}
	case <-time.After(2 * time.Second):
		t.Fatal("active interrupt did not return to REPL for /exit")
	}
	if stopCalls.Load() != 1 {
		t.Fatalf("interrupt subscription stop calls = %d, want 1", stopCalls.Load())
	}
	if len(stores) != 1 {
		t.Fatalf("created stores = %d, want 1", len(stores))
	}
	if stores[0].closeCalls.Load() != 1 {
		t.Fatalf("store close calls = %d, want 1", stores[0].closeCalls.Load())
	}
}

func TestRunInjectedSignalWhileIdleExits130AndCleansUp(t *testing.T) {
	interrupts := make(chan os.Signal, 1)
	subscribed := make(chan struct{})
	var stopCalls atomic.Int32
	deps := defaultRunDependencies()
	deps.subscribeInterrupts = func() interruptSubscription {
		close(subscribed)
		return interruptSubscription{
			signals: interrupts,
			stop:    func() { stopCalls.Add(1) },
		}
	}
	var stores []*trackingSession
	deps.newSession = func(_ bool, _ string, workspace string, runtime config.Runtime) (session.Session, error) {
		store := &trackingSession{Session: session.NewMemory(session.Header{
			Version: 1, ID: fmt.Sprintf("session-%d", len(stores)+1), Workspace: workspace,
			Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model, CreatedAt: time.Now().UTC(),
		})}
		stores = append(stores, store)
		return store, nil
	}

	reader, writer := io.Pipe()
	defer reader.Close()
	defer writer.Close()
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	var stdout, stderr bytes.Buffer
	codeCh := make(chan int, 1)
	go func() {
		codeCh <- runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace}, reader, &stdout, &stderr, testGetenv(map[string]string{
			"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
		}), deps)
	}()

	select {
	case <-subscribed:
	case <-time.After(time.Second):
		t.Fatal("interrupt subscription did not start")
	}
	interrupts <- os.Interrupt
	select {
	case code := <-codeCh:
		if code != 130 {
			t.Fatalf("code = %d, stderr = %q, want 130", code, stderr.String())
		}
	case <-time.After(2 * time.Second):
		t.Fatal("idle interrupt did not stop command")
	}
	if stopCalls.Load() != 1 {
		t.Fatalf("interrupt subscription stop calls = %d, want 1", stopCalls.Load())
	}
	if len(stores) != 1 {
		t.Fatalf("created stores = %d, want 1", len(stores))
	}
	if stores[0].closeCalls.Load() != 1 {
		t.Fatalf("store close calls = %d, want 1", stores[0].closeCalls.Load())
	}
}

func TestRunInjectedSignalCancelsActiveTUITurnExits130AndClosesOnce(t *testing.T) {
	interrupts := make(chan os.Signal, 1)
	subscribed := make(chan struct{})
	var stopCalls atomic.Int32
	runnerStarted := make(chan struct{})
	runnerCanceled := make(chan struct{})

	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	store := &trackingSession{Session: session.NewMemory(session.Header{
		Version: 1, ID: "signal-tui", Workspace: workspace, Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Now().UTC(),
	})}
	deps := defaultRunDependencies()
	deps.subscribeInterrupts = func() interruptSubscription {
		close(subscribed)
		return interruptSubscription{
			signals: interrupts,
			stop:    func() { stopCalls.Add(1) },
		}
	}
	deps.detectTerminal = func(io.Reader, io.Writer) bool { return true }
	deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) {
		return store, nil
	}
	deps.newRunner = func(session.Session) app.Runner {
		return commandRunnerFunc(func(ctx context.Context, _ string, _ func(agent.Event)) error {
			close(runnerStarted)
			<-ctx.Done()
			close(runnerCanceled)
			return ctx.Err()
		})
	}
	deps.runTUI = func(ctx context.Context, _ io.Reader, _ io.Writer, backend app.Backend) error {
		promptDone := make(chan error, 1)
		go func() { promptDone <- backend.Prompt(ctx, "active TUI prompt", nil) }()
		select {
		case err := <-promptDone:
			return err
		case <-ctx.Done():
			return <-promptDone
		}
	}

	var stdout, stderr bytes.Buffer
	codeDone := make(chan int, 1)
	go func() {
		codeDone <- runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace, "--ui", "tui"}, strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{
			"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
		}), deps)
	}()

	select {
	case <-subscribed:
	case <-time.After(time.Second):
		t.Fatal("interrupt subscription did not start")
	}
	select {
	case <-runnerStarted:
	case <-time.After(time.Second):
		t.Fatal("active TUI prompt did not start")
	}
	interrupts <- os.Interrupt
	select {
	case code := <-codeDone:
		if code != 130 {
			t.Fatalf("code = %d, stderr = %q, want 130", code, stderr.String())
		}
	case <-time.After(2 * time.Second):
		t.Fatal("TUI signal cancellation did not stop command")
	}
	select {
	case <-runnerCanceled:
	default:
		t.Fatal("TUI prompt context was not canceled")
	}
	if stopCalls.Load() != 1 {
		t.Fatalf("interrupt stop calls = %d, want 1", stopCalls.Load())
	}
	if store.closeCalls.Load() != 1 {
		t.Fatalf("session close calls = %d, want 1", store.closeCalls.Load())
	}
}

func TestRunClosesSessionsOnceAcrossExitPaths(t *testing.T) {
	fatalErr := errors.Join(session.ErrFatalPersistence, errors.New("injected append failure"))
	for _, test := range []struct {
		name      string
		input     string
		appendErr error
		wantCode  int
		wantCount int
	}{
		{name: "normal exit", input: "/exit\n", wantCode: 0, wantCount: 1},
		{name: "replacement", input: "/new\n/exit\n", wantCode: 0, wantCount: 2},
		{name: "fatal persistence", input: "hello\n", appendErr: fatalErr, wantCode: 1, wantCount: 1},
		{name: "REPL error", input: strings.Repeat("x", (1<<20)+1) + "\n", wantCode: 1, wantCount: 1},
	} {
		t.Run(test.name, func(t *testing.T) {
			home := t.TempDir()
			workspace := t.TempDir()
			configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
			deps := defaultRunDependencies()
			deps.subscribeInterrupts = func() interruptSubscription {
				return interruptSubscription{stop: func() {}}
			}
			var stores []*trackingSession
			deps.newSession = func(_ bool, _ string, workspace string, runtime config.Runtime) (session.Session, error) {
				store := &trackingSession{Session: session.NewMemory(session.Header{
					Version: 1, ID: fmt.Sprintf("session-%d", len(stores)+1), Workspace: workspace,
					Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model, CreatedAt: time.Now().UTC(),
				}), appendErr: test.appendErr}
				stores = append(stores, store)
				return store, nil
			}

			var stdout, stderr bytes.Buffer
			code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader(test.input), &stdout, &stderr, testGetenv(map[string]string{
				"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
			}), deps)
			if code != test.wantCode {
				t.Fatalf("code = %d, stderr = %q, want %d", code, stderr.String(), test.wantCode)
			}
			if len(stores) != test.wantCount {
				t.Fatalf("created stores = %d, want %d", len(stores), test.wantCount)
			}
			for i, store := range stores {
				if store.closeCalls.Load() != 1 {
					t.Fatalf("store %d close calls = %d, want 1", i, store.closeCalls.Load())
				}
			}
		})
	}
}

func TestRunDoesNotAcceptOrEchoAPIKeyArguments(t *testing.T) {
	const secret = "argument-secret"
	var stdout, stderr bytes.Buffer
	code := run(context.Background(), []string{"--api-key", secret}, strings.NewReader(""), &stdout, &stderr, func(string) string { return "" })
	if code == 0 {
		t.Fatal("--api-key unexpectedly accepted")
	}
	if strings.Contains(stdout.String(), secret) || strings.Contains(stderr.String(), secret) {
		t.Fatalf("secret echoed in output: stdout=%q stderr=%q", stdout.String(), stderr.String())
	}
}

type commandRunnerFunc func(context.Context, string, func(agent.Event)) error

func (f commandRunnerFunc) Run(ctx context.Context, text string, emit func(agent.Event)) error {
	return f(ctx, text, emit)
}

type trackingSession struct {
	session.Session
	appendErr  error
	closeErr   error
	closeCalls atomic.Int32
}

func (s *trackingSession) UpdateRuntime(ctx context.Context, metadata session.RuntimeMetadata) error {
	updater, ok := s.Session.(session.RuntimeUpdater)
	if !ok {
		return errors.New("wrapped session does not support runtime updates")
	}
	return updater.UpdateRuntime(ctx, metadata)
}

func (s *trackingSession) Append(ctx context.Context, message model.Message) error {
	if s.appendErr != nil {
		return s.appendErr
	}
	return s.Session.Append(ctx, message)
}

func (s *trackingSession) Close() error {
	s.closeCalls.Add(1)
	if err := s.Session.Close(); err != nil {
		return err
	}
	return s.closeErr
}

func writeCLIConfig(t *testing.T, providerName, keyEnv, baseURL string) string {
	return writeCLIConfigWithUI(t, providerName, keyEnv, baseURL, "")
}

func writeCLIConfigWithUI(t *testing.T, providerName, keyEnv, baseURL, uiMode string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "otto.toml")
	content := "default_profile = \"test\"\n"
	if uiMode != "" {
		content += fmt.Sprintf(`[ui]
mode = %q
`, uiMode)
	}
	content += fmt.Sprintf(`[profiles.test]
provider = %q
base_url = %q
model = "test-model"
api_key_env = %q
`, providerName, baseURL, keyEnv)
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

func testGetenv(values map[string]string) func(string) string {
	return func(key string) string { return values[key] }
}

func createCLISession(t *testing.T, root, workspace, id string) string {
	t.Helper()
	store, err := session.Create(root, session.Header{Version: session.CurrentVersion, ID: id, Workspace: workspace, Provider: "openai-compatible", Profile: "test", Model: "test-model", CreatedAt: time.Now().UTC()})
	if err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	return path
}

func onlySessionPath(t *testing.T, home string) string {
	t.Helper()
	paths, err := filepath.Glob(filepath.Join(home, ".otto", "sessions", "*", "*.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	if len(paths) != 1 {
		t.Fatalf("session paths = %v, want one", paths)
	}
	return paths[0]
}

func writeOldOttoV1Session(t *testing.T, root, workspace, id string) string {
	t.Helper()
	path := filepath.Join(cliSessionDirectory(t, root, workspace), id+".jsonl")
	content := fmt.Sprintf("{\"type\":\"header\",\"header\":{\"version\":1,\"id\":%q,\"workspace\":%q,\"provider\":\"openai-compatible\",\"profile\":\"test\",\"model\":\"test-model\",\"created_at\":\"2026-08-27T12:00:00Z\"}}\n", id, workspace)
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

func writeCorruptPiSession(t *testing.T, root, workspace, id string) string {
	t.Helper()
	path := filepath.Join(cliSessionDirectory(t, root, workspace), id+".jsonl")
	content := fmt.Sprintf("{\"type\":\"session\",\"version\":3,\"id\":%q,\"timestamp\":\"2026-08-27T12:00:00Z\",\"cwd\":%q}\n{\"type\":\"custom\",\"id\":\"71000001\",\"parentId\":null,\"timestamp\":\"2026-08-27T12:00:01Z\",\"customType\":\"otto.runtime\",\"data\":{\"profile\":\"test\",\"provider\":\"openai-compatible\",\"model\":\"test-model\"}}\n{\"type\":\"message\",\"id\":\"bad-json\n", id, workspace)
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

func cliSessionDirectory(t *testing.T, root, workspace string) string {
	t.Helper()
	store, err := session.Create(root, session.Header{Version: session.CurrentVersion, ID: "directory-probe", Workspace: workspace, Provider: "openai-compatible", Profile: "test", Model: "test-model", CreatedAt: time.Now().UTC()})
	if err != nil {
		t.Fatal(err)
	}
	directory := filepath.Dir(store.Path())
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(store.Path()); err != nil {
		t.Fatal(err)
	}
	return directory
}

func setCLISessionMTime(t *testing.T, path string, modified time.Time) {
	t.Helper()
	if err := os.Chtimes(path, modified, modified); err != nil {
		t.Fatal(err)
	}
}

func writeSSE(w http.ResponseWriter, chunk string) {
	w.Header().Set("Content-Type", "text/event-stream")
	_, _ = fmt.Fprintf(w, "data: %s\n\ndata: [DONE]\n\n", chunk)
}
