package tui

import (
	"context"
	"fmt"
	"strings"
	"time"
	"unicode"

	tea "charm.land/bubbletea/v2"
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

func memoryManagerFromBackend(backend app.Backend) (memoryManager, bool) {
	manager, ok := backend.(memoryManager)
	return manager, ok
}

type memoryCommandResultMsg struct {
	generation uint64
	text       string
	errText    string
}

func (m Model) handleMemoryCommand(argument string) (tea.Model, tea.Cmd) {
	if m.running || m.newSessionPending {
		m.statusText = app.ErrPromptActive.Error()
		return m, nil
	}
	manager, userScope, workspaceScope, ok := memoryManagerAndScopes(m.backend)
	if !ok {
		m.statusText = app.ErrMemoryUnavailable.Error()
		return m, nil
	}

	fields := strings.Fields(argument)
	if len(fields) == 0 {
		m.statusText = memoryUsage
		return m, nil
	}
	scopes := []memory.Scope{userScope, workspaceScope}
	sub, rest := fields[0], fields[1:]

	switch sub {
	case "search":
		m.clearEditor()
		m.memoryGeneration++
		generation := m.memoryGeneration
		m.statusText = ""
		return m, runMemorySearchCommand(m.rootCtx, manager, generation, strings.Join(rest, " "), scopes)
	case "forget":
		if len(rest) != 1 {
			m.statusText = memoryUsage
			return m, nil
		}
		m.clearEditor()
		m.memoryGeneration++
		generation := m.memoryGeneration
		m.statusText = ""
		return m, runMemoryForgetCommand(m.rootCtx, manager, generation, rest[0], scopes)
	case "review":
		if len(rest) != 2 || (rest[1] != string(memory.ReviewAccept) && rest[1] != string(memory.ReviewReject)) {
			m.statusText = memoryUsage
			return m, nil
		}
		decision := memory.ReviewDecision(rest[1])
		m.clearEditor()
		m.memoryGeneration++
		generation := m.memoryGeneration
		m.statusText = ""
		return m, runMemoryReviewCommand(m.rootCtx, manager, generation, rest[0], decision, scopes)
	default:
		m.statusText = memoryUsage
		return m, nil
	}
}

func (m Model) handleRememberCommand(argument string) (tea.Model, tea.Cmd) {
	if m.running || m.newSessionPending {
		m.statusText = app.ErrPromptActive.Error()
		return m, nil
	}
	manager, userScope, workspaceScope, ok := memoryManagerAndScopes(m.backend)
	if !ok {
		m.statusText = app.ErrMemoryUnavailable.Error()
		return m, nil
	}

	scopeFlag, kind, key, text := parseRememberArgument(argument)
	if text == "" {
		m.statusText = rememberUsage
		return m, nil
	}
	scope := workspaceScope
	switch scopeFlag {
	case "", "workspace":
	case "user":
		scope = userScope
	default:
		m.statusText = fmt.Sprintf("unknown scope %q, want user or workspace", scopeFlag)
		return m, nil
	}

	m.clearEditor()
	m.memoryGeneration++
	generation := m.memoryGeneration
	m.statusText = ""
	return m, runMemoryRememberCommand(m.rootCtx, manager, generation, memory.RememberRequest{
		Scope: scope, Kind: kind, Key: key, Text: text,
	})
}

func memoryManagerAndScopes(backend app.Backend) (memoryManager, memory.Scope, memory.Scope, bool) {
	manager, ok := memoryManagerFromBackend(backend)
	if !ok {
		return nil, memory.Scope{}, memory.Scope{}, false
	}
	userScope, workspaceScope, ok := manager.MemoryScopes()
	if !ok {
		return nil, memory.Scope{}, memory.Scope{}, false
	}
	return manager, userScope, workspaceScope, true
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

func runMemorySearchCommand(ctx context.Context, manager memoryManager, generation uint64, query string, scopes []memory.Scope) tea.Cmd {
	return func() tea.Msg {
		result, err := manager.SearchMemory(rootContext(ctx), memory.SearchRequest{
			Query:       query,
			Scopes:      scopes,
			Limit:       memorySearchLimit,
			TokenBudget: memorySearchTokenBudget,
			Now:         time.Now().UTC(),
		})
		if err != nil {
			return memoryCommandResultMsg{generation: generation, errText: err.Error()}
		}
		return memoryCommandResultMsg{generation: generation, text: renderMemorySearchResult(result)}
	}
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

func runMemoryForgetCommand(ctx context.Context, manager memoryManager, generation uint64, id string, scopes []memory.Scope) tea.Cmd {
	return func() tea.Msg {
		resolved := rootContext(ctx)
		var lastErr error
		for _, scope := range scopes {
			ref := memory.RecordRef{Scope: scope, ID: id}
			record, err := manager.GetMemory(resolved, ref)
			if err != nil {
				lastErr = err
				continue
			}
			result, err := manager.ForgetMemory(resolved, memory.ForgetRequest{Ref: ref, ExpectedRevision: record.Revision})
			if err != nil {
				return memoryCommandResultMsg{generation: generation, errText: err.Error()}
			}
			return memoryCommandResultMsg{generation: generation, text: fmt.Sprintf("forgot %s (revision %d)", result.Tombstone.ID, record.Revision)}
		}
		return memoryCommandResultMsg{generation: generation, errText: fmt.Sprintf("record %s not found: %v", id, lastErr)}
	}
}

func runMemoryReviewCommand(ctx context.Context, manager memoryManager, generation uint64, id string, decision memory.ReviewDecision, scopes []memory.Scope) tea.Cmd {
	return func() tea.Msg {
		resolved := rootContext(ctx)
		var lastErr error
		for _, scope := range scopes {
			_, err := manager.ReviewMemoryCandidate(resolved, memory.ReviewRequest{
				Ref:      memory.CandidateRef{Scope: scope, ID: id},
				Decision: decision,
			})
			if err != nil {
				lastErr = err
				continue
			}
			return memoryCommandResultMsg{generation: generation, text: fmt.Sprintf("reviewed %s: %s", id, decision)}
		}
		return memoryCommandResultMsg{generation: generation, errText: fmt.Sprintf("candidate %s not found: %v", id, lastErr)}
	}
}

func runMemoryRememberCommand(ctx context.Context, manager memoryManager, generation uint64, request memory.RememberRequest) tea.Cmd {
	return func() tea.Msg {
		record, err := manager.RememberMemory(rootContext(ctx), request)
		if err != nil {
			return memoryCommandResultMsg{generation: generation, errText: err.Error()}
		}
		return memoryCommandResultMsg{generation: generation, text: fmt.Sprintf("remembered %s (scope=%s/%s kind=%s)", record.ID, record.Scope.Namespace, record.Scope.ID, record.Kind)}
	}
}

func (m Model) applyMemoryCommandResult(msg memoryCommandResultMsg) (tea.Model, tea.Cmd) {
	if msg.generation != m.memoryGeneration {
		return m, nil
	}
	if msg.errText != "" {
		m.statusText = msg.errText
		m.entries = append(m.entries, Entry{ID: m.nextLiveEntryID("memory"), Kind: EntryError, Raw: msg.errText})
		m.rerenderAndRefreshViewportContent(!m.autoFollow)
		return m, nil
	}
	m.statusText = ""
	m.entries = append(m.entries, Entry{ID: m.nextLiveEntryID("memory"), Kind: EntrySystem, Raw: msg.text})
	m.rerenderAndRefreshViewportContent(!m.autoFollow)
	return m, nil
}
