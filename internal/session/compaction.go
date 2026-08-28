package session

import (
	"context"
	"encoding/json"
	"fmt"
	"math"
	"strings"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/model"
)

func (m *Memory) AppendCompaction(ctx context.Context, checkpoint CompactionCheckpoint) (CompactionMetadata, error) {
	if err := ctx.Err(); err != nil {
		return CompactionMetadata{}, err
	}

	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed {
		return CompactionMetadata{}, errSessionClosed
	}
	if err := validateCompactionCheckpoint(checkpoint); err != nil {
		return CompactionMetadata{}, err
	}

	firstKept := -1
	for index := range m.messages {
		if m.messages[index].ID == checkpoint.FirstKeptEntryID {
			firstKept = index
			break
		}
	}
	if firstKept < 0 {
		return CompactionMetadata{}, fmt.Errorf("%w: compaction first-kept message is not in the active context", ErrInvalidSession)
	}

	seen := make(map[string]struct{}, len(m.messages))
	for _, message := range m.messages {
		if piEntryIDPattern.MatchString(message.ID) {
			seen[message.ID] = struct{}{}
		}
	}
	checkpointID, err := newPiEntryID(seen)
	if err != nil {
		return CompactionMetadata{}, fmt.Errorf("generate compaction entry id: %w", err)
	}
	usage := cloneUsage(checkpoint.Usage)
	details := cloneCompactionDetails(checkpoint.Details)
	contextMessage := newContextMessage(
		checkpointID,
		compactionContextType,
		true,
		"[Compaction summary]\n"+checkpoint.Summary,
		checkpoint.CreatedAt,
		cloneUsage(usage),
	)
	contextMessage.ContextTokensBefore = checkpoint.TokensBefore
	candidateMessages := make([]model.Message, 0, 1+len(m.messages)-firstKept)
	candidateMessages = append(candidateMessages, contextMessage)
	candidateMessages = append(candidateMessages, cloneMessages(m.messages[firstKept:])...)
	if _, err := pendingToolCalls(candidateMessages); err != nil {
		return CompactionMetadata{}, err
	}
	if err := ctx.Err(); err != nil {
		return CompactionMetadata{}, err
	}

	metadata := CompactionMetadata{
		ID: checkpointID, Summary: checkpoint.Summary, FirstKeptEntryID: checkpoint.FirstKeptEntryID,
		TokensBefore: checkpoint.TokensBefore, Usage: usage, Details: details,
	}
	m.messages = candidateMessages
	if checkpoint.Usage != nil {
		m.aggregateUsage = addResolvedUsage(m.aggregateUsage, checkpoint.Usage)
		m.usagePresent = true
	}
	m.latestCompaction = cloneCompactionMetadata(metadata)
	m.hasLatestCompaction = true
	return cloneCompactionMetadata(metadata), nil
}

func (m *Memory) LatestCompaction() (CompactionMetadata, bool) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if !m.hasLatestCompaction {
		return CompactionMetadata{}, false
	}
	return cloneCompactionMetadata(m.latestCompaction), true
}

func (s *Store) AppendCompaction(ctx context.Context, checkpoint CompactionCheckpoint) (CompactionMetadata, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return CompactionMetadata{}, errSessionClosed
	}
	if s.fatalErr != nil {
		return CompactionMetadata{}, s.fatalErr
	}
	if err := ctx.Err(); err != nil {
		return CompactionMetadata{}, err
	}
	if err := validateCompactionCheckpoint(checkpoint); err != nil {
		return CompactionMetadata{}, err
	}

	firstKeptEntryID, err := s.compactionAnchorLocked(checkpoint.FirstKeptEntryID)
	if err != nil {
		return CompactionMetadata{}, err
	}
	timestamp, err := formatPersistedTimestamp(checkpoint.CreatedAt, "compaction")
	if err != nil {
		return CompactionMetadata{}, err
	}
	entryID, err := newPiEntryID(s.entryIDs)
	if err != nil {
		return CompactionMetadata{}, fmt.Errorf("generate compaction entry id: %w", err)
	}
	usage, err := compactionUsageToPi(checkpoint.Usage)
	if err != nil {
		return CompactionMetadata{}, err
	}
	details := cloneCompactionDetails(checkpoint.Details)
	entry := piEntry{
		piEntryBase: piEntryBase{
			Type: "compaction", ID: entryID, ParentID: cloneStringPointer(s.leafID), Timestamp: timestamp,
		},
		Compaction: &piCompaction{
			Summary: checkpoint.Summary, FirstKeptEntryID: stringPointer(firstKeptEntryID),
			TokensBefore: int64(checkpoint.TokensBefore), Usage: usage,
		},
	}
	candidateEntries := append(append([]piEntry(nil), s.entries...), entry)
	resolved, _, err := buildContext(candidateEntries, entryID)
	if err != nil {
		return CompactionMetadata{}, err
	}
	if compactionDetailsPresent(details) {
		detailsJSON, err := json.Marshal(details)
		if err != nil {
			return CompactionMetadata{}, fmt.Errorf("encode compaction details: %w", err)
		}
		entry.Compaction.Details = detailsJSON
	}
	candidateEntries[len(candidateEntries)-1] = entry
	metadata, ok, err := latestCompactionMetadata(candidateEntries, entryID)
	if err != nil {
		return CompactionMetadata{}, err
	}
	if !ok || metadata.ID != entryID {
		return CompactionMetadata{}, fmt.Errorf("%w: candidate compaction did not become active", ErrInvalidSession)
	}

	encoded, err := encodePiRecord(entry)
	if err != nil {
		return CompactionMetadata{}, err
	}
	recordBytes := int64(len(encoded) + 1)
	if recordBytes > int64(maxSessionFileBytes)-s.fileBytes {
		return CompactionMetadata{}, sizeError(ErrSessionFileTooLarge, maxSessionFileBytes)
	}
	if err := ctx.Err(); err != nil {
		return CompactionMetadata{}, err
	}
	if err := s.ensureFileLocked(); err != nil {
		s.fatalErr = &fatalPersistenceError{cause: err}
		return CompactionMetadata{}, s.fatalErr
	}
	if _, err := writeEncodedPiRecord(s.writer, encoded); err != nil {
		s.fatalErr = &fatalPersistenceError{cause: err}
		return CompactionMetadata{}, s.fatalErr
	}

	s.entries = candidateEntries
	s.entryIDs[entryID] = struct{}{}
	s.leafID = stringPointer(entryID)
	s.messages = cloneMessages(resolved.Messages)
	s.aggregateUsage = resolved.Usage
	s.usagePresent = resolved.UsagePresent
	s.fileBytes += recordBytes
	s.latestCompaction = cloneCompactionMetadata(metadata)
	s.hasLatestCompaction = true
	return cloneCompactionMetadata(metadata), nil
}

func (s *Store) LatestCompaction() (CompactionMetadata, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if !s.hasLatestCompaction {
		return CompactionMetadata{}, false
	}
	return cloneCompactionMetadata(s.latestCompaction), true
}

func (s *Store) compactionAnchorLocked(requested string) (string, error) {
	index, _, err := indexContextEntries(s.entries)
	if err != nil {
		return "", err
	}
	leaf := ""
	if s.leafID != nil {
		leaf = *s.leafID
	}
	path, err := activeContextPath(s.entries, leaf, index)
	if err != nil {
		return "", err
	}

	anchor := requested
	if s.hasLatestCompaction && s.latestCompaction.RetainedTailOnly {
		anchor = s.latestCompaction.FirstPostCheckpointMessageID
		if anchor == "" {
			return "", fmt.Errorf("%w: retained-tail compaction has no real post-checkpoint anchor", ErrInvalidSession)
		}
	}
	for _, entry := range path {
		if entry.ID == anchor {
			return anchor, nil
		}
	}
	return "", fmt.Errorf("%w: compaction firstKeptEntryId is not a real entry on the active path", ErrInvalidSession)
}

func validateCompactionCheckpoint(checkpoint CompactionCheckpoint) error {
	if strings.TrimSpace(checkpoint.Summary) == "" || !utf8.ValidString(checkpoint.Summary) {
		return fmt.Errorf("%w: compaction summary must be nonempty UTF-8", ErrInvalidSession)
	}
	if strings.TrimSpace(checkpoint.FirstKeptEntryID) == "" {
		return fmt.Errorf("%w: compaction first-kept entry id is required", ErrInvalidSession)
	}
	if checkpoint.TokensBefore < 0 {
		return fmt.Errorf("%w: compaction tokens before must be nonnegative", ErrInvalidSession)
	}
	if _, err := formatPersistedTimestamp(checkpoint.CreatedAt, "compaction"); err != nil {
		return err
	}
	if checkpoint.Usage != nil {
		if err := checkpoint.Usage.Validate(); err != nil {
			return fmt.Errorf("%w: %v", ErrInvalidSession, err)
		}
	}
	if checkpoint.Details.OmittedReadFiles < 0 || checkpoint.Details.OmittedModifiedFiles < 0 {
		return fmt.Errorf("%w: compaction omitted file counts must be nonnegative", ErrInvalidSession)
	}
	for _, paths := range [][]string{checkpoint.Details.ReadFiles, checkpoint.Details.ModifiedFiles} {
		for _, path := range paths {
			if !utf8.ValidString(path) {
				return fmt.Errorf("%w: compaction file details must be UTF-8", ErrInvalidSession)
			}
		}
	}
	return nil
}

func compactionUsageToPi(usage *model.Usage) (*piUsage, error) {
	if usage == nil {
		return nil, nil
	}
	return modelUsageToPi(usage)
}

func latestCompactionMetadata(entries []piEntry, leafID string) (CompactionMetadata, bool, error) {
	if len(entries) == 0 {
		return CompactionMetadata{}, false, nil
	}
	index, _, err := indexContextEntries(entries)
	if err != nil {
		return CompactionMetadata{}, false, err
	}
	path, err := activeContextPath(entries, leafID, index)
	if err != nil {
		return CompactionMetadata{}, false, err
	}
	latest := -1
	for index := range path {
		if path[index].Type == "compaction" {
			latest = index
		}
	}
	if latest < 0 {
		return CompactionMetadata{}, false, nil
	}
	entry := path[latest]
	if entry.Compaction == nil {
		return CompactionMetadata{}, false, fmt.Errorf("%w: compaction payload is required", ErrInvalidSession)
	}
	firstKept, retainedTailOnly, err := resolveCompactionBoundary(path, latest)
	if err != nil {
		return CompactionMetadata{}, false, err
	}
	usage, err := optionalPiUsageToModel(entry.Compaction.Usage)
	if err != nil {
		return CompactionMetadata{}, false, err
	}
	metadata := CompactionMetadata{
		ID: entry.ID, Summary: entry.Compaction.Summary, FirstKeptEntryID: firstKept,
		TokensBefore: safeContextTokenCount(entry.Compaction.TokensBefore), Usage: usage,
		Details: decodeCompactionDetails(entry.Compaction.Details), RetainedTailOnly: retainedTailOnly,
	}
	for index := latest + 1; index < len(path); index++ {
		if path[index].Type == "message" {
			metadata.FirstPostCheckpointMessageID = path[index].ID
			break
		}
	}
	return metadata, true, nil
}

func resolveCompactionBoundary(path []piEntry, checkpointIndex int) (string, bool, error) {
	compaction := path[checkpointIndex].Compaction
	if compaction == nil {
		return "", false, fmt.Errorf("%w: compaction payload is required", ErrInvalidSession)
	}
	if compaction.FirstKeptEntryID != nil {
		for index := 0; index < checkpointIndex; index++ {
			if path[index].ID == *compaction.FirstKeptEntryID {
				return *compaction.FirstKeptEntryID, false, nil
			}
		}
	}
	if compactionHasRetainedTail(compaction) {
		return "", true, nil
	}
	if compaction.FirstKeptEntryID == nil {
		return "", false, fmt.Errorf("%w: compaction requires firstKeptEntryId or retainedTail", ErrInvalidSession)
	}
	return "", false, fmt.Errorf("%w: compaction firstKeptEntryId is not on the active path before the checkpoint", ErrInvalidSession)
}

func decodeCompactionDetails(raw json.RawMessage) CompactionDetails {
	if len(raw) == 0 {
		return CompactionDetails{}
	}
	var details CompactionDetails
	if err := json.Unmarshal(raw, &details); err != nil {
		return CompactionDetails{}
	}
	if details.OmittedReadFiles < 0 {
		details.OmittedReadFiles = 0
	}
	if details.OmittedModifiedFiles < 0 {
		details.OmittedModifiedFiles = 0
	}
	return cloneCompactionDetails(details)
}

func safeContextTokenCount(tokens int64) int {
	if tokens <= 0 {
		return 0
	}
	if uint64(tokens) > uint64(math.MaxInt) {
		return math.MaxInt
	}
	return int(tokens)
}

func compactionDetailsPresent(details CompactionDetails) bool {
	return len(details.ReadFiles) != 0 || len(details.ModifiedFiles) != 0 ||
		details.OmittedReadFiles != 0 || details.OmittedModifiedFiles != 0
}

func cloneCompactionMetadata(metadata CompactionMetadata) CompactionMetadata {
	metadata.Usage = cloneUsage(metadata.Usage)
	metadata.Details = cloneCompactionDetails(metadata.Details)
	return metadata
}

func cloneCompactionDetails(details CompactionDetails) CompactionDetails {
	details.ReadFiles = append([]string(nil), details.ReadFiles...)
	details.ModifiedFiles = append([]string(nil), details.ModifiedFiles...)
	return details
}

func cloneUsage(usage *model.Usage) *model.Usage {
	if usage == nil {
		return nil
	}
	cloned := *usage
	return &cloned
}
