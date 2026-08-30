package memory

import (
	"testing"
	"time"
)

func TestCloneRecordDoesNotAlias(t *testing.T) {
	expiry := time.Date(2026, 8, 29, 1, 2, 3, 0, time.UTC)
	decision := expiry.Add(-time.Hour)
	original := Record{
		Labels:    []string{"one"},
		Metadata:  map[string]string{"k": "v"},
		ExpiresAt: &expiry,
		Source: Provenance{
			MessageIDs: []string{"m1"},
			DecisionAt: &decision,
		},
	}
	cloned := CloneRecord(original)
	cloned.Labels[0], cloned.Metadata["k"], cloned.Source.MessageIDs[0] = "two", "changed", "m2"
	*cloned.ExpiresAt = expiry.Add(time.Hour)
	*cloned.Source.DecisionAt = decision.Add(time.Hour)
	if original.Labels[0] != "one" || original.Metadata["k"] != "v" || original.Source.MessageIDs[0] != "m1" || !original.ExpiresAt.Equal(expiry) || !original.Source.DecisionAt.Equal(decision) {
		t.Fatalf("clone aliases original: original=%#v clone=%#v", original, cloned)
	}
}

func TestCloneProposalsDoesNotAlias(t *testing.T) {
	expiry := time.Date(2026, 8, 29, 1, 2, 3, 0, time.UTC)
	original := []Proposal{{
		Labels:    []string{"one"},
		Metadata:  map[string]string{"k": "v"},
		ExpiresAt: &expiry,
	}}
	cloned := CloneProposals(original)
	cloned[0].Labels[0] = "changed"
	cloned[0].Metadata["k"] = "changed"
	*cloned[0].ExpiresAt = expiry.Add(time.Hour)
	if original[0].Labels[0] != "one" || original[0].Metadata["k"] != "v" || !original[0].ExpiresAt.Equal(expiry) {
		t.Fatalf("clone aliases original: original=%#v clone=%#v", original, cloned)
	}

	if got := CloneProposals(nil); got != nil {
		t.Fatalf("nil proposals became non-nil: %#v", got)
	}
	empty := CloneProposals([]Proposal{})
	if empty == nil || len(empty) != 0 {
		t.Fatalf("non-nil empty proposals changed: %#v", empty)
	}
}

func TestCloneCandidateAndReviewResultDoNotAlias(t *testing.T) {
	expiry := time.Date(2026, 8, 29, 1, 2, 3, 0, time.UTC)
	decided := expiry.Add(time.Hour)
	original := ReviewResult{
		Candidate: Candidate{
			Proposed:  Record{Labels: []string{"one"}, Metadata: map[string]string{"k": "v"}, ExpiresAt: &expiry},
			DecidedAt: &decided,
		},
		Record:    &Record{Labels: []string{"active"}, Metadata: map[string]string{"record": "value"}},
		Tombstone: &Tombstone{ID: "tombstone"},
	}
	cloned := CloneReviewResult(original)
	cloned.Candidate.Proposed.Labels[0] = "changed"
	cloned.Candidate.Proposed.Metadata["k"] = "changed"
	*cloned.Candidate.Proposed.ExpiresAt = expiry.Add(time.Hour)
	*cloned.Candidate.DecidedAt = decided.Add(time.Hour)
	cloned.Record.Labels[0] = "changed"
	cloned.Record.Metadata["record"] = "changed"
	cloned.Tombstone.ID = "changed"
	if original.Candidate.Proposed.Labels[0] != "one" || original.Candidate.Proposed.Metadata["k"] != "v" || !original.Candidate.Proposed.ExpiresAt.Equal(expiry) || !original.Candidate.DecidedAt.Equal(decided) || original.Record.Labels[0] != "active" || original.Record.Metadata["record"] != "value" || original.Tombstone.ID != "tombstone" {
		t.Fatalf("clone aliases original: original=%#v clone=%#v", original, cloned)
	}
}

func TestClonePagesAndResultsDoNotAlias(t *testing.T) {
	recordPage := RecordPage{Records: []Record{{Labels: []string{"record"}}}}
	clonedRecordPage := CloneRecordPage(recordPage)
	clonedRecordPage.Records[0].Labels[0] = "changed"
	if recordPage.Records[0].Labels[0] != "record" {
		t.Fatal("record page clone aliases input")
	}

	candidatePage := CandidatePage{Candidates: []Candidate{{Proposed: Record{Metadata: map[string]string{"k": "v"}}}}}
	clonedCandidatePage := CloneCandidatePage(candidatePage)
	clonedCandidatePage.Candidates[0].Proposed.Metadata["k"] = "changed"
	if candidatePage.Candidates[0].Proposed.Metadata["k"] != "v" {
		t.Fatal("candidate page clone aliases input")
	}

	retrieval := RetrievalResult{Matches: []RetrievalMatch{{Record: Record{Labels: []string{"match"}}}}}
	clonedRetrieval := CloneRetrievalResult(retrieval)
	clonedRetrieval.Matches[0].Record.Labels[0] = "changed"
	if retrieval.Matches[0].Record.Labels[0] != "match" {
		t.Fatal("retrieval result clone aliases input")
	}

	search := SearchResult{Records: []Record{{Labels: []string{"search"}}}, Candidates: []Candidate{{Proposed: Record{Labels: []string{"candidate"}}}}}
	clonedSearch := CloneSearchResult(search)
	clonedSearch.Records[0].Labels[0] = "changed"
	clonedSearch.Candidates[0].Proposed.Labels[0] = "changed"
	if search.Records[0].Labels[0] != "search" || search.Candidates[0].Proposed.Labels[0] != "candidate" {
		t.Fatal("search result clone aliases input")
	}
}

func TestCloneRequestsAndReceiptsDoNotAlias(t *testing.T) {
	expiry := time.Date(2026, 8, 29, 1, 2, 3, 0, time.UTC)
	revision := uint64(3)
	remember := RememberRequest{
		Labels:           []string{"one"},
		Metadata:         map[string]string{"k": "v"},
		ExpiresAt:        &expiry,
		ExpectedRevision: &revision,
		Source:           Provenance{MessageIDs: []string{"m1"}},
	}
	clonedRemember := CloneRememberRequest(remember)
	clonedRemember.Labels[0] = "changed"
	clonedRemember.Metadata["k"] = "changed"
	*clonedRemember.ExpiresAt = expiry.Add(time.Hour)
	*clonedRemember.ExpectedRevision = 4
	clonedRemember.Source.MessageIDs[0] = "changed"
	if remember.Labels[0] != "one" || remember.Metadata["k"] != "v" || !remember.ExpiresAt.Equal(expiry) || *remember.ExpectedRevision != 3 || remember.Source.MessageIDs[0] != "m1" {
		t.Fatal("remember request clone aliases input")
	}

	receipt := ObservationReceipt{CandidateIDs: []string{"c1"}}
	clonedReceipt := CloneObservationReceipt(receipt)
	clonedReceipt.CandidateIDs[0] = "changed"
	if receipt.CandidateIDs[0] != "c1" {
		t.Fatal("observation receipt clone aliases input")
	}

	options := BindOptions{Scopes: []Scope{{Namespace: NamespaceUser, ID: "u1"}}}
	clonedOptions := CloneBindOptions(options)
	clonedOptions.Scopes[0].ID = "changed"
	if options.Scopes[0].ID != "u1" {
		t.Fatal("bind options clone aliases input")
	}
}

func TestClonePreservesNilAndIndependentEmptyCollections(t *testing.T) {
	if got := CloneRecord(Record{}); got.Labels != nil || got.Metadata != nil || got.Source.MessageIDs != nil || got.ExpiresAt != nil {
		t.Fatalf("nil values changed: %#v", got)
	}
	original := Record{Labels: []string{}, Metadata: map[string]string{}, Source: Provenance{MessageIDs: []string{}}}
	cloned := CloneRecord(original)
	if cloned.Labels == nil || cloned.Metadata == nil || cloned.Source.MessageIDs == nil {
		t.Fatalf("non-nil empty values became nil: %#v", cloned)
	}
	cloned.Labels = append(cloned.Labels, "clone")
	if len(original.Labels) != 0 {
		t.Fatal("empty label slices alias")
	}
	original.Metadata["original"] = "value"
	cloned.Metadata["clone"] = "value"
	if _, ok := cloned.Metadata["original"]; ok {
		t.Fatal("empty maps alias")
	}
	if _, ok := original.Metadata["clone"]; ok {
		t.Fatal("empty maps alias")
	}
}
