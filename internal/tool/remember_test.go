package tool

import (
	"context"
	"encoding/json"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/memory"
)

type fakeMemoryProposer struct {
	got   memory.ProposeRequest
	batch memory.CandidateBatch
	err   error
}

func (p *fakeMemoryProposer) Propose(_ context.Context, request memory.ProposeRequest) (memory.CandidateBatch, error) {
	p.got = request
	return p.batch, p.err
}

func TestRememberToolProposesCreateCandidate(t *testing.T) {
	proposer := &fakeMemoryProposer{batch: memory.CandidateBatch{Candidates: []memory.Candidate{{ID: "cand-1", State: memory.CandidatePending}}}}
	scope := memory.Scope{Namespace: "workspace", ID: "ws1"}
	rememberSubject := NewRememberTool(proposer, scope)

	result := rememberSubject.Execute(context.Background(), json.RawMessage(`{"kind":"preference","text":"likes dark mode"}`))
	if result.IsError {
		t.Fatalf("unexpected error: %+v", result)
	}
	if !strings.Contains(result.Content, "cand-1") || !strings.Contains(result.Content, "pending") {
		t.Fatalf("content = %q", result.Content)
	}
	if proposer.got.Action != memory.CandidateCreate {
		t.Fatalf("action = %v, want create", proposer.got.Action)
	}
	if proposer.got.Scope != scope {
		t.Fatalf("scope = %#v, want %#v", proposer.got.Scope, scope)
	}
	if proposer.got.Source.Origin != memory.OriginModel {
		t.Fatalf("origin = %v, want model", proposer.got.Source.Origin)
	}
}

func TestRememberToolProposesUpdateCandidateWhenTargetIDSet(t *testing.T) {
	proposer := &fakeMemoryProposer{batch: memory.CandidateBatch{Candidates: []memory.Candidate{{ID: "cand-2", State: memory.CandidatePending}}}}
	rememberSubject := NewRememberTool(proposer, memory.Scope{Namespace: "user", ID: "u1"})

	result := rememberSubject.Execute(context.Background(), json.RawMessage(`{"kind":"preference","text":"prefers vim","target_id":"rec-1","base_revision":3}`))
	if result.IsError {
		t.Fatalf("unexpected error: %+v", result)
	}
	if proposer.got.Action != memory.CandidateUpdate || proposer.got.TargetID != "rec-1" || proposer.got.BaseRevision != 3 {
		t.Fatalf("propose request = %#v", proposer.got)
	}
}

func TestRememberToolRequiresKindAndText(t *testing.T) {
	proposer := &fakeMemoryProposer{}
	rememberSubject := NewRememberTool(proposer, memory.Scope{})
	result := rememberSubject.Execute(context.Background(), json.RawMessage(`{"kind":"preference"}`))
	if !result.IsError {
		t.Fatalf("expected error for missing text")
	}
}
