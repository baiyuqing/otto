package app

import (
	"context"
	"testing"

	"github.com/baiyuqing/otto/internal/memory"
)

func TestParseRememberArgumentParsesFlagsAndText(t *testing.T) {
	scope, kind, key, text := ParseRememberArgument(`--scope user --kind fact --key nickname call me Bai`)
	if scope != "user" || kind != "fact" || key != "nickname" || text != "call me Bai" {
		t.Fatalf("got scope=%q kind=%q key=%q text=%q", scope, kind, key, text)
	}
}

func TestParseRememberArgumentDefaultsKind(t *testing.T) {
	_, kind, _, text := ParseRememberArgument("just some text")
	if kind != MemoryDefaultKind || text != "just some text" {
		t.Fatalf("got kind=%q text=%q", kind, text)
	}
}

func TestRenderMemorySearchResultReportsNoMatches(t *testing.T) {
	if got := RenderMemorySearchResult(memory.SearchResult{}); got != "no matching records" {
		t.Fatalf("got %q", got)
	}
}

func TestRenderMemorySearchResultListsRecords(t *testing.T) {
	result := memory.SearchResult{Records: []memory.Record{
		{ID: "rec-1", Scope: memory.Scope{Namespace: "user", ID: "u1"}, Kind: "note", Key: "k", Revision: 3, Text: "hello"},
	}}
	got := RenderMemorySearchResult(result)
	want := "1 records:\nid=rec-1 scope=user/u1 kind=note key=k revision=3 text=hello"
	if got != want {
		t.Fatalf("got %q, want %q", got, want)
	}
}

type stubMemoryManager struct {
	Backend
	userScope, workspaceScope memory.Scope
	scopesOK                  bool
}

func (s *stubMemoryManager) SearchMemory(context.Context, memory.SearchRequest) (memory.SearchResult, error) {
	return memory.SearchResult{}, nil
}
func (s *stubMemoryManager) RememberMemory(context.Context, memory.RememberRequest) (memory.Record, error) {
	return memory.Record{}, nil
}
func (s *stubMemoryManager) ForgetMemory(context.Context, memory.ForgetRequest) (memory.ForgetResult, error) {
	return memory.ForgetResult{}, nil
}
func (s *stubMemoryManager) ReviewMemoryCandidate(context.Context, memory.ReviewRequest) (memory.ReviewResult, error) {
	return memory.ReviewResult{}, nil
}
func (s *stubMemoryManager) GetMemory(context.Context, memory.RecordRef) (memory.Record, error) {
	return memory.Record{}, nil
}
func (s *stubMemoryManager) MemoryScopes() (memory.Scope, memory.Scope, bool) {
	return s.userScope, s.workspaceScope, s.scopesOK
}

type notMemoryManagerBackend struct{ Backend }

func TestMemoryManagerAndScopesReturnsFalseWhenBackendLacksCapability(t *testing.T) {
	if _, _, _, ok := MemoryManagerAndScopes(notMemoryManagerBackend{}); ok {
		t.Fatalf("ok = true, want false")
	}
}

func TestMemoryManagerAndScopesReturnsScopesWhenAvailable(t *testing.T) {
	backend := &stubMemoryManager{
		userScope:      memory.Scope{Namespace: "user", ID: "u1"},
		workspaceScope: memory.Scope{Namespace: "workspace", ID: "w1"},
		scopesOK:       true,
	}
	manager, userScope, workspaceScope, ok := MemoryManagerAndScopes(backend)
	if !ok || manager == nil || userScope != backend.userScope || workspaceScope != backend.workspaceScope {
		t.Fatalf("got manager=%v userScope=%v workspaceScope=%v ok=%v", manager, userScope, workspaceScope, ok)
	}
}
