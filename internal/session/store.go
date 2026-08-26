package session

import (
	"bufio"
	"bytes"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/baiyuqing/otto/internal/model"
)

type durableRecordWriter interface {
	Write([]byte) (int, error)
	Sync() error
}

type Store struct {
	mu       sync.Mutex
	header   Header
	messages []model.Message
	path     string
	file     *os.File
	writer   durableRecordWriter
	fatalErr error
	closed   bool
}

func Create(root string, header Header) (*Store, error) {
	workspaceKey, err := workspaceKey(header.Workspace)
	if err != nil {
		return nil, err
	}
	if err := os.MkdirAll(filepath.Join(root, workspaceKey), 0o700); err != nil {
		return nil, fmt.Errorf("create session directory: %w", err)
	}
	if err := os.Chmod(filepath.Join(root, workspaceKey), 0o700); err != nil {
		return nil, fmt.Errorf("chmod session directory: %w", err)
	}

	path := filepath.Join(root, workspaceKey, header.ID+".jsonl")
	file, err := os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_RDWR, 0o600)
	if err != nil {
		return nil, fmt.Errorf("create session file: %w", err)
	}
	if err := writeRecord(file, newPersistedHeaderRecord(header)); err != nil {
		file.Close()
		return nil, err
	}

	return &Store{header: header, path: path, file: file, writer: file}, nil
}

func ReadHeader(path string) (Header, error) {
	file, err := os.Open(path)
	if err != nil {
		return Header{}, fmt.Errorf("open session file: %w", err)
	}
	defer file.Close()

	line, err := bufio.NewReader(file).ReadBytes('\n')
	if err != nil && !errors.Is(err, io.EOF) {
		return Header{}, fmt.Errorf("read session header: %w", err)
	}
	line = bytes.TrimSuffix(line, []byte{'\n'})
	if len(line) == 0 {
		return Header{}, errors.New("session file is empty")
	}
	record, err := decodeRecord(line)
	if err != nil {
		return Header{}, fmt.Errorf("decode session header: %w", err)
	}
	if err := validateHeaderRecord(record); err != nil {
		return Header{}, err
	}
	return record.Header.sessionHeader(), nil
}

func Open(path string) (*Store, []Warning, error) {
	file, err := os.OpenFile(path, os.O_RDWR, 0)
	if err != nil {
		return nil, nil, fmt.Errorf("open session file: %w", err)
	}

	data, err := io.ReadAll(file)
	if err != nil {
		file.Close()
		return nil, nil, fmt.Errorf("read session file: %w", err)
	}
	lines := splitLines(data)
	if len(lines) == 0 {
		file.Close()
		return nil, nil, errors.New("session file is empty")
	}

	var (
		header         Header
		messages       []model.Message
		warnings       []Warning
		pendingCalls   = make(map[string]string)
		seenCallIDs    = make(map[string]struct{})
		seenMessageIDs = make(map[string]struct{})
	)

	for i, line := range lines {
		if len(line.content) == 0 {
			file.Close()
			return nil, nil, fmt.Errorf("session line %d: empty line", i+1)
		}

		record, err := decodeRecord(line.content)
		if err != nil {
			if i > 0 && i == len(lines)-1 && isIncompleteJSON(line.content) {
				if err := truncateSession(file, line.start); err != nil {
					file.Close()
					return nil, nil, err
				}
				warnings = append(warnings, Warning{Message: fmt.Sprintf("truncated incomplete final session line at %s", path)})
				break
			}
			file.Close()
			return nil, nil, fmt.Errorf("session line %d: %w", i+1, err)
		}

		switch i {
		case 0:
			if err := validateHeaderRecord(record); err != nil {
				file.Close()
				return nil, nil, err
			}
			header = record.Header.sessionHeader()
		default:
			if err := validateMessageRecord(record, pendingCalls, seenCallIDs, seenMessageIDs); err != nil {
				file.Close()
				return nil, nil, fmt.Errorf("session line %d: %w", i+1, err)
			}
			messages = append(messages, record.Message.modelMessage())
		}
	}

	if _, err := file.Seek(0, io.SeekEnd); err != nil {
		file.Close()
		return nil, nil, fmt.Errorf("seek session file: %w", err)
	}

	store := &Store{header: header, messages: messages, path: path, file: file, writer: file}
	repairWarnings, err := store.repairDanglingToolCalls()
	if err != nil {
		file.Close()
		return nil, nil, err
	}
	warnings = append(warnings, repairWarnings...)
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
	if err := writeRecord(s.writer, newPersistedMessageRecord(message)); err != nil {
		s.fatalErr = &fatalPersistenceError{cause: err}
		return s.fatalErr
	}
	s.messages = append(s.messages, cloneMessage(message))
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
	pending := make(map[string]model.Block)
	order := make([]string, 0)

	for _, message := range s.messages {
		switch message.Role {
		case model.RoleAssistant:
			for _, block := range message.Blocks {
				if block.Type == model.BlockToolCall && block.ToolCallID != "" {
					if _, exists := pending[block.ToolCallID]; !exists {
						order = append(order, block.ToolCallID)
					}
					pending[block.ToolCallID] = block
				}
			}
		case model.RoleTool:
			for _, block := range message.Blocks {
				if block.ToolCallID != "" {
					delete(pending, block.ToolCallID)
				}
			}
		}
	}

	var warnings []Warning
	for _, toolCallID := range order {
		block, ok := pending[toolCallID]
		if !ok {
			continue
		}
		id, err := randomID()
		if err != nil {
			return nil, fmt.Errorf("generate repair id: %w", err)
		}
		message := model.Message{
			ID:        id,
			Role:      model.RoleTool,
			CreatedAt: time.Now().UTC(),
			Blocks: []model.Block{{
				Type:       model.BlockToolResult,
				Text:       "tool result missing from prior session",
				ToolCallID: toolCallID,
				ToolName:   block.ToolName,
				IsError:    true,
			}},
		}
		if err := s.Append(context.Background(), message); err != nil {
			return nil, fmt.Errorf("repair dangling tool call %q: %w", toolCallID, err)
		}
		warnings = append(warnings, Warning{Message: fmt.Sprintf("repaired dangling tool call %s", toolCallID)})
	}

	return warnings, nil
}

func writeRecord(file durableRecordWriter, record persistedRecord) error {
	encoded, err := json.Marshal(record)
	if err != nil {
		return fmt.Errorf("encode session record: %w", err)
	}
	if _, err := file.Write(append(encoded, '\n')); err != nil {
		return fmt.Errorf("write session record: %w", err)
	}
	if err := file.Sync(); err != nil {
		return fmt.Errorf("sync session file: %w", err)
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

func decodeRecord(line []byte) (persistedRecord, error) {
	decoder := json.NewDecoder(bytes.NewReader(line))
	decoder.DisallowUnknownFields()
	var record persistedRecord
	if err := decoder.Decode(&record); err != nil {
		return persistedRecord{}, fmt.Errorf("decode session record: %w", err)
	}
	var trailing any
	if err := decoder.Decode(&trailing); !errors.Is(err, io.EOF) {
		if err == nil {
			return persistedRecord{}, errors.New("decode session record: trailing JSON value")
		}
		return persistedRecord{}, fmt.Errorf("decode session record: %w", err)
	}
	return record, nil
}

func isIncompleteJSON(line []byte) bool {
	var value any
	err := json.Unmarshal(line, &value)
	return err != nil && strings.Contains(err.Error(), "unexpected end of JSON input")
}

func validateHeaderRecord(record persistedRecord) error {
	if record.Type != recordTypeHeader || record.Header == nil || record.Message != nil {
		return errors.New("session header record is missing or malformed")
	}
	header := record.Header
	if header.Version != currentVersion {
		return fmt.Errorf("unsupported session version %d", header.Version)
	}
	if strings.TrimSpace(header.ID) == "" {
		return errors.New("session header id is required")
	}
	if strings.TrimSpace(header.Workspace) == "" {
		return errors.New("session header workspace is required")
	}
	if strings.TrimSpace(header.Provider) == "" {
		return errors.New("session header provider is required")
	}
	if strings.TrimSpace(header.Model) == "" {
		return errors.New("session header model is required")
	}
	if header.CreatedAt.IsZero() {
		return errors.New("session header timestamp is required")
	}
	return nil
}

func validateMessageRecord(record persistedRecord, pendingCalls map[string]string, seenCallIDs, seenMessageIDs map[string]struct{}) error {
	if record.Type != recordTypeMessage || record.Message == nil || record.Header != nil {
		return errors.New("invalid message record shape")
	}
	message := record.Message
	if strings.TrimSpace(message.ID) == "" {
		return errors.New("message id is required")
	}
	if _, exists := seenMessageIDs[message.ID]; exists {
		return fmt.Errorf("duplicate message id %q", message.ID)
	}
	seenMessageIDs[message.ID] = struct{}{}
	if message.CreatedAt.IsZero() {
		return errors.New("message timestamp is required")
	}
	if message.Usage != nil {
		if message.Role != model.RoleAssistant {
			return errors.New("usage is only valid on assistant messages")
		}
		if message.Usage.InputTokens < 0 || message.Usage.OutputTokens < 0 {
			return errors.New("usage token counts must be nonnegative")
		}
	}

	if len(pendingCalls) > 0 && message.Role != model.RoleTool {
		return errors.New("unresolved tool calls must be followed by tool results")
	}

	hasToolCall := false
	switch message.Role {
	case model.RoleUser:
		if message.FinishReason != "" {
			return errors.New("finish reason is not valid on user messages")
		}
	case model.RoleAssistant:
		if !validFinishReason(message.FinishReason) {
			return fmt.Errorf("invalid assistant finish reason %q", message.FinishReason)
		}
	case model.RoleTool:
		if message.FinishReason != "" {
			return errors.New("finish reason is not valid on tool messages")
		}
	default:
		return fmt.Errorf("invalid message role %q", message.Role)
	}
	if len(message.Blocks) == 0 && message.Role != model.RoleAssistant {
		return errors.New("message blocks are required")
	}

	for _, block := range message.Blocks {
		switch block.Type {
		case model.BlockText:
			if message.Role != model.RoleUser && message.Role != model.RoleAssistant {
				return fmt.Errorf("text block is incompatible with role %q", message.Role)
			}
			if block.ToolCallID != "" || block.ToolName != "" || len(block.Arguments) != 0 || block.IsError {
				return errors.New("text block contains incompatible fields")
			}
		case model.BlockToolCall:
			if message.Role != model.RoleAssistant {
				return fmt.Errorf("tool-call block is incompatible with role %q", message.Role)
			}
			if strings.TrimSpace(block.ToolCallID) == "" || strings.TrimSpace(block.ToolName) == "" {
				return errors.New("tool-call id and name are required")
			}
			if !validToolArguments(block.Arguments) {
				return errors.New("tool-call arguments must be a valid JSON object")
			}
			if block.Text != "" || block.IsError {
				return errors.New("tool-call block contains incompatible fields")
			}
			if _, exists := seenCallIDs[block.ToolCallID]; exists {
				return fmt.Errorf("duplicate tool-call id %q", block.ToolCallID)
			}
			seenCallIDs[block.ToolCallID] = struct{}{}
			pendingCalls[block.ToolCallID] = block.ToolName
			hasToolCall = true
		case model.BlockToolResult:
			if message.Role != model.RoleTool {
				return fmt.Errorf("tool-result block is incompatible with role %q", message.Role)
			}
			if strings.TrimSpace(block.ToolCallID) == "" || strings.TrimSpace(block.ToolName) == "" {
				return errors.New("tool-result id and name are required")
			}
			if len(block.Arguments) != 0 {
				return errors.New("tool-result block contains incompatible arguments")
			}
			name, exists := pendingCalls[block.ToolCallID]
			if !exists {
				return fmt.Errorf("tool result %q has no pending call", block.ToolCallID)
			}
			if name != block.ToolName {
				return fmt.Errorf("tool result %q name does not match pending call", block.ToolCallID)
			}
			delete(pendingCalls, block.ToolCallID)
		default:
			return fmt.Errorf("invalid block type %q", block.Type)
		}
	}
	if message.Role == model.RoleAssistant {
		if hasToolCall && message.FinishReason != model.FinishToolCalls {
			return errors.New("assistant tool calls require tool_calls finish reason")
		}
		if !hasToolCall && message.FinishReason == model.FinishToolCalls {
			return errors.New("tool_calls finish reason requires a tool call")
		}
	}
	return nil
}

func validFinishReason(reason model.FinishReason) bool {
	switch reason {
	case model.FinishStop, model.FinishToolCalls, model.FinishLength, model.FinishUnknown:
		return true
	default:
		return false
	}
}

func validToolArguments(arguments json.RawMessage) bool {
	trimmed := bytes.TrimSpace(arguments)
	if len(trimmed) < 2 || trimmed[0] != '{' || trimmed[len(trimmed)-1] != '}' || !json.Valid(trimmed) {
		return false
	}
	var object map[string]json.RawMessage
	return json.Unmarshal(trimmed, &object) == nil && object != nil
}

type lineInfo struct {
	start   int64
	content []byte
}

func splitLines(data []byte) []lineInfo {
	lines := make([]lineInfo, 0)
	var start int64
	for i, b := range data {
		if b != '\n' {
			continue
		}
		lines = append(lines, lineInfo{start: start, content: data[start:int64(i)]})
		start = int64(i + 1)
	}
	if start < int64(len(data)) {
		lines = append(lines, lineInfo{start: start, content: data[start:]})
	}
	return lines
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

func randomID() (string, error) {
	buf := make([]byte, 16)
	if _, err := rand.Read(buf); err != nil {
		return "", err
	}
	return hex.EncodeToString(buf), nil
}
