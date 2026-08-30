package memory

import (
	"encoding/json"
	"fmt"
	"math"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"
)

func invalidRecord(field string, limit ...int) error {
	if len(limit) != 0 {
		return fmt.Errorf("%w: %s exceeds %d bytes", ErrInvalidRecord, field, limit[0])
	}
	return fmt.Errorf("%w: %s", ErrInvalidRecord, field)
}

func invalidRequest(field string, limit ...int) error {
	if len(limit) != 0 {
		return fmt.Errorf("%w: %s exceeds %d", ErrInvalidRequest, field, limit[0])
	}
	return fmt.Errorf("%w: %s", ErrInvalidRequest, field)
}

func invalidCount(base error, field string, limit int) error {
	return fmt.Errorf("%w: %s exceeds count limit %d", base, field, limit)
}

func ValidateScope(scope Scope) error {
	if !validName(scope.Namespace, MaxNamespaceBytes) {
		return invalidRequest("scope namespace", MaxNamespaceBytes)
	}
	if !validOpaqueID(scope.ID, MaxScopeIDBytes) {
		return invalidRequest("scope ID", MaxScopeIDBytes)
	}
	return nil
}

func validName(value string, max int) bool {
	if len(value) == 0 || len(value) > max || value[0] < 'a' || value[0] > 'z' {
		return false
	}
	for i := 1; i < len(value); i++ {
		c := value[i]
		if c >= 'a' && c <= 'z' || c >= '0' && c <= '9' {
			continue
		}
		switch c {
		case '.', '_', '-':
		default:
			return false
		}
	}
	return true
}

func validText(value string, max int) bool {
	if !utf8.ValidString(value) || len(value) > max {
		return false
	}
	for _, r := range value {
		if r <= 0x1f || r >= 0x7f && r <= 0x9f {
			return false
		}
	}
	return true
}

func validSemantic(value string, max int, required bool) bool {
	if !validText(value, max) || value != strings.TrimSpace(value) {
		return false
	}
	return !required || value != ""
}

func validCursor(value string) bool { return validText(value, MaxCursorBytes) }

func invalidCursor() error {
	return fmt.Errorf("%w: %w: cursor", ErrInvalidRequest, ErrInvalidCursor)
}

func validTimestamp(value time.Time) bool {
	return !value.IsZero() && value.Location() == time.UTC && value.Year() >= 1 && value.Year() <= 9999 && value == value.Round(0)
}

func validOptionalTimestamp(value *time.Time) bool { return value == nil || validTimestamp(*value) }

func validRevision(value uint64) bool { return value >= 1 && value <= math.MaxInt64 }

func validOrigin(origin Origin) bool {
	switch origin {
	case OriginHuman, OriginModel, OriginExtractor, OriginImport, OriginMigration:
		return true
	default:
		return false
	}
}

func pendingOrigin(origin Origin) bool {
	return origin == OriginModel || origin == OriginExtractor || origin == OriginImport
}

func decisionOrigin(origin Origin) bool { return origin == OriginHuman || origin == OriginMigration }

func provenanceZero(value Provenance) bool {
	return value.Origin == "" && value.SessionID == "" && len(value.MessageIDs) == 0 && value.ObservationID == "" && value.DecisionAt == nil && value.DecisionSource == ""
}

func validateProvenance(value Provenance, active bool, base error) error {
	bad := func(field string) error {
		if base == ErrInvalidRecord {
			return invalidRecord(field)
		}
		return invalidRequest(field)
	}
	if !validOrigin(value.Origin) {
		return bad("source origin")
	}
	if value.SessionID != "" && !validOpaqueID(value.SessionID, MaxSessionIDBytes) {
		return bad("source session ID")
	}
	if value.ObservationID != "" && !validOpaqueID(value.ObservationID, MaxIDBytes) {
		return bad("source observation ID")
	}
	if len(value.MessageIDs) > MaxProvenanceMessageIDs {
		return invalidCount(base, "source message ID count", MaxProvenanceMessageIDs)
	}
	seen := make(map[string]struct{}, len(value.MessageIDs))
	for _, id := range value.MessageIDs {
		if !validOpaqueID(id, MaxMessageIDBytes) {
			return bad("source message ID")
		}
		if _, ok := seen[id]; ok {
			return bad("duplicate source message ID")
		}
		seen[id] = struct{}{}
	}
	if (value.DecisionAt == nil) != (value.DecisionSource == "") {
		return bad("source decision pair")
	}
	if value.DecisionAt != nil {
		if !validTimestamp(*value.DecisionAt) || !decisionOrigin(value.DecisionSource) {
			return bad("source decision")
		}
	}
	if active && pendingOrigin(value.Origin) && value.DecisionAt == nil {
		return bad("active source decision")
	}
	return nil
}

func ValidateRecord(record Record) error {
	if !validOpaqueID(record.ID, MaxIDBytes) {
		return invalidRecord("record ID", MaxIDBytes)
	}
	if err := ValidateScope(record.Scope); err != nil {
		return invalidRecord("record scope")
	}
	if !validName(record.Kind, MaxKindBytes) {
		return invalidRecord("record kind", MaxKindBytes)
	}
	if !validSemantic(record.Key, MaxSemanticKeyBytes, false) {
		return invalidRecord("record key", MaxSemanticKeyBytes)
	}
	if !validSemantic(record.Text, MaxRecordTextBytes, true) {
		return invalidRecord("record text", MaxRecordTextBytes)
	}
	if err := validateLabels(record.Labels, MaxLabels, ErrInvalidRecord); err != nil {
		return err
	}
	if err := validateMetadata(record.Metadata, ErrInvalidRecord); err != nil {
		return err
	}
	if err := validateProvenance(record.Source, true, ErrInvalidRecord); err != nil {
		return err
	}
	if math.IsNaN(record.Confidence) || math.IsInf(record.Confidence, 0) || record.Confidence < 0 || record.Confidence > 1 {
		return invalidRecord("record confidence")
	}
	if !validRevision(record.Revision) {
		return invalidRecord("record revision")
	}
	if !validTimestamp(record.CreatedAt) || !validTimestamp(record.UpdatedAt) || record.UpdatedAt.Before(record.CreatedAt) {
		return invalidRecord("record timestamps")
	}
	if !validOptionalTimestamp(record.ExpiresAt) || record.ExpiresAt != nil && record.ExpiresAt.Before(record.CreatedAt) {
		return invalidRecord("record expiry")
	}
	if record.Source.DecisionAt != nil && record.Source.DecisionAt.After(record.UpdatedAt) {
		return invalidRecord("source decision time")
	}
	return nil
}

func validateLabels(labels []string, maxCount int, base error) error {
	bad := func(field string, limit ...int) error {
		if base == ErrInvalidRecord {
			return invalidRecord(field, limit...)
		}
		return invalidRequest(field, limit...)
	}
	if len(labels) > maxCount {
		return invalidCount(base, "label count", maxCount)
	}
	normalized := make(map[string]struct{}, len(labels))
	for _, label := range labels {
		if !validSemantic(label, MaxLabelBytes, true) {
			return bad("label", MaxLabelBytes)
		}
		key := foldCanonical(strings.TrimSpace(label))
		if _, ok := normalized[key]; ok {
			return bad("duplicate label")
		}
		normalized[key] = struct{}{}
	}
	return nil
}

func validateMetadata(metadata map[string]string, base error) error {
	bad := func(field string, limit ...int) error {
		if base == ErrInvalidRecord {
			return invalidRecord(field, limit...)
		}
		return invalidRequest(field, limit...)
	}
	if len(metadata) > MaxMetadataEntries {
		return invalidCount(base, "metadata entry count", MaxMetadataEntries)
	}
	normalized := make(map[string]struct{}, len(metadata))
	for key, value := range metadata {
		if !validSemantic(key, MaxMetadataKeyBytes, true) {
			return bad("metadata key", MaxMetadataKeyBytes)
		}
		if !validText(value, MaxMetadataValueBytes) {
			return bad("metadata value", MaxMetadataValueBytes)
		}
		norm := foldCanonical(strings.TrimSpace(key))
		if _, ok := normalized[norm]; ok {
			return bad("duplicate metadata key")
		}
		normalized[norm] = struct{}{}
	}
	canonical, err := json.Marshal(metadata)
	if err != nil || len(canonical) > MaxMetadataBytes {
		return bad("metadata canonical JSON bytes", MaxMetadataBytes)
	}
	return nil
}

func foldCanonical(value string) string {
	var canonical strings.Builder
	canonical.Grow(len(value))
	for _, r := range value {
		folded := r
		for next := unicode.SimpleFold(r); next != r; next = unicode.SimpleFold(next) {
			if next < folded {
				folded = next
			}
		}
		canonical.WriteRune(folded)
	}
	return canonical.String()
}

func ValidateProposedRecord(record Record, action CandidateAction) error {
	if action != CandidateCreate && action != CandidateUpdate && action != CandidateForget {
		return invalidRecord("proposed action")
	}
	if err := ValidateScope(record.Scope); err != nil {
		return invalidRecord("proposed scope")
	}
	if record.ID != "" || record.Revision != 0 || !record.CreatedAt.IsZero() || !record.UpdatedAt.IsZero() {
		return invalidRecord("proposed persistence fields")
	}
	if record.ExpiresAt != nil && !validTimestamp(*record.ExpiresAt) {
		return invalidRecord("proposed expiry")
	}
	if !pendingOrigin(record.Source.Origin) || record.Source.DecisionAt != nil || record.Source.DecisionSource != "" {
		return invalidRecord("proposed source")
	}
	if err := validateProvenance(record.Source, false, ErrInvalidRecord); err != nil {
		return err
	}
	if action == CandidateForget {
		copy := record
		copy.Scope, copy.Source = Scope{}, Provenance{}
		if !reflectRecordZero(copy) {
			return invalidRecord("forget proposed content")
		}
		return nil
	}
	if !validName(record.Kind, MaxKindBytes) {
		return invalidRecord("proposed kind", MaxKindBytes)
	}
	if !validSemantic(record.Key, MaxSemanticKeyBytes, false) || !validSemantic(record.Text, MaxRecordTextBytes, true) {
		return invalidRecord("proposed content")
	}
	if err := validateLabels(record.Labels, MaxLabels, ErrInvalidRecord); err != nil {
		return err
	}
	if err := validateMetadata(record.Metadata, ErrInvalidRecord); err != nil {
		return err
	}
	if math.IsNaN(record.Confidence) || math.IsInf(record.Confidence, 0) || record.Confidence < 0 || record.Confidence > 1 {
		return invalidRecord("proposed confidence")
	}
	return nil
}

func reflectRecordZero(record Record) bool {
	return record.ID == "" && record.Kind == "" && record.Key == "" && record.Text == "" && len(record.Labels) == 0 && len(record.Metadata) == 0 && record.Confidence == 0 && record.Revision == 0 && record.CreatedAt.IsZero() && record.UpdatedAt.IsZero() && record.ExpiresAt == nil
}

func ValidateCandidate(candidate Candidate) error {
	if !validOpaqueID(candidate.ID, MaxIDBytes) {
		return invalidRecord("candidate ID")
	}
	if !validTimestamp(candidate.CreatedAt) {
		return invalidRecord("candidate created time")
	}
	switch candidate.Action {
	case CandidateCreate:
		if candidate.TargetID != "" || candidate.BaseRevision != 0 {
			return invalidRecord("create target")
		}
	case CandidateUpdate, CandidateForget:
		if !validOpaqueID(candidate.TargetID, MaxIDBytes) || !validRevision(candidate.BaseRevision) {
			return invalidRecord("candidate target")
		}
	default:
		return invalidRecord("candidate action")
	}
	switch candidate.State {
	case CandidatePending:
		if candidate.DecidedAt != nil || candidate.DecisionSource != "" || candidate.ResultRecordID != "" || candidate.ResultRevision != 0 {
			return invalidRecord("pending decision fields")
		}
		if err := ValidateProposedRecord(candidate.Proposed, candidate.Action); err != nil {
			return err
		}
		if !validSemantic(candidate.Reason, MaxReasonBytes, false) {
			return invalidRecord("candidate reason", MaxReasonBytes)
		}
	case CandidateAccepted, CandidateRejected:
		if !validOptionalTimestamp(candidate.DecidedAt) || candidate.DecidedAt == nil || candidate.DecidedAt.Before(candidate.CreatedAt) || !decisionOrigin(candidate.DecisionSource) {
			return invalidRecord("candidate decision")
		}
		if candidate.Reason != "" || !decidedProposedEmpty(candidate.Proposed) {
			return invalidRecord("decided candidate retained content")
		}
		if candidate.State == CandidateRejected {
			if candidate.ResultRecordID != "" || candidate.ResultRevision != 0 {
				return invalidRecord("rejected result")
			}
		} else {
			if !validRevision(candidate.ResultRevision) {
				return invalidRecord("accepted result revision")
			}
			if candidate.Action == CandidateCreate {
				if !validOpaqueID(candidate.ResultRecordID, MaxIDBytes) {
					return invalidRecord("accepted create result ID")
				}
			} else if candidate.ResultRecordID != "" {
				return invalidRecord("accepted result ID")
			}
		}
	default:
		return invalidRecord("candidate state")
	}
	return nil
}

func decidedProposedEmpty(record Record) bool {
	if err := ValidateScope(record.Scope); err != nil {
		return false
	}
	if record.Source.Origin != "" || record.Source.SessionID != "" || len(record.Source.MessageIDs) != 0 || record.Source.ObservationID != "" || record.Source.DecisionAt != nil || record.Source.DecisionSource != "" {
		return false
	}
	record.Scope = Scope{}
	return reflectRecordZero(record)
}

func validateScopes(scopes []Scope, allowEmpty, phaseOne bool) error {
	if len(scopes) == 0 && !allowEmpty {
		return invalidRequest("scope count")
	}
	if len(scopes) > MaxRequestScopes {
		return invalidCount(ErrInvalidRequest, "scope count", MaxRequestScopes)
	}
	seen := make(map[Scope]struct{}, len(scopes))
	users, workspaces := 0, 0
	for _, scope := range scopes {
		if err := ValidateScope(scope); err != nil {
			return err
		}
		if _, ok := seen[scope]; ok {
			return invalidRequest("duplicate scope")
		}
		seen[scope] = struct{}{}
		if phaseOne {
			switch scope.Namespace {
			case NamespaceUser:
				users++
			case NamespaceWorkspace:
				workspaces++
			default:
				return invalidRequest("phase 1 namespace")
			}
		}
	}
	if phaseOne && (users != 1 || workspaces > 1) {
		return invalidRequest("phase 1 scope set")
	}
	return nil
}

func validateFilters(kinds, labels []string) error {
	if len(kinds) > MaxRequestKinds {
		return invalidCount(ErrInvalidRequest, "kind filter count", MaxRequestKinds)
	}
	seen := map[string]struct{}{}
	for _, kind := range kinds {
		if !validName(kind, MaxKindBytes) {
			return invalidRequest("kind filter")
		}
		if _, ok := seen[kind]; ok {
			return invalidRequest("duplicate kind filter")
		}
		seen[kind] = struct{}{}
	}
	if len(labels) > MaxRequestLabels {
		return invalidCount(ErrInvalidRequest, "label filter count", MaxRequestLabels)
	}
	seen = make(map[string]struct{}, len(labels))
	for _, label := range labels {
		if !validSemantic(label, MaxLabelBytes, true) {
			return invalidRequest("label filter", MaxLabelBytes)
		}
		if _, ok := seen[label]; ok {
			return invalidRequest("duplicate label filter")
		}
		seen[label] = struct{}{}
	}
	return nil
}

func validatePage(limit int, cursor string, max int) error {
	if limit <= 0 || limit > max {
		return invalidRequest("page limit", max)
	}
	if !validCursor(cursor) {
		return invalidCursor()
	}
	return nil
}

func validateNow(now time.Time, includeExpired bool) error {
	if includeExpired && now.IsZero() {
		return nil
	}
	if !validTimestamp(now) {
		return invalidRequest("request time")
	}
	return nil
}

func ValidateListRequest(request ListRequest) error {
	if err := validateScopes(request.Scopes, true, false); err != nil {
		return err
	}
	if err := validateFilters(request.Kinds, request.Labels); err != nil {
		return err
	}
	if err := validatePage(request.Limit, request.Cursor, MaxPageSize); err != nil {
		return err
	}
	return validateNow(request.Now, request.IncludeExpired)
}

func ValidateTombstoneListRequest(request TombstoneListRequest) error {
	if err := validateScopes(request.Scopes, true, false); err != nil {
		return err
	}
	return validatePage(request.Limit, request.Cursor, MaxPageSize)
}

func validCandidateState(state CandidateState) bool {
	return state == CandidatePending || state == CandidateAccepted || state == CandidateRejected
}

func ValidateCandidateListRequest(request CandidateListRequest) error {
	if err := validateScopes(request.Scopes, true, false); err != nil {
		return err
	}
	if len(request.States) > MaxRequestKinds {
		return invalidRequest("candidate state count")
	}
	seen := map[CandidateState]struct{}{}
	for _, state := range request.States {
		if !validCandidateState(state) {
			return invalidRequest("candidate state")
		}
		if _, ok := seen[state]; ok {
			return invalidRequest("duplicate candidate state")
		}
		seen[state] = struct{}{}
	}
	return validatePage(request.Limit, request.Cursor, MaxPageSize)
}

func validateQuery(value string) error {
	if len(value) <= MaxQueryBytes && utf8.ValidString(value) && strings.TrimSpace(value) == "" {
		return nil
	}
	if !validSemantic(value, MaxQueryBytes, true) {
		return invalidRequest("query", MaxQueryBytes)
	}
	return nil
}

func validateBudget(limit, budget, maxLimit int) error {
	if limit <= 0 || limit > maxLimit {
		return invalidRequest("result limit", maxLimit)
	}
	if budget <= 0 || budget > MaxTokenBudget {
		return invalidRequest("token budget", MaxTokenBudget)
	}
	return nil
}

func ValidateRetrievalRequest(request RetrievalRequest) error {
	if err := validateQuery(request.Query); err != nil {
		return err
	}
	if err := validateScopes(request.Scopes, false, true); err != nil {
		return err
	}
	if err := validateFilters(request.Kinds, request.Labels); err != nil {
		return err
	}
	if err := validateBudget(request.Limit, request.TokenBudget, MaxPageSize); err != nil {
		return err
	}
	if !validCursor(request.Cursor) {
		return invalidCursor()
	}
	return validateNow(request.Now, request.IncludeExpired)
}

func ValidateSearchRequest(request SearchRequest) error {
	if err := validateQuery(request.Query); err != nil {
		return err
	}
	if err := validateScopes(request.Scopes, false, true); err != nil {
		return err
	}
	if err := validateFilters(request.Kinds, request.Labels); err != nil {
		return err
	}
	if err := validateBudget(request.Limit, request.TokenBudget, MaxPageSize); err != nil {
		return err
	}
	if !validCursor(request.Cursor) {
		return invalidCursor()
	}
	if !request.IncludeCandidates && len(request.CandidateStates) != 0 {
		return invalidRequest("candidate states require inclusion")
	}
	seen := map[CandidateState]struct{}{}
	for _, state := range request.CandidateStates {
		if !validCandidateState(state) {
			return invalidRequest("candidate state")
		}
		if _, ok := seen[state]; ok {
			return invalidRequest("duplicate candidate state")
		}
		seen[state] = struct{}{}
	}
	return validateNow(request.Now, request.IncludeExpired)
}

func ValidateRecallRequest(request RecallRequest) error {
	if err := validateQuery(request.Query); err != nil {
		return err
	}
	if err := validateFilters(request.Kinds, nil); err != nil {
		return err
	}
	return validateBudget(request.Limit, request.TokenBudget, MaxRecallRecords)
}

func validateTransientContent(scope Scope, kind, key, text string, labels []string, metadata map[string]string, confidence float64, expires *time.Time, source Provenance, allowedOrigin func(Origin) bool) error {
	if err := ValidateScope(scope); err != nil {
		return err
	}
	if !validName(kind, MaxKindBytes) || !validSemantic(key, MaxSemanticKeyBytes, false) || !validSemantic(text, MaxRecordTextBytes, true) {
		return invalidRequest("content fields")
	}
	if err := validateLabels(labels, MaxLabels, ErrInvalidRequest); err != nil {
		return err
	}
	if err := validateMetadata(metadata, ErrInvalidRequest); err != nil {
		return err
	}
	if math.IsNaN(confidence) || math.IsInf(confidence, 0) || confidence < 0 || confidence > 1 {
		return invalidRequest("confidence")
	}
	if !validOptionalTimestamp(expires) {
		return invalidRequest("expiry")
	}
	if !allowedOrigin(source.Origin) {
		return invalidRequest("source origin")
	}
	return validateProvenance(source, false, ErrInvalidRequest)
}

func ValidateRememberRequest(request RememberRequest) error {
	if provenanceZero(request.Source) {
		request.Source.Origin = OriginHuman
	}
	if err := validateTransientContent(request.Scope, request.Kind, request.Key, request.Text, request.Labels, request.Metadata, request.Confidence, request.ExpiresAt, request.Source, func(o Origin) bool { return o == OriginHuman || o == OriginMigration }); err != nil {
		return err
	}
	if request.Source.DecisionAt != nil {
		return invalidRequest("manager source decision")
	}
	if request.ExpectedRevision == nil {
		if request.ID != "" {
			return invalidRequest("create ID")
		}
	} else if !validRevision(*request.ExpectedRevision) || request.ID == "" && request.Key == "" || request.ID != "" && !validOpaqueID(request.ID, MaxIDBytes) {
		return invalidRequest("update identity and revision")
	}
	return nil
}

func ValidateProposeRequest(request ProposeRequest) error {
	if !validSemantic(request.Reason, MaxReasonBytes, false) {
		return invalidRequest("proposal reason", MaxReasonBytes)
	}
	if !pendingOrigin(request.Source.Origin) || request.Source.DecisionAt != nil || request.Source.DecisionSource != "" {
		return invalidRequest("proposal source origin or decision")
	}
	if request.Action == CandidateForget {
		if err := ValidateScope(request.Scope); err != nil {
			return err
		}
		if !validOpaqueID(request.TargetID, MaxIDBytes) || !validRevision(request.BaseRevision) {
			return invalidRequest("forget proposal target")
		}
		if request.Kind != "" || request.Key != "" || request.Text != "" || len(request.Labels) != 0 || len(request.Metadata) != 0 || request.Confidence != 0 || request.ExpiresAt != nil {
			return invalidRequest("forget proposal content")
		}
		return validateProvenance(request.Source, false, ErrInvalidRequest)
	}
	if err := validateTransientContent(request.Scope, request.Kind, request.Key, request.Text, request.Labels, request.Metadata, request.Confidence, request.ExpiresAt, request.Source, pendingOrigin); err != nil {
		return err
	}
	switch request.Action {
	case CandidateCreate:
		if request.TargetID != "" || request.BaseRevision != 0 {
			return invalidRequest("create proposal target")
		}
	case CandidateUpdate:
		if !validOpaqueID(request.TargetID, MaxIDBytes) || !validRevision(request.BaseRevision) {
			return invalidRequest("update proposal target")
		}
	default:
		return invalidRequest("proposal action")
	}
	return nil
}

func ValidateForgetRequest(request ForgetRequest) error {
	if err := ValidateRecordRef(request.Ref); err != nil {
		return err
	}
	if !validRevision(request.ExpectedRevision) {
		return invalidRequest("expected revision")
	}
	if request.PurgeBackups && !request.ConfirmPurge {
		return invalidRequest("purge confirmation")
	}
	return nil
}

func ValidateRecordRef(ref RecordRef) error {
	if err := ValidateScope(ref.Scope); err != nil {
		return err
	}
	if !validOpaqueID(ref.ID, MaxIDBytes) {
		return invalidRequest("record ID")
	}
	return nil
}

func ValidateCandidateRef(ref CandidateRef) error {
	if err := ValidateScope(ref.Scope); err != nil {
		return err
	}
	if !validOpaqueID(ref.ID, MaxIDBytes) {
		return invalidRequest("candidate ID")
	}
	return nil
}

func ValidateRecordKey(key RecordKey) error {
	if err := ValidateScope(key.Scope); err != nil {
		return err
	}
	if !validName(key.Kind, MaxKindBytes) || !validSemantic(key.Key, MaxSemanticKeyBytes, true) {
		return invalidRequest("record key")
	}
	return nil
}

func ValidateUpsertRequest(request UpsertRequest) error {
	if request.ExpectedRevision == nil {
		if request.Record.Revision != 0 {
			return invalidRequest("create revision")
		}
		request.Record.Revision = 1
		return ValidateRecord(request.Record)
	}
	if !validRevision(*request.ExpectedRevision) || request.Record.Revision != *request.ExpectedRevision {
		return invalidRequest("expected revision")
	}
	return ValidateRecord(request.Record)
}

func ValidateStoreForgetRequest(request StoreForgetRequest) error {
	if err := ValidateRecordRef(request.Ref); err != nil {
		return err
	}
	if !validRevision(request.ExpectedRevision) || !validTimestamp(request.ForgottenAt) {
		return invalidRequest("forget revision or time")
	}
	return nil
}

func ValidateProposalBatch(batch ProposalBatch) error {
	if len(batch.Candidates) == 0 {
		return invalidRequest("candidate batch count")
	}
	if len(batch.Candidates) > MaxCandidateBatch {
		return invalidCount(ErrInvalidRequest, "candidate batch count", MaxCandidateBatch)
	}
	for _, candidate := range batch.Candidates {
		if err := ValidateCandidate(candidate); err != nil {
			return err
		}
		if candidate.State != CandidatePending {
			return invalidRequest("candidate batch state")
		}
	}
	return validateCandidateBatchBytes(batch.Candidates)
}

func validateCandidateBatchBytes(candidates []Candidate) error {
	if len(candidates) > MaxCandidateBatch {
		return invalidCount(ErrInvalidRequest, "candidate batch count", MaxCandidateBatch)
	}
	total := 0
	add := func(value string) error {
		if len(value) > MaxCandidateBatchBytes-total {
			return invalidRequest("candidate batch bytes", MaxCandidateBatchBytes)
		}
		total += len(value)
		return nil
	}
	for _, c := range candidates {
		if len(c.Proposed.Labels) > MaxLabels || len(c.Proposed.Metadata) > MaxMetadataEntries || len(c.Proposed.Source.MessageIDs) > MaxProvenanceMessageIDs {
			return invalidRequest("candidate batch shape")
		}
		for _, value := range []string{
			c.ID, string(c.Action), c.TargetID, c.Reason, string(c.State), string(c.DecisionSource), c.ResultRecordID,
			c.Proposed.ID, c.Proposed.Scope.Namespace, c.Proposed.Scope.ID, c.Proposed.Kind, c.Proposed.Key, c.Proposed.Text,
			string(c.Proposed.Source.Origin), c.Proposed.Source.SessionID, c.Proposed.Source.ObservationID,
			string(c.Proposed.Source.DecisionSource),
		} {
			if err := add(value); err != nil {
				return err
			}
		}
		for _, value := range c.Proposed.Labels {
			if err := add(value); err != nil {
				return err
			}
		}
		for key, value := range c.Proposed.Metadata {
			if err := add(key); err != nil {
				return err
			}
			if err := add(value); err != nil {
				return err
			}
		}
		for _, value := range c.Proposed.Source.MessageIDs {
			if err := add(value); err != nil {
				return err
			}
		}
	}
	return nil
}

type canonicalToolFact struct {
	ToolName string `json:"tool_name"`
	Text     string `json:"text"`
}

type canonicalObservation struct {
	ID            string              `json:"id"`
	UserText      string              `json:"user_text"`
	AssistantText string              `json:"assistant_text"`
	SessionID     string              `json:"session_id"`
	ToolFacts     []canonicalToolFact `json:"tool_facts"`
	MessageIDs    []string            `json:"message_ids"`
}

func ValidateObservation(observation Observation) error {
	if !validOpaqueID(observation.ID, MaxIDBytes) || observation.SessionID != "" && !validOpaqueID(observation.SessionID, MaxSessionIDBytes) {
		return invalidRequest("observation ID")
	}
	if len(observation.MessageIDs) > MaxObservationMessageIDs || len(observation.ToolFacts) > MaxToolFacts {
		return invalidRequest("observation count")
	}
	if !validText(observation.UserText, MaxObservationBytes) || !validText(observation.AssistantText, MaxObservationBytes) {
		return invalidRequest("observation text", MaxObservationBytes)
	}
	for _, id := range observation.MessageIDs {
		if !validOpaqueID(id, MaxMessageIDBytes) {
			return invalidRequest("observation message ID")
		}
	}
	facts := make([]canonicalToolFact, len(observation.ToolFacts))
	messageIDs := append([]string{}, observation.MessageIDs...)
	for index, fact := range observation.ToolFacts {
		if !validSemantic(fact.ToolName, MaxToolNameBytes, true) || !validText(fact.Text, MaxToolFactTextBytes) {
			return invalidRequest("tool fact")
		}
		facts[index] = canonicalToolFact(fact)
	}
	canonical, err := json.Marshal(canonicalObservation{
		ID: observation.ID, UserText: observation.UserText, AssistantText: observation.AssistantText,
		SessionID: observation.SessionID, ToolFacts: facts, MessageIDs: messageIDs,
	})
	if err != nil || len(canonical) > MaxObservationBytes {
		return invalidRequest("observation canonical bytes", MaxObservationBytes)
	}
	return nil
}

func ValidateObservationCommit(commit ObservationCommit) error {
	if !validOpaqueID(commit.ObservationID, MaxIDBytes) || !validTimestamp(commit.CreatedAt) {
		return invalidRequest("observation commit")
	}
	if len(commit.Candidates) > MaxCandidateBatch {
		return invalidCount(ErrInvalidRequest, "candidate batch count", MaxCandidateBatch)
	}
	for _, candidate := range commit.Candidates {
		if err := ValidateCandidate(candidate); err != nil {
			return err
		}
		if candidate.State != CandidatePending {
			return invalidRequest("candidate batch state")
		}
	}
	return validateCandidateBatchBytes(commit.Candidates)
}

func ValidateObservationReceipt(receipt ObservationReceipt) error {
	if !validOpaqueID(receipt.ObservationID, MaxIDBytes) || len(receipt.CandidateIDs) > MaxCandidateBatch {
		return invalidRequest("observation receipt")
	}
	for _, id := range receipt.CandidateIDs {
		if !validOpaqueID(id, MaxIDBytes) {
			return invalidRequest("receipt candidate ID")
		}
	}
	return nil
}

func ValidateStoreReviewRequest(request StoreReviewRequest, candidate Candidate) error {
	if err := ValidateCandidateRef(request.Ref); err != nil {
		return err
	}
	if err := ValidateCandidate(candidate); err != nil {
		return err
	}
	if candidate.State != CandidatePending {
		return invalidRequest("review candidate state")
	}
	if request.Ref.Scope != candidate.Proposed.Scope || request.Ref.ID != candidate.ID {
		return invalidRequest("review candidate reference")
	}
	if request.Decision != ReviewAccept && request.Decision != ReviewReject || !decisionOrigin(request.DecisionSource) || !validTimestamp(request.DecidedAt) {
		return invalidRequest("review decision")
	}
	if request.ResultRecordID != "" && !validOpaqueID(request.ResultRecordID, MaxIDBytes) {
		return invalidRequest("review result ID")
	}
	if request.TargetRevision != nil && !validRevision(*request.TargetRevision) {
		return invalidRequest("review target revision")
	}
	if request.DecidedAt.Before(candidate.CreatedAt) {
		return invalidRequest("review decision time")
	}
	if request.Decision == ReviewReject {
		if request.Edited != nil || request.ResultRecordID != "" || request.TargetRevision != nil {
			return invalidRequest("rejected review fields")
		}
		return nil
	}
	if candidate.Action == CandidateForget && request.Edited != nil {
		return invalidRequest("forget edit")
	}
	if candidate.Action == CandidateCreate {
		if !validOpaqueID(request.ResultRecordID, MaxIDBytes) || request.TargetRevision != nil {
			return invalidRequest("accepted create fields")
		}
	} else {
		if request.ResultRecordID != "" {
			return invalidRequest("accepted target result ID")
		}
		target := candidate.BaseRevision
		if request.TargetRevision != nil {
			target = *request.TargetRevision
		}
		if candidate.Action == CandidateUpdate && target != candidate.BaseRevision && request.Edited == nil {
			return invalidRequest("review rebase edit")
		}
	}
	if request.Edited != nil {
		if candidate.Action == CandidateForget || request.Edited.Scope != candidate.Proposed.Scope {
			return invalidRequest("review edit scope")
		}
		if err := ValidateProposedRecord(*request.Edited, candidate.Action); err != nil {
			return err
		}
	}
	return nil
}

func ValidateReviewRequest(request ReviewRequest) error {
	if err := ValidateCandidateRef(request.Ref); err != nil {
		return err
	}
	if request.Decision != ReviewAccept && request.Decision != ReviewReject {
		return invalidRequest("review decision")
	}
	if request.Decision == ReviewReject && (request.Edited != nil || request.TargetRevision != nil) {
		return invalidRequest("rejected review fields")
	}
	if request.TargetRevision != nil && !validRevision(*request.TargetRevision) {
		return invalidRequest("target revision")
	}
	if request.Edited != nil {
		if request.Edited.Scope != request.Ref.Scope {
			return invalidRequest("review edit scope")
		}
		if err := ValidateProposedRecord(*request.Edited, CandidateUpdate); err != nil {
			return err
		}
	}
	return nil
}

func ValidateTombstone(tombstone Tombstone) error {
	if !validOpaqueID(tombstone.ID, MaxIDBytes) {
		return invalidRecord("tombstone ID")
	}
	if err := ValidateScope(tombstone.Scope); err != nil {
		return invalidRecord("tombstone scope")
	}
	if !validRevision(tombstone.Revision) || !validTimestamp(tombstone.CreatedAt) || !validTimestamp(tombstone.UpdatedAt) || !validTimestamp(tombstone.ForgottenAt) {
		return invalidRecord("tombstone revision or time")
	}
	if tombstone.UpdatedAt.Before(tombstone.CreatedAt) || tombstone.ForgottenAt.Before(tombstone.CreatedAt) || tombstone.ForgottenAt.After(tombstone.UpdatedAt) {
		return invalidRecord("tombstone time relationship")
	}
	return nil
}

func ValidateProposal(proposal Proposal) error {
	if err := ValidateScope(proposal.Scope); err != nil {
		return err
	}
	if !validSemantic(proposal.Reason, MaxReasonBytes, false) {
		return invalidRequest("proposal reason", MaxReasonBytes)
	}
	if proposal.Action == CandidateForget {
		if !validOpaqueID(proposal.TargetID, MaxIDBytes) || !validRevision(proposal.BaseRevision) {
			return invalidRequest("forget proposal target")
		}
		if proposal.Kind != "" || proposal.Key != "" || proposal.Text != "" || len(proposal.Labels) != 0 || len(proposal.Metadata) != 0 || proposal.Confidence != 0 || proposal.ExpiresAt != nil {
			return invalidRequest("forget proposal content")
		}
		return nil
	}
	if !validName(proposal.Kind, MaxKindBytes) || !validSemantic(proposal.Key, MaxSemanticKeyBytes, false) || !validSemantic(proposal.Text, MaxRecordTextBytes, true) {
		return invalidRequest("proposal content")
	}
	if err := validateLabels(proposal.Labels, MaxLabels, ErrInvalidRequest); err != nil {
		return err
	}
	if err := validateMetadata(proposal.Metadata, ErrInvalidRequest); err != nil {
		return err
	}
	if math.IsNaN(proposal.Confidence) || math.IsInf(proposal.Confidence, 0) || proposal.Confidence < 0 || proposal.Confidence > 1 || !validOptionalTimestamp(proposal.ExpiresAt) {
		return invalidRequest("proposal confidence or expiry")
	}
	switch proposal.Action {
	case CandidateCreate:
		if proposal.TargetID != "" || proposal.BaseRevision != 0 {
			return invalidRequest("create proposal target")
		}
	case CandidateUpdate:
		if !validOpaqueID(proposal.TargetID, MaxIDBytes) || !validRevision(proposal.BaseRevision) {
			return invalidRequest("update proposal target")
		}
	default:
		return invalidRequest("proposal action")
	}
	return nil
}

func ValidateExtractRequest(request ExtractRequest) error {
	if err := ValidateObservation(request.Observation); err != nil {
		return err
	}
	if len(request.Existing) > MaxBaselineRecords {
		return invalidRequest("extraction baseline", MaxBaselineRecords)
	}
	for _, record := range request.Existing {
		if err := ValidateRecord(record); err != nil {
			return err
		}
	}
	return nil
}

func ValidatePolicyRequest(request PolicyRequest) error {
	if !validOrigin(request.Origin) || request.Action != CandidateCreate && request.Action != CandidateUpdate && request.Action != CandidateForget {
		return invalidRequest("policy origin or action")
	}
	if err := ValidateScope(request.Scope); err != nil {
		return err
	}
	if request.Action != CandidateForget && !validName(request.Kind, MaxKindBytes) {
		return invalidRequest("policy kind")
	}
	if math.IsNaN(request.Confidence) || math.IsInf(request.Confidence, 0) || request.Confidence < 0 || request.Confidence > 1 {
		return invalidRequest("policy confidence")
	}
	return validateProvenance(request.Source, false, ErrInvalidRequest)
}

func ValidatePurgeForgottenRequest(request PurgeForgottenRequest) error {
	if err := ValidateTombstone(request.Tombstone); err != nil {
		return err
	}
	if !request.Confirm {
		return invalidRequest("purge confirmation")
	}
	return nil
}

func ValidateBindOptions(options BindOptions) error {
	if err := validateScopes(options.Scopes, false, true); err != nil {
		return err
	}
	if err := ValidateScope(options.DefaultWriteScope); err != nil {
		return err
	}
	for _, scope := range options.Scopes {
		if scope == options.DefaultWriteScope {
			return nil
		}
	}
	return invalidRequest("default write scope")
}
