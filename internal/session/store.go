package session

import (
	"bytes"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"time"

	"github.com/baiyuqing/otto/internal/model"
)

const ottoRuntimeCustomType = "otto.runtime"

var piEntryIDPattern = regexp.MustCompile(`^[0-9a-f]{8}$`)

type durableRecordWriter interface {
	Write([]byte) (int, error)
	Sync() error
}

type Store struct {
	mu             sync.Mutex
	header         Header
	messages       []model.Message
	aggregateUsage model.Usage
	usagePresent   bool
	entries        []piEntry
	entryIDs       map[string]struct{}
	leafID         *string
	path           string
	file           *os.File
	writer         durableRecordWriter
	fileBytes      int64
	fatalErr       error
	closed         bool
}

func Create(root string, header Header) (*Store, error) {
	header.Version = CurrentVersion
	createdAt, err := validateDomainHeader(header)
	if err != nil {
		return nil, err
	}
	directory, err := sessionDirectory(root, header.Workspace)
	if err != nil {
		return nil, err
	}
	if err := os.MkdirAll(directory, 0o700); err != nil {
		return nil, fmt.Errorf("create session directory: %w", err)
	}
	if err := os.Chmod(directory, 0o700); err != nil {
		return nil, fmt.Errorf("chmod session directory: %w", err)
	}

	path := filepath.Join(directory, header.ID+".jsonl")
	file, err := os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_RDWR, 0o600)
	if err != nil {
		return nil, fmt.Errorf("create session file: %w", err)
	}
	cleanup := func(writeErr error) (*Store, error) {
		_ = file.Close()
		if removeErr := os.Remove(path); removeErr != nil && !errors.Is(removeErr, os.ErrNotExist) {
			return nil, errors.Join(writeErr, fmt.Errorf("remove incomplete session file: %w", removeErr))
		}
		return nil, writeErr
	}

	piHeaderRecord := piHeader{
		Type:      "session",
		Version:   PiSessionVersion,
		ID:        header.ID,
		Timestamp: createdAt,
		CWD:       header.Workspace,
	}
	fileBytes, err := writePiRecord(file, piHeaderRecord)
	if err != nil {
		return cleanup(fmt.Errorf("write session header: %w", err))
	}

	entryIDs := make(map[string]struct{})
	runtimeID, err := newPiEntryID(entryIDs)
	if err != nil {
		return cleanup(fmt.Errorf("generate runtime entry id: %w", err))
	}
	runtimeData, err := json.Marshal(RuntimeMetadata{Profile: header.Profile, Provider: header.Provider, Model: header.Model})
	if err != nil {
		return cleanup(fmt.Errorf("encode runtime metadata: %w", err))
	}
	runtimeEntry := piEntry{
		piEntryBase: piEntryBase{Type: "custom", ID: runtimeID, ParentID: nil, Timestamp: createdAt},
		Custom:      &piCustom{CustomType: ottoRuntimeCustomType, Data: runtimeData},
	}
	written, err := writePiRecord(file, runtimeEntry)
	if err != nil {
		return cleanup(fmt.Errorf("write runtime metadata: %w", err))
	}
	fileBytes += written
	entryIDs[runtimeID] = struct{}{}
	leafID := runtimeID

	return &Store{
		header:    header,
		entries:   []piEntry{runtimeEntry},
		entryIDs:  entryIDs,
		leafID:    &leafID,
		path:      path,
		file:      file,
		writer:    file,
		fileBytes: fileBytes,
	}, nil
}

func ReadHeader(path string) (Header, error) {
	file, err := os.Open(path)
	if err != nil {
		return Header{}, fmt.Errorf("open session file: %w", err)
	}
	defer file.Close()

	if err := rejectOversizedSessionFile(file); err != nil {
		return Header{}, err
	}
	decoded, err := decodePiFileReadOnly(file)
	if err != nil {
		return Header{}, err
	}
	state, err := resolvePiStoreState(decoded)
	if err != nil {
		return Header{}, err
	}
	return state.header, nil
}

func Open(path string) (*Store, []Warning, error) {
	prepared, err := Prepare(context.Background(), path)
	if err != nil {
		return nil, nil, err
	}
	return prepared.Activate(context.Background())
}

// openStoreFromFile consumes file on both success and failure.
func openStoreFromFile(file *os.File, path string) (*Store, []Warning, error) {
	closeOnError := func(openErr error) (*Store, []Warning, error) {
		_ = file.Close()
		return nil, nil, openErr
	}

	if err := rejectOversizedSessionFile(file); err != nil {
		return closeOnError(err)
	}
	decoded, warnings, err := decodePiFileForOpen(file, path)
	if err != nil {
		return closeOnError(err)
	}
	state, err := resolvePiStoreState(decoded)
	if err != nil {
		return closeOnError(err)
	}
	warnings = append(warnings, state.warnings...)
	position, err := file.Seek(0, io.SeekEnd)
	if err != nil {
		return closeOnError(fmt.Errorf("seek session file: %w", err))
	}

	store := &Store{
		header:         state.header,
		messages:       state.messages,
		aggregateUsage: state.aggregateUsage,
		usagePresent:   state.usagePresent,
		entries:        append([]piEntry(nil), decoded.Entries...),
		entryIDs:       state.entryIDs,
		leafID:         cloneStringPointer(state.leafID),
		path:           path,
		file:           file,
		writer:         file,
		fileBytes:      position,
	}
	repairWarnings, err := store.repairDanglingToolCalls()
	if err != nil {
		return closeOnError(err)
	}
	warnings = append(warnings, repairWarnings...)
	if _, err := file.Seek(0, io.SeekEnd); err != nil {
		return closeOnError(fmt.Errorf("seek session file after repair: %w", err))
	}
	return store, warnings, nil
}

func (s *Store) Header() Header {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.header
}

func (s *Store) Messages() []model.Message {
	s.mu.Lock()
	defer s.mu.Unlock()
	return cloneMessages(s.messages)
}

func (s *Store) AggregateUsage() (model.Usage, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.aggregateUsage, s.usagePresent
}

func (s *Store) Append(ctx context.Context, message model.Message) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return errSessionClosed
	}
	if s.fatalErr != nil {
		return s.fatalErr
	}
	if err := ctx.Err(); err != nil {
		return err
	}

	entryID, err := newPiEntryID(s.entryIDs)
	if err != nil {
		return fmt.Errorf("generate session entry id: %w", err)
	}
	entry, persistedMessage, err := modelMessageToPiEntry(message, entryID, s.leafID, s.header)
	if err != nil {
		return err
	}
	candidateMessages := append(cloneMessages(s.messages), persistedMessage)
	if _, err := pendingToolCalls(candidateMessages); err != nil {
		return err
	}
	encoded, err := encodePiRecord(entry)
	if err != nil {
		return err
	}
	recordBytes := int64(len(encoded) + 1)
	if recordBytes > int64(maxSessionFileBytes)-s.fileBytes {
		return sizeError(ErrSessionFileTooLarge, maxSessionFileBytes)
	}
	if _, err := writeEncodedPiRecord(s.writer, encoded); err != nil {
		s.fatalErr = &fatalPersistenceError{cause: err}
		return s.fatalErr
	}

	s.entries = append(s.entries, entry)
	s.entryIDs[entryID] = struct{}{}
	s.leafID = stringPointer(entryID)
	s.fileBytes += recordBytes
	s.messages = append(s.messages, cloneMessage(persistedMessage))
	if persistedMessage.Role == model.RoleAssistant {
		s.aggregateUsage = addResolvedUsage(s.aggregateUsage, persistedMessage.Usage)
		s.usagePresent = true
	}
	return nil
}

func (s *Store) Path() string {
	return s.path
}

func (s *Store) Close() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return nil
	}
	s.closed = true
	if err := s.file.Close(); err != nil {
		return fmt.Errorf("close session file: %w", err)
	}
	return nil
}

func (s *Store) repairDanglingToolCalls() ([]Warning, error) {
	pending, err := pendingToolCalls(s.Messages())
	if err != nil {
		return nil, err
	}
	var warnings []Warning
	for _, call := range pending {
		message := model.Message{
			Role:      model.RoleTool,
			CreatedAt: time.Now().UTC(),
			Blocks: []model.Block{{
				Type:       model.BlockToolResult,
				Text:       "tool result missing from prior session",
				ToolCallID: call.ToolCallID,
				ToolName:   call.ToolName,
				IsError:    true,
			}},
		}
		if err := s.Append(context.Background(), message); err != nil {
			return nil, fmt.Errorf("repair dangling tool call %q: %w", call.ToolCallID, err)
		}
		warnings = append(warnings, Warning{Message: fmt.Sprintf("repaired dangling tool call %s", call.ToolCallID)})
	}
	return warnings, nil
}

type resolvedPiStoreState struct {
	header         Header
	messages       []model.Message
	aggregateUsage model.Usage
	usagePresent   bool
	entryIDs       map[string]struct{}
	leafID         *string
	warnings       []Warning
}

func resolvePiStoreState(decoded piFile) (resolvedPiStoreState, error) {
	createdAt, err := validatePiHeader(decoded.Header)
	if err != nil {
		return resolvedPiStoreState{}, err
	}
	entryIDs := make(map[string]struct{}, len(decoded.Entries))
	for _, entry := range decoded.Entries {
		entryIDs[entry.ID] = struct{}{}
	}
	var leafID *string
	if len(decoded.Entries) > 0 {
		leafID = stringPointer(decoded.Entries[len(decoded.Entries)-1].ID)
	}
	leaf := ""
	if leafID != nil {
		leaf = *leafID
	}
	resolved, warnings, err := buildContext(decoded.Entries, leaf)
	if err != nil {
		return resolvedPiStoreState{}, err
	}
	return resolvedPiStoreState{
		header: Header{
			Version: CurrentVersion, ID: decoded.Header.ID, Workspace: decoded.Header.CWD,
			Provider: resolved.Runtime.Provider, Profile: resolved.Runtime.Profile, Model: resolved.Runtime.Model, CreatedAt: createdAt,
		},
		messages:       resolved.Messages,
		aggregateUsage: resolved.Usage,
		usagePresent:   resolved.UsagePresent,
		entryIDs:       entryIDs,
		leafID:         leafID,
		warnings:       warnings,
	}, nil
}
func validateDomainHeader(header Header) (string, error) {
	if strings.TrimSpace(header.ID) == "" || strings.ContainsAny(header.ID, `/\\`) || header.ID == "." || header.ID == ".." {
		return "", fmt.Errorf("%w: session id is invalid", ErrInvalidSession)
	}
	if strings.TrimSpace(header.Workspace) == "" {
		return "", fmt.Errorf("%w: session workspace is required", ErrInvalidSession)
	}
	if strings.TrimSpace(header.Provider) == "" {
		return "", fmt.Errorf("%w: session provider is required", ErrInvalidSession)
	}
	if strings.TrimSpace(header.Model) == "" {
		return "", fmt.Errorf("%w: session model is required", ErrInvalidSession)
	}
	return formatPersistedTimestamp(header.CreatedAt, "session")
}

func formatPersistedTimestamp(timestamp time.Time, subject string) (string, error) {
	if timestamp.IsZero() {
		return "", fmt.Errorf("%w: %s timestamp is required", ErrInvalidSession, subject)
	}
	formatted := timestamp.Format(time.RFC3339Nano)
	roundTripped, err := time.Parse(time.RFC3339Nano, formatted)
	if err != nil || !roundTripped.Equal(timestamp) || roundTripped.Format(time.RFC3339Nano) != formatted {
		return "", fmt.Errorf("%w: %s timestamp is outside the RFC3339Nano range", ErrInvalidSession, subject)
	}
	return formatted, nil
}

func validatePiHeader(header piHeader) (time.Time, error) {
	if strings.TrimSpace(header.ID) == "" {
		return time.Time{}, fmt.Errorf("%w: session header id is required", ErrInvalidSession)
	}
	if strings.TrimSpace(header.CWD) == "" {
		return time.Time{}, fmt.Errorf("%w: session header cwd is required", ErrInvalidSession)
	}
	createdAt, err := time.Parse(time.RFC3339Nano, header.Timestamp)
	if err != nil {
		return time.Time{}, fmt.Errorf("%w: session header timestamp is invalid", ErrInvalidSession)
	}
	return createdAt, nil
}

func decodeRuntimeMetadata(raw json.RawMessage) (RuntimeMetadata, error) {
	object, err := decodeObject(raw, "otto.runtime data")
	if err != nil {
		return RuntimeMetadata{}, err
	}
	provider, err := requiredString(object, "provider", "otto.runtime data.provider")
	if err != nil || strings.TrimSpace(provider) == "" {
		return RuntimeMetadata{}, fmt.Errorf("%w: otto.runtime provider is required", ErrInvalidSession)
	}
	modelID, err := requiredString(object, "model", "otto.runtime data.model")
	if err != nil || strings.TrimSpace(modelID) == "" {
		return RuntimeMetadata{}, fmt.Errorf("%w: otto.runtime model is required", ErrInvalidSession)
	}
	profile, err := optionalString(object, "profile", "otto.runtime data.profile", false)
	if err != nil {
		return RuntimeMetadata{}, err
	}
	metadata := RuntimeMetadata{Provider: provider, Model: modelID}
	if profile != nil {
		metadata.Profile = *profile
	}
	return metadata, nil
}

func modelMessageToPiEntry(message model.Message, entryID string, parentID *string, header Header) (piEntry, model.Message, error) {
	timestamp, err := formatPersistedTimestamp(message.CreatedAt, "message")
	if err != nil {
		return piEntry{}, model.Message{}, err
	}
	content, err := modelBlocksToPiContent(message.Role, message.Blocks)
	if err != nil {
		return piEntry{}, model.Message{}, err
	}
	piMessage := &piMessage{Role: string(message.Role), Content: content, Timestamp: message.CreatedAt.UnixMilli()}

	switch message.Role {
	case model.RoleUser:
		if message.FinishReason != "" || message.Usage != nil {
			return piEntry{}, model.Message{}, fmt.Errorf("%w: user message contains assistant-only metadata", ErrInvalidSession)
		}
	case model.RoleAssistant:
		stopReason, err := modelFinishReasonToPi(message.FinishReason)
		if err != nil {
			return piEntry{}, model.Message{}, err
		}
		if err := validateAssistantToolFinish(message.Blocks, message.FinishReason); err != nil {
			return piEntry{}, model.Message{}, err
		}
		usage, err := modelUsageToPi(message.Usage)
		if err != nil {
			return piEntry{}, model.Message{}, err
		}
		piMessage.API = "openai-completions"
		piMessage.Provider = header.Provider
		piMessage.Model = header.Model
		piMessage.Usage = usage
		piMessage.StopReason = stopReason
		if message.FinishReason == model.FinishUnknown {
			piMessage.ErrorMessage = "assistant response ended with an unknown finish reason"
		}
	case model.RoleTool:
		if message.FinishReason != "" || message.Usage != nil || len(message.Blocks) != 1 {
			return piEntry{}, model.Message{}, fmt.Errorf("%w: tool result message is malformed", ErrInvalidSession)
		}
		block := message.Blocks[0]
		if block.Type != model.BlockToolResult || strings.TrimSpace(block.ToolCallID) == "" || strings.TrimSpace(block.ToolName) == "" {
			return piEntry{}, model.Message{}, fmt.Errorf("%w: tool result id and name are required", ErrInvalidSession)
		}
		piMessage.Role = "toolResult"
		piMessage.ToolCallID = block.ToolCallID
		piMessage.ToolName = block.ToolName
		isError := block.IsError
		piMessage.IsError = &isError
	default:
		return piEntry{}, model.Message{}, fmt.Errorf("%w: unsupported message role", ErrInvalidSession)
	}

	entry := piEntry{
		piEntryBase: piEntryBase{
			Type:      "message",
			ID:        entryID,
			ParentID:  cloneStringPointer(parentID),
			Timestamp: timestamp,
		},
		Message: piMessage,
	}
	persisted := cloneMessage(message)
	persisted.ID = entryID
	return entry, persisted, nil
}

func modelBlocksToPiContent(role model.Role, blocks []model.Block) (json.RawMessage, error) {
	if len(blocks) == 0 && role != model.RoleAssistant {
		return nil, fmt.Errorf("%w: message content is required", ErrInvalidSession)
	}
	content := make([]piContentBlock, len(blocks))
	for index, block := range blocks {
		switch block.Type {
		case model.BlockText:
			if role != model.RoleUser && role != model.RoleAssistant {
				return nil, fmt.Errorf("%w: text content is incompatible with message role", ErrInvalidSession)
			}
			if block.ToolCallID != "" || block.ToolName != "" || len(block.Arguments) != 0 || block.IsError {
				return nil, fmt.Errorf("%w: text block contains incompatible fields", ErrInvalidSession)
			}
			content[index] = piContentBlock{Type: "text", Text: block.Text}
		case model.BlockToolCall:
			if role != model.RoleAssistant || strings.TrimSpace(block.ToolCallID) == "" || strings.TrimSpace(block.ToolName) == "" {
				return nil, fmt.Errorf("%w: assistant tool-call id and name are required", ErrInvalidSession)
			}
			if !validToolArguments(block.Arguments) {
				return nil, fmt.Errorf("%w: tool-call arguments must be a JSON object", ErrInvalidSession)
			}
			if block.Text != "" || block.IsError {
				return nil, fmt.Errorf("%w: tool-call block contains incompatible fields", ErrInvalidSession)
			}
			content[index] = piContentBlock{Type: "toolCall", ID: block.ToolCallID, Name: block.ToolName, Arguments: cloneRaw(block.Arguments)}
		case model.BlockToolResult:
			if role != model.RoleTool || len(blocks) != 1 {
				return nil, fmt.Errorf("%w: tool-result block is incompatible with message role", ErrInvalidSession)
			}
			content[index] = piContentBlock{Type: "text", Text: block.Text}
		default:
			return nil, fmt.Errorf("%w: unsupported message block type", ErrInvalidSession)
		}
	}
	encoded, err := json.Marshal(content)
	if err != nil {
		return nil, fmt.Errorf("encode Pi message content: %w", err)
	}
	return encoded, nil
}

func modelFinishReasonToPi(reason model.FinishReason) (string, error) {
	switch reason {
	case model.FinishStop:
		return "stop", nil
	case model.FinishToolCalls:
		return "toolUse", nil
	case model.FinishLength:
		return "length", nil
	case model.FinishUnknown:
		return "error", nil
	default:
		return "", fmt.Errorf("%w: unsupported assistant finish reason", ErrInvalidSession)
	}
}

func modelUsageToPi(usage *model.Usage) (*piUsage, error) {
	var input, output int64
	if usage != nil {
		if usage.InputTokens < 0 || usage.OutputTokens < 0 {
			return nil, fmt.Errorf("%w: usage token counts must be nonnegative", ErrInvalidSession)
		}
		input = int64(usage.InputTokens)
		output = int64(usage.OutputTokens)
	}
	if output > math.MaxInt64-input {
		return nil, fmt.Errorf("%w: usage token total overflows", ErrInvalidSession)
	}
	return &piUsage{Input: input, Output: output, TotalTokens: input + output}, nil
}

func piStopReasonToModel(reason string) (model.FinishReason, error) {
	switch reason {
	case "stop":
		return model.FinishStop, nil
	case "toolUse":
		return model.FinishToolCalls, nil
	case "length":
		return model.FinishLength, nil
	case "error", "aborted":
		return model.FinishUnknown, nil
	case "pending":
		return "", fmt.Errorf("%w: pending assistant message cannot be persisted", ErrInvalidSession)
	default:
		return "", fmt.Errorf("%w: unsupported Pi assistant stop reason", ErrInvalidSession)
	}
}

func piUsageToModel(usage *piUsage) (*model.Usage, error) {
	if usage == nil {
		return nil, fmt.Errorf("%w: assistant usage is required", ErrInvalidSession)
	}
	if usage.Input < 0 || usage.Output < 0 || usage.Input > int64(math.MaxInt) || usage.Output > int64(math.MaxInt) {
		return nil, fmt.Errorf("%w: assistant usage is outside the supported range", ErrInvalidSession)
	}
	if usage.Input == 0 && usage.Output == 0 {
		return nil, nil
	}
	return &model.Usage{InputTokens: int(usage.Input), OutputTokens: int(usage.Output)}, nil
}

func validateAssistantToolFinish(blocks []model.Block, reason model.FinishReason) error {
	hasToolCall := false
	for _, block := range blocks {
		if block.Type == model.BlockToolCall {
			hasToolCall = true
		}
	}
	if hasToolCall && reason != model.FinishToolCalls {
		return fmt.Errorf("%w: assistant tool calls require tool_calls finish reason", ErrInvalidSession)
	}
	if !hasToolCall && reason == model.FinishToolCalls {
		return fmt.Errorf("%w: tool_calls finish reason requires a tool call", ErrInvalidSession)
	}
	return nil
}

func pendingToolCalls(messages []model.Message) ([]model.Block, error) {
	pending := make(map[string]model.Block)
	seen := make(map[string]struct{})
	order := make([]string, 0)
	for _, message := range messages {
		switch message.Role {
		case model.RoleAssistant:
			if len(pending) != 0 {
				return nil, fmt.Errorf("%w: unresolved tool calls must be followed by tool results", ErrInvalidSession)
			}
			for _, block := range message.Blocks {
				if block.Type != model.BlockToolCall {
					continue
				}
				if _, duplicate := seen[block.ToolCallID]; duplicate {
					return nil, fmt.Errorf("%w: duplicate tool-call id", ErrInvalidSession)
				}
				seen[block.ToolCallID] = struct{}{}
				pending[block.ToolCallID] = block
				order = append(order, block.ToolCallID)
			}
		case model.RoleTool:
			if len(message.Blocks) == 0 {
				return nil, fmt.Errorf("%w: tool message must contain a tool result", ErrInvalidSession)
			}
			for _, block := range message.Blocks {
				if block.Type != model.BlockToolResult {
					return nil, fmt.Errorf("%w: tool message contains a non-result block", ErrInvalidSession)
				}
				call, exists := pending[block.ToolCallID]
				if !exists {
					return nil, fmt.Errorf("%w: tool result has no pending call", ErrInvalidSession)
				}
				if call.ToolName != block.ToolName {
					return nil, fmt.Errorf("%w: tool result name does not match pending call", ErrInvalidSession)
				}
				delete(pending, block.ToolCallID)
			}
		default:
			if len(pending) != 0 {
				return nil, fmt.Errorf("%w: unresolved tool calls must be followed by tool results", ErrInvalidSession)
			}
		}
	}
	result := make([]model.Block, 0, len(pending))
	for _, id := range order {
		if call, exists := pending[id]; exists {
			result = append(result, call)
		}
	}
	return result, nil
}

func decodePiFileReadOnly(file *os.File) (piFile, error) {
	finalStart, _, incomplete, err := finalPiRecordState(file)
	if err != nil {
		return piFile{}, err
	}
	if incomplete && finalStart > 0 {
		decoded, _, err := decodePiFile(io.NewSectionReader(file, 0, finalStart))
		return decoded, err
	}
	if _, err := file.Seek(0, io.SeekStart); err != nil {
		return piFile{}, fmt.Errorf("seek session file: %w", err)
	}
	decoded, _, err := decodePiFile(file)
	return decoded, err
}

func decodePiFileForOpen(file *os.File, path string) (piFile, []Warning, error) {
	finalStart, missingLF, incomplete, err := finalPiRecordState(file)
	if err != nil {
		return piFile{}, nil, err
	}
	if incomplete && finalStart > 0 {
		decoded, warnings, err := decodePiFile(io.NewSectionReader(file, 0, finalStart))
		if err != nil {
			return piFile{}, nil, err
		}
		if _, err := resolvePiStoreState(decoded); err != nil {
			return piFile{}, nil, err
		}
		if err := truncateSession(file, finalStart); err != nil {
			return piFile{}, nil, err
		}
		warnings = append(warnings, Warning{Message: fmt.Sprintf("truncated incomplete final session line at %s", path)})
		return decoded, warnings, nil
	}
	if _, err := file.Seek(0, io.SeekStart); err != nil {
		return piFile{}, nil, fmt.Errorf("seek session file: %w", err)
	}
	decoded, warnings, err := decodePiFile(file)
	if err != nil {
		return piFile{}, nil, err
	}
	if missingLF {
		if _, err := resolvePiStoreState(decoded); err != nil {
			return piFile{}, nil, err
		}
		if err := appendSessionDelimiter(file); err != nil {
			return piFile{}, nil, err
		}
		warnings = append(warnings, Warning{Message: fmt.Sprintf("repaired missing final session delimiter at %s", path)})
	}
	return decoded, warnings, nil
}

func finalPiRecordState(file *os.File) (int64, bool, bool, error) {
	info, err := file.Stat()
	if err != nil {
		return 0, false, false, fmt.Errorf("stat session file: %w", err)
	}
	size := info.Size()
	if size == 0 {
		return 0, false, false, nil
	}
	last := []byte{0}
	if _, err := file.ReadAt(last, size-1); err != nil {
		return 0, false, false, fmt.Errorf("read session tail: %w", err)
	}
	if last[0] == '\n' {
		return 0, false, false, nil
	}
	window := int64(maxSessionEntryBytes + 1)
	start := size - window
	if start < 0 {
		start = 0
	}
	buffer := make([]byte, size-start)
	if _, err := file.ReadAt(buffer, start); err != nil && !errors.Is(err, io.EOF) {
		return 0, false, false, fmt.Errorf("read final session record: %w", err)
	}
	separator := bytes.LastIndexByte(buffer, '\n')
	if separator < 0 && start > 0 {
		return 0, true, false, nil
	}
	finalStart := start
	final := buffer
	if separator >= 0 {
		finalStart += int64(separator + 1)
		final = buffer[separator+1:]
	}
	return finalStart, true, isIncompleteJSON(final), nil
}

func writePiRecord(writer durableRecordWriter, record any) (int64, error) {
	encoded, err := encodePiRecord(record)
	if err != nil {
		return 0, err
	}
	return writeEncodedPiRecord(writer, encoded)
}

func writeEncodedPiRecord(writer durableRecordWriter, encoded []byte) (int64, error) {
	record := make([]byte, len(encoded)+1)
	copy(record, encoded)
	record[len(encoded)] = '\n'
	written, err := writer.Write(record)
	if err != nil {
		return 0, fmt.Errorf("write session record: %w", err)
	}
	if written != len(record) {
		return 0, fmt.Errorf("write session record: %w", io.ErrShortWrite)
	}
	if err := writer.Sync(); err != nil {
		return 0, fmt.Errorf("sync session file: %w", err)
	}
	return int64(written), nil
}

func appendSessionDelimiter(file *os.File) error {
	if _, err := file.Seek(0, io.SeekEnd); err != nil {
		return fmt.Errorf("seek session file for delimiter repair: %w", err)
	}
	written, err := file.Write([]byte{'\n'})
	if err != nil {
		return fmt.Errorf("write session delimiter: %w", err)
	}
	if written != 1 {
		return fmt.Errorf("write session delimiter: %w", io.ErrShortWrite)
	}
	if err := file.Sync(); err != nil {
		return fmt.Errorf("sync session delimiter: %w", err)
	}
	return nil
}

func truncateSession(file *os.File, size int64) error {
	if err := file.Truncate(size); err != nil {
		return fmt.Errorf("truncate session file: %w", err)
	}
	if _, err := file.Seek(size, io.SeekStart); err != nil {
		return fmt.Errorf("seek truncated session file: %w", err)
	}
	if err := file.Sync(); err != nil {
		return fmt.Errorf("sync truncated session file: %w", err)
	}
	return nil
}

func rejectOversizedSessionFile(file *os.File) error {
	info, err := file.Stat()
	if err != nil {
		return fmt.Errorf("stat session file: %w", err)
	}
	if info.Size() > int64(maxSessionFileBytes) {
		return sizeError(ErrSessionFileTooLarge, maxSessionFileBytes)
	}
	return nil
}

func isIncompleteJSON(line []byte) bool {
	var value any
	err := json.Unmarshal(line, &value)
	return err != nil && strings.Contains(err.Error(), "unexpected end of JSON input")
}

func validToolArguments(arguments json.RawMessage) bool {
	trimmed := bytes.TrimSpace(arguments)
	if len(trimmed) < 2 || trimmed[0] != '{' || trimmed[len(trimmed)-1] != '}' || !json.Valid(trimmed) {
		return false
	}
	var object map[string]json.RawMessage
	return json.Unmarshal(trimmed, &object) == nil && object != nil
}

func workspaceKey(workspace string) (string, error) {
	canonical, err := canonicalWorkspace(workspace)
	if err != nil {
		return "", err
	}
	sum := sha256.Sum256([]byte(canonical))
	return hex.EncodeToString(sum[:])[:16], nil
}

func canonicalWorkspace(path string) (string, error) {
	abs, err := filepath.Abs(path)
	if err != nil {
		return "", fmt.Errorf("resolve workspace path: %w", err)
	}
	canonical, err := filepath.EvalSymlinks(abs)
	if err == nil {
		return canonical, nil
	}
	if errors.Is(err, os.ErrNotExist) {
		return filepath.Clean(abs), nil
	}
	return "", fmt.Errorf("resolve workspace symlinks: %w", err)
}

func cloneMessages(messages []model.Message) []model.Message {
	cloned := make([]model.Message, len(messages))
	for i, message := range messages {
		cloned[i] = cloneMessage(message)
	}
	return cloned
}

func cloneMessage(message model.Message) model.Message {
	cloned := message
	if message.Blocks != nil {
		cloned.Blocks = make([]model.Block, len(message.Blocks))
		for i, block := range message.Blocks {
			cloned.Blocks[i] = block
			if block.Arguments != nil {
				cloned.Blocks[i].Arguments = append(json.RawMessage(nil), block.Arguments...)
			}
		}
	}
	if message.Usage != nil {
		usage := *message.Usage
		cloned.Usage = &usage
	}
	return cloned
}

func newPiEntryID(seen map[string]struct{}) (string, error) {
	for attempts := 0; attempts < 1024; attempts++ {
		buffer := make([]byte, 4)
		if _, err := rand.Read(buffer); err != nil {
			return "", err
		}
		id := hex.EncodeToString(buffer)
		if _, collision := seen[id]; !collision {
			return id, nil
		}
	}
	return "", errors.New("could not generate a collision-free session entry id")
}

func cloneStringPointer(value *string) *string {
	if value == nil {
		return nil
	}
	return stringPointer(*value)
}

func stringPointer(value string) *string {
	return &value
}
