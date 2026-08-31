package config

import (
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
)

func TestResolveMemoryDefaults(t *testing.T) {
	home := filepath.Join(t.TempDir(), "home")

	runtime, err := ResolveMemory(File{}, map[string]string{"HOME": home}, Overrides{})
	if err != nil {
		t.Fatal(err)
	}
	want := MemoryRuntime{
		Enabled:      true,
		Backend:      "sqlite",
		RecallTokens: 2000,
		MaxResults:   12,
		SQLitePath:   filepath.Join(home, ".otto", "memory", "memory.db"),
	}
	if runtime.Enabled != want.Enabled || runtime.Backend != want.Backend || runtime.RecallTokens != want.RecallTokens ||
		runtime.MaxResults != want.MaxResults || runtime.SQLitePath != want.SQLitePath ||
		runtime.SQLiteBusyTimeout != want.SQLiteBusyTimeout || runtime.WorkspaceIDs != nil {
		t.Fatalf("ResolveMemory() = %+v, want %+v", runtime, want)
	}
}

// TestResolveMemoryDefaultPathUsesInjectedHomeNotProcessEnv guards against
// ResolveMemory reaching past its env parameter to the real process
// environment (via os.UserHomeDir) for the default SQLite path, which would
// make it non-hermetic under test harnesses that inject HOME only through a
// map (as cmd/otto's tests do) rather than via t.Setenv.
func TestResolveMemoryDefaultPathUsesInjectedHomeNotProcessEnv(t *testing.T) {
	realHome := filepath.Join(t.TempDir(), "real-home")
	t.Setenv("HOME", realHome)
	injectedHome := filepath.Join(t.TempDir(), "injected-home")

	runtime, err := ResolveMemory(File{}, map[string]string{"HOME": injectedHome}, Overrides{})
	if err != nil {
		t.Fatal(err)
	}
	want := filepath.Join(injectedHome, ".otto", "memory", "memory.db")
	if runtime.SQLitePath != want {
		t.Fatalf("SQLitePath = %q, want %q (from injected env, not real process HOME %q)", runtime.SQLitePath, want, realHome)
	}
}

func TestResolveMemoryRejectsUnresolvableHome(t *testing.T) {
	t.Setenv("HOME", "")

	_, err := ResolveMemory(File{}, map[string]string{"HOME": ""}, Overrides{})
	if err == nil || !strings.Contains(err.Error(), "home") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestResolveMemoryAppliesTOMLOverrides(t *testing.T) {
	file := File{Memory: Memory{
		Enabled:           boolPointer(false),
		Backend:           "sqlite",
		Required:          true,
		RecallTokens:      intPointer(500),
		MaxResults:        intPointer(3),
		RequireEncryption: true,
		WorkspaceIDs:      map[string]string{"/old/path": "stable-id"},
		SQLite: MemorySQLite{
			Path:        "/custom/memory.db",
			BusyTimeout: "10s",
		},
	}}

	runtime, err := ResolveMemory(file, nil, Overrides{})
	if err != nil {
		t.Fatal(err)
	}
	want := MemoryRuntime{
		Enabled:           false,
		Backend:           "sqlite",
		Required:          true,
		RecallTokens:      500,
		MaxResults:        3,
		RequireEncryption: true,
		WorkspaceIDs:      map[string]string{"/old/path": "stable-id"},
		SQLitePath:        "/custom/memory.db",
		SQLiteBusyTimeout: 10 * time.Second,
	}
	if runtime.Enabled != want.Enabled || runtime.Backend != want.Backend || runtime.Required != want.Required ||
		runtime.RecallTokens != want.RecallTokens || runtime.MaxResults != want.MaxResults ||
		runtime.RequireEncryption != want.RequireEncryption || runtime.SQLitePath != want.SQLitePath ||
		runtime.SQLiteBusyTimeout != want.SQLiteBusyTimeout || runtime.WorkspaceIDs["/old/path"] != "stable-id" {
		t.Fatalf("ResolveMemory() = %+v, want %+v", runtime, want)
	}
}

func TestResolveMemoryRejectsUnsupportedBackend(t *testing.T) {
	_, err := ResolveMemory(File{Memory: Memory{Backend: "postgres"}}, nil, Overrides{})
	if err == nil || !strings.Contains(err.Error(), "postgres") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestResolveMemoryRejectsNonPositiveRecallTokens(t *testing.T) {
	_, err := ResolveMemory(File{Memory: Memory{RecallTokens: intPointer(0)}}, nil, Overrides{})
	if err == nil || !strings.Contains(err.Error(), "recall_tokens") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestResolveMemoryRejectsNonPositiveMaxResults(t *testing.T) {
	_, err := ResolveMemory(File{Memory: Memory{MaxResults: intPointer(-1)}}, nil, Overrides{})
	if err == nil || !strings.Contains(err.Error(), "max_results") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestResolveMemoryRejectsRecallTokensAboveCeiling(t *testing.T) {
	_, err := ResolveMemory(File{Memory: Memory{RecallTokens: intPointer(memory.MaxTokenBudget + 1)}}, nil, Overrides{})
	if err == nil || !strings.Contains(err.Error(), "recall_tokens") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestResolveMemoryRejectsMaxResultsAboveCeiling(t *testing.T) {
	_, err := ResolveMemory(File{Memory: Memory{MaxResults: intPointer(memory.MaxRecallRecords + 1)}}, nil, Overrides{})
	if err == nil || !strings.Contains(err.Error(), "max_results") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestResolveMemoryRejectsInvalidBusyTimeout(t *testing.T) {
	_, err := ResolveMemory(File{Memory: Memory{SQLite: MemorySQLite{BusyTimeout: "not-a-duration"}}}, nil, Overrides{})
	if err == nil || !strings.Contains(err.Error(), "busy_timeout") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestResolveMemoryRejectsNonPositiveBusyTimeout(t *testing.T) {
	_, err := ResolveMemory(File{Memory: Memory{SQLite: MemorySQLite{BusyTimeout: "0s"}}}, nil, Overrides{})
	if err == nil || !strings.Contains(err.Error(), "busy_timeout") {
		t.Fatalf("unexpected error: %v", err)
	}
}
