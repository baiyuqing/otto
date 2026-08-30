package memory

import (
	"context"
	"crypto/sha256"
	"crypto/subtle"
	"errors"
	"fmt"
	"net/url"
	"regexp"
	"strings"
	"unicode/utf8"
)

var (
	guardURLPattern             = regexp.MustCompile(`(?i)https?://[^\s<>"']+`)
	credentialAssignmentPattern = regexp.MustCompile(`(?i)(?:^|[\s,;{(])(?:api[_-]?key|access[_-]?token|auth[_-]?token|client[_-]?secret|secret|password|passwd)\s*[:=]\s*[^\s,;}]+`)
	privateKeyPattern           = regexp.MustCompile(`(?i)-----\s*(?:BEGIN|END)\s+(?:[A-Z0-9]+\s+)*PRIVATE KEY\s*-----`)
)

type DefaultGuard struct{}

func (DefaultGuard) Check(ctx context.Context, input GuardInput) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if err := validateGuardInput(input); err != nil {
		return err
	}
	for i, field := range input.Fields {
		if i&31 == 0 {
			if err := ctx.Err(); err != nil {
				return err
			}
		}
		value := field.Value
		if strings.Contains(value, "[REDACTED]") {
			return sensitive("redaction marker")
		}
		if hasURIUserinfo(value) {
			return sensitive("URI userinfo")
		}
		if field.Opaque {
			continue
		}
		if privateKeyPattern.MatchString(value) {
			return sensitive("private key delimiter")
		}
		if hasSensitiveHeader(value) {
			return sensitive("credential header")
		}
		if credentialAssignmentPattern.MatchString(value) {
			return sensitive("credential assignment")
		}
	}
	return nil
}

func sensitive(category string) error { return fmt.Errorf("%w: %s", ErrSensitiveMemory, category) }

func validateGuardInput(input GuardInput) error {
	if len(input.Fields) > MaxGuardFields {
		return sensitive("field limit")
	}
	total := 0
	for _, field := range input.Fields {
		if !utf8.ValidString(field.Value) {
			return sensitive("invalid text")
		}
		total += len(field.Value)
		if total > MaxGuardBytes {
			return sensitive("byte limit")
		}
	}
	return nil
}

func hasSensitiveHeader(value string) bool {
	for _, line := range strings.Split(value, "\n") {
		line = strings.TrimSpace(strings.TrimSuffix(line, "\r"))
		name, rest, ok := strings.Cut(line, ":")
		if !ok || strings.TrimSpace(rest) == "" {
			continue
		}
		switch strings.ToLower(strings.TrimSpace(name)) {
		case "authorization", "proxy-authorization", "cookie", "set-cookie":
			return true
		}
	}
	return false
}

func hasURIUserinfo(value string) bool {
	for _, candidate := range guardURLPattern.FindAllString(value, -1) {
		candidate = strings.TrimRight(candidate, ".,;!?)]}>")
		parsed, err := url.Parse(candidate)
		if err == nil && parsed.User != nil && parsed.User.Username() != "" {
			return true
		}
	}
	return false
}

type exactFingerprint struct {
	length int
	digest [sha256.Size]byte
}

type ExactGuard struct{ fingerprints []exactFingerprint }

func NewExactGuard(values []string) (*ExactGuard, error) {
	if len(values) > MaxExactGuardValues {
		return nil, invalidRequest("exact guard value count", MaxExactGuardValues)
	}
	fingerprints := make([]exactFingerprint, 0, len(values))
	seen := make(map[exactFingerprint]struct{}, len(values))
	for _, value := range values {
		if value == "" || !utf8.ValidString(value) || len(value) > MaxExactGuardValueBytes {
			return nil, invalidRequest("exact guard value", MaxExactGuardValueBytes)
		}
		fingerprint := exactFingerprint{length: len(value), digest: sha256.Sum256([]byte(value))}
		if _, ok := seen[fingerprint]; ok {
			continue
		}
		seen[fingerprint] = struct{}{}
		fingerprints = append(fingerprints, fingerprint)
	}
	return &ExactGuard{fingerprints: fingerprints}, nil
}

func (guard *ExactGuard) Check(ctx context.Context, input GuardInput) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if guard == nil {
		return ErrUnavailable
	}
	if err := validateGuardInput(input); err != nil {
		return err
	}
	spans := 0
	check := func(value string) (bool, error) {
		spans++
		if spans > MaxExactGuardSpans {
			return false, sensitive("exact span limit")
		}
		digest := sha256.Sum256([]byte(value))
		for _, fingerprint := range guard.fingerprints {
			if len(value) == fingerprint.length && subtle.ConstantTimeCompare(digest[:], fingerprint.digest[:]) == 1 {
				return true, nil
			}
		}
		return false, nil
	}
	for index, field := range input.Fields {
		if index&31 == 0 {
			if err := ctx.Err(); err != nil {
				return err
			}
		}
		if match, err := check(field.Value); err != nil {
			return err
		} else if match {
			return sensitive("configured value")
		}
		for _, token := range strings.Fields(field.Value) {
			if match, err := check(token); err != nil {
				return err
			} else if match {
				return sensitive("configured value")
			}
			trimmed := strings.Trim(token, `"'()[]{}<>,;!?.`)
			if trimmed == "" || trimmed == token {
				continue
			}
			if match, err := check(trimmed); err != nil {
				return err
			} else if match {
				return sensitive("configured value")
			}
		}
		for _, line := range strings.Split(field.Value, "\n") {
			if _, rest, ok := strings.Cut(line, ":"); ok {
				span := strings.TrimSpace(rest)
				if span != "" {
					if match, err := check(span); err != nil {
						return err
					} else if match {
						return sensitive("configured value")
					}
				}
			}
		}
		for _, span := range guardURLPattern.FindAllString(field.Value, -1) {
			span = strings.TrimRight(span, ".,;!?)]}>")
			if match, err := check(span); err != nil {
				return err
			} else if match {
				return sensitive("configured value")
			}
		}
	}
	return nil
}

type CompositeGuard struct{ members []ContentGuard }

func NewCompositeGuard(members ...ContentGuard) *CompositeGuard {
	copied := make([]ContentGuard, len(members))
	copy(copied, members)
	return &CompositeGuard{members: copied}
}

func (guard *CompositeGuard) Check(ctx context.Context, input GuardInput) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if guard == nil || len(guard.members) == 0 {
		return ErrUnavailable
	}
	if err := validateGuardInput(input); err != nil {
		return err
	}
	for _, member := range guard.members {
		if member == nil {
			return ErrUnavailable
		}
		err := member.Check(ctx, input)
		if err == nil {
			continue
		}
		if ctxErr := ctx.Err(); ctxErr != nil {
			return ctxErr
		}
		switch {
		case errors.Is(err, context.Canceled):
			return context.Canceled
		case errors.Is(err, context.DeadlineExceeded):
			return context.DeadlineExceeded
		case errors.Is(err, ErrSensitiveMemory):
			return ErrSensitiveMemory
		case errors.Is(err, ErrUnavailable):
			return ErrUnavailable
		default:
			return ErrUnavailable
		}
	}
	return nil
}

func GuardRecord(ctx context.Context, guard ContentGuard, record Record) error {
	fields := make([]GuardField, 0, 16+len(record.Labels)+len(record.Metadata)*2+len(record.Source.MessageIDs))
	appendRecordFields(&fields, record, "record ")
	return runGuard(ctx, guard, fields)
}

func GuardCandidate(ctx context.Context, guard ContentGuard, candidate Candidate) error {
	fields := make([]GuardField, 0, 24+len(candidate.Proposed.Labels)+len(candidate.Proposed.Metadata)*2+len(candidate.Proposed.Source.MessageIDs))
	appendCandidateFields(&fields, candidate)
	return runGuard(ctx, guard, fields)
}

func GuardObservation(ctx context.Context, guard ContentGuard, observation Observation) error {
	fields := make([]GuardField, 0, 4+len(observation.MessageIDs)+len(observation.ToolFacts)*2)
	appendField(&fields, "observation ID", observation.ID, true)
	appendField(&fields, "observation user text", observation.UserText, false)
	appendField(&fields, "observation assistant text", observation.AssistantText, false)
	appendField(&fields, "observation session ID", observation.SessionID, true)
	for _, id := range observation.MessageIDs {
		appendField(&fields, "observation message ID", id, true)
	}
	for _, fact := range observation.ToolFacts {
		appendField(&fields, "tool name", fact.ToolName, false)
		appendField(&fields, "tool fact text", fact.Text, false)
	}
	return runGuard(ctx, guard, fields)
}

func GuardObservationCommit(ctx context.Context, guard ContentGuard, commit ObservationCommit) error {
	fields := make([]GuardField, 0, 1+len(commit.Candidates)*16)
	appendField(&fields, "observation ID", commit.ObservationID, true)
	for _, candidate := range commit.Candidates {
		appendCandidateFields(&fields, candidate)
	}
	return runGuard(ctx, guard, fields)
}

func GuardObservationReceipt(ctx context.Context, guard ContentGuard, receipt ObservationReceipt) error {
	fields := make([]GuardField, 0, 1+len(receipt.CandidateIDs))
	appendField(&fields, "observation ID", receipt.ObservationID, true)
	for _, id := range receipt.CandidateIDs {
		appendField(&fields, "candidate ID", id, true)
	}
	return runGuard(ctx, guard, fields)
}

func appendCandidateFields(fields *[]GuardField, candidate Candidate) {
	appendField(fields, "candidate ID", candidate.ID, true)
	appendRecordFields(fields, candidate.Proposed, "candidate proposed ")
	appendField(fields, "candidate action", string(candidate.Action), false)
	appendField(fields, "candidate target ID", candidate.TargetID, true)
	appendField(fields, "candidate reason", candidate.Reason, false)
	appendField(fields, "candidate state", string(candidate.State), false)
	appendField(fields, "candidate decision source", string(candidate.DecisionSource), false)
	appendField(fields, "candidate result record ID", candidate.ResultRecordID, true)
}

func appendRecordFields(fields *[]GuardField, record Record, prefix string) {
	appendField(fields, prefix+"ID", record.ID, true)
	appendField(fields, prefix+"scope namespace", record.Scope.Namespace, false)
	appendField(fields, prefix+"scope ID", record.Scope.ID, true)
	appendField(fields, prefix+"kind", record.Kind, false)
	appendField(fields, prefix+"key", record.Key, false)
	appendField(fields, prefix+"text", record.Text, false)
	for _, label := range record.Labels {
		appendField(fields, prefix+"label", label, false)
	}
	for key, value := range record.Metadata {
		appendField(fields, prefix+"metadata key", key, false)
		appendField(fields, prefix+"metadata value", value, false)
	}
	appendField(fields, prefix+"source origin", string(record.Source.Origin), false)
	appendField(fields, prefix+"source session ID", record.Source.SessionID, true)
	for _, id := range record.Source.MessageIDs {
		appendField(fields, prefix+"source message ID", id, true)
	}
	appendField(fields, prefix+"source observation ID", record.Source.ObservationID, true)
	appendField(fields, prefix+"source decision source", string(record.Source.DecisionSource), false)
}

func appendField(fields *[]GuardField, name, value string, opaque bool) {
	*fields = append(*fields, GuardField{Name: name, Value: value, Opaque: opaque})
}

func runGuard(ctx context.Context, guard ContentGuard, fields []GuardField) error {
	input := GuardInput{Fields: fields}
	if err := ctx.Err(); err != nil {
		return err
	}
	if err := validateGuardInput(input); err != nil {
		return err
	}
	if guard == nil {
		return ErrUnavailable
	}
	return guard.Check(ctx, input)
}
