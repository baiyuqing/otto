package bridge

import (
	"testing"
	"time"
)

func TestLockSessionReleasesEntries(t *testing.T) {
	t.Parallel()
	b := &Bridge{sessLocks: map[string]*sessLock{}}
	var unlocks []func()
	for _, id := range []string{"a", "b", "c"} {
		unlocks = append(unlocks, b.lockSession(id))
	}
	if len(b.sessLocks) != 3 {
		t.Fatalf("held entries = %d, want 3", len(b.sessLocks))
	}
	for _, unlock := range unlocks {
		unlock()
	}
	if len(b.sessLocks) != 0 {
		t.Fatalf("entries after unlock = %d, want 0", len(b.sessLocks))
	}
}

func TestLockSessionSerializesOneID(t *testing.T) {
	t.Parallel()
	b := &Bridge{sessLocks: map[string]*sessLock{}}
	first := b.lockSession("a")
	acquired := make(chan func())
	go func() { acquired <- b.lockSession("a") }()
	select {
	case <-acquired:
		t.Fatal("second holder acquired the lock while the first held it")
	case <-time.After(50 * time.Millisecond):
	}
	first()
	var second func()
	select {
	case second = <-acquired:
	case <-time.After(5 * time.Second):
		t.Fatal("second holder did not acquire the lock after unlock")
	}
	second()
	b.mu.Lock()
	defer b.mu.Unlock()
	if len(b.sessLocks) != 0 {
		t.Fatalf("entries after both unlocks = %d, want 0", len(b.sessLocks))
	}
}
