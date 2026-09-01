package main

import (
	"bytes"
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"strings"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/auth"
	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/memory"
)

var (
	memoryOpenService                = openMemoryService
	memoryWorkspaceScopeFunc         = workspaceMemoryScope
	errMemoryCommandUnavailable      = errors.New("memory command is unavailable")
	errMemoryConfigUnavailable       = errors.New("memory configuration is invalid or unavailable")
	errMemoryBackendUnavailable      = errors.New("memory backend is unavailable")
	errMemoryWorkingDirectoryInvalid = errors.New("working directory is invalid or unavailable")
	errMemoryWorkspaceUnavailable    = errors.New("memory workspace is invalid or unavailable")
)

const memoryStoreUnavailableWarning = "warning: memory store unavailable, continuing without memory"

// runMemoryCommand handles the standalone "otto memory status|forget <id>"
// CLI, dispatched before the main flag set is parsed. It builds only the
// memory.Service — no provider, session, or controller.
func runMemoryCommand(ctx context.Context, args []string, stdout, stderr io.Writer, lookup environmentLookup) int {
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

	home, err := resolveHome(lookup, currentOSUserHome)
	if err != nil {
		return fail(stderr, "%v", err)
	}
	_, configFile, err := loadConfig(cliOptions{configPath: *configPath, explicitConfig: *configPath != ""}, home)
	if err != nil {
		return fail(stderr, "load config: configuration is invalid or unavailable")
	}
	environment := configEnvironment(configFile, lookup)
	environment["HOME"] = home
	memoryCfg, err := config.ResolveMemory(configFile, environment, config.Overrides{})
	if err != nil {
		return fail(stderr, "%v", errMemoryConfigUnavailable)
	}
	capturedAuth := captureAuthCredentials(auth.PathForHome(home))
	collector := runtimeBuilder{
		config:                 configFile,
		environment:            environment,
		sandboxSecretsComplete: capturedAuth.complete,
		authCredentials:        capturedAuth.credentials,
	}
	secretValues, complete := collector.boundarySecretValues(nil)
	if !complete {
		return fail(stderr, "%v", errMemoryCommandUnavailable)
	}

	switch subcommand {
	case "status":
		return runMemoryStatus(ctx, memoryCfg, secretValues, stdout, stderr)
	case "forget":
		workspacePath, err := canonicalDirectory(*cwd)
		if err != nil {
			return fail(stderr, "resolve cwd: %v", errMemoryWorkingDirectoryInvalid)
		}
		return runMemoryForget(ctx, memoryCfg, secretValues, workspacePath, recordID, stdout, stderr)
	default:
		return fail(stderr, "unknown memory subcommand %q", subcommand)
	}
}

func runMemoryStatus(ctx context.Context, cfg config.MemoryRuntime, secretValues []string, stdout, stderr io.Writer) int {
	var warning bytes.Buffer
	service, _, usable, err := memoryOpenService(ctx, cfg, secretValues, &warning)
	if err != nil {
		return fail(stderr, "%v", errMemoryBackendUnavailable)
	}
	if service != nil {
		defer service.Close()
	}
	redactor := agent.NewRedactorWithCompleteness(secretValues, true)
	_, _ = fmt.Fprintf(stdout, "enabled: %t\n", cfg.Enabled)
	_, _ = fmt.Fprintf(stdout, "backend: %s\n", cfg.Backend)
	_, _ = fmt.Fprintf(stdout, "path: %s\n", redactor.RedactString(cfg.SQLitePath))
	_, _ = fmt.Fprintf(stdout, "usable: %t\n", usable)
	if hasMemoryWarning(warning.String()) {
		_, _ = fmt.Fprintln(stdout, memoryStoreUnavailableWarning)
	}
	return 0
}

func runMemoryForget(ctx context.Context, cfg config.MemoryRuntime, secretValues []string, workspacePath, id string, stdout, stderr io.Writer) int {
	var warning bytes.Buffer
	service, userScope, usable, err := memoryOpenService(ctx, cfg, secretValues, &warning)
	if err != nil {
		return fail(stderr, "%v", errMemoryBackendUnavailable)
	}
	if service != nil {
		defer service.Close()
	}
	if !usable {
		return fail(stderr, "memory is not usable")
	}
	workspaceScope, err := memoryWorkspaceScopeFunc(cfg, workspacePath)
	if err != nil {
		return fail(stderr, "%v", errMemoryWorkspaceUnavailable)
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

func hasMemoryWarning(text string) bool {
	return strings.TrimSpace(text) != ""
}
