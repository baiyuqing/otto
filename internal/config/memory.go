package config

import (
	"fmt"
	"os"
	"path/filepath"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
)

const (
	defaultMemoryBackend      = "sqlite"
	defaultMemoryRecallTokens = 2000
	defaultMemoryMaxResults   = 12
)

type Memory struct {
	Enabled           *bool             `toml:"enabled"`
	Backend           string            `toml:"backend"`
	Required          bool              `toml:"required"`
	RecallTokens      *int              `toml:"recall_tokens"`
	MaxResults        *int              `toml:"max_results"`
	RequireEncryption bool              `toml:"require_encryption"`
	WorkspaceIDs      map[string]string `toml:"workspace_ids"`
	SQLite            MemorySQLite      `toml:"sqlite"`
}

type MemorySQLite struct {
	Path        string `toml:"path"`
	BusyTimeout string `toml:"busy_timeout"`
}

type MemoryRuntime struct {
	Enabled           bool
	Backend           string
	Required          bool
	RecallTokens      int
	MaxResults        int
	RequireEncryption bool
	WorkspaceIDs      map[string]string
	SQLitePath        string
	SQLiteBusyTimeout time.Duration
}

// homeFromEnv prefers the injected HOME (so callers stay hermetic under
// test), falling back to the real process environment only when the caller
// did not supply one.
func homeFromEnv(env map[string]string) string {
	if home := env["HOME"]; home != "" {
		return home
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	return home
}

func ResolveMemory(file File, env map[string]string, overrides Overrides) (MemoryRuntime, error) {
	enabled := true
	if file.Memory.Enabled != nil {
		enabled = *file.Memory.Enabled
	}

	backend := defaultMemoryBackend
	if file.Memory.Backend != "" {
		backend = file.Memory.Backend
	}
	if backend != "sqlite" {
		return MemoryRuntime{}, fmt.Errorf("unsupported memory backend %q: must be sqlite", backend)
	}

	recallTokens := defaultMemoryRecallTokens
	if file.Memory.RecallTokens != nil {
		if *file.Memory.RecallTokens <= 0 {
			return MemoryRuntime{}, fmt.Errorf("invalid memory recall_tokens: must be greater than zero")
		}
		if *file.Memory.RecallTokens > memory.MaxTokenBudget {
			return MemoryRuntime{}, fmt.Errorf("invalid memory recall_tokens: must be at most %d", memory.MaxTokenBudget)
		}
		recallTokens = *file.Memory.RecallTokens
	}

	maxResults := defaultMemoryMaxResults
	if file.Memory.MaxResults != nil {
		if *file.Memory.MaxResults <= 0 {
			return MemoryRuntime{}, fmt.Errorf("invalid memory max_results: must be greater than zero")
		}
		if *file.Memory.MaxResults > memory.MaxRecallRecords {
			return MemoryRuntime{}, fmt.Errorf("invalid memory max_results: must be at most %d", memory.MaxRecallRecords)
		}
		maxResults = *file.Memory.MaxResults
	}

	sqlitePath := file.Memory.SQLite.Path
	if sqlitePath == "" {
		home := homeFromEnv(env)
		if home == "" {
			return MemoryRuntime{}, fmt.Errorf("resolve home directory for default memory sqlite path: $HOME is not defined")
		}
		sqlitePath = filepath.Join(home, ".otto", "memory", "memory.db")
	}

	var busyTimeout time.Duration
	if file.Memory.SQLite.BusyTimeout != "" {
		duration, err := time.ParseDuration(file.Memory.SQLite.BusyTimeout)
		if err != nil {
			return MemoryRuntime{}, fmt.Errorf("invalid memory sqlite busy_timeout: %w", err)
		}
		if duration <= 0 {
			return MemoryRuntime{}, fmt.Errorf("invalid memory sqlite busy_timeout: must be greater than zero")
		}
		busyTimeout = duration
	}

	return MemoryRuntime{
		Enabled:           enabled,
		Backend:           backend,
		Required:          file.Memory.Required,
		RecallTokens:      recallTokens,
		MaxResults:        maxResults,
		RequireEncryption: file.Memory.RequireEncryption,
		WorkspaceIDs:      file.Memory.WorkspaceIDs,
		SQLitePath:        sqlitePath,
		SQLiteBusyTimeout: busyTimeout,
	}, nil
}
