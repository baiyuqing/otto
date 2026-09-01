package main

import (
	"bytes"
	"context"
	"errors"
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

func TestRunMemoryCommandConfigAndResolveErrorsUseFixedDiagnostics(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	missingConfig := filepath.Join(t.TempDir(), "config-secret", "missing.toml")
	invalidConfig := filepath.Join(t.TempDir(), "invalid-memory.toml")
	if err := os.WriteFile(invalidConfig, []byte("[memory]\n[memory.sqlite]\nbusy_timeout = \"busy-timeout-secret\"\n"), 0o600); err != nil {
		t.Fatal(err)
	}

	for _, test := range []struct {
		name  string
		args  []string
		want  string
		avoid []string
	}{
		{
			name:  "load config",
			args:  []string{"memory", "status", "--config", missingConfig, "--cwd", workspace},
			want:  "otto: load config: configuration is invalid or unavailable\n",
			avoid: []string{missingConfig, "config-secret"},
		},
		{
			name:  "resolve memory",
			args:  []string{"memory", "status", "--config", invalidConfig, "--cwd", workspace},
			want:  "otto: memory configuration is invalid or unavailable\n",
			avoid: []string{"busy-timeout-secret", invalidConfig},
		},
	} {
		t.Run(test.name, func(t *testing.T) {
			var stdout, stderr bytes.Buffer
			code := runWithDependencies(context.Background(), test.args,
				strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{"HOME": home}), defaultRunDependencies())
			if code != 1 || stderr.String() != test.want {
				t.Fatalf("code/stderr = %d/%q, want 1/%q", code, stderr.String(), test.want)
			}
			for _, avoid := range test.avoid {
				if strings.Contains(stderr.String(), avoid) {
					t.Fatalf("stderr leaked %q: %q", avoid, stderr.String())
				}
			}
		})
	}
}

func TestRunMemoryCommandCWDErrorUsesFixedDiagnostics(t *testing.T) {
	home := t.TempDir()
	configPath := writeMemoryConfig(t, filepath.Join(t.TempDir(), "memory", "memory.db"))
	cwd := filepath.Join(t.TempDir(), "cwd-secret-does-not-exist")

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(),
		[]string{"memory", "forget", "rec-1", "--config", configPath, "--cwd", cwd},
		strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{"HOME": home}), defaultRunDependencies())
	if code != 1 || stderr.String() != "otto: resolve cwd: working directory is invalid or unavailable\n" {
		t.Fatalf("code/stderr = %d/%q", code, stderr.String())
	}
	if strings.Contains(stderr.String(), cwd) || strings.Contains(stderr.String(), "cwd-secret") {
		t.Fatalf("stderr leaked cwd detail: %q", stderr.String())
	}
}

func TestRunMemoryCommandStatusRedactsConfiguredPathAndWarning(t *testing.T) {
	const accountID = "acct-secret"
	home := t.TempDir()
	workspace := t.TempDir()
	dbPath := filepath.Join(t.TempDir(), accountID, "memory.db")
	configPath := writeMemoryConfig(t, dbPath)
	if err := (auth.Credentials{AccessToken: "access-token", RefreshToken: "refresh-token", AccountID: accountID}).Save(auth.PathForHome(home)); err != nil {
		t.Fatal(err)
	}
	original := memoryOpenService
	memoryOpenService = func(_ context.Context, _ config.MemoryRuntime, _ []string, warning io.Writer) (memory.Service, memory.Scope, bool, error) {
		_, _ = io.WriteString(warning, "warning: memory store unavailable, continuing without memory: "+accountID+"\n")
		return memory.NewNullService(memory.ErrUnavailable), memory.Scope{}, false, nil
	}
	defer func() { memoryOpenService = original }()

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(),
		[]string{"memory", "status", "--config", configPath, "--cwd", workspace},
		strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{"HOME": home}), defaultRunDependencies())
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	out := stdout.String()
	for _, forbidden := range []string{accountID, dbPath} {
		if strings.Contains(out, forbidden) {
			t.Fatalf("stdout leaked %q: %q", forbidden, out)
		}
	}
	for _, want := range []string{"enabled: true", "backend: sqlite", "path: ", "memory.db", "usable: false", "warning: memory store unavailable, continuing without memory"} {
		if !strings.Contains(out, want) {
			t.Fatalf("stdout = %q, want %q", out, want)
		}
	}
}

func TestRunMemoryCommandOpenErrorUsesFixedDiagnostics(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	configPath := writeMemoryConfig(t, filepath.Join(t.TempDir(), "memory", "memory.db"))
	original := memoryOpenService
	memoryOpenService = func(context.Context, config.MemoryRuntime, []string, io.Writer) (memory.Service, memory.Scope, bool, error) {
		return nil, memory.Scope{}, false, errors.New("memory-open-secret")
	}
	defer func() { memoryOpenService = original }()

	var stdout, stderr bytes.Buffer
	code := runWithDependencies(context.Background(),
		[]string{"memory", "status", "--config", configPath, "--cwd", workspace},
		strings.NewReader(""), &stdout, &stderr, testGetenv(map[string]string{"HOME": home}), defaultRunDependencies())
	if code != 1 || stderr.String() != "otto: memory backend is unavailable\n" {
		t.Fatalf("code/stderr = %d/%q", code, stderr.String())
	}
	if strings.Contains(stderr.String(), "memory-open-secret") {
		t.Fatalf("stderr leaked backend detail: %q", stderr.String())
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
