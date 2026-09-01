package app

import (
	"context"
	"fmt"
	"strings"
	"unicode"

	"github.com/baiyuqing/otto/internal/memory"
)

const (
	MemorySearchLimit       = 20
	MemorySearchTokenBudget = 4000
	MemoryDefaultKind       = "note"
	MemoryUsage             = "usage: /memory search <query> | /memory forget <id> | /memory review <id> accept|reject"
	RememberUsage           = "usage: /remember [--scope user|workspace] [--kind K] [--key K] <text>"
)

// MemoryManager is the capability a Backend must implement for explicit
// memory management commands (/memory, /remember). Checked via type
// assertion, the same pattern as SessionBrowser/SessionArchiver.
type MemoryManager interface {
	SearchMemory(ctx context.Context, request memory.SearchRequest) (memory.SearchResult, error)
	RememberMemory(ctx context.Context, request memory.RememberRequest) (memory.Record, error)
	ForgetMemory(ctx context.Context, request memory.ForgetRequest) (memory.ForgetResult, error)
	ReviewMemoryCandidate(ctx context.Context, request memory.ReviewRequest) (memory.ReviewResult, error)
	GetMemory(ctx context.Context, ref memory.RecordRef) (memory.Record, error)
	MemoryScopes() (memory.Scope, memory.Scope, bool)
}

// MemoryManagerAndScopes checks whether backend implements MemoryManager and,
// if so, returns it along with its bound user and workspace scopes.
func MemoryManagerAndScopes(backend Backend) (MemoryManager, memory.Scope, memory.Scope, bool) {
	manager, ok := backend.(MemoryManager)
	if !ok {
		return nil, memory.Scope{}, memory.Scope{}, false
	}
	userScope, workspaceScope, ok := manager.MemoryScopes()
	if !ok {
		return nil, memory.Scope{}, memory.Scope{}, false
	}
	return manager, userScope, workspaceScope, true
}

// ParseRememberArgument splits a /remember argument string into its
// --scope/--kind/--key flags and trailing free text.
func ParseRememberArgument(argument string) (scopeFlag, kind, key, text string) {
	kind = MemoryDefaultKind
	remaining := argument
	for {
		trimmed := strings.TrimSpace(remaining)
		switch {
		case strings.HasPrefix(trimmed, "--scope "):
			scopeFlag, remaining = SplitFirstToken(strings.TrimPrefix(trimmed, "--scope "))
		case strings.HasPrefix(trimmed, "--kind "):
			kind, remaining = SplitFirstToken(strings.TrimPrefix(trimmed, "--kind "))
		case strings.HasPrefix(trimmed, "--key "):
			key, remaining = SplitFirstToken(strings.TrimPrefix(trimmed, "--key "))
		default:
			text = trimmed
			return
		}
	}
}

// SplitFirstToken splits value on its first whitespace run, returning the
// leading token and the (trimmed) remainder.
func SplitFirstToken(value string) (first, rest string) {
	value = strings.TrimSpace(value)
	if index := strings.IndexFunc(value, unicode.IsSpace); index >= 0 {
		return value[:index], strings.TrimSpace(value[index:])
	}
	return value, ""
}

// RenderMemorySearchResult renders a memory search result as plain text for
// display in a frontend transcript or REPL output.
func RenderMemorySearchResult(result memory.SearchResult) string {
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
