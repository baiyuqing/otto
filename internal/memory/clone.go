package memory

import "time"

func CloneProvenance(value Provenance) Provenance {
	value.MessageIDs = cloneStrings(value.MessageIDs)
	value.DecisionAt = cloneTime(value.DecisionAt)
	return value
}

func CloneRecord(value Record) Record {
	value.Labels = cloneStrings(value.Labels)
	value.Metadata = cloneStringMap(value.Metadata)
	value.Source = CloneProvenance(value.Source)
	value.ExpiresAt = cloneTime(value.ExpiresAt)
	return value
}

func CloneCandidate(value Candidate) Candidate {
	value.Proposed = CloneRecord(value.Proposed)
	value.DecidedAt = cloneTime(value.DecidedAt)
	return value
}

func CloneRecordPage(value RecordPage) RecordPage {
	value.Records = cloneRecords(value.Records)
	return value
}

func CloneTombstonePage(value TombstonePage) TombstonePage {
	value.Tombstones = cloneSlice(value.Tombstones)
	return value
}

func CloneCandidatePage(value CandidatePage) CandidatePage {
	value.Candidates = cloneCandidates(value.Candidates)
	return value
}

func CloneCandidateBatch(value CandidateBatch) CandidateBatch {
	value.Candidates = cloneCandidates(value.Candidates)
	return value
}

func CloneListRequest(value ListRequest) ListRequest {
	value.Scopes = cloneSlice(value.Scopes)
	value.Kinds = cloneStrings(value.Kinds)
	value.Labels = cloneStrings(value.Labels)
	return value
}

func CloneTombstoneListRequest(value TombstoneListRequest) TombstoneListRequest {
	value.Scopes = cloneSlice(value.Scopes)
	return value
}

func CloneCandidateListRequest(value CandidateListRequest) CandidateListRequest {
	value.Scopes = cloneSlice(value.Scopes)
	value.States = cloneSlice(value.States)
	return value
}

func CloneUpsertRequest(value UpsertRequest) UpsertRequest {
	value.Record = CloneRecord(value.Record)
	value.ExpectedRevision = cloneUint64(value.ExpectedRevision)
	return value
}

func CloneProposalBatch(value ProposalBatch) ProposalBatch {
	value.Candidates = cloneCandidates(value.Candidates)
	return value
}

func CloneObservationCommit(value ObservationCommit) ObservationCommit {
	value.Candidates = cloneCandidates(value.Candidates)
	return value
}

func CloneObservationReceipt(value ObservationReceipt) ObservationReceipt {
	value.CandidateIDs = cloneStrings(value.CandidateIDs)
	return value
}

func CloneStoreReviewRequest(value StoreReviewRequest) StoreReviewRequest {
	value.Edited = cloneRecord(value.Edited)
	value.TargetRevision = cloneUint64(value.TargetRevision)
	return value
}

func CloneReviewResult(value ReviewResult) ReviewResult {
	value.Candidate = CloneCandidate(value.Candidate)
	value.Record = cloneRecord(value.Record)
	value.Tombstone = cloneTombstone(value.Tombstone)
	return value
}

func CloneRetrievalRequest(value RetrievalRequest) RetrievalRequest {
	value.Scopes = cloneSlice(value.Scopes)
	value.Kinds = cloneStrings(value.Kinds)
	value.Labels = cloneStrings(value.Labels)
	return value
}

func CloneRetrievalMatch(value RetrievalMatch) RetrievalMatch {
	value.Record = CloneRecord(value.Record)
	return value
}

func CloneRetrievalResult(value RetrievalResult) RetrievalResult {
	if value.Matches == nil {
		return value
	}
	matches := make([]RetrievalMatch, len(value.Matches))
	for i := range value.Matches {
		matches[i] = CloneRetrievalMatch(value.Matches[i])
	}
	value.Matches = matches
	return value
}

func CloneRecallRequest(value RecallRequest) RecallRequest {
	value.Kinds = cloneStrings(value.Kinds)
	return value
}

func CloneRecallResult(value RecallResult) RecallResult {
	value.Records = cloneRecords(value.Records)
	return value
}

func CloneObservation(value Observation) Observation {
	value.ToolFacts = cloneSlice(value.ToolFacts)
	value.MessageIDs = cloneStrings(value.MessageIDs)
	return value
}

func CloneObserveResult(value ObserveResult) ObserveResult {
	value.CandidateIDs = cloneStrings(value.CandidateIDs)
	return value
}

func CloneSearchRequest(value SearchRequest) SearchRequest {
	value.Scopes = cloneSlice(value.Scopes)
	value.Kinds = cloneStrings(value.Kinds)
	value.Labels = cloneStrings(value.Labels)
	value.CandidateStates = cloneSlice(value.CandidateStates)
	return value
}

func CloneSearchResult(value SearchResult) SearchResult {
	value.Records = cloneRecords(value.Records)
	value.Candidates = cloneCandidates(value.Candidates)
	return value
}

func CloneRememberRequest(value RememberRequest) RememberRequest {
	value.Labels = cloneStrings(value.Labels)
	value.Metadata = cloneStringMap(value.Metadata)
	value.ExpiresAt = cloneTime(value.ExpiresAt)
	value.ExpectedRevision = cloneUint64(value.ExpectedRevision)
	value.Source = CloneProvenance(value.Source)
	return value
}

func CloneForgetResult(value ForgetResult) ForgetResult {
	value.PurgedBackupIDs = cloneStrings(value.PurgedBackupIDs)
	return value
}

func CloneProposeRequest(value ProposeRequest) ProposeRequest {
	value.Labels = cloneStrings(value.Labels)
	value.Metadata = cloneStringMap(value.Metadata)
	value.ExpiresAt = cloneTime(value.ExpiresAt)
	value.Source = CloneProvenance(value.Source)
	return value
}

func CloneReviewRequest(value ReviewRequest) ReviewRequest {
	value.Edited = cloneRecord(value.Edited)
	value.TargetRevision = cloneUint64(value.TargetRevision)
	return value
}

func CloneExtractRequest(value ExtractRequest) ExtractRequest {
	value.Observation = CloneObservation(value.Observation)
	value.Existing = cloneRecords(value.Existing)
	return value
}

func CloneProposal(value Proposal) Proposal {
	value.Labels = cloneStrings(value.Labels)
	value.Metadata = cloneStringMap(value.Metadata)
	value.ExpiresAt = cloneTime(value.ExpiresAt)
	return value
}

func ClonePolicyRequest(value PolicyRequest) PolicyRequest {
	value.Source = CloneProvenance(value.Source)
	return value
}

func CloneGuardInput(value GuardInput) GuardInput {
	value.Fields = cloneSlice(value.Fields)
	return value
}

func CloneBindOptions(value BindOptions) BindOptions {
	value.Scopes = cloneSlice(value.Scopes)
	return value
}

func ClonePurgeForgottenResult(value PurgeForgottenResult) PurgeForgottenResult {
	value.PurgedBackupIDs = cloneStrings(value.PurgedBackupIDs)
	return value
}

func cloneRecords(values []Record) []Record {
	if values == nil {
		return nil
	}
	cloned := make([]Record, len(values))
	for i := range values {
		cloned[i] = CloneRecord(values[i])
	}
	return cloned
}

func cloneCandidates(values []Candidate) []Candidate {
	if values == nil {
		return nil
	}
	cloned := make([]Candidate, len(values))
	for i := range values {
		cloned[i] = CloneCandidate(values[i])
	}
	return cloned
}

func cloneRecord(value *Record) *Record {
	if value == nil {
		return nil
	}
	cloned := CloneRecord(*value)
	return &cloned
}

func cloneTombstone(value *Tombstone) *Tombstone {
	if value == nil {
		return nil
	}
	cloned := *value
	return &cloned
}

func cloneTime(value *time.Time) *time.Time {
	if value == nil {
		return nil
	}
	cloned := *value
	return &cloned
}

func cloneUint64(value *uint64) *uint64 {
	if value == nil {
		return nil
	}
	cloned := *value
	return &cloned
}

func cloneStrings(values []string) []string {
	return cloneSlice(values)
}

func cloneStringMap(values map[string]string) map[string]string {
	if values == nil {
		return nil
	}
	cloned := make(map[string]string, len(values))
	for key, value := range values {
		cloned[key] = value
	}
	return cloned
}

func cloneSlice[T any](values []T) []T {
	if values == nil {
		return nil
	}
	cloned := make([]T, len(values))
	copy(cloned, values)
	return cloned
}
