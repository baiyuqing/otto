package sqlite

import (
	"bytes"
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"math"
	"sort"
	"strconv"
	"strings"

	"github.com/baiyuqing/otto/internal/memory"
)

const retrievalCursorVersion = 1

type retrievalCursor struct {
	Version     int    `json:"v"`
	Fingerprint string `json:"fingerprint"`
	Generation  string `json:"generation"`
	Ordinal     int    `json:"ordinal"`
	ID          string `json:"id"`
}

type retrievalFingerprint struct {
	Domain          string         `json:"domain"`
	QueryHash       string         `json:"query_hash"`
	Scopes          []memory.Scope `json:"scopes"`
	Kinds           []string       `json:"kinds"`
	Labels          []string       `json:"labels"`
	Now             string         `json:"now"`
	IncludeExpired  bool           `json:"include_expired"`
	IncludeBaseline bool           `json:"include_baseline"`
	Limit           int            `json:"limit"`
	TokenBudget     int            `json:"token_budget"`
}

type retrievalCandidate struct {
	record    memory.Record
	baseline  bool
	workspace bool
	lexical   int
}

type rankedRowScanner struct {
	row      rowScanner
	recordID *sql.NullString
	text     *sql.NullString
	kind     *sql.NullString
	key      *sql.NullString
	labels   *sql.NullString
	rank     *sql.NullFloat64
}

func (scanner rankedRowScanner) Scan(destinations ...any) error {
	return scanner.row.Scan(append(destinations, scanner.recordID, scanner.text, scanner.kind, scanner.key, scanner.labels, scanner.rank)...)
}

func (store *Store) Retrieve(ctx context.Context, input memory.RetrievalRequest) (memory.RetrievalResult, error) {
	if err := ctx.Err(); err != nil {
		return memory.RetrievalResult{}, err
	}
	ctx, operationDone, err := store.startOperation(ctx)
	if err != nil {
		return memory.RetrievalResult{}, err
	}
	defer operationDone()
	request := memory.CloneRetrievalRequest(input)
	if err := memory.ValidateRetrievalRequest(request); err != nil {
		return memory.RetrievalResult{}, err
	}
	expression, err := buildFTSLiteralExpression(request.Query)
	if err != nil {
		return memory.RetrievalResult{}, err
	}
	fingerprint, err := fingerprintRetrieval(request)
	if err != nil {
		return memory.RetrievalResult{}, memory.ErrInvalidCursor
	}
	cursor, cursorGeneration, err := decodeRetrievalCursor(request.Cursor, fingerprint)
	if err != nil {
		return memory.RetrievalResult{}, memory.ErrInvalidCursor
	}

	candidates, generation, err := store.readRetrievalSnapshot(ctx, request, expression, cursorGeneration, request.Cursor != "")
	if err != nil {
		return memory.RetrievalResult{}, err
	}
	if cursor.Ordinal > len(candidates) || cursor.Ordinal > 0 && candidates[cursor.Ordinal-1].record.ID != cursor.ID {
		return memory.RetrievalResult{}, memory.ErrInvalidCursor
	}

	result := memory.RetrievalResult{Matches: make([]memory.RetrievalMatch, 0, request.Limit)}
	lastExamined := cursor.Ordinal
	for index := cursor.Ordinal; index < len(candidates); index++ {
		if err := ctx.Err(); err != nil {
			return memory.RetrievalResult{}, err
		}
		candidate := candidates[index]
		if err := memory.GuardRecord(ctx, store.guard, candidate.record); err != nil {
			return memory.RetrievalResult{}, err
		}
		budgetText, err := retrievalBudgetText(candidate.record)
		if err != nil {
			return memory.RetrievalResult{}, memory.ErrCorrupt
		}
		estimate := estimateRetrievalTokens(request.EstimateTokens, budgetText)
		if estimate > request.TokenBudget {
			// Individually oversized records are examined and permanently skipped.
			lastExamined = index + 1
			continue
		}
		if len(result.Matches) >= request.Limit || len(result.Matches) != 0 && estimate > request.TokenBudget-result.UsedTokens {
			if lastExamined <= 0 {
				return memory.RetrievalResult{}, memory.ErrCorrupt
			}
			result.NextCursor, err = encodeRetrievalCursor(fingerprint, generation, lastExamined, candidates[lastExamined-1].record.ID)
			if err != nil {
				return memory.RetrievalResult{}, memory.ErrInvalidCursor
			}
			return memory.CloneRetrievalResult(result), nil
		}
		result.Matches = append(result.Matches, memory.RetrievalMatch{Record: candidate.record, Rank: index + 1})
		result.UsedTokens += estimate
		lastExamined = index + 1
	}
	return memory.CloneRetrievalResult(result), nil
}

func fingerprintRetrieval(request memory.RetrievalRequest) (string, error) {
	queryDigest := sha256.Sum256([]byte(request.Query))
	value := retrievalFingerprint{
		Domain: "retrieval", QueryHash: hex.EncodeToString(queryDigest[:]), Scopes: append([]memory.Scope(nil), request.Scopes...),
		Kinds: append([]string(nil), request.Kinds...), Labels: append([]string(nil), request.Labels...), Now: formatTimestamp(request.Now),
		IncludeExpired: request.IncludeExpired, IncludeBaseline: request.IncludeBaseline, Limit: request.Limit, TokenBudget: request.TokenBudget,
	}
	sort.Slice(value.Scopes, func(i, j int) bool {
		if value.Scopes[i].Namespace != value.Scopes[j].Namespace {
			return value.Scopes[i].Namespace < value.Scopes[j].Namespace
		}
		return value.Scopes[i].ID < value.Scopes[j].ID
	})
	sort.Strings(value.Kinds)
	sort.Strings(value.Labels)
	raw, err := json.Marshal(value)
	if err != nil {
		return "", err
	}
	digest := sha256.Sum256(raw)
	return hex.EncodeToString(digest[:]), nil
}

func encodeRetrievalCursor(fingerprint string, generation uint64, ordinal int, id string) (string, error) {
	if ordinal < 1 || ordinal > memory.MaxRetrievalCandidates || !validCursorID(id) {
		return "", memory.ErrInvalidCursor
	}
	payload := retrievalCursor{Version: retrievalCursorVersion, Fingerprint: fingerprint, Generation: strconv.FormatUint(generation, 10), Ordinal: ordinal, ID: id}
	raw, err := json.Marshal(payload)
	if err != nil || len(raw) > maxDecodedCursorBytes {
		return "", memory.ErrInvalidCursor
	}
	encoded := base64.RawURLEncoding.EncodeToString(raw)
	if len(encoded) > memory.MaxCursorBytes {
		return "", memory.ErrInvalidCursor
	}
	return encoded, nil
}

func decodeRetrievalCursor(value, fingerprint string) (retrievalCursor, uint64, error) {
	if value == "" {
		return retrievalCursor{}, 0, nil
	}
	if len(value) > memory.MaxCursorBytes {
		return retrievalCursor{}, 0, memory.ErrInvalidCursor
	}
	raw, err := base64.RawURLEncoding.DecodeString(value)
	if err != nil || len(raw) > maxDecodedCursorBytes {
		return retrievalCursor{}, 0, memory.ErrInvalidCursor
	}
	var payload retrievalCursor
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&payload); err != nil {
		return retrievalCursor{}, 0, memory.ErrInvalidCursor
	}
	var extra any
	if err := decoder.Decode(&extra); err == nil {
		return retrievalCursor{}, 0, memory.ErrInvalidCursor
	}
	canonical, err := json.Marshal(payload)
	if err != nil || !bytes.Equal(canonical, raw) || payload.Version != retrievalCursorVersion || payload.Fingerprint != fingerprint ||
		payload.Ordinal < 1 || payload.Ordinal > memory.MaxRetrievalCandidates || !validCursorID(payload.ID) {
		return retrievalCursor{}, 0, memory.ErrInvalidCursor
	}
	generation, err := strconv.ParseUint(payload.Generation, 10, 64)
	if err != nil || strconv.FormatUint(generation, 10) != payload.Generation {
		return retrievalCursor{}, 0, memory.ErrInvalidCursor
	}
	return payload, generation, nil
}

func (store *Store) readRetrievalSnapshot(ctx context.Context, request memory.RetrievalRequest, expression string, cursorGeneration uint64, hasCursor bool) ([]retrievalCandidate, uint64, error) {
	done, err := store.continueOperation(ctx)
	if err != nil {
		return nil, 0, err
	}
	conn, err := store.borrowConnection(ctx)
	if err != nil {
		done()
		return nil, 0, err
	}
	callBorrowedReadHook("retrieval", conn)
	contaminated, err := beginReadTransaction(ctx, conn)
	if err != nil {
		if contaminated {
			store.quarantine(conn)
		} else {
			store.returnConnection(conn)
		}
		done()
		return nil, 0, err
	}

	if hook := loadTestHooks().beforeReadGeneration; hook != nil {
		hook(conn)
	}
	generation, readErr := readGeneration(ctx, conn)
	if readErr == nil && hasCursor && generation != cursorGeneration {
		readErr = memory.ErrConflict
	}
	var candidates []retrievalCandidate
	user, workspace := retrievalScopes(request.Scopes)
	if readErr == nil && request.IncludeBaseline {
		query, arguments := buildBaselineQuery(request, user)
		var records []memory.Record
		records, readErr = queryRetrievalRecords(ctx, conn, query, arguments, memory.MaxBaselineRecords)
		for _, record := range records {
			candidates = append(candidates, makeRetrievalCandidate(record, true, math.MaxInt))
		}
	}
	if readErr == nil && expression != "" {
		query, arguments := buildFTSCandidateQuery(request, expression)
		var ranked []retrievalCandidate
		ranked, readErr = queryRankedRetrievalRecords(ctx, conn, query, arguments)
		candidates = mergeRetrievalCandidates(candidates, ranked)
	}
	if readErr == nil {
		sortRetrievalCandidates(candidates)
		if len(candidates) > memory.MaxRetrievalCandidates {
			candidates = candidates[:memory.MaxRetrievalCandidates]
		}
		if workspace != nil {
			keys := retrievalCandidateKeys(candidates)
			if len(keys) != 0 {
				query, arguments := buildWorkspaceReplacementQuery(request, *workspace, keys)
				var replacements []memory.Record
				replacements, readErr = queryRetrievalRecords(ctx, conn, query, arguments, memory.MaxRetrievalCandidates)
				if readErr == nil {
					candidates = applyWorkspaceReplacements(candidates, replacements)
				}
			}
		}
	}
	endErr := endReadTransaction(conn)
	readErr = store.finishReadTransaction(conn, readErr, endErr)
	done()
	if readErr != nil {
		return nil, 0, safeRecordReadError(ctx, readErr)
	}

	candidates = filterRetrievalCandidateLabels(candidates, request.Labels)
	candidates = dedupeRetrievalCandidates(candidates)
	sortRetrievalCandidates(candidates)
	if len(candidates) > memory.MaxRetrievalCandidates {
		candidates = candidates[:memory.MaxRetrievalCandidates]
	}
	return candidates, generation, nil
}

func retrievalScopes(scopes []memory.Scope) (memory.Scope, *memory.Scope) {
	var user memory.Scope
	var workspace *memory.Scope
	for _, scope := range scopes {
		if scope.Namespace == memory.NamespaceUser {
			user = scope
		} else if scope.Namespace == memory.NamespaceWorkspace {
			copyScope := scope
			workspace = &copyScope
		}
	}
	return user, workspace
}

func queryRetrievalRecords(ctx context.Context, conn *sql.Conn, query string, arguments []any, limit int) ([]memory.Record, error) {
	rows, err := conn.QueryContext(ctx, query, arguments...)
	if err != nil {
		return nil, safeRecordReadError(ctx, err)
	}
	records := make([]memory.Record, 0, limit)
	var result error
	for rows.Next() {
		if len(records) >= limit {
			result = memory.ErrCorrupt
			break
		}
		record, err := decodeRecordRow(rows)
		if err != nil {
			result = safeRecordReadError(ctx, err)
			break
		}
		records = append(records, record)
	}
	if err := rows.Err(); result == nil && err != nil {
		result = safeRecordReadError(ctx, err)
	}
	if err := rows.Close(); result == nil && err != nil {
		result = safeRecordReadError(ctx, err)
	}
	return records, result
}

func queryRankedRetrievalRecords(ctx context.Context, conn *sql.Conn, query string, arguments []any) ([]retrievalCandidate, error) {
	rows, err := conn.QueryContext(ctx, query, arguments...)
	if err != nil {
		return nil, safeRecordReadError(ctx, err)
	}
	result := make([]retrievalCandidate, 0, memory.MaxRetrievalCandidates)
	seen := make(map[string]struct{}, memory.MaxRetrievalCandidates)
	var readErr error
	for rows.Next() {
		if len(result) >= memory.MaxRetrievalCandidates {
			readErr = memory.ErrCorrupt
			break
		}
		var recordID, text, kind, key, labels sql.NullString
		var rank sql.NullFloat64
		record, err := decodeRecordRow(rankedRowScanner{row: rows, recordID: &recordID, text: &text, kind: &kind, key: &key, labels: &labels, rank: &rank})
		if err != nil {
			readErr = safeRecordReadError(ctx, err)
			break
		}
		if !recordID.Valid || !text.Valid || !kind.Valid || !key.Valid || !labels.Valid || !rank.Valid ||
			recordID.String != record.ID || text.String != record.Text || kind.String != record.Kind || key.String != record.Key || labels.String != ftsLabels(record.Labels) ||
			math.IsNaN(rank.Float64) || math.IsInf(rank.Float64, 0) {
			readErr = memory.ErrCorrupt
			break
		}
		if _, duplicate := seen[record.ID]; duplicate {
			readErr = memory.ErrCorrupt
			break
		}
		seen[record.ID] = struct{}{}
		result = append(result, makeRetrievalCandidate(record, false, len(result)+1))
	}
	if err := rows.Err(); readErr == nil && err != nil {
		readErr = safeRecordReadError(ctx, err)
	}
	if err := rows.Close(); readErr == nil && err != nil {
		readErr = safeRecordReadError(ctx, err)
	}
	return result, readErr
}

func makeRetrievalCandidate(record memory.Record, baseline bool, lexical int) retrievalCandidate {
	return retrievalCandidate{record: record, baseline: baseline, workspace: record.Scope.Namespace == memory.NamespaceWorkspace, lexical: lexical}
}

func mergeRetrievalCandidates(left, right []retrievalCandidate) []retrievalCandidate {
	byID := make(map[string]int, len(left)+len(right))
	result := make([]retrievalCandidate, 0, len(left)+len(right))
	for _, candidate := range append(append([]retrievalCandidate(nil), left...), right...) {
		if index, ok := byID[candidate.record.ID]; ok {
			result[index].baseline = result[index].baseline || candidate.baseline
			if candidate.lexical < result[index].lexical {
				result[index].lexical = candidate.lexical
			}
			continue
		}
		byID[candidate.record.ID] = len(result)
		result = append(result, candidate)
	}
	return result
}

func sortRetrievalCandidates(candidates []retrievalCandidate) {
	sort.SliceStable(candidates, func(i, j int) bool {
		left, right := candidates[i], candidates[j]
		if left.baseline != right.baseline {
			return left.baseline
		}
		if left.workspace != right.workspace {
			return left.workspace
		}
		if left.lexical != right.lexical {
			return left.lexical < right.lexical
		}
		if !left.record.UpdatedAt.Equal(right.record.UpdatedAt) {
			return left.record.UpdatedAt.After(right.record.UpdatedAt)
		}
		return left.record.ID < right.record.ID
	})
}

func retrievalCandidateKeys(candidates []retrievalCandidate) []retrievalKey {
	seen := make(map[retrievalKey]struct{})
	keys := make([]retrievalKey, 0, len(candidates))
	for _, candidate := range candidates {
		if candidate.record.Scope.Namespace != memory.NamespaceUser || candidate.record.Key == "" {
			continue
		}
		key := retrievalKey{kind: candidate.record.Kind, key: candidate.record.Key}
		if _, ok := seen[key]; !ok {
			seen[key] = struct{}{}
			keys = append(keys, key)
		}
	}
	return keys
}

func applyWorkspaceReplacements(candidates []retrievalCandidate, records []memory.Record) []retrievalCandidate {
	replacements := make(map[retrievalKey]memory.Record, len(records))
	for _, record := range records {
		replacements[retrievalKey{kind: record.Kind, key: record.Key}] = record
	}
	result := make([]retrievalCandidate, 0, len(candidates))
	for _, candidate := range candidates {
		if candidate.record.Scope.Namespace != memory.NamespaceUser || candidate.record.Key == "" {
			result = append(result, candidate)
			continue
		}
		replacement, ok := replacements[retrievalKey{kind: candidate.record.Kind, key: candidate.record.Key}]
		if !ok {
			result = append(result, candidate)
			continue
		}
		result = append(result, makeRetrievalCandidate(replacement, candidate.baseline, candidate.lexical))
	}
	return mergeRetrievalCandidates(nil, result)
}

func filterRetrievalCandidateLabels(candidates []retrievalCandidate, required []string) []retrievalCandidate {
	if len(required) == 0 {
		return candidates
	}
	result := candidates[:0]
	for _, candidate := range candidates {
		if recordHasAllLabels(candidate.record, required) {
			result = append(result, candidate)
		}
	}
	return result
}

func recordHasAllLabels(record memory.Record, required []string) bool {
	set := make(map[string]struct{}, len(record.Labels))
	for _, label := range record.Labels {
		set[label] = struct{}{}
	}
	for _, label := range required {
		if _, ok := set[label]; !ok {
			return false
		}
	}
	return true
}

func dedupeRetrievalCandidates(candidates []retrievalCandidate) []retrievalCandidate {
	byDigest := make(map[[sha256.Size]byte]int, len(candidates))
	result := make([]retrievalCandidate, 0, len(candidates))
	for _, candidate := range candidates {
		digest := sha256.Sum256([]byte(strings.TrimSpace(candidate.record.Text)))
		index, ok := byDigest[digest]
		if !ok {
			byDigest[digest] = len(result)
			result = append(result, candidate)
			continue
		}
		baseline := result[index].baseline || candidate.baseline
		if betterRetrievalDedupeWinner(candidate, result[index]) {
			candidate.baseline = baseline
			result[index] = candidate
		} else {
			result[index].baseline = baseline
		}
	}
	return result
}

func betterRetrievalDedupeWinner(left, right retrievalCandidate) bool {
	if left.workspace != right.workspace {
		return left.workspace
	}
	if !left.record.UpdatedAt.Equal(right.record.UpdatedAt) {
		return left.record.UpdatedAt.After(right.record.UpdatedAt)
	}
	if left.lexical != right.lexical {
		return left.lexical < right.lexical
	}
	return left.record.ID < right.record.ID
}

func retrievalBudgetText(record memory.Record) (string, error) {
	labels := append([]string(nil), record.Labels...)
	sort.Strings(labels)
	value := struct {
		ID     string       `json:"id"`
		Scope  memory.Scope `json:"scope"`
		Kind   string       `json:"kind"`
		Key    string       `json:"key"`
		Labels []string     `json:"labels"`
		Text   string       `json:"text"`
	}{record.ID, record.Scope, record.Kind, record.Key, labels, record.Text}
	raw, err := json.Marshal(value)
	return string(raw), err
}

func estimateRetrievalTokens(estimator func(string) int, text string) int {
	if estimator != nil {
		value := estimator(text)
		if value <= 0 {
			return 1
		}
		return value
	}
	if text == "" {
		return 0
	}
	return 1 + (len([]byte(text))-1)/3
}

var _ memory.Retriever = (*Store)(nil)
