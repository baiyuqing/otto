package main

import (
	"bytes"
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/memory"
)

// unopenableMemoryPath returns a path whose parent is a regular file, so any
// attempt to open a SQLite database there fails deterministically.
func unopenableMemoryPath(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	blocker := filepath.Join(dir, "blocker-file")
	if err := os.WriteFile(blocker, []byte("not a directory"), 0o600); err != nil {
		t.Fatal(err)
	}
	return filepath.Join(blocker, "memory.db")
}

func TestOpenMemoryServiceDisabledReturnsUnusableNullService(t *testing.T) {
	var stderr bytes.Buffer
	service, scope, usable, err := openMemoryService(context.Background(), config.MemoryRuntime{Enabled: false}, nil, &stderr)
	if err != nil {
		t.Fatalf("openMemoryService() error = %v, want nil", err)
	}
	defer service.Close()
	if usable {
		t.Fatalf("usable = true, want false for disabled memory")
	}
	if scope != (memory.Scope{}) {
		t.Fatalf("scope = %+v, want zero value", scope)
	}
	if _, err := service.Search(context.Background(), memory.SearchRequest{}); !errors.Is(err, memory.ErrDisabled) {
		t.Fatalf("Search() error = %v, want ErrDisabled", err)
	}
	if stderr.Len() != 0 {
		t.Fatalf("stderr = %q, want empty", stderr.String())
	}
}

func TestOpenMemoryServiceOpenFailureNotRequiredDegradesToNull(t *testing.T) {
	unwritable := unopenableMemoryPath(t)
	var stderr bytes.Buffer
	cfg := config.MemoryRuntime{Enabled: true, Backend: "sqlite", Required: false, SQLitePath: unwritable}
	service, scope, usable, err := openMemoryService(context.Background(), cfg, nil, &stderr)
	if err != nil {
		t.Fatalf("openMemoryService() error = %v, want nil (should degrade)", err)
	}
	defer service.Close()
	if usable {
		t.Fatalf("usable = true, want false when open failed")
	}
	if scope != (memory.Scope{}) {
		t.Fatalf("scope = %+v, want zero value", scope)
	}
	if _, err := service.Search(context.Background(), memory.SearchRequest{}); !errors.Is(err, memory.ErrUnavailable) {
		t.Fatalf("Search() error = %v, want ErrUnavailable", err)
	}
	if !strings.Contains(stderr.String(), "warning") {
		t.Fatalf("stderr = %q, want a warning about the degraded store", stderr.String())
	}
}

func TestOpenMemoryServiceOpenFailureRequiredReturnsError(t *testing.T) {
	unwritable := unopenableMemoryPath(t)
	var stderr bytes.Buffer
	cfg := config.MemoryRuntime{Enabled: true, Backend: "sqlite", Required: true, SQLitePath: unwritable}
	service, _, usable, err := openMemoryService(context.Background(), cfg, nil, &stderr)
	if err == nil {
		t.Fatalf("openMemoryService() error = nil, want an error when required and open fails")
	}
	if service != nil {
		t.Fatalf("service = %v, want nil on required failure", service)
	}
	if usable {
		t.Fatalf("usable = true, want false")
	}
}

func TestOpenMemoryServiceSuccessReturnsStableUserScope(t *testing.T) {
	// The "memory" directory is deliberately left for the store to create:
	// t.TempDir() itself is mode 0755 (Go's testing package creates it via
	// os.Mkdir(dir, 0777)), but the store rejects a group/other-accessible
	// parent directory, so the DB must live one level below in a directory
	// the store creates itself at 0700.
	path := filepath.Join(t.TempDir(), "memory", "memory.db")
	cfg := config.MemoryRuntime{Enabled: true, Backend: "sqlite", SQLitePath: path}

	service, scope, usable, err := openMemoryService(context.Background(), cfg, nil, &bytes.Buffer{})
	if err != nil {
		t.Fatalf("openMemoryService() error = %v, want nil", err)
	}
	if !usable {
		t.Fatalf("usable = false, want true")
	}
	if scope.Namespace != memory.NamespaceUser || scope.ID == "" {
		t.Fatalf("scope = %+v, want a populated user scope", scope)
	}
	if err := service.Close(); err != nil {
		t.Fatalf("Close() error = %v", err)
	}

	// Reopening the same database must yield the same installation-local ID.
	reopened, scope2, usable2, err := openMemoryService(context.Background(), cfg, nil, &bytes.Buffer{})
	if err != nil {
		t.Fatalf("openMemoryService() (reopen) error = %v, want nil", err)
	}
	defer reopened.Close()
	if !usable2 {
		t.Fatalf("usable (reopen) = false, want true")
	}
	if scope2 != scope {
		t.Fatalf("scope (reopen) = %+v, want %+v", scope2, scope)
	}
}

func TestOpenMemoryServiceRequireEncryptionFailsWhenBackendCannotSatisfyIt(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory", "memory.db")
	cfg := config.MemoryRuntime{Enabled: true, Backend: "sqlite", RequireEncryption: true, SQLitePath: path}

	var stderr bytes.Buffer
	service, _, usable, err := openMemoryService(context.Background(), cfg, nil, &stderr)
	if err == nil {
		t.Fatalf("openMemoryService() error = nil, want an error: sqlite does not advertise EncryptionAtRest")
	}
	if !strings.Contains(err.Error(), "encrypt") {
		t.Fatalf("error = %v, want it to mention encryption", err)
	}
	if service != nil {
		t.Fatalf("service = %v, want nil on encryption-requirement failure", service)
	}
	if usable {
		t.Fatalf("usable = true, want false")
	}
}

func TestOpenMemoryServiceRejectsSecretValuesInRememberedText(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory", "memory.db")
	cfg := config.MemoryRuntime{Enabled: true, Backend: "sqlite", SQLitePath: path}

	service, scope, usable, err := openMemoryService(context.Background(), cfg, []string{"sk-configured-secret"}, &bytes.Buffer{})
	if err != nil {
		t.Fatalf("openMemoryService() error = %v, want nil", err)
	}
	defer service.Close()
	if !usable {
		t.Fatalf("usable = false, want true")
	}

	_, err = service.Remember(context.Background(), memory.RememberRequest{
		Scope: scope,
		Kind:  "preference",
		Text:  "the api key is sk-configured-secret",
	})
	if !errors.Is(err, memory.ErrSensitiveMemory) {
		t.Fatalf("Remember() error = %v, want ErrSensitiveMemory", err)
	}
}

func TestOpenMemoryServiceInvalidSecretValueFailsToOpen(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory", "memory.db")
	cfg := config.MemoryRuntime{Enabled: true, Backend: "sqlite", SQLitePath: path}

	service, _, usable, err := openMemoryService(context.Background(), cfg, []string{""}, &bytes.Buffer{})
	if err == nil {
		t.Fatalf("openMemoryService() error = nil, want an error for an invalid secret value")
	}
	if service != nil {
		t.Fatalf("service = %v, want nil", service)
	}
	if usable {
		t.Fatalf("usable = true, want false")
	}
}

func TestWorkspaceMemoryScopeUsesConfiguredOverride(t *testing.T) {
	cfg := config.MemoryRuntime{WorkspaceIDs: map[string]string{"/work/otto": "custom-id"}}
	scope, err := workspaceMemoryScope(cfg, "/work/otto")
	if err != nil {
		t.Fatalf("workspaceMemoryScope() error = %v, want nil", err)
	}
	if scope.Namespace != memory.NamespaceWorkspace || scope.ID != "custom-id" {
		t.Fatalf("scope = %+v, want workspace/custom-id", scope)
	}
}

func TestWorkspaceMemoryScopeDerivesFromPathWithoutOverride(t *testing.T) {
	dir := t.TempDir()
	cfg := config.MemoryRuntime{}
	scope, err := workspaceMemoryScope(cfg, dir)
	if err != nil {
		t.Fatalf("workspaceMemoryScope() error = %v, want nil", err)
	}
	if scope.Namespace != memory.NamespaceWorkspace || !strings.HasPrefix(scope.ID, "sha256:") {
		t.Fatalf("scope = %+v, want a derived workspace/sha256:... scope", scope)
	}
}
