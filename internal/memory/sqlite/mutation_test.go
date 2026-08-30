package sqlite

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"strconv"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
	"github.com/baiyuqing/otto/internal/memory/memorytest"
)

func TestMutationConformance(t *testing.T) {
	memorytest.RunMutationConformance(t, newMutationFixture)
}

func TestMutationDetectsGuardedSnapshotRacesAtomically(t *testing.T) {
	t.Run("same revision digest race", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "memory.db")
		store := openTestStore(t, path)
		created := mustSQLiteCreate(t, store, sqliteTestRecord("digest-race", "digest-race", time.Date(2026, 8, 29, 19, 0, 0, 0, time.UTC)))
		var raced atomic.Bool
		var racedState memorytest.MutationPersistence
		installTestHooks(t, testHooks{afterMutationPreRead: func() {
			if !raced.CompareAndSwap(false, true) {
				return
			}
			database := openExternalSQLite(t, path)
			defer database.Close()
			if _, err := database.Exec(`UPDATE memory_records SET text_value='raced persistent text' WHERE id=?`, created.ID); err != nil {
				t.Fatal(err)
			}
			if _, err := database.Exec(`UPDATE memory_records_fts SET text_value='raced persistent text' WHERE record_id=?`, created.ID); err != nil {
				t.Fatal(err)
			}
			var err error
			racedState, err = inspectMutationPersistence(context.Background(), path, created.ID)
			if err != nil {
				t.Fatal(err)
			}
		}})
		desired := memory.CloneRecord(created)
		desired.Text = "outer update"
		desired.UpdatedAt = desired.UpdatedAt.Add(time.Hour)
		expected := uint64(1)
		if _, err := store.Upsert(context.Background(), memory.UpsertRequest{Record: desired, ExpectedRevision: &expected}); !errors.Is(err, memory.ErrConflict) {
			t.Fatalf("digest race = %v", err)
		}
		after, err := inspectMutationPersistence(context.Background(), path, created.ID)
		if err != nil || !reflect.DeepEqual(after, racedState) {
			t.Fatalf("digest race persistence = %#v, %v; want %#v", after, err, racedState)
		}
	})

	t.Run("active revision race", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "memory.db")
		store := openTestStore(t, path)
		created := mustSQLiteCreate(t, store, sqliteTestRecord("revision-race", "revision-race", time.Date(2026, 8, 29, 19, 5, 0, 0, time.UTC)))
		inner := memory.CloneRecord(created)
		inner.Text = "winning concurrent update"
		inner.UpdatedAt = inner.UpdatedAt.Add(time.Hour)
		expected := uint64(1)
		var raced atomic.Bool
		var racedState memorytest.MutationPersistence
		installTestHooks(t, testHooks{afterMutationPreRead: func() {
			if !raced.CompareAndSwap(false, true) {
				return
			}
			if _, err := store.Upsert(context.Background(), memory.UpsertRequest{Record: inner, ExpectedRevision: &expected}); err != nil {
				t.Fatal(err)
			}
			var err error
			racedState, err = inspectMutationPersistence(context.Background(), path, created.ID)
			if err != nil {
				t.Fatal(err)
			}
		}})
		outer := memory.CloneRecord(created)
		outer.Text = "losing concurrent update"
		outer.UpdatedAt = outer.UpdatedAt.Add(2 * time.Hour)
		if _, err := store.Upsert(context.Background(), memory.UpsertRequest{Record: outer, ExpectedRevision: &expected}); !errors.Is(err, memory.ErrConflict) {
			t.Fatalf("revision race = %v", err)
		}
		after, err := inspectMutationPersistence(context.Background(), path, created.ID)
		if err != nil || !reflect.DeepEqual(after, racedState) {
			t.Fatalf("revision race persistence = %#v, %v; want %#v", after, err, racedState)
		}
	})

	t.Run("active to tombstone race", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "memory.db")
		store := openTestStore(t, path)
		created := mustSQLiteCreate(t, store, sqliteTestRecord("state-race", "state-race", time.Date(2026, 8, 29, 19, 10, 0, 0, time.UTC)))
		inner := memory.StoreForgetRequest{Ref: memory.RecordRef{Scope: created.Scope, ID: created.ID}, ExpectedRevision: 1, ForgottenAt: created.UpdatedAt.Add(time.Hour)}
		var raced atomic.Bool
		var racedState memorytest.MutationPersistence
		installTestHooks(t, testHooks{afterMutationPreRead: func() {
			if !raced.CompareAndSwap(false, true) {
				return
			}
			if _, err := store.Forget(context.Background(), inner); err != nil {
				t.Fatal(err)
			}
			var err error
			racedState, err = inspectMutationPersistence(context.Background(), path, created.ID)
			if err != nil {
				t.Fatal(err)
			}
		}})
		outer := inner
		outer.ForgottenAt = outer.ForgottenAt.Add(time.Hour)
		if _, err := store.Forget(context.Background(), outer); !errors.Is(err, memory.ErrConflict) {
			t.Fatalf("state race = %v", err)
		}
		after, err := inspectMutationPersistence(context.Background(), path, created.ID)
		if err != nil || !reflect.DeepEqual(after, racedState) {
			t.Fatalf("state race persistence = %#v, %v; want %#v", after, err, racedState)
		}
	})
}

func TestMutationUpdateReplacesOneExactFTSRow(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	store := openTestStore(t, path)
	created := mustSQLiteCreate(t, store, sqliteTestRecord("fts-update", "old-key", time.Date(2026, 8, 29, 19, 15, 0, 0, time.UTC)))
	desired := memory.CloneRecord(created)
	desired.Text, desired.Kind, desired.Key = "new searchable text", "fact", "new-key"
	desired.Labels = []string{"zulu", "alpha"}
	desired.UpdatedAt = desired.UpdatedAt.Add(time.Hour)
	expected := uint64(1)
	if _, err := store.Upsert(context.Background(), memory.UpsertRequest{Record: desired, ExpectedRevision: &expected}); err != nil {
		t.Fatal(err)
	}
	database := openExternalSQLite(t, path)
	defer database.Close()
	var count int
	var text, kind, key, labels string
	if err := database.QueryRow(`SELECT count(*),text_value,kind,semantic_key,labels FROM memory_records_fts WHERE record_id=?`, created.ID).
		Scan(&count, &text, &kind, &key, &labels); err != nil {
		t.Fatal(err)
	}
	if count != 1 || text != desired.Text || kind != desired.Kind || key != desired.Key || labels != "alpha\nzulu" {
		t.Fatalf("FTS replacement = %d / %q / %q / %q / %q", count, text, kind, key, labels)
	}
}

func TestTombstoneProjectionDoesNotMaterializeForgottenContent(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	store := openTestStore(t, path)
	record := mustSQLiteCreate(t, store, sqliteTestRecord("unsafe-tombstone", "unsafe-tombstone", time.Date(2026, 8, 29, 19, 20, 0, 0, time.UTC)))
	database := openExternalSQLite(t, path)
	forgotten := formatTimestamp(record.UpdatedAt.Add(time.Hour))
	secret := strings.Repeat("FORGOTTEN-CONTENT-MUST-NOT-CROSS-DRIVER-", (1<<20)/40+1)
	if _, err := database.Exec(`UPDATE memory_records SET state='tombstone',revision=2,updated_at=?,forgotten_at=?,text_value=? WHERE id=?`, forgotten, forgotten, secret, record.ID); err != nil {
		t.Fatal(err)
	}
	if err := database.Close(); err != nil {
		t.Fatal(err)
	}
	ref := memory.RecordRef{Scope: record.Scope, ID: record.ID}
	if _, err := store.GetTombstone(context.Background(), ref); !errors.Is(err, memory.ErrCorrupt) || strings.Contains(err.Error(), "FORGOTTEN-CONTENT") {
		t.Fatalf("unsafe tombstone Get = %v", err)
	}
	if _, err := store.ListTombstones(context.Background(), memory.TombstoneListRequest{Scopes: []memory.Scope{record.Scope}, Limit: 1}); !errors.Is(err, memory.ErrCorrupt) || strings.Contains(err.Error(), "FORGOTTEN-CONTENT") {
		t.Fatalf("unsafe tombstone List = %v", err)
	}
}

func newMutationFixture(t *testing.T) memorytest.Fixture {
	t.Helper()
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
	forget := func(ctx context.Context, request memory.StoreForgetRequest) (memory.Tombstone, error) {
		mutation, ok := any(store).(interface {
			Forget(context.Context, memory.StoreForgetRequest) (memory.Tombstone, error)
		})
		if !ok {
			return memory.Tombstone{}, memory.ErrUnsupported
		}
		return mutation.Forget(ctx, request)
	}
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
		InspectMutation: func(ctx context.Context, id string) (memorytest.MutationPersistence, error) {
			return inspectMutationPersistence(ctx, path, id)
		},
		ForgetBeforeCommitCancel: func(ctx context.Context, request memory.StoreForgetRequest) (memory.Tombstone, error) {
			ctx, cancel := context.WithCancel(ctx)
			defer cancel()
			restore := setTestHooks(testHooks{beforeCommitCheck: cancel})
			defer restore()
			return forget(ctx, request)
		},
		ForgetCommitResponseLoss: func(ctx context.Context, request memory.StoreForgetRequest) (memory.Tombstone, error) {
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
			return forget(ctx, request)
		},
		ForbiddenValue: testForbidden,
	}
}

func inspectMutationPersistence(ctx context.Context, path, id string) (memorytest.MutationPersistence, error) {
	database, err := sql.Open(driverName, path)
	if err != nil {
		return memorytest.MutationPersistence{}, err
	}
	defer database.Close()
	database.SetMaxOpenConns(1)
	var state memorytest.MutationPersistence
	var generation string
	if err := database.QueryRowContext(ctx, `SELECT value FROM memory_meta WHERE key='generation'`).Scan(&generation); err != nil {
		return state, err
	}
	state.Generation, err = strconv.ParseUint(generation, 10, 64)
	if err != nil {
		return state, err
	}
	if err := database.QueryRowContext(ctx, `SELECT count(*),count(*) FILTER (WHERE state='active'),count(*) FILTER (WHERE state='tombstone') FROM memory_records WHERE id=?`, id).
		Scan(&state.RecordRows, &state.ActiveRows, &state.TombstoneRows); err != nil {
		return state, err
	}
	if err := database.QueryRowContext(ctx, `SELECT count(*) FROM memory_records_fts WHERE record_id=?`, id).Scan(&state.FTSRows); err != nil {
		return state, err
	}
	var cleared int
	if err := database.QueryRowContext(ctx, `SELECT count(*) FROM memory_records WHERE id=? AND state='tombstone'
		AND kind='' AND semantic_key='' AND text_value='' AND labels_json='[]' AND metadata_json='{}'
		AND source_json='{}' AND confidence=0.0 AND expires_at IS NULL`, id).Scan(&cleared); err != nil {
		return state, err
	}
	state.ContentCleared = state.TombstoneRows == 1 && cleared == 1
	state.RowDigest, err = digestQuery(ctx, database, `SELECT id,scope_namespace,scope_id,kind,semantic_key,text_value,labels_json,metadata_json,source_json,confidence,revision,created_at,updated_at,expires_at,state,forgotten_at FROM memory_records WHERE id=? ORDER BY id`, id)
	if err != nil {
		return state, err
	}
	state.FTSDigest, err = digestQuery(ctx, database, `SELECT record_id,text_value,kind,semantic_key,labels FROM memory_records_fts WHERE record_id=? ORDER BY rowid`, id)
	return state, err
}

func digestQuery(ctx context.Context, database *sql.DB, query string, arguments ...any) (string, error) {
	rows, err := database.QueryContext(ctx, query, arguments...)
	if err != nil {
		return "", err
	}
	defer rows.Close()
	columns, err := rows.Columns()
	if err != nil {
		return "", err
	}
	digest := sha256.New()
	for rows.Next() {
		values := make([]any, len(columns))
		pointers := make([]any, len(columns))
		for index := range values {
			pointers[index] = &values[index]
		}
		if err := rows.Scan(pointers...); err != nil {
			return "", err
		}
		for _, value := range values {
			switch value := value.(type) {
			case nil:
				digest.Write([]byte{0})
			case int64:
				digest.Write([]byte("i" + strconv.FormatInt(value, 10) + "\x00"))
			case float64:
				digest.Write([]byte("f" + strconv.FormatFloat(value, 'g', -1, 64) + "\x00"))
			case bool:
				digest.Write([]byte("b" + strconv.FormatBool(value) + "\x00"))
			case []byte:
				digest.Write([]byte("s" + strconv.Itoa(len(value)) + ":"))
				digest.Write(value)
			case string:
				digest.Write([]byte("s" + strconv.Itoa(len(value)) + ":" + value))
			}
		}
		digest.Write([]byte{0xff})
	}
	if err := rows.Err(); err != nil {
		return "", err
	}
	return hex.EncodeToString(digest.Sum(nil)), nil
}
