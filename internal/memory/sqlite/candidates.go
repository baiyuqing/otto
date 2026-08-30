package sqlite

import (
	"bytes"
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/json"
	"errors"
	"math"
	"strconv"
	"strings"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
)

const (
	maxProposedJSONBytes  = 32 * 1024
	maxObservationIDsJSON = 1024
)

type proposedRecordJSON struct {
	ScopeNamespace string            `json:"scope_namespace"`
	ScopeID        string            `json:"scope_id"`
	Kind           string            `json:"kind"`
	Key            string            `json:"key"`
	Text           string            `json:"text"`
	Labels         []string          `json:"labels"`
	Metadata       map[string]string `json:"metadata"`
	Source         json.RawMessage   `json:"source"`
	Confidence     float64           `json:"confidence"`
	Expiry         *string           `json:"expiry"`
}

func normalizeProposedCollections(record *memory.Record) {
	if record.Labels == nil {
		record.Labels = make([]string, 0)
	}
	if record.Metadata == nil {
		record.Metadata = make(map[string]string)
	}
	if record.Source.MessageIDs == nil {
		record.Source.MessageIDs = make([]string, 0)
	}
}

func encodeProposedRecord(record memory.Record) ([]byte, error) {
	normalizeProposedCollections(&record)
	var source []byte
	var err error
	if record.Source.Origin == "" && record.Source.SessionID == "" && len(record.Source.MessageIDs) == 0 && record.Source.ObservationID == "" && record.Source.DecisionAt == nil && record.Source.DecisionSource == "" {
		source = []byte("{}")
	} else {
		wire := provenanceJSON{
			Origin: record.Source.Origin, SessionID: record.Source.SessionID, MessageIDs: record.Source.MessageIDs,
			ObservationID: record.Source.ObservationID, DecisionSource: record.Source.DecisionSource,
		}
		if record.Source.DecisionAt != nil {
			value := formatTimestamp(*record.Source.DecisionAt)
			wire.DecisionAt = &value
		}
		source, err = json.Marshal(wire)
		if err != nil || len(source) > maxSourceJSONBytes {
			return nil, memory.ErrCorrupt
		}
	}
	var expiry *string
	if record.ExpiresAt != nil {
		value := formatTimestamp(*record.ExpiresAt)
		expiry = &value
	}
	wire := proposedRecordJSON{
		ScopeNamespace: record.Scope.Namespace, ScopeID: record.Scope.ID, Kind: record.Kind, Key: record.Key,
		Text: record.Text, Labels: record.Labels, Metadata: record.Metadata, Source: source,
		Confidence: record.Confidence, Expiry: expiry,
	}
	raw, err := json.Marshal(wire)
	if err != nil || len(raw) > maxProposedJSONBytes {
		return nil, memory.ErrCorrupt
	}
	return raw, nil
}

func decodeProposedRecord(raw []byte) (memory.Record, error) {
	if len(raw) == 0 || len(raw) > maxProposedJSONBytes {
		return memory.Record{}, memory.ErrCorrupt
	}
	var wire proposedRecordJSON
	if err := strictJSON(raw, &wire); err != nil || wire.Labels == nil || wire.Metadata == nil || wire.Source == nil {
		return memory.Record{}, memory.ErrCorrupt
	}
	var source memory.Provenance
	var err error
	if bytes.Equal(wire.Source, []byte("{}")) {
		source = memory.Provenance{}
	} else {
		source, err = decodeProvenance(wire.Source)
		if err != nil {
			return memory.Record{}, memory.ErrCorrupt
		}
	}
	record := memory.Record{
		Scope: memory.Scope{Namespace: wire.ScopeNamespace, ID: wire.ScopeID}, Kind: wire.Kind, Key: wire.Key,
		Text: wire.Text, Labels: wire.Labels, Metadata: wire.Metadata, Source: source, Confidence: wire.Confidence,
	}
	if wire.Expiry != nil {
		value, err := parseTimestamp(*wire.Expiry)
		if err != nil {
			return memory.Record{}, memory.ErrCorrupt
		}
		record.ExpiresAt = &value
	}
	canonical, err := encodeProposedRecord(record)
	if err != nil || !bytes.Equal(raw, canonical) || !validStoredFloat(record.Confidence) {
		return memory.Record{}, memory.ErrCorrupt
	}
	return record, nil
}

var candidateProjectionSafety = strings.Join([]string{
	"typeof(id)='text' AND length(CAST(id AS BLOB)) BETWEEN 1 AND " + strconv.Itoa(memory.MaxIDBytes),
	"typeof(scope_namespace)='text' AND length(CAST(scope_namespace AS BLOB)) BETWEEN 1 AND " + strconv.Itoa(memory.MaxNamespaceBytes),
	"typeof(scope_id)='text' AND length(CAST(scope_id AS BLOB)) BETWEEN 1 AND " + strconv.Itoa(memory.MaxScopeIDBytes),
	"typeof(action)='text' AND action IN ('create','update','forget')",
	"typeof(target_id)='text' AND length(CAST(target_id AS BLOB))<=" + strconv.Itoa(memory.MaxIDBytes),
	"typeof(base_revision)='integer' AND base_revision BETWEEN 0 AND " + strconv.FormatInt(math.MaxInt64, 10),
	"(observation_id IS NULL OR (typeof(observation_id)='text' AND length(CAST(observation_id AS BLOB)) BETWEEN 1 AND " + strconv.Itoa(memory.MaxIDBytes) + "))",
	"typeof(proposed_json)='text' AND length(CAST(proposed_json AS BLOB)) BETWEEN 1 AND " + strconv.Itoa(maxProposedJSONBytes),
	"json_valid(CASE WHEN typeof(proposed_json)='text' AND length(CAST(proposed_json AS BLOB)) BETWEEN 1 AND " + strconv.Itoa(maxProposedJSONBytes) + " THEN proposed_json END)",
	"json_type(CASE WHEN typeof(proposed_json)='text' AND length(CAST(proposed_json AS BLOB)) BETWEEN 1 AND " + strconv.Itoa(maxProposedJSONBytes) + " THEN proposed_json END)='object'",
	"typeof(reason)='text' AND length(CAST(reason AS BLOB))<=" + strconv.Itoa(memory.MaxReasonBytes),
	"typeof(state)='text' AND state IN ('pending','accepted','rejected')",
	"typeof(created_at)='text' AND length(CAST(created_at AS BLOB))=" + strconv.Itoa(len(timestampLayout)),
	"(decided_at IS NULL OR (typeof(decided_at)='text' AND length(CAST(decided_at AS BLOB))=" + strconv.Itoa(len(timestampLayout)) + "))",
	"typeof(decision_source)='text' AND length(CAST(decision_source AS BLOB))<=" + strconv.Itoa(memory.MaxKindBytes),
	"typeof(result_record_id)='text' AND length(CAST(result_record_id AS BLOB))<=" + strconv.Itoa(memory.MaxIDBytes),
	"typeof(result_revision)='integer' AND result_revision BETWEEN 0 AND " + strconv.FormatInt(math.MaxInt64, 10),
}, " AND ")

func candidateProjection() string {
	columns := []string{"id", "scope_namespace", "scope_id", "action", "target_id", "base_revision", "observation_id", "proposed_json", "reason", "state", "created_at", "decided_at", "decision_source", "result_record_id", "result_revision"}
	parts := make([]string, 0, len(columns)+1)
	parts = append(parts, "CASE WHEN "+candidateProjectionSafety+" THEN 1 ELSE 0 END")
	for _, column := range columns {
		parts = append(parts, "CASE WHEN "+candidateProjectionSafety+" THEN "+column+" END")
	}
	return strings.Join(parts, ",")
}

type candidateSnapshot struct {
	candidate     memory.Candidate
	observationID string
	digest        [sha256.Size]byte
}

func decodeCandidateRow(scanner rowScanner) (candidateSnapshot, error) {
	var valid, baseRevision, resultRevision sql.NullInt64
	var id, namespace, scopeID, action, targetID, observationID, proposed, reason, state sql.NullString
	var createdAt, decidedAt, decisionSource, resultRecordID sql.NullString
	if err := scanner.Scan(&valid, &id, &namespace, &scopeID, &action, &targetID, &baseRevision, &observationID, &proposed, &reason, &state, &createdAt, &decidedAt, &decisionSource, &resultRecordID, &resultRevision); err != nil {
		return candidateSnapshot{}, recordScanError{err: err}
	}
	mandatory := []sql.NullString{id, namespace, scopeID, action, targetID, proposed, reason, state, createdAt, decisionSource, resultRecordID}
	if !valid.Valid || valid.Int64 != 1 || !baseRevision.Valid || !resultRevision.Valid {
		return candidateSnapshot{}, memory.ErrCorrupt
	}
	for _, value := range mandatory {
		if !value.Valid {
			return candidateSnapshot{}, memory.ErrCorrupt
		}
	}
	proposedRecord, err := decodeProposedRecord([]byte(proposed.String))
	if err != nil {
		return candidateSnapshot{}, memory.ErrCorrupt
	}
	created, err := parseTimestamp(createdAt.String)
	if err != nil {
		return candidateSnapshot{}, memory.ErrCorrupt
	}
	candidate := memory.Candidate{
		ID: id.String, Proposed: proposedRecord, Action: memory.CandidateAction(action.String), TargetID: targetID.String,
		BaseRevision: uint64(baseRevision.Int64), Reason: reason.String, State: memory.CandidateState(state.String), CreatedAt: created,
		DecisionSource: memory.Origin(decisionSource.String), ResultRecordID: resultRecordID.String, ResultRevision: uint64(resultRevision.Int64),
	}
	if decidedAt.Valid {
		value, err := parseTimestamp(decidedAt.String)
		if err != nil {
			return candidateSnapshot{}, memory.ErrCorrupt
		}
		candidate.DecidedAt = &value
	}
	// Schema v1 requires an internal result ID for every accepted row. The
	// neutral API exposes it only for create; update/forget already retain TargetID.
	if candidate.State == memory.CandidateAccepted && candidate.Action != memory.CandidateCreate {
		if candidate.ResultRecordID != candidate.TargetID {
			return candidateSnapshot{}, memory.ErrCorrupt
		}
		candidate.ResultRecordID = ""
	}
	if candidate.Proposed.Scope != (memory.Scope{Namespace: namespace.String, ID: scopeID.String}) || memory.ValidateCandidate(candidate) != nil {
		return candidateSnapshot{}, memory.ErrCorrupt
	}
	snapshot := candidateSnapshot{candidate: candidate}
	if observationID.Valid {
		snapshot.observationID = observationID.String
	}
	snapshot.digest, err = digestCandidateSnapshot(snapshot)
	if err != nil {
		return candidateSnapshot{}, err
	}
	return snapshot, nil
}

type candidateDigestJSON struct {
	ID             string                 `json:"id"`
	Proposed       json.RawMessage        `json:"proposed"`
	Action         memory.CandidateAction `json:"action"`
	TargetID       string                 `json:"target_id"`
	BaseRevision   uint64                 `json:"base_revision"`
	ObservationID  string                 `json:"observation_id"`
	Reason         string                 `json:"reason"`
	State          memory.CandidateState  `json:"state"`
	CreatedAt      string                 `json:"created_at"`
	DecidedAt      *string                `json:"decided_at"`
	DecisionSource memory.Origin          `json:"decision_source"`
	ResultRecordID string                 `json:"result_record_id"`
	ResultRevision uint64                 `json:"result_revision"`
}

func digestCandidateSnapshot(snapshot candidateSnapshot) ([sha256.Size]byte, error) {
	proposed, err := encodeProposedRecord(snapshot.candidate.Proposed)
	if err != nil {
		return [sha256.Size]byte{}, memory.ErrCorrupt
	}
	resultID := snapshot.candidate.ResultRecordID
	if snapshot.candidate.State == memory.CandidateAccepted && snapshot.candidate.Action != memory.CandidateCreate {
		resultID = snapshot.candidate.TargetID
	}
	wire := candidateDigestJSON{
		ID: snapshot.candidate.ID, Proposed: proposed, Action: snapshot.candidate.Action, TargetID: snapshot.candidate.TargetID,
		BaseRevision: snapshot.candidate.BaseRevision, ObservationID: snapshot.observationID, Reason: snapshot.candidate.Reason,
		State: snapshot.candidate.State, CreatedAt: formatTimestamp(snapshot.candidate.CreatedAt), DecisionSource: snapshot.candidate.DecisionSource,
		ResultRecordID: resultID, ResultRevision: snapshot.candidate.ResultRevision,
	}
	if snapshot.candidate.DecidedAt != nil {
		value := formatTimestamp(*snapshot.candidate.DecidedAt)
		wire.DecidedAt = &value
	}
	raw, err := json.Marshal(wire)
	if err != nil {
		return [sha256.Size]byte{}, memory.ErrCorrupt
	}
	return sha256.Sum256(raw), nil
}

func candidateInsertArguments(candidate memory.Candidate, observationID *string) ([]any, error) {
	proposed, err := encodeProposedRecord(candidate.Proposed)
	if err != nil {
		return nil, err
	}
	var observation any
	if observationID != nil {
		observation = *observationID
	}
	return []any{
		candidate.ID, candidate.Proposed.Scope.Namespace, candidate.Proposed.Scope.ID, string(candidate.Action), candidate.TargetID,
		candidate.BaseRevision, observation, string(proposed), candidate.Reason, string(candidate.State), formatTimestamp(candidate.CreatedAt),
		nil, "", "", 0,
	}, nil
}

const insertCandidateSQL = `INSERT INTO memory_candidates(
	id,scope_namespace,scope_id,action,target_id,base_revision,observation_id,proposed_json,reason,state,
	created_at,decided_at,decision_source,result_record_id,result_revision
) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`

func preflightCandidateBatchShape(candidates []memory.Candidate, allowEmpty bool) error {
	if len(candidates) > memory.MaxCandidateBatch || !allowEmpty && len(candidates) == 0 {
		return memory.ErrInvalidRequest
	}
	total := 0
	add := func(value string) error {
		if len(value) > memory.MaxCandidateBatchBytes-total {
			return memory.ErrInvalidRequest
		}
		total += len(value)
		return nil
	}
	for _, candidate := range candidates {
		if len(candidate.Proposed.Labels) > memory.MaxLabels || len(candidate.Proposed.Metadata) > memory.MaxMetadataEntries || len(candidate.Proposed.Source.MessageIDs) > memory.MaxProvenanceMessageIDs {
			return memory.ErrInvalidRequest
		}
		for _, value := range []string{candidate.ID, candidate.TargetID, candidate.Reason, candidate.Proposed.Scope.Namespace, candidate.Proposed.Scope.ID, candidate.Proposed.Kind, candidate.Proposed.Key, candidate.Proposed.Text, candidate.Proposed.Source.SessionID, candidate.Proposed.Source.ObservationID} {
			if err := add(value); err != nil {
				return err
			}
		}
		for _, value := range candidate.Proposed.Labels {
			if err := add(value); err != nil {
				return err
			}
		}
		for key, value := range candidate.Proposed.Metadata {
			if err := add(key); err != nil {
				return err
			}
			if err := add(value); err != nil {
				return err
			}
		}
		for _, value := range candidate.Proposed.Source.MessageIDs {
			if err := add(value); err != nil {
				return err
			}
		}
	}
	return nil
}

func prepareCandidateBatch(ctx context.Context, guard memory.ContentGuard, batch memory.ProposalBatch) (memory.ProposalBatch, error) {
	if err := preflightCandidateBatchShape(batch.Candidates, false); err != nil {
		return memory.ProposalBatch{}, err
	}
	batch = memory.CloneProposalBatch(batch)
	for index := range batch.Candidates {
		normalizeProposedCollections(&batch.Candidates[index].Proposed)
	}
	if err := memory.ValidateProposalBatch(batch); err != nil {
		return memory.ProposalBatch{}, err
	}
	seen := make(map[string]struct{}, len(batch.Candidates))
	for _, candidate := range batch.Candidates {
		if candidate.Proposed.Source.ObservationID != "" {
			return memory.ProposalBatch{}, memory.ErrInvalidRequest
		}
		if _, exists := seen[candidate.ID]; exists {
			return memory.ProposalBatch{}, memory.ErrConflict
		}
		seen[candidate.ID] = struct{}{}
		if err := memory.GuardCandidate(ctx, guard, candidate); err != nil {
			return memory.ProposalBatch{}, err
		}
	}
	return batch, nil
}

func (store *Store) Propose(ctx context.Context, input memory.ProposalBatch) (memory.CandidateBatch, error) {
	if err := ctx.Err(); err != nil {
		return memory.CandidateBatch{}, err
	}
	batch, err := prepareCandidateBatch(ctx, store.guard, input)
	if err != nil {
		return memory.CandidateBatch{}, err
	}
	ids := make([]string, len(batch.Candidates))
	for index, candidate := range batch.Candidates {
		ids[index] = candidate.ID
	}
	commitUnknown, err := memory.NewCommitUnknownError(memory.CommitPropose, ids)
	if err != nil {
		return memory.CandidateBatch{}, memory.ErrInvalidRequest
	}
	err = store.withWrite(ctx, memory.CommitPropose, ids, func(ctx context.Context, conn *sql.Conn) error {
		for _, candidate := range batch.Candidates {
			arguments, err := candidateInsertArguments(candidate, nil)
			if err != nil {
				return err
			}
			if _, err := conn.ExecContext(ctx, insertCandidateSQL, arguments...); err != nil {
				return err
			}
		}
		if err := bumpGeneration(ctx, conn); err != nil {
			return err
		}
		return candidateBeforeCommitHook()
	})
	if err != nil {
		return memory.CandidateBatch{}, err
	}
	if err := store.publishCandidateCommit(commitUnknown); err != nil {
		return memory.CandidateBatch{}, err
	}
	return memory.CloneCandidateBatch(memory.CandidateBatch(batch)), nil
}

func (store *Store) publishCandidateCommit(commitUnknown *memory.CommitUnknownError) error {
	if hook := loadTestHooks().afterCandidateCommit; hook != nil {
		if err := hook(); err != nil {
			store.poison()
			return commitUnknown
		}
	}
	return nil
}

func candidateBeforeCommitHook() error {
	if hook := loadTestHooks().beforeCandidateCommit; hook != nil {
		return hook()
	}
	return nil
}

func (store *Store) guardFields(ctx context.Context, fields ...memory.GuardField) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if store.guard == nil {
		return memory.ErrUnavailable
	}
	return store.guard.Check(ctx, memory.GuardInput{Fields: fields})
}

func (store *Store) guardCandidateRef(ctx context.Context, ref memory.CandidateRef) error {
	return store.guardFields(ctx,
		memory.GuardField{Name: "candidate reference scope namespace", Value: ref.Scope.Namespace},
		memory.GuardField{Name: "candidate reference scope ID", Value: ref.Scope.ID, Opaque: true},
		memory.GuardField{Name: "candidate reference ID", Value: ref.ID, Opaque: true},
	)
}

func (store *Store) GetCandidate(ctx context.Context, ref memory.CandidateRef) (memory.Candidate, error) {
	if err := ctx.Err(); err != nil {
		return memory.Candidate{}, err
	}
	if err := memory.ValidateCandidateRef(ref); err != nil {
		return memory.Candidate{}, err
	}
	if err := store.guardCandidateRef(ctx, ref); err != nil {
		return memory.Candidate{}, err
	}
	snapshot, err := store.readCandidateSnapshot(ctx, ref, true)
	if err != nil {
		return memory.Candidate{}, err
	}
	if err := memory.GuardCandidate(ctx, store.guard, snapshot.candidate); err != nil {
		return memory.Candidate{}, err
	}
	return memory.CloneCandidate(snapshot.candidate), nil
}

func readCandidateSnapshotConn(ctx context.Context, conn *sql.Conn, ref memory.CandidateRef, verifyObservation bool) (candidateSnapshot, error) {
	query := "SELECT " + candidateProjection() + " FROM memory_candidates WHERE scope_namespace=? AND scope_id=? AND id=? LIMIT 1"
	snapshot, err := decodeCandidateRow(conn.QueryRowContext(ctx, query, ref.Scope.Namespace, ref.Scope.ID, ref.ID))
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return candidateSnapshot{}, memory.ErrNotFound
		}
		return candidateSnapshot{}, safeRecordReadError(ctx, err)
	}
	if snapshot.candidate.State == memory.CandidatePending {
		if snapshot.observationID == "" && snapshot.candidate.Proposed.Source.ObservationID != "" || snapshot.observationID != "" && snapshot.candidate.Proposed.Source.ObservationID != snapshot.observationID {
			return candidateSnapshot{}, memory.ErrCorrupt
		}
	}
	if verifyObservation && snapshot.observationID != "" {
		receipt, err := readObservationReceiptConn(ctx, conn, snapshot.observationID, true)
		if err != nil {
			if errors.Is(err, memory.ErrNotFound) {
				return candidateSnapshot{}, memory.ErrCorrupt
			}
			return candidateSnapshot{}, err
		}
		if !receiptContainsExactlyOnce(receipt, snapshot.candidate.ID) {
			return candidateSnapshot{}, memory.ErrCorrupt
		}
	}
	return snapshot, nil
}

func (store *Store) readCandidateSnapshot(ctx context.Context, ref memory.CandidateRef, verifyObservation bool) (candidateSnapshot, error) {
	done, err := store.admit()
	if err != nil {
		return candidateSnapshot{}, err
	}
	conn, err := store.borrowConnection(ctx)
	if err != nil {
		done()
		return candidateSnapshot{}, err
	}
	snapshot, readErr := readCandidateSnapshotConn(ctx, conn, ref, verifyObservation)
	store.returnConnection(conn)
	done()
	return snapshot, readErr
}

func (store *Store) ListCandidates(ctx context.Context, input memory.CandidateListRequest) (memory.CandidatePage, error) {
	if err := ctx.Err(); err != nil {
		return memory.CandidatePage{}, err
	}
	if len(input.Scopes) > memory.MaxRequestScopes || len(input.States) > memory.MaxRequestKinds || len(input.Cursor) > memory.MaxCursorBytes {
		return memory.CandidatePage{}, memory.ErrInvalidRequest
	}
	request := memory.CloneCandidateListRequest(input)
	if err := memory.ValidateCandidateListRequest(request); err != nil {
		return memory.CandidatePage{}, err
	}
	fields := make([]memory.GuardField, 0, len(request.Scopes)*2+len(request.States)+1)
	for _, scope := range request.Scopes {
		fields = append(fields,
			memory.GuardField{Name: "candidate list scope namespace", Value: scope.Namespace},
			memory.GuardField{Name: "candidate list scope ID", Value: scope.ID, Opaque: true},
		)
	}
	for _, state := range request.States {
		fields = append(fields, memory.GuardField{Name: "candidate list state", Value: string(state)})
	}
	fields = append(fields, memory.GuardField{Name: "candidate list cursor", Value: request.Cursor, Opaque: true})
	if err := store.guardFields(ctx, fields...); err != nil {
		return memory.CandidatePage{}, err
	}
	fingerprint, err := fingerprintCandidates(request)
	if err != nil {
		return memory.CandidatePage{}, memory.ErrInvalidCursor
	}
	cursor, cursorGeneration, err := decodeRecordCursor(request.Cursor, fingerprint)
	if err != nil {
		return memory.CandidatePage{}, memory.ErrInvalidCursor
	}
	done, err := store.admit()
	if err != nil {
		return memory.CandidatePage{}, err
	}
	conn, err := store.borrowConnection(ctx)
	if err != nil {
		done()
		return memory.CandidatePage{}, err
	}
	contaminated, err := beginReadTransaction(ctx, conn)
	if err != nil {
		if contaminated {
			store.quarantine(conn)
		} else {
			store.returnConnection(conn)
		}
		done()
		return memory.CandidatePage{}, err
	}
	generation, readErr := readGeneration(ctx, conn)
	if readErr == nil && request.Cursor != "" && generation != cursorGeneration {
		readErr = memory.ErrConflict
	}
	var snapshots []candidateSnapshot
	if readErr == nil && len(request.Scopes) != 0 {
		query, arguments := buildCandidateListQuery(request, cursor)
		rows, queryErr := conn.QueryContext(ctx, query, arguments...)
		if queryErr != nil {
			readErr = safeRecordReadError(ctx, queryErr)
		} else {
			for rows.Next() {
				if len(snapshots) >= memory.MaxPageSize+1 {
					readErr = memory.ErrCorrupt
					break
				}
				snapshot, decodeErr := decodeCandidateRow(rows)
				if decodeErr != nil {
					readErr = safeRecordReadError(ctx, decodeErr)
					break
				}
				if snapshot.observationID != "" {
					receipt, verifyErr := readObservationReceiptConn(ctx, conn, snapshot.observationID, true)
					if verifyErr != nil {
						if errors.Is(verifyErr, memory.ErrNotFound) {
							readErr = memory.ErrCorrupt
						} else {
							readErr = verifyErr
						}
						break
					}
					if !receiptContainsExactlyOnce(receipt, snapshot.candidate.ID) {
						readErr = memory.ErrCorrupt
						break
					}
				}
				snapshots = append(snapshots, snapshot)
			}
			if rowsErr := rows.Err(); readErr == nil && rowsErr != nil {
				readErr = safeRecordReadError(ctx, rowsErr)
			}
			if closeErr := rows.Close(); readErr == nil && closeErr != nil {
				readErr = safeRecordReadError(ctx, closeErr)
			}
		}
	}
	endErr := endReadTransaction(conn)
	if readErr == nil && endErr != nil {
		readErr = endErr
	}
	if endErr != nil {
		store.quarantine(conn)
	} else {
		store.returnConnection(conn)
	}
	done()
	if readErr != nil {
		return memory.CandidatePage{}, readErr
	}
	candidates := make([]memory.Candidate, len(snapshots))
	for index, snapshot := range snapshots {
		if err := memory.GuardCandidate(ctx, store.guard, snapshot.candidate); err != nil {
			return memory.CandidatePage{}, err
		}
		candidates[index] = snapshot.candidate
	}
	page := memory.CandidatePage{Candidates: candidates}
	if len(page.Candidates) > request.Limit {
		page.Candidates = page.Candidates[:request.Limit]
		last := page.Candidates[len(page.Candidates)-1]
		page.NextCursor, err = encodeRecordCursor(fingerprint, generation, formatTimestamp(last.CreatedAt), last.ID)
		if err != nil {
			return memory.CandidatePage{}, memory.ErrInvalidCursor
		}
	}
	return memory.CloneCandidatePage(page), nil
}

func buildCandidateListQuery(request memory.CandidateListRequest, cursor recordCursor) (string, []any) {
	clauses := make([]string, 0, 4)
	arguments := make([]any, 0, len(request.Scopes)*2+len(request.States)+4)
	scopes := make([]string, 0, len(request.Scopes))
	for _, scope := range request.Scopes {
		scopes = append(scopes, "(scope_namespace=? AND scope_id=?)")
		arguments = append(arguments, scope.Namespace, scope.ID)
	}
	clauses = append(clauses, "("+strings.Join(scopes, " OR ")+")")
	if len(request.States) != 0 {
		clauses = append(clauses, "state IN ("+placeholders(len(request.States))+")")
		for _, state := range request.States {
			arguments = append(arguments, state)
		}
	}
	if cursor.Version != 0 {
		clauses = append(clauses, "(created_at<? OR (created_at=? AND id>?))")
		arguments = append(arguments, cursor.UpdatedAt, cursor.UpdatedAt, cursor.ID)
	}
	arguments = append(arguments, request.Limit+1)
	return "SELECT " + candidateProjection() + " FROM memory_candidates WHERE " + strings.Join(clauses, " AND ") + " ORDER BY created_at DESC,id ASC LIMIT ?", arguments
}

var observationProjectionSafety = buildObservationProjectionSafety()

func buildObservationProjectionSafety() string {
	gate := "typeof(candidate_ids_json)='text' AND length(CAST(candidate_ids_json AS BLOB)) BETWEEN 2 AND " + strconv.Itoa(maxObservationIDsJSON)
	bounded := "CASE WHEN " + gate + " THEN candidate_ids_json END"
	valid := gate + " AND json_valid(" + bounded + ")"
	array := "CASE WHEN " + valid + " AND json_type(" + bounded + ")='array' THEN " + bounded + " ELSE '[]' END"
	return strings.Join([]string{
		"typeof(id)='text' AND length(CAST(id AS BLOB)) BETWEEN 1 AND " + strconv.Itoa(memory.MaxIDBytes),
		valid, "json_type(" + bounded + ")='array'", "json_array_length(" + array + ")<=" + strconv.Itoa(memory.MaxCandidateBatch),
		"NOT EXISTS (SELECT 1 FROM json_each(" + array + ") WHERE type<>'text' OR length(CAST(value AS BLOB)) NOT BETWEEN 1 AND " + strconv.Itoa(memory.MaxIDBytes) + ")",
		"typeof(created_at)='text' AND length(CAST(created_at AS BLOB))=" + strconv.Itoa(len(timestampLayout)),
	}, " AND ")
}

func observationProjection() string {
	columns := []string{"id", "candidate_ids_json", "created_at"}
	parts := []string{"CASE WHEN " + observationProjectionSafety + " THEN 1 ELSE 0 END"}
	for _, column := range columns {
		parts = append(parts, "CASE WHEN "+observationProjectionSafety+" THEN "+column+" END")
	}
	return strings.Join(parts, ",")
}

func decodeObservationRow(scanner rowScanner) (memory.ObservationReceipt, time.Time, error) {
	var valid sql.NullInt64
	var id, idsJSON, createdAt sql.NullString
	if err := scanner.Scan(&valid, &id, &idsJSON, &createdAt); err != nil {
		return memory.ObservationReceipt{}, time.Time{}, recordScanError{err: err}
	}
	if !valid.Valid || valid.Int64 != 1 || !id.Valid || !idsJSON.Valid || !createdAt.Valid {
		return memory.ObservationReceipt{}, time.Time{}, memory.ErrCorrupt
	}
	if len(idsJSON.String) > maxObservationIDsJSON {
		return memory.ObservationReceipt{}, time.Time{}, memory.ErrCorrupt
	}
	var ids []string
	if err := strictJSON([]byte(idsJSON.String), &ids); err != nil || ids == nil || len(ids) > memory.MaxCandidateBatch {
		return memory.ObservationReceipt{}, time.Time{}, memory.ErrCorrupt
	}
	canonical, err := json.Marshal(ids)
	if err != nil || !bytes.Equal(canonical, []byte(idsJSON.String)) {
		return memory.ObservationReceipt{}, time.Time{}, memory.ErrCorrupt
	}
	receipt := memory.ObservationReceipt{ObservationID: id.String, CandidateIDs: ids}
	if err := memory.ValidateObservationReceipt(receipt); err != nil {
		return memory.ObservationReceipt{}, time.Time{}, memory.ErrCorrupt
	}
	created, err := parseTimestamp(createdAt.String)
	if err != nil {
		return memory.ObservationReceipt{}, time.Time{}, memory.ErrCorrupt
	}
	return receipt, created, nil
}

func readObservationReceiptConn(ctx context.Context, conn *sql.Conn, id string, verify bool) (memory.ObservationReceipt, error) {
	query := "SELECT " + observationProjection() + " FROM memory_observations WHERE id=? LIMIT 1"
	receipt, _, err := decodeObservationRow(conn.QueryRowContext(ctx, query, id))
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return memory.ObservationReceipt{}, memory.ErrNotFound
		}
		return memory.ObservationReceipt{}, safeRecordReadError(ctx, err)
	}
	if verify {
		seen := make(map[string]struct{}, len(receipt.CandidateIDs))
		for _, candidateID := range receipt.CandidateIDs {
			if _, duplicate := seen[candidateID]; duplicate {
				return memory.ObservationReceipt{}, memory.ErrCorrupt
			}
			seen[candidateID] = struct{}{}
			var valid sql.NullInt64
			proposedGate := "typeof(proposed_json)='text' AND length(CAST(proposed_json AS BLOB)) BETWEEN 1 AND " + strconv.Itoa(maxProposedJSONBytes)
			gatedProposed := "CASE WHEN " + proposedGate + " THEN proposed_json END"
			gate := "typeof(id)='text' AND length(CAST(id AS BLOB)) BETWEEN 1 AND " + strconv.Itoa(memory.MaxIDBytes) +
				" AND typeof(observation_id)='text' AND length(CAST(observation_id AS BLOB)) BETWEEN 1 AND " + strconv.Itoa(memory.MaxIDBytes) +
				" AND typeof(state)='text' AND state IN ('pending','accepted','rejected') AND " + proposedGate + " AND json_valid(" + gatedProposed + ") AND json_type(" + gatedProposed + ")='object'" +
				" AND (state<>'pending' OR (json_type(" + gatedProposed + ",'$.source.observation_id')='text' AND json_extract(" + gatedProposed + ",'$.source.observation_id')=?))"
			if hook := loadTestHooks().beforeObservationCorrespondence; hook != nil {
				hook(observationCandidateCorrespondence)
			}
			scanErr := conn.QueryRowContext(ctx, "SELECT CASE WHEN "+gate+" AND observation_id=? THEN 1 END FROM memory_candidates WHERE id=? LIMIT 1", id, id, candidateID).Scan(&valid)
			if scanErr != nil {
				if errors.Is(scanErr, sql.ErrNoRows) {
					return memory.ObservationReceipt{}, memory.ErrCorrupt
				}
				return memory.ObservationReceipt{}, safeRecordReadError(ctx, scanErr)
			}
			if !valid.Valid || valid.Int64 != 1 {
				return memory.ObservationReceipt{}, memory.ErrCorrupt
			}
		}

		if hook := loadTestHooks().beforeObservationCorrespondence; hook != nil {
			hook(observationAssociationCorrespondence)
		}
		associationGate := "typeof(id)='text' AND length(CAST(id AS BLOB)) BETWEEN 1 AND " + strconv.Itoa(memory.MaxIDBytes) +
			" AND typeof(observation_id)='text' AND length(CAST(observation_id AS BLOB)) BETWEEN 1 AND " + strconv.Itoa(memory.MaxIDBytes)
		query := "SELECT CASE WHEN " + associationGate + " THEN id END FROM memory_candidates WHERE observation_id=? LIMIT " + strconv.Itoa(memory.MaxCandidateBatch+1)
		rows, queryErr := conn.QueryContext(ctx, query, id)
		if queryErr != nil {
			return memory.ObservationReceipt{}, safeRecordReadError(ctx, queryErr)
		}
		associated := make(map[string]struct{}, len(receipt.CandidateIDs))
		var correspondenceErr error
		for rows.Next() {
			var candidateID sql.NullString
			if scanErr := rows.Scan(&candidateID); scanErr != nil {
				correspondenceErr = safeRecordReadError(ctx, recordScanError{err: scanErr})
				break
			}
			if !candidateID.Valid || len(associated) >= memory.MaxCandidateBatch {
				correspondenceErr = memory.ErrCorrupt
				break
			}
			if _, duplicate := associated[candidateID.String]; duplicate {
				correspondenceErr = memory.ErrCorrupt
				break
			}
			associated[candidateID.String] = struct{}{}
		}
		if rowsErr := rows.Err(); correspondenceErr == nil && rowsErr != nil {
			correspondenceErr = safeRecordReadError(ctx, rowsErr)
		}
		if closeErr := rows.Close(); correspondenceErr == nil && closeErr != nil {
			correspondenceErr = safeRecordReadError(ctx, closeErr)
		}
		if correspondenceErr != nil {
			return memory.ObservationReceipt{}, correspondenceErr
		}
		if len(associated) != len(seen) {
			return memory.ObservationReceipt{}, memory.ErrCorrupt
		}
		for candidateID := range seen {
			if _, exists := associated[candidateID]; !exists {
				return memory.ObservationReceipt{}, memory.ErrCorrupt
			}
		}
	}
	return receipt, nil
}

func (store *Store) GetObservationReceipt(ctx context.Context, id string) (memory.ObservationReceipt, error) {
	if err := ctx.Err(); err != nil {
		return memory.ObservationReceipt{}, err
	}
	if !validCandidateOpaqueID(id) {
		return memory.ObservationReceipt{}, memory.ErrInvalidRequest
	}
	if err := store.guardFields(ctx, memory.GuardField{Name: "observation receipt ID", Value: id, Opaque: true}); err != nil {
		return memory.ObservationReceipt{}, err
	}
	done, err := store.admit()
	if err != nil {
		return memory.ObservationReceipt{}, err
	}
	conn, err := store.borrowConnection(ctx)
	if err != nil {
		done()
		return memory.ObservationReceipt{}, err
	}
	receipt, readErr := readObservationReceiptConn(ctx, conn, id, true)
	store.returnConnection(conn)
	done()
	if readErr != nil {
		return memory.ObservationReceipt{}, readErr
	}
	receipt.Existing = true
	if err := memory.GuardObservationReceipt(ctx, store.guard, receipt); err != nil {
		return memory.ObservationReceipt{}, err
	}
	return memory.CloneObservationReceipt(receipt), nil
}

func validCandidateOpaqueID(id string) bool {
	if len(id) == 0 || len(id) > memory.MaxIDBytes {
		return false
	}
	for _, char := range []byte(id) {
		if char >= 'a' && char <= 'z' || char >= 'A' && char <= 'Z' || char >= '0' && char <= '9' || strings.ContainsRune("._:-", rune(char)) {
			continue
		}
		return false
	}
	return true
}

func prepareObservationCommit(ctx context.Context, guard memory.ContentGuard, input memory.ObservationCommit) (memory.ObservationCommit, error) {
	if err := preflightCandidateBatchShape(input.Candidates, true); err != nil {
		return memory.ObservationCommit{}, err
	}
	commit := memory.CloneObservationCommit(input)
	if !validCandidateOpaqueID(commit.ObservationID) || commit.CreatedAt.IsZero() || commit.CreatedAt.Location() != time.UTC || commit.CreatedAt.Year() < 1 || commit.CreatedAt.Year() > 9999 || commit.CreatedAt != commit.CreatedAt.Round(0) {
		return memory.ObservationCommit{}, memory.ErrInvalidRequest
	}
	if len(commit.Candidates) > memory.MaxCandidateBatch {
		return memory.ObservationCommit{}, memory.ErrInvalidRequest
	}
	for index := range commit.Candidates {
		normalizeProposedCollections(&commit.Candidates[index].Proposed)
	}
	if len(commit.Candidates) != 0 {
		if err := memory.ValidateProposalBatch(memory.ProposalBatch{Candidates: commit.Candidates}); err != nil {
			return memory.ObservationCommit{}, err
		}
	}
	seen := make(map[string]struct{}, len(commit.Candidates))
	for _, candidate := range commit.Candidates {
		if _, duplicate := seen[candidate.ID]; duplicate {
			return memory.ObservationCommit{}, memory.ErrConflict
		}
		seen[candidate.ID] = struct{}{}
		if candidate.Proposed.Source.ObservationID != commit.ObservationID {
			return memory.ObservationCommit{}, memory.ErrInvalidRequest
		}
	}
	if err := memory.GuardObservationCommit(ctx, guard, commit); err != nil {
		return memory.ObservationCommit{}, err
	}
	for _, candidate := range commit.Candidates {
		if err := memory.GuardCandidate(ctx, guard, candidate); err != nil {
			return memory.ObservationCommit{}, err
		}
	}
	return commit, nil
}

func (store *Store) CommitObservation(ctx context.Context, input memory.ObservationCommit) (memory.ObservationReceipt, error) {
	if err := ctx.Err(); err != nil {
		return memory.ObservationReceipt{}, err
	}
	if !validCandidateOpaqueID(input.ObservationID) {
		return memory.ObservationReceipt{}, memory.ErrInvalidRequest
	}
	if existing, err := store.GetObservationReceipt(ctx, input.ObservationID); err == nil {
		return existing, nil
	} else if !errors.Is(err, memory.ErrNotFound) {
		return memory.ObservationReceipt{}, err
	}
	if hook := loadTestHooks().afterObservationInitialMiss; hook != nil {
		hook(store)
	}
	commit, err := prepareObservationCommit(ctx, store.guard, input)
	if err != nil {
		return memory.ObservationReceipt{}, err
	}
	ids := make([]string, len(commit.Candidates))
	for index, candidate := range commit.Candidates {
		ids[index] = candidate.ID
	}
	idsJSON, err := json.Marshal(ids)
	if err != nil || len(idsJSON) > maxObservationIDsJSON {
		return memory.ObservationReceipt{}, memory.ErrInvalidRequest
	}
	receipt := memory.ObservationReceipt{ObservationID: commit.ObservationID, CandidateIDs: ids}
	if err := memory.GuardObservationReceipt(ctx, store.guard, receipt); err != nil {
		return memory.ObservationReceipt{}, err
	}
	entityIDs := append([]string{commit.ObservationID}, ids...)
	commitUnknown, err := memory.NewCommitUnknownError(memory.CommitObserve, entityIDs)
	if err != nil {
		return memory.ObservationReceipt{}, memory.ErrInvalidRequest
	}
	var raced memory.ObservationReceipt
	err = store.withWrite(ctx, memory.CommitObserve, entityIDs, func(ctx context.Context, conn *sql.Conn) error {
		existing, readErr := readObservationReceiptConn(ctx, conn, commit.ObservationID, true)
		if readErr == nil {
			raced = existing
			return memory.ErrConflict // withWrite rolls the no-op transaction back.
		}
		if !errors.Is(readErr, memory.ErrNotFound) {
			return readErr
		}
		if _, err := conn.ExecContext(ctx, `INSERT INTO memory_observations(id,candidate_ids_json,created_at) VALUES(?,?,?)`, commit.ObservationID, string(idsJSON), formatTimestamp(commit.CreatedAt)); err != nil {
			return err
		}
		for _, candidate := range commit.Candidates {
			arguments, err := candidateInsertArguments(candidate, &commit.ObservationID)
			if err != nil {
				return err
			}
			if _, err := conn.ExecContext(ctx, insertCandidateSQL, arguments...); err != nil {
				return err
			}
		}
		verified, err := readObservationReceiptConn(ctx, conn, commit.ObservationID, true)
		if err != nil {
			if errors.Is(err, memory.ErrNotFound) {
				return memory.ErrCorrupt
			}
			return err
		}
		if !equalStrings(verified.CandidateIDs, ids) {
			return memory.ErrCorrupt
		}
		if err := bumpGeneration(ctx, conn); err != nil {
			return err
		}
		return candidateBeforeCommitHook()
	})
	if len(raced.CandidateIDs) != 0 || raced.ObservationID != "" {
		if err != memory.ErrConflict {
			return memory.ObservationReceipt{}, err
		}
		raced.Existing = true
		if guardErr := memory.GuardObservationReceipt(ctx, store.guard, raced); guardErr != nil {
			return memory.ObservationReceipt{}, guardErr
		}
		return memory.CloneObservationReceipt(raced), nil
	}
	if err != nil {
		return memory.ObservationReceipt{}, err
	}
	if err := store.publishCandidateCommit(commitUnknown); err != nil {
		return memory.ObservationReceipt{}, err
	}
	return memory.CloneObservationReceipt(receipt), nil
}

func receiptContainsExactlyOnce(receipt memory.ObservationReceipt, candidateID string) bool {
	count := 0
	for _, id := range receipt.CandidateIDs {
		if id == candidateID {
			count++
		}
	}
	return count == 1
}

func equalStrings(left, right []string) bool {
	if len(left) != len(right) {
		return false
	}
	for index := range left {
		if left[index] != right[index] {
			return false
		}
	}
	return true
}

func (store *Store) Review(ctx context.Context, input memory.StoreReviewRequest) (memory.ReviewResult, error) {
	if err := ctx.Err(); err != nil {
		return memory.ReviewResult{}, err
	}
	if err := memory.ValidateCandidateRef(input.Ref); err != nil {
		return memory.ReviewResult{}, err
	}
	if err := store.guardCandidateRef(ctx, input.Ref); err != nil {
		return memory.ReviewResult{}, err
	}
	guarded, err := store.readCandidateSnapshot(ctx, input.Ref, true)
	if err != nil {
		return memory.ReviewResult{}, err
	}
	if err := memory.GuardCandidate(ctx, store.guard, guarded.candidate); err != nil {
		return memory.ReviewResult{}, err
	}
	if guarded.candidate.State != memory.CandidatePending {
		return memory.ReviewResult{}, memory.ErrConflict
	}
	if input.Edited != nil {
		if len(input.Edited.Labels) > memory.MaxLabels || len(input.Edited.Metadata) > memory.MaxMetadataEntries || len(input.Edited.Source.MessageIDs) > memory.MaxProvenanceMessageIDs {
			return memory.ReviewResult{}, memory.ErrInvalidRequest
		}
	}
	request := memory.CloneStoreReviewRequest(input)
	if request.Edited != nil {
		normalizeProposedCollections(request.Edited)
	}
	if err := memory.ValidateStoreReviewRequest(request, guarded.candidate); err != nil {
		return memory.ReviewResult{}, err
	}
	reviewFields := []memory.GuardField{
		{Name: "review result record ID", Value: request.ResultRecordID, Opaque: true},
		{Name: "review decision", Value: string(request.Decision)},
		{Name: "review decision source", Value: string(request.DecisionSource)},
	}
	if err := store.guardFields(ctx, reviewFields...); err != nil {
		return memory.ReviewResult{}, err
	}
	if request.Edited != nil {
		if err := memory.GuardRecord(ctx, store.guard, *request.Edited); err != nil {
			return memory.ReviewResult{}, err
		}
	}

	var guardedTarget mutationSnapshot
	if guarded.candidate.Action != memory.CandidateCreate {
		guardedTarget, err = store.readMutationSnapshot(ctx, memory.RecordRef{Scope: guarded.candidate.Proposed.Scope, ID: guarded.candidate.TargetID})
		if err != nil {
			return memory.ReviewResult{}, err
		}
		if guardedTarget.tombstone != nil {
			return memory.ReviewResult{}, conflictRecord(guarded.candidate.TargetID, guarded.candidate.BaseRevision, guardedTarget.tombstone.Revision)
		}
		if request.Decision == memory.ReviewAccept {
			expected := guarded.candidate.BaseRevision
			if request.TargetRevision != nil {
				expected = *request.TargetRevision
			}
			if guardedTarget.record.Revision != expected {
				return memory.ReviewResult{}, conflictRecord(guarded.candidate.TargetID, expected, guardedTarget.record.Revision)
			}
		}
	}

	result := memory.ReviewResult{}
	decidedCandidate := memory.CloneCandidate(guarded.candidate)
	decidedCandidate.State = memory.CandidateRejected
	decidedCandidate.DecidedAt = timePointer(request.DecidedAt)
	decidedCandidate.DecisionSource = request.DecisionSource
	decidedCandidate.Reason = ""
	decidedCandidate.Proposed = clearedProposed(guarded.candidate.Proposed.Scope)

	var desiredRecord *memory.Record
	var desiredTombstone *memory.Tombstone
	if request.Decision == memory.ReviewAccept {
		decidedCandidate.State = memory.CandidateAccepted
		switch guarded.candidate.Action {
		case memory.CandidateCreate:
			desired := acceptedRecordContent(guarded.candidate, request)
			desired.ID, desired.Revision = request.ResultRecordID, 1
			desired.CreatedAt, desired.UpdatedAt = request.DecidedAt, request.DecidedAt
			if err := memory.ValidateRecord(desired); err != nil {
				return memory.ReviewResult{}, err
			}
			if err := memory.GuardRecord(ctx, store.guard, desired); err != nil {
				return memory.ReviewResult{}, err
			}
			desiredRecord = &desired
			decidedCandidate.ResultRecordID, decidedCandidate.ResultRevision = desired.ID, desired.Revision
		case memory.CandidateUpdate:
			desired := acceptedRecordContent(guarded.candidate, request)
			target := *guardedTarget.record
			if request.DecidedAt.Before(target.UpdatedAt) || target.Revision >= math.MaxInt64 {
				return memory.ReviewResult{}, memory.ErrInvalidRequest
			}
			desired.ID, desired.Revision = target.ID, target.Revision+1
			desired.CreatedAt, desired.UpdatedAt = target.CreatedAt, request.DecidedAt
			if err := memory.ValidateRecord(desired); err != nil {
				return memory.ReviewResult{}, err
			}
			if err := memory.GuardRecord(ctx, store.guard, desired); err != nil {
				return memory.ReviewResult{}, err
			}
			desiredRecord = &desired
			decidedCandidate.ResultRevision = desired.Revision
		case memory.CandidateForget:
			target := *guardedTarget.record
			if request.DecidedAt.Before(target.UpdatedAt) || target.Revision >= math.MaxInt64 {
				return memory.ReviewResult{}, memory.ErrInvalidRequest
			}
			desired := memory.Tombstone{ID: target.ID, Scope: target.Scope, Revision: target.Revision + 1, CreatedAt: target.CreatedAt, UpdatedAt: request.DecidedAt, ForgottenAt: request.DecidedAt}
			if err := memory.ValidateTombstone(desired); err != nil {
				return memory.ReviewResult{}, err
			}
			if err := store.guardTombstone(ctx, desired); err != nil {
				return memory.ReviewResult{}, err
			}
			desiredTombstone = &desired
			decidedCandidate.ResultRevision = desired.Revision
		}
	}
	if err := memory.ValidateCandidate(decidedCandidate); err != nil {
		return memory.ReviewResult{}, memory.ErrCorrupt
	}
	if err := memory.GuardCandidate(ctx, store.guard, decidedCandidate); err != nil {
		return memory.ReviewResult{}, err
	}

	entityIDs := []string{guarded.candidate.ID}
	if desiredRecord != nil {
		entityIDs = append(entityIDs, desiredRecord.ID)
	} else if desiredTombstone != nil {
		entityIDs = append(entityIDs, desiredTombstone.ID)
	}
	commitUnknown, err := memory.NewCommitUnknownError(memory.CommitReview, entityIDs)
	if err != nil {
		return memory.ReviewResult{}, memory.ErrInvalidRequest
	}
	err = store.withWrite(ctx, memory.CommitReview, entityIDs, func(ctx context.Context, conn *sql.Conn) error {
		current, err := readCandidateSnapshotConn(ctx, conn, request.Ref, true)
		if err != nil {
			return err
		}
		if current.candidate.State != memory.CandidatePending || current.digest != guarded.digest {
			return memory.ErrConflict
		}
		if guarded.candidate.Action != memory.CandidateCreate {
			if err := compareReviewTarget(ctx, conn, guardedTarget, guarded.candidate, guardedTarget.record.Revision); err != nil {
				return err
			}
		}
		if desiredRecord != nil && guarded.candidate.Action == memory.CandidateCreate {
			if err := insertAcceptedRecord(ctx, conn, *desiredRecord); err != nil {
				return err
			}
		} else if desiredRecord != nil {
			if err := updateAcceptedRecord(ctx, conn, *desiredRecord, desiredRecord.Revision-1); err != nil {
				return err
			}
		} else if desiredTombstone != nil {
			if err := forgetAcceptedRecord(ctx, conn, *desiredTombstone, desiredTombstone.Revision-1); err != nil {
				return err
			}
		}
		if hook := loadTestHooks().beforeCandidateDecision; hook != nil {
			if err := hook(); err != nil {
				return err
			}
		}
		cleared, err := encodeProposedRecord(decidedCandidate.Proposed)
		if err != nil {
			return err
		}
		storedResultID := decidedCandidate.ResultRecordID
		if decidedCandidate.State == memory.CandidateAccepted && decidedCandidate.Action != memory.CandidateCreate {
			storedResultID = decidedCandidate.TargetID
		}
		updated, err := conn.ExecContext(ctx, `UPDATE memory_candidates SET proposed_json=?,reason='',state=?,decided_at=?,decision_source=?,result_record_id=?,result_revision=? WHERE id=? AND scope_namespace=? AND scope_id=? AND state='pending'`,
			string(cleared), string(decidedCandidate.State), formatTimestamp(request.DecidedAt), string(request.DecisionSource), storedResultID,
			decidedCandidate.ResultRevision, decidedCandidate.ID, decidedCandidate.Proposed.Scope.Namespace, decidedCandidate.Proposed.Scope.ID)
		if err != nil {
			return err
		}
		changed, err := updated.RowsAffected()
		if err != nil || changed != 1 {
			return memory.ErrConflict
		}
		if err := bumpGeneration(ctx, conn); err != nil {
			return err
		}
		return candidateBeforeCommitHook()
	})
	if err != nil {
		var conflict *memory.ConflictError
		if errors.As(err, &conflict) {
			return memory.ReviewResult{}, conflict
		}
		if errors.Is(err, memory.ErrConflict) {
			return memory.ReviewResult{}, memory.ErrConflict
		}
		return memory.ReviewResult{}, err
	}
	if err := store.publishCandidateCommit(commitUnknown); err != nil {
		return memory.ReviewResult{}, err
	}
	result.Candidate = decidedCandidate
	result.Record = desiredRecord
	result.Tombstone = desiredTombstone
	return memory.CloneReviewResult(result), nil
}

func acceptedRecordContent(candidate memory.Candidate, request memory.StoreReviewRequest) memory.Record {
	value := memory.CloneRecord(candidate.Proposed)
	if request.Edited != nil {
		value = memory.CloneRecord(*request.Edited)
	}
	value.Scope = candidate.Proposed.Scope
	value.Source.DecisionAt = timePointer(request.DecidedAt)
	value.Source.DecisionSource = request.DecisionSource
	normalizeRecordCollections(&value)
	return value
}

func timePointer(value time.Time) *time.Time { return &value }

func clearedProposed(scope memory.Scope) memory.Record {
	return memory.Record{Scope: scope, Labels: make([]string, 0), Metadata: make(map[string]string)}
}

func compareReviewTarget(ctx context.Context, conn *sql.Conn, guarded mutationSnapshot, candidate memory.Candidate, expected uint64) error {
	current, err := readMutationSnapshotConn(ctx, conn, memory.RecordRef{Scope: candidate.Proposed.Scope, ID: candidate.TargetID})
	if err != nil {
		return err
	}
	if current.tombstone != nil {
		return conflictRecord(candidate.TargetID, expected, current.tombstone.Revision)
	}
	if current.record.Revision != expected || current.digest != guarded.digest {
		return conflictRecord(candidate.TargetID, expected, current.record.Revision)
	}
	return nil
}

func insertAcceptedRecord(ctx context.Context, conn *sql.Conn, record memory.Record) error {
	encoded, err := encodeRecord(record)
	if err != nil {
		return err
	}
	idGate := "typeof(id)='text' AND length(CAST(id AS BLOB)) BETWEEN 1 AND " + strconv.Itoa(memory.MaxIDBytes)
	var existingID sql.NullString
	err = conn.QueryRowContext(ctx, "SELECT CASE WHEN "+idGate+" THEN id END FROM memory_records WHERE id=? LIMIT 1", record.ID).Scan(&existingID)
	if err == nil {
		if !existingID.Valid {
			return memory.ErrCorrupt
		}
		return memory.ErrConflict
	}
	if !errors.Is(err, sql.ErrNoRows) {
		return err
	}
	if err := requireFTSRowCount(ctx, conn, record.ID, 0); err != nil {
		return err
	}
	if _, err := conn.ExecContext(ctx, `INSERT INTO memory_records(id,scope_namespace,scope_id,kind,semantic_key,text_value,labels_json,metadata_json,source_json,confidence,revision,created_at,updated_at,expires_at,state,forgotten_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,'active',NULL)`,
		record.ID, record.Scope.Namespace, record.Scope.ID, record.Kind, record.Key, record.Text, string(encoded.labels), string(encoded.metadata), string(encoded.source), record.Confidence, record.Revision, encoded.created, encoded.updated, encoded.expires); err != nil {
		return err
	}
	if _, err := conn.ExecContext(ctx, `INSERT INTO memory_records_fts(record_id,text_value,kind,semantic_key,labels) VALUES(?,?,?,?,?)`, record.ID, record.Text, record.Kind, record.Key, ftsLabels(record.Labels)); err != nil {
		return err
	}
	return requireFTSRowCount(ctx, conn, record.ID, 1)
}

func updateAcceptedRecord(ctx context.Context, conn *sql.Conn, record memory.Record, expected uint64) error {
	encoded, err := encodeRecord(record)
	if err != nil {
		return err
	}
	result, err := conn.ExecContext(ctx, `UPDATE memory_records SET kind=?,semantic_key=?,text_value=?,labels_json=?,metadata_json=?,source_json=?,confidence=?,revision=?,updated_at=?,expires_at=? WHERE id=? AND scope_namespace=? AND scope_id=? AND state='active' AND revision=?`,
		record.Kind, record.Key, record.Text, string(encoded.labels), string(encoded.metadata), string(encoded.source), record.Confidence, record.Revision, encoded.updated, encoded.expires,
		record.ID, record.Scope.Namespace, record.Scope.ID, expected)
	if err != nil {
		return err
	}
	changed, err := result.RowsAffected()
	if err != nil {
		return err
	}
	if changed != 1 {
		return classifyConditionalMiss(ctx, conn, memory.RecordRef{Scope: record.Scope, ID: record.ID}, expected)
	}
	return replaceFTS(ctx, conn, record)
}

func forgetAcceptedRecord(ctx context.Context, conn *sql.Conn, tombstone memory.Tombstone, expected uint64) error {
	result, err := conn.ExecContext(ctx, `UPDATE memory_records SET kind='',semantic_key='',text_value='',labels_json='[]',metadata_json='{}',source_json='{}',confidence=0.0,revision=?,updated_at=?,expires_at=NULL,state='tombstone',forgotten_at=? WHERE id=? AND scope_namespace=? AND scope_id=? AND state='active' AND revision=?`,
		tombstone.Revision, formatTimestamp(tombstone.UpdatedAt), formatTimestamp(tombstone.ForgottenAt), tombstone.ID, tombstone.Scope.Namespace, tombstone.Scope.ID, expected)
	if err != nil {
		return err
	}
	changed, err := result.RowsAffected()
	if err != nil {
		return err
	}
	if changed != 1 {
		return classifyConditionalMiss(ctx, conn, memory.RecordRef{Scope: tombstone.Scope, ID: tombstone.ID}, expected)
	}
	if err := requireFTSRowCount(ctx, conn, tombstone.ID, 1); err != nil {
		return err
	}
	deleted, err := conn.ExecContext(ctx, `DELETE FROM memory_records_fts WHERE record_id=?`, tombstone.ID)
	if err != nil {
		return err
	}
	removed, err := deleted.RowsAffected()
	if err != nil || removed != 1 {
		return memory.ErrCorrupt
	}
	return nil
}

var _ memory.Store = (*Store)(nil)
