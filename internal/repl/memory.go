package repl

import (
	"context"
	"fmt"
	"strings"
	"time"
	"unicode"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/memory"
)

const (
	memorySearchLimit       = 20
	memorySearchTokenBudget = 4000
	memoryDefaultKind       = "note"
	memoryUsage             = "usage: /memory search <query> | /memory forget <id> | /memory review <id> accept|reject"
	rememberUsage           = "usage: /remember [--scope user|workspace] [--kind K] [--key K] <text>"
)

type memoryManager interface {
	SearchMemory(ctx context.Context, request memory.SearchRequest) (memory.SearchResult, error)
	RememberMemory(ctx context.Context, request memory.RememberRequest) (memory.Record, error)
	ForgetMemory(ctx context.Context, request memory.ForgetRequest) (memory.ForgetResult, error)
	ReviewMemoryCandidate(ctx context.Context, request memory.ReviewRequest) (memory.ReviewResult, error)
	GetMemory(ctx context.Context, ref memory.RecordRef) (memory.Record, error)
	MemoryScopes() (memory.Scope, memory.Scope, bool)
}

func memoryManagerAndScopes(backend app.Backend) (memoryManager, memory.Scope, memory.Scope, bool) {
	manager, ok := backend.(memoryManager)
	if !ok {
		return nil, memory.Scope{}, memory.Scope{}, false
	}
	userScope, workspaceScope, ok := manager.MemoryScopes()
	if !ok {
		return nil, memory.Scope{}, memory.Scope{}, false
	}
	return manager, userScope, workspaceScope, true
}

func (r *REPL) memoryCommand(ctx context.Context, args string) (bool, error) {
	manager, userScope, workspaceScope, ok := memoryManagerAndScopes(r.backend)
	if !ok {
		return false, &commandError{command: "/memory", err: app.ErrMemoryUnavailable}
	}
	fields := strings.Fields(args)
	if len(fields) == 0 {
		_, _ = fmt.Fprintln(r.stderr, memoryUsage)
		return false, nil
	}
	scopes := []memory.Scope{userScope, workspaceScope}
	sub, rest := fields[0], fields[1:]

	switch sub {
	case "search":
		result, err := manager.SearchMemory(ctx, memory.SearchRequest{
			Query:       strings.Join(rest, " "),
			Scopes:      scopes,
			Limit:       memorySearchLimit,
			TokenBudget: memorySearchTokenBudget,
			Now:         time.Now().UTC(),
		})
		if err != nil {
			return false, &commandError{command: "/memory search", err: err}
		}
		_, _ = fmt.Fprintln(r.stdout, renderMemorySearchResult(result))
		return false, nil
	case "forget":
		if len(rest) != 1 {
			_, _ = fmt.Fprintln(r.stderr, memoryUsage)
			return false, nil
		}
		id := rest[0]
		var lastErr error
		for _, scope := range scopes {
			ref := memory.RecordRef{Scope: scope, ID: id}
			record, err := manager.GetMemory(ctx, ref)
			if err != nil {
				lastErr = err
				continue
			}
			result, err := manager.ForgetMemory(ctx, memory.ForgetRequest{Ref: ref, ExpectedRevision: record.Revision})
			if err != nil {
				return false, &commandError{command: "/memory forget", err: err}
			}
			_, _ = fmt.Fprintf(r.stdout, "forgot %s (revision %d)\n", result.Tombstone.ID, record.Revision)
			return false, nil
		}
		return false, &commandError{command: "/memory forget", err: fmt.Errorf("record %s not found: %w", id, lastErr)}
	case "review":
		if len(rest) != 2 || (rest[1] != string(memory.ReviewAccept) && rest[1] != string(memory.ReviewReject)) {
			_, _ = fmt.Fprintln(r.stderr, memoryUsage)
			return false, nil
		}
		id := rest[0]
		decision := memory.ReviewDecision(rest[1])
		var lastErr error
		for _, scope := range scopes {
			_, err := manager.ReviewMemoryCandidate(ctx, memory.ReviewRequest{
				Ref:      memory.CandidateRef{Scope: scope, ID: id},
				Decision: decision,
			})
			if err != nil {
				lastErr = err
				continue
			}
			_, _ = fmt.Fprintf(r.stdout, "reviewed %s: %s\n", id, decision)
			return false, nil
		}
		return false, &commandError{command: "/memory review", err: fmt.Errorf("candidate %s not found: %w", id, lastErr)}
	default:
		_, _ = fmt.Fprintln(r.stderr, memoryUsage)
		return false, nil
	}
}

func (r *REPL) rememberCommand(ctx context.Context, args string) (bool, error) {
	manager, userScope, workspaceScope, ok := memoryManagerAndScopes(r.backend)
	if !ok {
		return false, &commandError{command: "/remember", err: app.ErrMemoryUnavailable}
	}
	scopeFlag, kind, key, text := parseRememberArgument(args)
	if text == "" {
		_, _ = fmt.Fprintln(r.stderr, rememberUsage)
		return false, nil
	}
	scope := workspaceScope
	switch scopeFlag {
	case "", "workspace":
	case "user":
		scope = userScope
	default:
		_, _ = fmt.Fprintf(r.stderr, "unknown scope %q, want user or workspace\n", scopeFlag)
		return false, nil
	}
	record, err := manager.RememberMemory(ctx, memory.RememberRequest{Scope: scope, Kind: kind, Key: key, Text: text})
	if err != nil {
		return false, &commandError{command: "/remember", err: err}
	}
	_, _ = fmt.Fprintf(r.stdout, "remembered %s (scope=%s/%s kind=%s)\n", record.ID, record.Scope.Namespace, record.Scope.ID, record.Kind)
	return false, nil
}

func renderMemorySearchResult(result memory.SearchResult) string {
	if len(result.Records) == 0 {
		return "no matching records"
	}
	var content strings.Builder
	fmt.Fprintf(&content, "%d records:\n", len(result.Records))
	for _, record := range result.Records {
		fmt.Fprintf(&content, "id=%s scope=%s/%s kind=%s key=%s revision=%d text=%s\n",
			record.ID, record.Scope.Namespace, record.Scope.ID, record.Kind, record.Key, record.Revision, record.Text)
	}
	return strings.TrimRight(content.String(), "\n")
}

func parseRememberArgument(argument string) (scopeFlag, kind, key, text string) {
	kind = memoryDefaultKind
	remaining := argument
	for {
		trimmed := strings.TrimSpace(remaining)
		switch {
		case strings.HasPrefix(trimmed, "--scope "):
			scopeFlag, remaining = splitFirstToken(strings.TrimPrefix(trimmed, "--scope "))
		case strings.HasPrefix(trimmed, "--kind "):
			kind, remaining = splitFirstToken(strings.TrimPrefix(trimmed, "--kind "))
		case strings.HasPrefix(trimmed, "--key "):
			key, remaining = splitFirstToken(strings.TrimPrefix(trimmed, "--key "))
		default:
			text = trimmed
			return
		}
	}
}

func splitFirstToken(value string) (first, rest string) {
	value = strings.TrimSpace(value)
	if index := strings.IndexFunc(value, unicode.IsSpace); index >= 0 {
		return value[:index], strings.TrimSpace(value[index:])
	}
	return value, ""
}
