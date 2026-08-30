package sqlite

import (
	"context"
	"os"
	"path/filepath"
	"testing"

	"github.com/baiyuqing/otto/internal/memory/memorytest"
)

func TestRecordConformance(t *testing.T) {
	memorytest.RunRecordConformance(t, func(t *testing.T) memorytest.Fixture {
		directory := t.TempDir()
		if err := os.Chmod(directory, 0o700); err != nil {
			t.Fatal(err)
		}
		path := filepath.Join(directory, "memory.db")
		options := testOptions(t)
		store, err := Open(context.Background(), path, options)
		if err != nil {
			t.Fatal(err)
		}
		current := store
		return memorytest.Fixture{
			Store: store,
			Reopen: func() (memorytest.RecordStore, error) {
				reopened, err := Open(context.Background(), path, options)
				if err == nil {
					current = reopened
				}
				return reopened, err
			},
			Cleanup: func() {
				if current != nil {
					_ = current.Close()
				}
			},
		}
	})
}
