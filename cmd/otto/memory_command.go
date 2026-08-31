package main

import (
	"bytes"
	"context"
	"flag"
	"fmt"
	"io"
	"strings"

	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/memory"
)

// runMemoryCommand handles the standalone "otto memory status|forget <id>"
// CLI, dispatched before the main flag set is parsed. It builds only the
// memory.Service — no provider, session, or controller.
func runMemoryCommand(ctx context.Context, args []string, stdout, stderr io.Writer, getenv func(string) string) int {
	if len(args) == 0 {
		return fail(stderr, "usage: otto memory status|forget <id> [--config PATH] [--cwd PATH]")
	}
	subcommand := args[0]
	rest := args[1:]

	var recordID string
	if subcommand == "forget" {
		if len(rest) == 0 || strings.HasPrefix(rest[0], "-") {
			return fail(stderr, "usage: otto memory forget <id> [--config PATH] [--cwd PATH]")
		}
		recordID = rest[0]
		rest = rest[1:]
	}

	flags := flag.NewFlagSet("otto memory "+subcommand, flag.ContinueOnError)
	flags.SetOutput(stderr)
	configPath := flags.String("config", "", "configuration file")
	cwd := flags.String("cwd", ".", "workspace directory")
	if err := flags.Parse(rest); err != nil {
		return 2
	}

	home, err := resolveHome(getenv)
	if err != nil {
		return fail(stderr, "%v", err)
	}
	configFile, err := loadConfig(cliOptions{configPath: *configPath, explicitConfig: *configPath != ""}, home)
	if err != nil {
		return fail(stderr, "load config: %v", err)
	}
	environment := configEnvironment(configFile, getenv)
	memoryCfg, err := config.ResolveMemory(configFile, environment, config.Overrides{})
	if err != nil {
		return fail(stderr, "%v", err)
	}

	switch subcommand {
	case "status":
		return runMemoryStatus(ctx, memoryCfg, stdout, stderr)
	case "forget":
		workspacePath, err := canonicalDirectory(*cwd)
		if err != nil {
			return fail(stderr, "resolve cwd: %v", err)
		}
		return runMemoryForget(ctx, memoryCfg, workspacePath, recordID, stdout, stderr)
	default:
		return fail(stderr, "unknown memory subcommand %q", subcommand)
	}
}

func runMemoryStatus(ctx context.Context, cfg config.MemoryRuntime, stdout, stderr io.Writer) int {
	var warning bytes.Buffer
	service, _, usable, err := openMemoryService(ctx, cfg, &warning)
	if err != nil {
		return fail(stderr, "%v", err)
	}
	defer service.Close()
	_, _ = fmt.Fprintf(stdout, "enabled: %t\n", cfg.Enabled)
	_, _ = fmt.Fprintf(stdout, "backend: %s\n", cfg.Backend)
	_, _ = fmt.Fprintf(stdout, "path: %s\n", cfg.SQLitePath)
	_, _ = fmt.Fprintf(stdout, "usable: %t\n", usable)
	if warning.Len() > 0 {
		_, _ = io.Copy(stdout, &warning)
	}
	return 0
}

func runMemoryForget(ctx context.Context, cfg config.MemoryRuntime, workspacePath, id string, stdout, stderr io.Writer) int {
	var warning bytes.Buffer
	service, userScope, usable, err := openMemoryService(ctx, cfg, &warning)
	if err != nil {
		return fail(stderr, "%v", err)
	}
	defer service.Close()
	if !usable {
		return fail(stderr, "memory is not usable: %s", strings.TrimSpace(warning.String()))
	}
	workspaceScope, err := workspaceMemoryScope(cfg, workspacePath)
	if err != nil {
		return fail(stderr, "%v", err)
	}

	var lastErr error
	for _, scope := range []memory.Scope{userScope, workspaceScope} {
		ref := memory.RecordRef{Scope: scope, ID: id}
		record, getErr := service.Get(ctx, ref)
		if getErr != nil {
			lastErr = getErr
			continue
		}
		result, forgetErr := service.Forget(ctx, memory.ForgetRequest{Ref: ref, ExpectedRevision: record.Revision})
		if forgetErr != nil {
			return fail(stderr, "%v", forgetErr)
		}
		_, _ = fmt.Fprintf(stdout, "forgot %s (revision %d)\n", result.Tombstone.ID, record.Revision)
		return 0
	}
	return fail(stderr, "record %s not found: %v", id, lastErr)
}
