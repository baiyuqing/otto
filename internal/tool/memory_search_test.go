package tool

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/memory"
)

type fakeMemoryReader struct {
	result memory.SearchResult
	err    error
	got    memory.SearchRequest
}

func (r *fakeMemoryReader) Get(context.Context, memory.RecordRef) (memory.Record, error) {
	return memory.Record{}, nil
}

func (r *fakeMemoryReader) GetByKey(context.Context, memory.RecordKey) (memory.Record, error) {
	return memory.Record{}, nil
}

func (r *fakeMemoryReader) GetTombstone(context.Context, memory.RecordRef) (memory.Tombstone, error) {
	return memory.Tombstone{}, nil
}

func (r *fakeMemoryReader) GetCandidate(context.Context, memory.CandidateRef) (memory.Candidate, error) {
	return memory.Candidate{}, nil
}

func (r *fakeMemoryReader) Search(_ context.Context, request memory.SearchRequest) (memory.SearchResult, error) {
	r.got = request
	return r.result, r.err
}

func TestMemorySearchToolRendersRecordsAndPlaceholder(t *testing.T) {
	scope := memory.Scope{Namespace: "user", ID: "u1"}
	reader := &fakeMemoryReader{result: memory.SearchResult{Records: []memory.Record{
		{ID: "rec-1", Scope: scope, Kind: "preference", Key: "editor", Text: "prefers vim", Revision: 1},
		{ID: "rec-2", Scope: scope, Kind: "instruction", Text: "always run tests before commit", Revision: 4},
	}}}
	searchTool := NewMemorySearchTool(reader, []memory.Scope{scope}, 8192)

	result := searchTool.Execute(context.Background(), json.RawMessage(`{"query":"editor"}`))
	if result.IsError {
		t.Fatalf("unexpected error result: %+v", result)
	}
	if !strings.Contains(result.Content, "rec-1") || !strings.Contains(result.Content, "prefers vim") || !strings.Contains(result.Content, "revision=1") {
		t.Fatalf("content missing record detail: %q", result.Content)
	}
	if !strings.Contains(result.Content, "rec-2") || !strings.Contains(result.Content, "always run tests before commit") {
		t.Fatalf("content missing second record detail: %q", result.Content)
	}
	if result.PersistedContent != "2 records: rec-1, rec-2" {
		t.Fatalf("persisted content = %q, want bounded placeholder", result.PersistedContent)
	}
	if reader.got.Query != "editor" {
		t.Fatalf("search request query = %q, want %q", reader.got.Query, "editor")
	}
	if len(reader.got.Scopes) != 1 || reader.got.Scopes[0] != scope {
		t.Fatalf("search request scopes = %#v, want %#v", reader.got.Scopes, []memory.Scope{scope})
	}
}

func TestMemorySearchToolHandlesNoMatches(t *testing.T) {
	reader := &fakeMemoryReader{}
	searchTool := NewMemorySearchTool(reader, nil, 8192)
	result := searchTool.Execute(context.Background(), json.RawMessage(`{"query":"nothing"}`))
	if result.IsError {
		t.Fatalf("unexpected error result: %+v", result)
	}
	if result.PersistedContent != "0 records" {
		t.Fatalf("persisted content = %q, want %q", result.PersistedContent, "0 records")
	}
}

func TestMemorySearchToolSurfacesReaderError(t *testing.T) {
	reader := &fakeMemoryReader{err: errors.New("boom")}
	searchTool := NewMemorySearchTool(reader, nil, 8192)
	result := searchTool.Execute(context.Background(), json.RawMessage(`{"query":"x"}`))
	if !result.IsError || !strings.Contains(result.Content, "boom") {
		t.Fatalf("result = %+v, want error containing boom", result)
	}
}
