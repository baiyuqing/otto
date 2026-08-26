package session

import (
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
	"sync"
	"time"

	"github.com/baiyuqing/otto/internal/model"
)

type Store struct {
	mu       sync.Mutex
	header   Header
	messages []model.Message
	path     string
	file     *os.File
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

	return &Store{header: header, path: path, file: file}, nil
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
		header   Header
		messages []model.Message
		warnings []Warning
	)

	for i, line := range lines {
		if len(line.content) == 0 {
			file.Close()
			return nil, nil, fmt.Errorf("session line %d: empty line", i+1)
		}

		record, err := decodeRecord(line.content)
		if err != nil {
			if i > 0 && i == len(lines)-1 {
				if err := truncateSession(file, line.start); err != nil {
					file.Close()
					return nil, nil, err
				}
				warnings = append(warnings, Warning{Message: fmt.Sprintf("truncated malformed final session line at %s", path)})
				break
			}
			file.Close()
			return nil, nil, fmt.Errorf("session line %d: %w", i+1, err)
		}

		switch i {
		case 0:
			if record.Type != recordTypeHeader || record.Header == nil {
				file.Close()
				return nil, nil, errors.New("session header record is missing")
			}
			if record.Header.Version != currentVersion {
				file.Close()
				return nil, nil, fmt.Errorf("unsupported session version %d", record.Header.Version)
			}
			header = record.Header.sessionHeader()
		default:
			if record.Type != recordTypeMessage || record.Message == nil {
				file.Close()
				return nil, nil, fmt.Errorf("session line %d: invalid record", i+1)
			}
			messages = append(messages, record.Message.modelMessage())
		}
	}

	if _, err := file.Seek(0, io.SeekEnd); err != nil {
		file.Close()
		return nil, nil, fmt.Errorf("seek session file: %w", err)
	}

	store := &Store{header: header, messages: messages, path: path, file: file}
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
	if err := ctx.Err(); err != nil {
		return err
	}

	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return errSessionClosed
	}
	if err := writeRecord(s.file, newPersistedMessageRecord(message)); err != nil {
		return err
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

func writeRecord(file *os.File, record persistedRecord) error {
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
	var record persistedRecord
	if err := json.Unmarshal(line, &record); err != nil {
		return persistedRecord{}, fmt.Errorf("decode session record: %w", err)
	}
	return record, nil
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
