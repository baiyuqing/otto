// Package state stores the connector's state file: the ACP session of each
// chat and the Telegram update offset.
package state

import (
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"maps"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
)

// ErrAlreadyRunning means another process holds the state lock.
var ErrAlreadyRunning = errors.New("another otto-connect is running")

type file struct {
	Sessions       map[string]string `json:"sessions"`
	TelegramOffset int64             `json:"telegram_offset,omitempty"`
}

// Store is the state file. Every setter writes the whole file (mode 0600,
// directory 0700) through a temporary file and a rename before it returns.
// Store is safe for concurrent use. Open takes an exclusive flock on
// "<path>.lock"; the lock is held until Close or process exit.
type Store struct {
	path string
	lock *os.File
	mu   sync.Mutex
	data file
}

// Open locks "<path>.lock" and reads path. A missing file is an empty state.
// It returns an error wrapping ErrAlreadyRunning when another process (or an
// open Store) holds the lock.
func Open(path string) (*Store, error) {
	lock, err := acquireLock(path + ".lock")
	if err != nil {
		return nil, err
	}
	s := &Store{path: path, lock: lock, data: file{Sessions: map[string]string{}}}
	raw, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return s, nil
	}
	if err != nil {
		lock.Close()
		return nil, err
	}
	if err := json.Unmarshal(raw, &s.data); err != nil {
		lock.Close()
		return nil, err
	}
	if s.data.Sessions == nil {
		s.data.Sessions = map[string]string{}
	}
	return s, nil
}

// Close releases the lock taken by Open.
func (s *Store) Close() error {
	if s.lock == nil {
		return nil
	}
	err := s.lock.Close()
	s.lock = nil
	return err
}

func acquireLock(lockPath string) (*os.File, error) {
	if err := os.MkdirAll(filepath.Dir(lockPath), 0o700); err != nil {
		return nil, fmt.Errorf("create state directory for %s: %w", lockPath, err)
	}
	f, err := os.OpenFile(lockPath, os.O_RDWR|os.O_CREATE, 0o600)
	if err != nil {
		return nil, fmt.Errorf("open lock %s: %w", lockPath, err)
	}
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		holder := readHolderPid(f)
		f.Close()
		if errors.Is(err, syscall.EWOULDBLOCK) {
			if holder != "" {
				return nil, fmt.Errorf("%w (pid %s); stop it before starting a new one", ErrAlreadyRunning, holder)
			}
			return nil, fmt.Errorf("%w; stop it before starting a new one", ErrAlreadyRunning)
		}
		return nil, fmt.Errorf("lock %s: %w", lockPath, err)
	}
	if err := f.Truncate(0); err != nil {
		f.Close()
		return nil, fmt.Errorf("write lock %s: %w", lockPath, err)
	}
	if _, err := f.WriteAt([]byte(strconv.Itoa(os.Getpid())+"\n"), 0); err != nil {
		f.Close()
		return nil, fmt.Errorf("write lock %s: %w", lockPath, err)
	}
	return f, nil
}

func readHolderPid(f *os.File) string {
	buf := make([]byte, 32)
	n, _ := f.ReadAt(buf, 0)
	return strings.TrimSpace(string(buf[:n]))
}

// Session returns the session id of chat ("<platform>:<chat id>"), or "".
func (s *Store) Session(chat string) string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.data.Sessions[chat]
}

// Sessions returns a copy of the chat to session id map.
func (s *Store) Sessions() map[string]string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return maps.Clone(s.data.Sessions)
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
