package sqlite

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/memory"
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
		options.Guard = testGuard(t, testForbidden)
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
			Inspect: func(ctx context.Context, id string) (memorytest.Persistence, error) {
				database := openExternalSQLite(t, path)
				defer database.Close()
				var state memorytest.Persistence
				var generation string
				if err := database.QueryRowContext(ctx, `SELECT value FROM memory_meta WHERE key='generation'`).Scan(&generation); err != nil {
					return state, err
				}
				parsed, err := strconv.ParseUint(generation, 10, 64)
				if err != nil {
					return state, err
				}
				state.Generation = parsed
				if err := database.QueryRowContext(ctx, `SELECT count(*) FROM memory_records WHERE id=?`, id).Scan(&state.RecordRows); err != nil {
					return state, err
				}
				if err := database.QueryRowContext(ctx, `SELECT count(*) FROM memory_records_fts WHERE record_id=?`, id).Scan(&state.FTSRows); err != nil {
					return state, err
				}
				return state, nil
			},
			Inject: func(record memory.Record, corruption memorytest.Corruption) error {
				row := makeRawRecord(t, record)
				switch corruption {
				case memorytest.CorruptTextOneOver:
					row.text = strings.Repeat("T", memory.MaxRecordTextBytes+1)
				case memorytest.CorruptMetadataOneOver:
					row.metadata = `{"x":"` + strings.Repeat("M", memory.MaxMetadataBytes) + `"}`
				case memorytest.CorruptTextOneMiB:
					row.text = strings.Repeat("HUGE-PROJECTION-", (1<<20)/16+1)
				case memorytest.CorruptLabelsMalformed:
					row.labels = `[`
				case memorytest.CorruptLabelsOversized:
					row.labels = `["` + strings.Repeat("L", maxLabelsJSONBytes) + `"]`
				case memorytest.CorruptLabelsWrongShape:
					row.labels = `[1]`
				case memorytest.CorruptExpiryMalformed:
					row.expires = "!malformed-expiry"
				case memorytest.CorruptExpiryNoncanonical:
					row.expires = "2026-08-29T13:20:00.000000000Y"
				case memorytest.CorruptSensitiveText:
					row.text = "prefix [REDACTED] suffix"
				default:
					return errors.New("unknown conformance corruption")
				}
				if corruption == memorytest.CorruptExpiryMalformed || corruption == memorytest.CorruptExpiryNoncanonical {
					now := record.CreatedAt
					rawExpiry, ok := row.expires.(string)
					if !ok || rawExpiry > formatTimestamp(now) {
						t.Fatalf("expiry corruption fixture %q = %q, must compare <= canonical Now", corruption, rawExpiry)
					}
					if corruption == memorytest.CorruptExpiryNoncanonical && len(rawExpiry) != len(timestampLayout) {
						t.Fatalf("noncanonical expiry fixture length = %d, want %d", len(rawExpiry), len(timestampLayout))
					}
				}
				database := openExternalSQLite(t, path)
				defer database.Close()
				insertRawRecord(t, database, row)
				return nil
			},
			UpsertBeforeCommitCancel: func(ctx context.Context, request memory.UpsertRequest) (memory.Record, error) {
				ctx, cancel := context.WithCancel(ctx)
				defer cancel()
				restore := setTestHooks(testHooks{beforeCommitCheck: cancel})
				defer restore()
				return store.Upsert(ctx, request)
			},
			UpsertCommitResponseLoss: func(ctx context.Context, request memory.UpsertRequest) (memory.Record, error) {
				restore := setTestHooks(testHooks{driverExec: func(statement string, exec func() error) error {
					if statement != "COMMIT" {
						return exec()
					}
					if err := exec(); err != nil {
						return err
					}
					return errors.New("synthetic commit response loss")
				}})
				defer restore()
				return store.Upsert(ctx, request)
			},
			ForbiddenValue: testForbidden,
		}
	})
}
