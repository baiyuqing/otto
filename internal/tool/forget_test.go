package tool

import (
	"context"
	"encoding/json"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/memory"
)

func TestForgetToolProposesForgetCandidate(t *testing.T) {
	proposer := &fakeMemoryProposer{batch: memory.CandidateBatch{Candidates: []memory.Candidate{{ID: "cand-3", State: memory.CandidatePending}}}}
	scopes := []memory.Scope{{Namespace: "user", ID: "u1"}, {Namespace: "workspace", ID: "ws1"}}
	forgetSubject := NewForgetTool(proposer, scopes)

	result := forgetSubject.Execute(context.Background(), json.RawMessage(`{"scope_namespace":"user","scope_id":"u1","id":"rec-1","revision":2}`))
	if result.IsError {
		t.Fatalf("unexpected error: %+v", result)
	}
	if !strings.Contains(result.Content, "cand-3") {
		t.Fatalf("content = %q", result.Content)
	}
	if proposer.got.Action != memory.CandidateForget || proposer.got.TargetID != "rec-1" || proposer.got.BaseRevision != 2 {
		t.Fatalf("propose request = %#v", proposer.got)
	}
	if proposer.got.Scope != (memory.Scope{Namespace: "user", ID: "u1"}) {
		t.Fatalf("scope = %#v", proposer.got.Scope)
	}
	if proposer.got.Source.Origin != memory.OriginModel {
		t.Fatalf("origin = %v, want model", proposer.got.Source.Origin)
	}
}

func TestForgetToolRequiresIdentifyingFields(t *testing.T) {
	proposer := &fakeMemoryProposer{}
	forgetSubject := NewForgetTool(proposer, []memory.Scope{{Namespace: "user", ID: "u1"}})
	result := forgetSubject.Execute(context.Background(), json.RawMessage(`{"id":"rec-1"}`))
	if !result.IsError {
		t.Fatalf("expected error for missing scope fields")
	}
}

func TestForgetToolRejectsScopeOutsideBoundScopes(t *testing.T) {
	proposer := &fakeMemoryProposer{batch: memory.CandidateBatch{Candidates: []memory.Candidate{{ID: "cand-3", State: memory.CandidatePending}}}}
	scopes := []memory.Scope{{Namespace: "user", ID: "u1"}, {Namespace: "workspace", ID: "ws1"}}
	forgetSubject := NewForgetTool(proposer, scopes)

	result := forgetSubject.Execute(context.Background(), json.RawMessage(`{"scope_namespace":"workspace","scope_id":"someone-elses-workspace","id":"rec-1","revision":2}`))
	if !result.IsError {
		t.Fatalf("expected error for scope outside bound scopes, got %+v", result)
	}
	if proposer.got.TargetID != "" {
		t.Fatalf("proposer should not have been called, got %#v", proposer.got)
	}
}
