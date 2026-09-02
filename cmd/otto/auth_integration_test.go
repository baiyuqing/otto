package main

import (
	"bytes"
	"context"
	"errors"
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/auth"
)

func TestRunREPLAuthCommandsUseCapturedAuthPath(t *testing.T) {
	capturedHome := t.TempDir()
	liveHome := t.TempDir()
	t.Setenv("HOME", liveHome)
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	capturedPath := auth.PathForHome(capturedHome)
	if err := (auth.Credentials{AccountID: "acct-captured"}).Save(capturedPath); err != nil {
		t.Fatal(err)
	}
	if err := (auth.Credentials{AccountID: "acct-live"}).Save(auth.PathForHome(liveHome)); err != nil {
		t.Fatal(err)
	}

	deps := deterministicRunDependencies(t)
	deps.openSandbox = func(context.Context, sandboxOpenOptions) sandboxRuntime {
		return fakeSandboxRuntime(app.SandboxInfo{Mode: app.SandboxSeatbelt, BashAvailable: true}, &recordingSandboxExecutor{}, []string{})
	}
	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(),
		[]string{"--config", configPath, "--cwd", workspace},
		strings.NewReader("/login status\n/logout\n/exit\n"), &stdout, &stderr,
		testEnviron(map[string]string{"HOME": capturedHome, "SHELL": "/bin/sh", "TEST_KEY": "secret"}),
		deps)
	if code != 0 {
		t.Fatalf("code = %d, stdout = %q, stderr = %q", code, stdout.String(), stderr.String())
	}
	if got := stdout.String(); !strings.Contains(got, "Signed in to ChatGPT") || !strings.Contains(got, "Signed out of ChatGPT.") {
		t.Fatalf("stdout = %q", got)
	}
	if _, err := auth.Load(capturedPath); !errors.Is(err, auth.ErrNoCredentials) {
		t.Fatalf("captured credentials still present: %v", err)
	}
	if _, err := auth.Load(auth.PathForHome(liveHome)); err != nil {
		t.Fatalf("live-home credentials were touched: %v", err)
	}
}

func TestRunTypicalOAuthCredentialsKeepSandboxAndSessionPersistenceEnabled(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	credentials := auth.Credentials{
		AccessToken:  strings.Repeat("a", 1708),
		RefreshToken: strings.Repeat("r", 196),
		IDToken:      strings.Repeat("i", 1774),
		AccountID:    strings.Repeat("c", 36),
	}
	if err := credentials.Save(auth.PathForHome(home)); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(filepath.Join(home, ".otto", "sessions"), 0o700); err != nil {
		t.Fatal(err)
	}
	configPath := writeCLIConfig(t, "chatgpt", "", "")
	deps := deterministicRunDependencies(t)
	deps.detectTerminal = func(io.Reader, io.Writer) bool { return true }
	deps.runTUI = func(ctx context.Context, _ io.Reader, _ io.Writer, backend app.Backend) error {
		info := backend.Info()
		if !info.Sandbox.BashAvailable || info.Sandbox.Mode != app.SandboxSeatbelt {
			return errors.New("default startup disabled the sandbox")
		}
		browser, ok := backend.(app.SessionBrowser)
		if !ok {
			return errors.New("backend omitted SessionBrowser")
		}
		if _, err := browser.ListSessions(ctx, 20); err != nil {
			return err
		}
		if err := backend.NewSession(); err != nil {
			return err
		}
		return nil
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(),
		[]string{"--config", configPath, "--cwd", workspace, "--ui", "tui"},
		strings.NewReader(""), &stdout, &stderr,
		testEnviron(map[string]string{"HOME": home, "SHELL": "/bin/sh"}), deps)
	if code != 0 {
		t.Fatalf("code = %d, stdout = %q, stderr = %q", code, stdout.String(), stderr.String())
	}
	if strings.Contains(stdout.String(), app.ErrPersistenceDisabled.Error()) || strings.Contains(stderr.String(), app.ErrPersistenceDisabled.Error()) {
		t.Fatalf("startup reported disabled session persistence: stdout=%q stderr=%q", stdout.String(), stderr.String())
	}
}

func TestRunSuppressedREPLAuthCommandsStayUnavailable(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", "http://127.0.0.1:1")
	path := auth.PathForHome(home)
	if err := (auth.Credentials{AccountID: "acct-captured"}).Save(path); err != nil {
		t.Fatal(err)
	}
	deps := deterministicRunDependencies(t)
	deps.openSandbox = func(context.Context, sandboxOpenOptions) sandboxRuntime {
		runtime := fakeSandboxRuntime(app.SandboxInfo{Mode: app.SandboxSeatbelt, BashAvailable: true}, &recordingSandboxExecutor{}, []string{})
		runtime.RedactionsComplete = false
		return runtime
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(),
		[]string{"--config", configPath, "--cwd", workspace},
		strings.NewReader("/login status\n/logout\n/exit\n"), &stdout, &stderr,
		testEnviron(map[string]string{"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret"}), deps)
	if code != 0 {
		t.Fatalf("code = %d, stdout = %q, stderr = %q", code, stdout.String(), stderr.String())
	}
	if got := stdout.String(); strings.Count(got, auth.ErrInteractiveUnavailable.Error()) != 2 {
		t.Fatalf("stdout = %q, want two unavailable notices", got)
	}
	if _, err := auth.Load(path); err != nil {
		t.Fatalf("suppressed auth commands mutated credentials: %v", err)
	}
}

func TestRunArchiveRedactsSecretFromSuccessPath(t *testing.T) {
	const secret = "archive-path-secret"
	base := t.TempDir()
	home := filepath.Join(base, secret+"-home")
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	workspace := t.TempDir()
	root := filepath.Join(home, ".otto", "sessions")
	path := createCLISession(t, root, workspace, "archive-me")

	var stdout, stderr bytes.Buffer
	code := run(context.Background(), []string{"--archive", path, "--cwd", workspace}, strings.NewReader(""), &stdout, &stderr, testEnviron(map[string]string{"HOME": home, "OTTO_API_KEY": secret}))
	if code != 0 {
		t.Fatalf("code = %d, stdout = %q, stderr = %q", code, stdout.String(), stderr.String())
	}
	if strings.Contains(stdout.String(), secret) {
		t.Fatalf("stdout leaked secret archive path: %q", stdout.String())
	}
}

func TestRunArchiveFailureUsesSafeDiagnostics(t *testing.T) {
	const secret = "archive-path-secret"
	base := t.TempDir()
	home := filepath.Join(base, secret+"-home")
	if err := os.MkdirAll(home, 0o700); err != nil {
		t.Fatal(err)
	}
	workspace := t.TempDir()
	root := filepath.Join(home, ".otto", "sessions")
	path := writeOldOttoV1Session(t, root, workspace, "old")

	var stdout, stderr bytes.Buffer
	code := run(context.Background(), []string{"--archive", path, "--cwd", workspace}, strings.NewReader(""), &stdout, &stderr, testEnviron(map[string]string{"HOME": home, "OTTO_API_KEY": secret}))
	if code == 0 {
		t.Fatalf("code = 0, want failure; stdout = %q", stdout.String())
	}
	if got := stderr.String(); strings.Contains(got, secret) || strings.Contains(got, path) {
		t.Fatalf("stderr leaked archive detail: %q", got)
	}
}

var _ = io.Discard
