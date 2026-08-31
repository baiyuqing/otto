package main

import (
	"context"
	"fmt"
	"io"

	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/memory"
	"github.com/baiyuqing/otto/internal/memory/sqlite"
)

// openMemoryService builds the process-wide memory.Service from resolved
// config. Disabled config, or an Open failure when Required is false,
// degrades to a NullService with a stderr warning instead of failing
// startup. usable reports whether the returned service is backed by a live
// store (and so worth exposing to the agent), as opposed to a NullService
// that always reports memory as disabled or unavailable. secretValues are
// exact-matched against remembered content (via memory.NewExactGuard) so
// configured provider credentials can't be written into memory.
func openMemoryService(ctx context.Context, cfg config.MemoryRuntime, secretValues []string, stderr io.Writer) (service memory.Service, userScope memory.Scope, usable bool, err error) {
	if !cfg.Enabled {
		return memory.NewNullService(memory.ErrDisabled), memory.Scope{}, false, nil
	}

	exactGuard, err := memory.NewExactGuard(secretValues)
	if err != nil {
		return nil, memory.Scope{}, false, fmt.Errorf("build memory secret guard: %w", err)
	}

	options := sqlite.Options{
		BusyTimeout: cfg.SQLiteBusyTimeout,
		NewID:       memory.NewID,
		Guard:       memory.NewCompositeGuard(memory.DefaultGuard{}, exactGuard),
	}
	components, openErr := sqlite.NewFactory(cfg.SQLitePath, options).Open(ctx)
	if openErr != nil {
		if cfg.Required {
			return nil, memory.Scope{}, false, fmt.Errorf("open memory store: %w", openErr)
		}
		_, _ = fmt.Fprintf(stderr, "warning: memory store unavailable, continuing without memory: %v\n", openErr)
		return memory.NewNullService(openErr), memory.Scope{}, false, nil
	}

	// require_encryption is a security requirement, not an availability
	// preference: fail startup outright rather than degrading to an
	// unencrypted store, regardless of cfg.Required.
	if cfg.RequireEncryption && !components.Capabilities.EncryptionAtRest {
		_ = components.Store.Close()
		return nil, memory.Scope{}, false, fmt.Errorf("memory backend %q does not support encryption at rest, but require_encryption is set", cfg.Backend)
	}

	identity, identityErr := components.Store.Identity(ctx)
	if identityErr != nil {
		_ = components.Store.Close()
		if cfg.Required {
			return nil, memory.Scope{}, false, fmt.Errorf("read memory store identity: %w", identityErr)
		}
		_, _ = fmt.Fprintf(stderr, "warning: memory store unavailable, continuing without memory: %v\n", identityErr)
		return memory.NewNullService(identityErr), memory.Scope{}, false, nil
	}

	service, err = memory.NewService(components.Store, components.Retriever, memory.DefaultPolicy{})
	if err != nil {
		_ = components.Store.Close()
		return nil, memory.Scope{}, false, fmt.Errorf("construct memory service: %w", err)
	}
	return service, identity.UserScope, true, nil
}

// workspaceMemoryScope resolves the workspace scope for canonicalPath,
// preferring a configured stable override so a moved workspace path can
// keep its records.
func workspaceMemoryScope(cfg config.MemoryRuntime, canonicalPath string) (memory.Scope, error) {
	return memory.NewWorkspaceScope(canonicalPath, cfg.WorkspaceIDs[canonicalPath])
}
