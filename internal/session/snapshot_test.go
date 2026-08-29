package session

import (
	"context"
	"encoding/json"
	"reflect"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/model"
)

func TestMemoryAndStoreSnapshotTracksContextInputAcrossCompaction(t *testing.T) {
	factories := map[string]func(*testing.T) compactionTestSession{
		"memory": func(t *testing.T) compactionTestSession { return createConversationMemory(t) },
		"store":  func(t *testing.T) compactionTestSession { return createConversationStore(t) },
	}

	for name, factory := range factories {
		t.Run(name, func(t *testing.T) {
			current := factory(t)
			defer current.Close()

			assertSnapshot(t, current, snapshotExpectation{
				aggregateUsage:          model.Usage{InputTokens: 80, CachedInputTokens: 30, OutputTokens: 9},
				expectAggregateUsage:    true,
				aggregateUsagePresent:   true,
				expectAggregatePresence: true,
				contextInputTokens:      30,
				contextInputPresent:     true,
			})

			if err := current.Append(context.Background(), model.Message{
				Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "latest request"}},
				CreatedAt: time.Date(2026, 8, 28, 12, 0, 0, 0, time.UTC),
			}); err != nil {
				t.Fatal(err)
			}
			if err := current.Append(context.Background(), model.Message{
				Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "latest answer without prompt usage"}},
				CreatedAt: time.Date(2026, 8, 28, 12, 0, 1, 0, time.UTC), FinishReason: model.FinishStop,
				Usage: &model.Usage{OutputTokens: 7},
			}); err != nil {
				t.Fatal(err)
			}
			assertSnapshot(t, current, snapshotExpectation{
				aggregateUsage:          model.Usage{InputTokens: 80, CachedInputTokens: 30, OutputTokens: 16},
				expectAggregateUsage:    true,
				aggregateUsagePresent:   true,
				expectAggregatePresence: true,
			})

			if _, err := current.AppendCompaction(context.Background(), CompactionCheckpoint{
				Summary:          testCompactionSummary,
				FirstKeptEntryID: current.Messages()[2].ID,
				TokensBefore:     500,
				CreatedAt:        time.Date(2026, 8, 28, 12, 0, 2, 0, time.UTC),
			}); err != nil {
				t.Fatal(err)
			}
			assertSnapshot(t, current, snapshotExpectation{contextInputPending: true})

			if err := current.Append(context.Background(), model.Message{
				Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "post-checkpoint request"}},
				CreatedAt: time.Date(2026, 8, 28, 12, 0, 4, 0, time.UTC),
			}); err != nil {
				t.Fatal(err)
			}
			assertSnapshot(t, current, snapshotExpectation{contextInputPending: true})

			if err := current.Append(context.Background(), model.Message{
				Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "post-checkpoint answer"}},
				CreatedAt: time.Date(2026, 8, 28, 12, 0, 5, 0, time.UTC), FinishReason: model.FinishStop,
				Usage: &model.Usage{InputTokens: 14, CachedInputTokens: 5, OutputTokens: 3},
			}); err != nil {
				t.Fatal(err)
			}
			assertSnapshot(t, current, snapshotExpectation{
				contextInputTokens:  14,
				contextInputPresent: true,
			})

			if err := current.Append(context.Background(), model.Message{
				Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "later request"}},
				CreatedAt: time.Date(2026, 8, 28, 12, 0, 6, 0, time.UTC),
			}); err != nil {
				t.Fatal(err)
			}
			if err := current.Append(context.Background(), model.Message{
				Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "later answer without prompt usage"}},
				CreatedAt: time.Date(2026, 8, 28, 12, 0, 7, 0, time.UTC), FinishReason: model.FinishStop,
				Usage: &model.Usage{OutputTokens: 4},
			}); err != nil {
				t.Fatal(err)
			}
			assertSnapshot(t, current, snapshotExpectation{})

			messages := current.Messages()
			if _, err := current.AppendCompaction(context.Background(), CompactionCheckpoint{
				Summary:          testCompactionSummary + "\nrepeated",
				FirstKeptEntryID: messages[len(messages)-1].ID,
				TokensBefore:     600,
				CreatedAt:        time.Date(2026, 8, 28, 12, 0, 8, 0, time.UTC),
			}); err != nil {
				t.Fatal(err)
			}
			assertSnapshot(t, current, snapshotExpectation{contextInputPending: true})
		})
	}
}

func TestStoreSnapshotReopenAndExternalRetainedTailIgnoreHistoricalUsage(t *testing.T) {
	t.Run("reopen preserves latest omitted post-checkpoint usage", func(t *testing.T) {
		store := createConversationStore(t)
		if _, err := store.AppendCompaction(context.Background(), CompactionCheckpoint{
			Summary:          testCompactionSummary,
			FirstKeptEntryID: store.Messages()[2].ID,
			TokensBefore:     500,
			CreatedAt:        time.Date(2026, 8, 28, 12, 30, 0, 0, time.UTC),
		}); err != nil {
			t.Fatal(err)
		}
		if err := store.Append(context.Background(), model.Message{
			Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "post-checkpoint request"}},
			CreatedAt: time.Date(2026, 8, 28, 12, 30, 1, 0, time.UTC),
		}); err != nil {
			t.Fatal(err)
		}
		if err := store.Append(context.Background(), model.Message{
			Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "post-checkpoint answer without prompt usage"}},
			CreatedAt: time.Date(2026, 8, 28, 12, 30, 2, 0, time.UTC), FinishReason: model.FinishStop,
			Usage: &model.Usage{OutputTokens: 5},
		}); err != nil {
			t.Fatal(err)
		}
		path := store.Path()
		if err := store.Close(); err != nil {
			t.Fatal(err)
		}

		reopened, warnings, err := Open(path)
		if err != nil {
			t.Fatal(err)
		}
		defer reopened.Close()
		if len(warnings) != 0 {
			t.Fatalf("warnings = %#v", warnings)
		}
		assertSnapshot(t, reopened, snapshotExpectation{})
	})

	t.Run("retained tail stays pending until a real post-checkpoint assistant arrives", func(t *testing.T) {
		retainedTailUsage := &piUsage{Input: 9, Output: 2, CacheRead: 3, TotalTokens: 14}
		path := writeExternalRetainedTailFixture(t, []piMessage{{
			Role:       "assistant",
			Content:    json.RawMessage(`[{"type":"text","text":"retained assistant"}]`),
			API:        "openai-completions",
			Provider:   "openai-compatible",
			Model:      "test-model",
			Usage:      retainedTailUsage,
			StopReason: "stop",
			Timestamp:  time.Date(2026, 8, 28, 15, 0, 2, 0, time.UTC).UnixMilli(),
		}}, nil)
		store, warnings, err := Open(path)
		if err != nil {
			t.Fatal(err)
		}
		defer store.Close()
		if len(warnings) != 0 {
			t.Fatalf("warnings = %#v", warnings)
		}
		assertSnapshot(t, store, snapshotExpectation{contextInputPending: true})

		if err := store.Append(context.Background(), model.Message{
			Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "real post-checkpoint request"}},
			CreatedAt: time.Date(2026, 8, 28, 15, 0, 3, 0, time.UTC),
		}); err != nil {
			t.Fatal(err)
		}
		assertSnapshot(t, store, snapshotExpectation{contextInputPending: true})

		if err := store.Append(context.Background(), model.Message{
			Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "real post-checkpoint answer"}},
			CreatedAt: time.Date(2026, 8, 28, 15, 0, 4, 0, time.UTC), FinishReason: model.FinishStop,
			Usage: &model.Usage{InputTokens: 17, CachedInputTokens: 4, OutputTokens: 6},
		}); err != nil {
			t.Fatal(err)
		}
		assertSnapshot(t, store, snapshotExpectation{contextInputTokens: 17, contextInputPresent: true})
	})
}

type snapshotExpectation struct {
	aggregateUsage          model.Usage
	expectAggregateUsage    bool
	aggregateUsagePresent   bool
	expectAggregatePresence bool
	contextInputTokens      int
	contextInputPresent     bool
	contextInputPending     bool
}

func assertSnapshot(t *testing.T, current any, want snapshotExpectation) {
	t.Helper()
	method := reflect.ValueOf(current).MethodByName("Snapshot")
	if !method.IsValid() {
		t.Fatalf("%T does not expose Snapshot()", current)
	}
	results := method.Call(nil)
	if len(results) != 1 {
		t.Fatalf("Snapshot() returned %d values, want 1", len(results))
	}
	snapshot := results[0]
	if snapshot.Kind() == reflect.Pointer {
		snapshot = snapshot.Elem()
	}
	if !snapshot.IsValid() {
		t.Fatal("Snapshot() returned an invalid value")
	}
	if want.expectAggregateUsage {
		if got := snapshot.FieldByName("AggregateUsage"); !got.IsValid() || got.Interface() != want.aggregateUsage {
			t.Fatalf("Snapshot().AggregateUsage = %#v, want %#v", got.Interface(), want.aggregateUsage)
		}
	}
	if want.expectAggregatePresence {
		if got := snapshot.FieldByName("AggregateUsagePresent"); !got.IsValid() || got.Bool() != want.aggregateUsagePresent {
			t.Fatalf("Snapshot().AggregateUsagePresent = %v, want %v", got.Bool(), want.aggregateUsagePresent)
		}
	}
	if got := snapshot.FieldByName("ContextInputTokens"); !got.IsValid() || int(got.Int()) != want.contextInputTokens {
		t.Fatalf("Snapshot().ContextInputTokens = %v, want %d", got.Int(), want.contextInputTokens)
	}
	if got := snapshot.FieldByName("ContextInputTokensPresent"); !got.IsValid() || got.Bool() != want.contextInputPresent {
		t.Fatalf("Snapshot().ContextInputTokensPresent = %v, want %v", got.Bool(), want.contextInputPresent)
	}
	if got := snapshot.FieldByName("ContextInputTokensPending"); !got.IsValid() || got.Bool() != want.contextInputPending {
		t.Fatalf("Snapshot().ContextInputTokensPending = %v, want %v", got.Bool(), want.contextInputPending)
	}
}
