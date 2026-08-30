package sqlite

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
)

var _ memory.Factory = Factory{}

func TestFactoryComponentsCapabilitiesAndMaintenance(t *testing.T) {
	directory := t.TempDir()
	if err := os.Chmod(directory, 0o700); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(directory, "memory.db")
	options := testOptions(t)
	factory := NewFactory(path, options)
	// NewFactory must retain its own value copy.
	options.Guard = nil
	options.BusyTimeout = -time.Second

	components, err := factory.Open(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if components.Store == nil || components.Retriever == nil || components.Maintenance == nil {
		t.Fatalf("incomplete components: %#v", components)
	}
	wantCapabilities := memory.Capabilities{
		LexicalSearch:       true,
		SemanticSearch:      false,
		OnlineBackup:        false,
		EncryptionAtRest:    false,
		ConcurrentProcesses: false,
	}
	if components.Capabilities != wantCapabilities {
		t.Fatalf("capabilities = %#v, want %#v", components.Capabilities, wantCapabilities)
	}
	if _, err := components.Store.Identity(context.Background()); err != nil {
		t.Fatalf("Identity: %v", err)
	}

	maintenanceCalls := []struct {
		name string
		call func(context.Context) error
	}{
		{"Backup", func(ctx context.Context) error {
			_, err := components.Maintenance.Backup(ctx, memory.BackupRequest{Class: "opaque"})
			return err
		}},
		{"ListBackups", func(ctx context.Context) error { _, err := components.Maintenance.ListBackups(ctx); return err }},
		{"VerifyBackup", func(ctx context.Context) error {
			_, err := components.Maintenance.VerifyBackup(ctx, "opaque-id")
			return err
		}},
		{"Restore", func(ctx context.Context) error {
			return components.Maintenance.Restore(ctx, memory.RestoreRequest{Backup: "opaque-id"})
		}},
		{"PurgeForgotten", func(ctx context.Context) error {
			_, err := components.Maintenance.PurgeForgotten(ctx, memory.PurgeForgottenRequest{})
			return err
		}},
	}
	for _, test := range maintenanceCalls {
		t.Run(test.name, func(t *testing.T) {
			if err := test.call(context.Background()); err != memory.ErrUnsupported {
				t.Fatalf("error = %v, want exact ErrUnsupported", err)
			}
			canceled, cancel := context.WithCancel(context.Background())
			cancel()
			if err := test.call(canceled); !errors.Is(err, context.Canceled) {
				t.Fatalf("canceled error = %v", err)
			}
		})
	}

	if err := components.Store.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := components.Store.Identity(context.Background()); !errors.Is(err, memory.ErrClosed) {
		t.Fatalf("Store after Close = %v", err)
	}
	if _, err := components.Retriever.Retrieve(context.Background(), memory.RetrievalRequest{}); !errors.Is(err, memory.ErrClosed) {
		t.Fatalf("Retriever after Close = %v", err)
	}
	if err := maintenanceCalls[0].call(context.Background()); err != memory.ErrUnsupported {
		t.Fatalf("Maintenance after Close = %v", err)
	}
}

func TestFactoryAndDirectOpenRejectCorruption(t *testing.T) {
	mutations := []struct {
		name   string
		mutate func(*testing.T, string, memory.Record)
	}{
		{"missing FTS", func(t *testing.T, path string, record memory.Record) {
			db := openExternalSQLite(t, path)
			defer db.Close()
			if _, err := db.Exec(`DELETE FROM memory_records_fts WHERE record_id=?`, record.ID); err != nil {
				t.Fatal(err)
			}
		}},
		{"duplicate FTS", func(t *testing.T, path string, record memory.Record) {
			db := openExternalSQLite(t, path)
			defer db.Close()
			if _, err := db.Exec(`INSERT INTO memory_records_fts(record_id,text_value,kind,semantic_key,labels) VALUES(?,?,?,?,?)`, record.ID, record.Text, record.Kind, record.Key, "alpha beta"); err != nil {
				t.Fatal(err)
			}
		}},
		{"orphan FTS", func(t *testing.T, path string, _ memory.Record) {
			db := openExternalSQLite(t, path)
			defer db.Close()
			if _, err := db.Exec(`INSERT INTO memory_records_fts(record_id,text_value,kind,semantic_key,labels) VALUES('orphan-record','opaque','note','orphan','alpha')`); err != nil {
				t.Fatal(err)
			}
		}},
		{"stale FTS", func(t *testing.T, path string, record memory.Record) {
			db := openExternalSQLite(t, path)
			defer db.Close()
			if _, err := db.Exec(`UPDATE memory_records_fts SET text_value='stale-index-value' WHERE record_id=?`, record.ID); err != nil {
				t.Fatal(err)
			}
		}},
		{"record domain", func(t *testing.T, path string, record memory.Record) {
			db := openExternalSQLite(t, path)
			defer db.Close()
			if _, err := db.Exec(`UPDATE memory_records SET source_json='{}' WHERE id=?`, record.ID); err != nil {
				t.Fatal(err)
			}
		}},
	}

	for index, test := range mutations {
		t.Run(test.name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "memory.db")
			store := openTestStore(t, path)
			record := sqliteTestRecord(fmt.Sprintf("integrity-%d", index), fmt.Sprintf("integrity-key-%d", index), time.Date(2026, 8, 29, 23, 0, index, 0, time.UTC))
			mustSQLiteCreate(t, store, record)
			if err := store.Close(); err != nil {
				t.Fatal(err)
			}
			test.mutate(t, path, record)

			var reopened *Store
			var err error
			if index%2 == 0 {
				reopened, err = Open(context.Background(), path, testOptions(t))
			} else {
				components, openErr := NewFactory(path, testOptions(t)).Open(context.Background())
				err = openErr
				if components.Store != nil {
					reopened = components.Store.(*Store)
				}
			}
			if reopened != nil {
				_ = reopened.Close()
			}
			if !errors.Is(err, memory.ErrCorrupt) {
				t.Fatalf("Open error = %v, want ErrCorrupt", err)
			}
		})
	}
}

func TestFactoryRejectsObservationCorrespondenceCorruption(t *testing.T) {
	mutations := []struct {
		name string
		sql  string
	}{
		{"receipt missing candidate", `UPDATE memory_observations SET candidate_ids_json='[]' WHERE id='integrity-observation'`},
		{"duplicate receipt candidate", `UPDATE memory_observations SET candidate_ids_json='["integrity-candidate","integrity-candidate"]' WHERE id='integrity-observation'`},
		{"candidate missing receipt reference", `UPDATE memory_candidates SET observation_id=NULL WHERE id='integrity-candidate'`},
	}
	for _, test := range mutations {
		t.Run(test.name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "memory.db")
			store := openTestStore(t, path)
			at := time.Date(2026, 8, 29, 23, 5, 0, 0, time.UTC)
			candidate := sqlitePendingCandidate("integrity-candidate", memory.Scope{Namespace: "user", ID: "integrity-observation-scope"}, at)
			candidate.Proposed.Source.ObservationID = "integrity-observation"
			if _, err := store.CommitObservation(context.Background(), memory.ObservationCommit{ObservationID: "integrity-observation", Candidates: []memory.Candidate{candidate}, CreatedAt: at}); err != nil {
				t.Fatal(err)
			}
			if err := store.Close(); err != nil {
				t.Fatal(err)
			}
			db := openExternalSQLite(t, path)
			if _, err := db.Exec(test.sql); err != nil {
				t.Fatal(err)
			}
			if err := db.Close(); err != nil {
				t.Fatal(err)
			}
			if reopened, err := Open(context.Background(), path, testOptions(t)); !errors.Is(err, memory.ErrCorrupt) {
				if reopened != nil {
					_ = reopened.Close()
				}
				t.Fatalf("Open error = %v, want ErrCorrupt", err)
			}
		})
	}
}

func TestFactoryReceiptIDsMayOutliveCandidates(t *testing.T) {
	path := filepath.Join(t.TempDir(), "memory.db")
	store := openTestStore(t, path)
	at := time.Date(2026, 8, 29, 23, 10, 0, 0, time.UTC)
	candidate := sqlitePendingCandidate("retained-receipt-candidate", memory.Scope{Namespace: "user", ID: "retained-receipt-scope"}, at)
	candidate.Proposed.Source.ObservationID = "retained-receipt"
	if _, err := store.CommitObservation(context.Background(), memory.ObservationCommit{ObservationID: "retained-receipt", Candidates: []memory.Candidate{candidate}, CreatedAt: at}); err != nil {
		t.Fatal(err)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	db := openExternalSQLite(t, path)
	if _, err := db.Exec(`DELETE FROM memory_candidates WHERE id=?`, candidate.ID); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	reopened, err := Open(context.Background(), path, testOptions(t))
	if err != nil {
		t.Fatalf("receipt surviving removed candidate: %v", err)
	}
	defer reopened.Close()
	receipt, err := reopened.GetObservationReceipt(context.Background(), "retained-receipt")
	if err != nil || !reflect.DeepEqual(receipt.CandidateIDs, []string{candidate.ID}) {
		t.Fatalf("receipt = %#v, %v", receipt, err)
	}
}

func TestFactoryOpenValidation(t *testing.T) {
	for name, factory := range map[string]Factory{
		"empty path":    NewFactory("", testOptions(t)),
		"empty options": NewFactory(filepath.Join(t.TempDir(), "memory.db"), Options{}),
	} {
		t.Run(name, func(t *testing.T) {
			components, err := factory.Open(context.Background())
			if !errors.Is(err, memory.ErrInvalidRequest) {
				t.Fatalf("error = %v", err)
			}
			if !reflect.DeepEqual(components, memory.Components{}) {
				t.Fatalf("components on failure = %#v", components)
			}
		})
	}
}
