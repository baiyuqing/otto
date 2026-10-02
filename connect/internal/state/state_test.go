package state

import (
	"os"
	"path/filepath"
	"testing"
)

func TestStoreRoundTripAndMode(t *testing.T) {
	path := filepath.Join(t.TempDir(), "connect", "state.json")
	s, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if err := s.SetSession("telegram:1", "abc"); err != nil {
		t.Fatal(err)
	}
	if err := s.SetSession("telegram:2", "def"); err != nil {
		t.Fatal(err)
	}
	if err := s.SetSession("telegram:2", ""); err != nil {
		t.Fatal(err)
	}
	if err := s.SetTelegramOffset(42); err != nil {
		t.Fatal(err)
	}

	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode().Perm() != 0o600 {
		t.Fatalf("mode = %v, want 0600", info.Mode().Perm())
	}
	dir, err := os.Stat(filepath.Dir(path))
	if err != nil {
		t.Fatal(err)
	}
	if dir.Mode().Perm() != 0o700 {
		t.Fatalf("dir mode = %v, want 0700", dir.Mode().Perm())
	}

	again, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := again.Session("telegram:1"); got != "abc" {
		t.Fatalf("session = %q, want abc", got)
	}
	if got := again.Session("telegram:2"); got != "" {
		t.Fatalf("removed session = %q, want empty", got)
	}
	if got := again.TelegramOffset(); got != 42 {
		t.Fatalf("offset = %d, want 42", got)
	}
}

func TestOpenMissingFileIsEmpty(t *testing.T) {
	s, err := Open(filepath.Join(t.TempDir(), "none.json"))
	if err != nil {
		t.Fatal(err)
	}
	if s.Session("x") != "" || s.TelegramOffset() != 0 {
		t.Fatal("missing file is not empty state")
	}
}
