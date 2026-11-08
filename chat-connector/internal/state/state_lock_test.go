package state

import (
	"errors"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
)

func TestSecondOpenFailsWhileFirstIsOpen(t *testing.T) {
	path := filepath.Join(t.TempDir(), "connect", "state.json")
	first, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer first.Close()

	second, err := Open(path)
	if err == nil {
		second.Close()
		t.Fatal("second Open succeeded while the first store holds the lock")
	}
	if !errors.Is(err, ErrAlreadyRunning) {
		t.Fatalf("err = %v, want ErrAlreadyRunning", err)
	}
	want := "another otto-connect is running (pid " + strconv.Itoa(os.Getpid()) + "); stop it before starting a new one"
	if err.Error() != want {
		t.Fatalf("message = %q, want %q", err.Error(), want)
	}
}

func TestOpenSucceedsAfterClose(t *testing.T) {
	path := filepath.Join(t.TempDir(), "state.json")
	first, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if err := first.Close(); err != nil {
		t.Fatal(err)
	}
	second, err := Open(path)
	if err != nil {
		t.Fatalf("Open after Close: %v", err)
	}
	second.Close()
}

func TestStaleLockFileDoesNotBlockOpen(t *testing.T) {
	path := filepath.Join(t.TempDir(), "state.json")
	if err := os.WriteFile(path+".lock", []byte("99999\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	s, err := Open(path)
	if err != nil {
		t.Fatalf("Open with stale lock file: %v", err)
	}
	defer s.Close()
	raw, err := os.ReadFile(path + ".lock")
	if err != nil {
		t.Fatal(err)
	}
	if got := strings.TrimSpace(string(raw)); got != strconv.Itoa(os.Getpid()) {
		t.Fatalf("lock file pid = %q, want %d", got, os.Getpid())
	}
}
