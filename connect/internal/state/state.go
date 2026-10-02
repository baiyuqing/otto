// Package state stores the connector's state file: the ACP session of each
// chat and the Telegram update offset.
package state

import (
	"encoding/json"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"sync"
)

type file struct {
	Sessions       map[string]string `json:"sessions"`
	TelegramOffset int64             `json:"telegram_offset,omitempty"`
}

// Store is the state file. Every setter writes the whole file (mode 0600,
// directory 0700) through a temporary file and a rename before it returns.
// Store is safe for concurrent use.
type Store struct {
	path string
	mu   sync.Mutex
	data file
}

// Open reads path. A missing file is an empty state.
func Open(path string) (*Store, error) {
	s := &Store{path: path, data: file{Sessions: map[string]string{}}}
	raw, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return s, nil
	}
	if err != nil {
		return nil, err
	}
	if err := json.Unmarshal(raw, &s.data); err != nil {
		return nil, err
	}
	if s.data.Sessions == nil {
		s.data.Sessions = map[string]string{}
	}
	return s, nil
}

// Session returns the session id of chat ("<platform>:<chat id>"), or "".
func (s *Store) Session(chat string) string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.data.Sessions[chat]
}

// SetSession records sessionID for chat; "" removes the entry.
func (s *Store) SetSession(chat, sessionID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if sessionID == "" {
		delete(s.data.Sessions, chat)
	} else {
		s.data.Sessions[chat] = sessionID
	}
	return s.save()
}

// TelegramOffset is the next getUpdates offset, 0 when none is stored.
func (s *Store) TelegramOffset() int64 {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.data.TelegramOffset
}

// SetTelegramOffset records the next getUpdates offset.
func (s *Store) SetTelegramOffset(offset int64) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.data.TelegramOffset = offset
	return s.save()
}

func (s *Store) save() error {
	raw, err := json.MarshalIndent(s.data, "", "  ")
	if err != nil {
		return err
	}
	dir := filepath.Dir(s.path)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return err
	}
	tmp, err := os.CreateTemp(dir, ".state-*.json")
	if err != nil {
		return err
	}
	defer os.Remove(tmp.Name())
	if _, err := tmp.Write(raw); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Chmod(0o600); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	return os.Rename(tmp.Name(), s.path)
}
