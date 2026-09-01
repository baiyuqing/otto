package main

import (
	"bytes"
	"context"
	"io"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/auth"
	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/memory"
)

func writeMemoryConfig(t *testing.T, dbPath string) string {
	t.Helper()
	configPath := filepath.Join(t.TempDir(), "otto.toml")
	content := "[memory]\nenabled = true\n[memory.sqlite]\npath = " + `"` + dbPath + `"` + "\n"
	if err := os.WriteFile(configPath, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	return configPath
}

func TestRunMemoryCommandStatusReportsConfiguredMemory(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	dbPath := filepath.Join(t.TempDir(), "memory", "memory.db")
	configPath := writeMemoryConfig(t, dbPath)

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(),
		[]string{"memory", "status", "--config", configPath, "--cwd", workspace},
		strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{"HOME": home}), defaultRunDependencies())
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	out := stdout.String()
	for _, want := range []string{"enabled: true", "backend: sqlite", dbPath, "usable: true"} {
		if !strings.Contains(out, want) {
			t.Fatalf("stdout = %q, want it to contain %q", out, want)
		}
	}
}

func TestRunMemoryCommandForgetRemovesRecordFromWorkspaceScope(t *testing.T) {
	home := t.TempDir()
	workspacePath := mustCanonicalDirectory(t, t.TempDir())
	dbPath := filepath.Join(t.TempDir(), "memory", "memory.db")
	configPath := writeMemoryConfig(t, dbPath)

	memoryCfg := config.MemoryRuntime{Enabled: true, Backend: "sqlite", SQLitePath: dbPath}
	service, _, usable, err := openMemoryService(context.Background(), memoryCfg, nil, &bytes.Buffer{})
	if err != nil {
		t.Fatalf("openMemoryService() error = %v", err)
	}
	if !usable {
		t.Fatal("openMemoryService() usable = false, want true")
	}
	workspaceScope, err := workspaceMemoryScope(memoryCfg, workspacePath)
	if err != nil {
		t.Fatalf("workspaceMemoryScope() error = %v", err)
	}
	record, err := service.Remember(context.Background(), memory.RememberRequest{
		Scope: workspaceScope, Kind: "preference", Key: "editor", Text: "vim",
	})
	if err != nil {
		t.Fatalf("Remember() error = %v", err)
	}
	if err := service.Close(); err != nil {
		t.Fatalf("Close() error = %v", err)
	}

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(),
		[]string{"memory", "forget", record.ID, "--config", configPath, "--cwd", workspacePath},
		strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{"HOME": home}), defaultRunDependencies())
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if !strings.Contains(stdout.String(), record.ID) {
		t.Fatalf("stdout = %q, want it to mention forgotten record %s", stdout.String(), record.ID)
	}

	reopened, _, usable, err := openMemoryService(context.Background(), memoryCfg, nil, &bytes.Buffer{})
	if err != nil {
		t.Fatalf("openMemoryService() (reopen) error = %v", err)
	}
	defer reopened.Close()
	if !usable {
		t.Fatal("openMemoryService() (reopen) usable = false, want true")
	}
	if _, err := reopened.Get(context.Background(), memory.RecordRef{Scope: workspaceScope, ID: record.ID}); err == nil {
		t.Fatal("Get() after forget succeeded, want an error")
	}
}

func TestRunMemoryCommandForgetMissingIDReturnsUsageError(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	dbPath := filepath.Join(t.TempDir(), "memory", "memory.db")
	configPath := writeMemoryConfig(t, dbPath)

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(),
		[]string{"memory", "forget", "--config", configPath, "--cwd", workspace},
		strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{"HOME": home}), defaultRunDependencies())
	if code == 0 {
		t.Fatalf("code = 0, want a usage error; stdout = %q", stdout.String())
	}
	if !strings.Contains(stderr.String(), "usage") {
		t.Fatalf("stderr = %q, want a usage message", stderr.String())
	}
}

func TestRunMemoryCommandUsesCapturedAuthPathAndDecodedAlias(t *testing.T) {
	capturedHome := t.TempDir()
	liveHome := t.TempDir()
	t.Setenv("HOME", liveHome)
	workspace := t.TempDir()
	dbPath := filepath.Join(t.TempDir(), "memory", "memory.db")
	configPath := writeMemoryConfig(t, dbPath)
	if err := (auth.Credentials{AccessToken: `\u0061\u0062\u0063`, AccountID: "acct-captured"}).Save(auth.PathForHome(capturedHome)); err != nil {
		t.Fatal(err)
	}
	if err := (auth.Credentials{AccessToken: "live-home-token", AccountID: "acct-live"}).Save(auth.PathForHome(liveHome)); err != nil {
		t.Fatal(err)
	}
	original := memoryOpenService
	var captured []string
	memoryOpenService = func(_ context.Context, _ config.MemoryRuntime, secretValues []string, _ io.Writer) (memory.Service, memory.Scope, bool, error) {
		captured = append([]string(nil), secretValues...)
		return memory.NewNullService(memory.ErrDisabled), memory.Scope{}, false, nil
	}
	defer func() { memoryOpenService = original }()

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(),
		[]string{"memory", "status", "--config", configPath, "--cwd", workspace},
		strings.NewReader(""), &stdout, &stderr, testEnviron(map[string]string{"HOME": capturedHome}), defaultRunDependencies())
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	for _, want := range []string{`\u0061\u0062\u0063`, "abc", "acct-captured"} {
		if !slices.Contains(captured, want) {
			t.Fatalf("captured secret values = %#v, want %q", captured, want)
		}
	}
	for _, forbidden := range []string{"live-home-token", "acct-live"} {
		if slices.Contains(captured, forbidden) {
			t.Fatalf("captured secret values = %#v, must not contain live-home value %q", captured, forbidden)
		}
	}
}

func TestRunMemoryCommandRejectsMalformedCapturedAuthBeforeOpeningMemory(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	dbPath := filepath.Join(t.TempDir(), "memory", "memory.db")
	configPath := writeMemoryConfig(t, dbPath)
	path := auth.PathForHome(home)
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(`{"access_token":"unterminated}`), 0o600); err != nil {
		t.Fatal(err)
	}
	original := memoryOpenService
	opened := 0
	memoryOpenService = func(context.Context, config.MemoryRuntime, []string, io.Writer) (memory.Service, memory.Scope, bool, error) {
		opened++
		return nil, memory.Scope{}, false, nil
	}
	defer func() { memoryOpenService = original }()

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(),
		[]string{"memory", "status", "--config", configPath, "--cwd", workspace},
		strings.NewReader(""), &stdout, &stderr, testEnviron(map[string]string{"HOME": home}), defaultRunDependencies())
	if code == 0 {
		t.Fatalf("code = 0, want failure; stdout = %q", stdout.String())
	}
	if opened != 0 {
		t.Fatalf("memory open calls = %d, want 0", opened)
	}
	if got := stderr.String(); strings.Contains(got, path) || strings.Contains(got, "unterminated") {
		t.Fatalf("stderr leaked malformed auth detail: %q", got)
	}
}

func TestRunMemoryCommandUnknownSubcommandFails(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(),
		[]string{"memory", "bogus", "--cwd", workspace},
		strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{"HOME": home}), defaultRunDependencies())
	if code == 0 {
		t.Fatalf("code = 0, want an error; stdout = %q", stdout.String())
	}
	if !strings.Contains(stderr.String(), "bogus") {
		t.Fatalf("stderr = %q, want it to mention the unknown subcommand", stderr.String())
	}
}
