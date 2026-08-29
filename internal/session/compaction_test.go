package session

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"os"
	"path/filepath"
	"reflect"
	"sort"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/model"
)

const testCompactionSummary = "## Goal\nkeep working\n## Constraints & Preferences\n- safe\n## Progress\n### Done\n- setup\n### In Progress\n- implementation\n### Blocked\n- none\n## Key Decisions\n- append only\n## Next Steps\n- test\n## Critical Context\n- exact"

type compactionTestSession interface {
	Header() Header
	Messages() []model.Message
	Append(context.Context, model.Message) error
	AppendCompaction(context.Context, CompactionCheckpoint) (CompactionMetadata, error)
	LatestCompaction() (CompactionMetadata, bool)
	AggregateUsage() (model.Usage, bool)
	Path() string
	Close() error
}

func TestStoreAppendCompactionWritesPiLegacyCheckpointAndReopens(t *testing.T) {
	store := createConversationStore(t)
	defer store.Close()
	keptID := store.Messages()[2].ID
	beforeLines := readJSONLines(t, store.Path())
	usage := &model.Usage{InputTokens: 100, CachedInputTokens: 40, OutputTokens: 20}
	details := CompactionDetails{ReadFiles: []string{"README.md"}, ModifiedFiles: []string{"internal/session/store.go"}, OmittedReadFiles: 2}
	createdAt := time.Date(2026, 8, 28, 12, 0, 0, 0, time.UTC)

	metadata, err := store.AppendCompaction(context.Background(), CompactionCheckpoint{
		Summary: testCompactionSummary, FirstKeptEntryID: keptID, TokensBefore: 258000,
		Usage: usage, Details: details, CreatedAt: createdAt,
	})
	if err != nil {
		t.Fatal(err)
	}
	if !piEntryIDPattern.MatchString(metadata.ID) {
		t.Fatalf("id=%q", metadata.ID)
	}
	wantMetadata := CompactionMetadata{
		ID: metadata.ID, Summary: testCompactionSummary, FirstKeptEntryID: keptID, TokensBefore: 258000,
		Usage: usage, Details: details,
	}
	if !reflect.DeepEqual(metadata, wantMetadata) {
		t.Fatalf("metadata = %#v, want %#v", metadata, wantMetadata)
	}
	assertCompactedConversation(t, store.Messages(), metadata.ID, keptID, createdAt, 258000)
	if got, present := store.AggregateUsage(); !present || got != (model.Usage{InputTokens: 180, CachedInputTokens: 70, OutputTokens: 29}) {
		t.Fatalf("AggregateUsage() = %#v, %v", got, present)
	}

	lines := readJSONLines(t, store.Path())
	if len(lines) != len(beforeLines)+1 {
		t.Fatalf("line count = %d, want %d", len(lines), len(beforeLines)+1)
	}
	var record map[string]json.RawMessage
	if err := json.Unmarshal(lines[len(lines)-1], &record); err != nil {
		t.Fatal(err)
	}
	for _, absent := range []string{"retainedTail", "fromHook"} {
		if _, exists := record[absent]; exists {
			t.Fatalf("legacy checkpoint unexpectedly contains %s: %s", absent, lines[len(lines)-1])
		}
	}
	if got, want := len(record), 9; got != want {
		t.Fatalf("checkpoint field count = %d, want %d: %s", got, want, lines[len(lines)-1])
	}
	var wire struct {
		Type             string            `json:"type"`
		ID               string            `json:"id"`
		ParentID         string            `json:"parentId"`
		Timestamp        string            `json:"timestamp"`
		Summary          string            `json:"summary"`
		FirstKeptEntryID string            `json:"firstKeptEntryId"`
		TokensBefore     int               `json:"tokensBefore"`
		Usage            piUsage           `json:"usage"`
		Details          CompactionDetails `json:"details"`
	}
	if err := json.Unmarshal(lines[len(lines)-1], &wire); err != nil {
		t.Fatal(err)
	}
	if wire.Type != "compaction" || wire.ID != metadata.ID || wire.FirstKeptEntryID != keptID ||
		wire.TokensBefore != 258000 || wire.Timestamp != createdAt.Format(time.RFC3339Nano) || wire.Summary != testCompactionSummary {
		t.Fatalf("checkpoint = %#v", wire)
	}
	if wire.ParentID == "" || wire.Usage.Input != 60 || wire.Usage.CacheRead != 40 || wire.Usage.Output != 20 ||
		!reflect.DeepEqual(wire.Details, details) {
		t.Fatalf("checkpoint usage/details = %#v / %#v", wire.Usage, wire.Details)
	}

	assertLatestCompaction(t, store, wantMetadata)
	mutateCompactionMetadata(metadata)
	assertLatestCompaction(t, store, wantMetadata)

	path := store.Path()
	wantMessages := store.Messages()
	wantUsage, wantUsagePresent := store.AggregateUsage()
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
	if !reflect.DeepEqual(reopened.Messages(), wantMessages) {
		t.Fatalf("reopened messages = %#v, want %#v", reopened.Messages(), wantMessages)
	}
	if got, present := reopened.AggregateUsage(); present != wantUsagePresent || got != wantUsage {
		t.Fatalf("reopened AggregateUsage() = %#v, %v; want %#v, %v", got, present, wantUsage, wantUsagePresent)
	}
	assertLatestCompaction(t, reopened, wantMetadata)
}

func TestMemoryAppendCompactionReplacesContextAccountsUsageAndTracksFirstPostMessage(t *testing.T) {
	memory := createConversationMemory(t)
	defer memory.Close()
	usage := &model.Usage{InputTokens: 12, CachedInputTokens: 5, OutputTokens: 3}
	details := CompactionDetails{ReadFiles: []string{"a.go"}, ModifiedFiles: []string{"b.go"}}
	createdAt := time.Date(2026, 8, 28, 12, 30, 0, 0, time.UTC)

	metadata, err := memory.AppendCompaction(context.Background(), CompactionCheckpoint{
		Summary: testCompactionSummary, FirstKeptEntryID: "memory-user-2", TokensBefore: 900,
		Usage: usage, Details: details, CreatedAt: createdAt,
	})
	if err != nil {
		t.Fatal(err)
	}
	if !piEntryIDPattern.MatchString(metadata.ID) || metadata.RetainedTailOnly || metadata.FirstPostCheckpointMessageID != "" {
		t.Fatalf("metadata = %#v", metadata)
	}
	assertCompactedConversation(t, memory.Messages(), metadata.ID, "memory-user-2", createdAt, 900)
	if got, present := memory.AggregateUsage(); !present || got != (model.Usage{InputTokens: 92, CachedInputTokens: 35, OutputTokens: 12}) {
		t.Fatalf("AggregateUsage() = %#v, %v", got, present)
	}
	want := cloneCompactionMetadata(metadata)
	mutateCompactionMetadata(metadata)
	assertLatestCompaction(t, memory, want)

	post := model.Message{ID: "memory-post", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "post checkpoint"}}, CreatedAt: createdAt.Add(time.Second)}
	if err := memory.Append(context.Background(), post); err != nil {
		t.Fatal(err)
	}
	want.FirstPostCheckpointMessageID = post.ID
	assertLatestCompaction(t, memory, want)

	second, err := memory.AppendCompaction(context.Background(), CompactionCheckpoint{
		Summary: testCompactionSummary + "\nsecond", FirstKeptEntryID: post.ID, TokensBefore: 1000,
		Usage: &model.Usage{InputTokens: 7, CachedInputTokens: 2, OutputTokens: 1}, CreatedAt: createdAt.Add(2 * time.Second),
	})
	if err != nil {
		t.Fatal(err)
	}
	if second.FirstPostCheckpointMessageID != "" || second.FirstKeptEntryID != post.ID {
		t.Fatalf("second metadata = %#v", second)
	}
	messages := memory.Messages()
	if len(messages) != 2 || messages[0].ID != second.ID || messages[1].ID != post.ID || strings.Contains(messages[0].Text(), testCompactionSummary+"\n[Compaction") {
		t.Fatalf("messages after repeated compaction = %#v", messages)
	}
	if got, present := memory.AggregateUsage(); !present || got != (model.Usage{InputTokens: 99, CachedInputTokens: 37, OutputTokens: 13}) {
		t.Fatalf("AggregateUsage() after repeated compaction = %#v, %v", got, present)
	}
}

func TestMemoryAndStoreCompactionPreserveExplicitZeroUsage(t *testing.T) {
	for name, current := range map[string]compactionTestSession{
		"memory": createConversationMemory(t),
		"store":  createConversationStore(t),
	} {
		t.Run(name, func(t *testing.T) {
			defer current.Close()
			checkpoint := validCheckpointFor(current)
			checkpoint.Usage = &model.Usage{}
			metadata, err := current.AppendCompaction(context.Background(), checkpoint)
			if err != nil {
				t.Fatal(err)
			}
			messages := current.Messages()
			if metadata.Usage == nil || *metadata.Usage != (model.Usage{}) || messages[0].Usage == nil || *messages[0].Usage != (model.Usage{}) {
				t.Fatalf("explicit zero usage was lost: metadata=%#v message=%#v", metadata.Usage, messages[0].Usage)
			}
		})
	}
}

func TestAppendCompactionEnforcesSummaryAndDetailsBoundsBeforeMutation(t *testing.T) {
	invalidUTF8 := string([]byte{0xff})
	tooManyPaths := make([]string, 1_025)
	for index := range tooManyPaths {
		tooManyPaths[index] = fmt.Sprintf("path-%04d", index)
	}

	tests := []struct {
		name   string
		mutate func(*CompactionCheckpoint)
		want   error
	}{
		{name: "summary above byte bound", mutate: func(checkpoint *CompactionCheckpoint) {
			checkpoint.Summary = strings.Repeat("s", 128*1024+1)
		}, want: ErrSessionEntryTooLarge},
		{name: "invalid UTF-8 summary", mutate: func(checkpoint *CompactionCheckpoint) {
			checkpoint.Summary = invalidUTF8
		}, want: ErrInvalidSession},
		{name: "negative omitted reads", mutate: func(checkpoint *CompactionCheckpoint) {
			checkpoint.Details.OmittedReadFiles = -1
		}, want: ErrInvalidSession},
		{name: "negative omitted modifications", mutate: func(checkpoint *CompactionCheckpoint) {
			checkpoint.Details.OmittedModifiedFiles = -1
		}, want: ErrInvalidSession},
		{name: "path count above bound", mutate: func(checkpoint *CompactionCheckpoint) {
			checkpoint.Details.ReadFiles = tooManyPaths
		}, want: ErrSessionEntryTooLarge},
		{name: "path text above byte bound", mutate: func(checkpoint *CompactionCheckpoint) {
			checkpoint.Details.ModifiedFiles = []string{strings.Repeat("m", 64*1024+1)}
		}, want: ErrSessionEntryTooLarge},
		{name: "empty path", mutate: func(checkpoint *CompactionCheckpoint) {
			checkpoint.Details.ReadFiles = []string{""}
		}, want: ErrInvalidSession},
		{name: "invalid UTF-8 path", mutate: func(checkpoint *CompactionCheckpoint) {
			checkpoint.Details.ReadFiles = []string{invalidUTF8}
		}, want: ErrInvalidSession},
		{name: "C0 control path", mutate: func(checkpoint *CompactionCheckpoint) {
			checkpoint.Details.ReadFiles = []string{"bad\npath"}
		}, want: ErrInvalidSession},
		{name: "DEL control path", mutate: func(checkpoint *CompactionCheckpoint) {
			checkpoint.Details.ReadFiles = []string{"bad\x7fpath"}
		}, want: ErrInvalidSession},
		{name: "C1 control path", mutate: func(checkpoint *CompactionCheckpoint) {
			checkpoint.Details.ReadFiles = []string{"bad\u0085path"}
		}, want: ErrInvalidSession},
		{name: "unclean path", mutate: func(checkpoint *CompactionCheckpoint) {
			checkpoint.Details.ReadFiles = []string{"dir/../file.go"}
		}, want: ErrInvalidSession},
		{name: "duplicate read path", mutate: func(checkpoint *CompactionCheckpoint) {
			checkpoint.Details.ReadFiles = []string{"same.go", "same.go"}
		}, want: ErrInvalidSession},
		{name: "duplicate modified path", mutate: func(checkpoint *CompactionCheckpoint) {
			checkpoint.Details.ModifiedFiles = []string{"same.go", "same.go"}
		}, want: ErrInvalidSession},
		{name: "read modified overlap", mutate: func(checkpoint *CompactionCheckpoint) {
			checkpoint.Details.ReadFiles = []string{"same.go"}
			checkpoint.Details.ModifiedFiles = []string{"same.go"}
		}, want: ErrInvalidSession},
	}

	factories := map[string]func(*testing.T) compactionTestSession{
		"memory": func(t *testing.T) compactionTestSession { return createConversationMemory(t) },
		"lazy store": func(t *testing.T) compactionTestSession {
			store, err := CreateLazy(t.TempDir(), testHeader(t))
			if err != nil {
				t.Fatal(err)
			}
			return store
		},
	}
	for factoryName, factory := range factories {
		for _, test := range tests {
			t.Run(factoryName+"/"+test.name, func(t *testing.T) {
				current := factory(t)
				defer current.Close()
				beforeMessages := current.Messages()
				beforeUsage, beforePresent := current.AggregateUsage()
				checkpoint := CompactionCheckpoint{
					Summary: testCompactionSummary, FirstKeptEntryID: "ffffffff", TokensBefore: 1,
					CreatedAt: time.Date(2026, 8, 28, 12, 0, 0, 0, time.UTC),
				}
				if len(beforeMessages) >= 3 {
					checkpoint.FirstKeptEntryID = beforeMessages[2].ID
				}
				test.mutate(&checkpoint)

				if _, err := current.AppendCompaction(context.Background(), checkpoint); !errors.Is(err, test.want) {
					t.Fatalf("AppendCompaction() error = %v, want %v", err, test.want)
				}
				if !reflect.DeepEqual(current.Messages(), beforeMessages) {
					t.Fatalf("rejected checkpoint changed messages: %#v", current.Messages())
				}
				if got, present := current.AggregateUsage(); present != beforePresent || got != beforeUsage {
					t.Fatalf("rejected checkpoint changed usage: %#v, %v", got, present)
				}
				if _, ok := current.LatestCompaction(); ok {
					t.Fatal("rejected checkpoint became latest")
				}
				if store, ok := current.(*Store); ok {
					if store.Path() != "" {
						t.Fatalf("rejected lazy checkpoint created %q", store.Path())
					}
					paths, err := filepath.Glob(filepath.Join(store.root, "*", "*.jsonl"))
					if err != nil {
						t.Fatal(err)
					}
					if len(paths) != 0 {
						t.Fatalf("rejected lazy checkpoint created files: %v", paths)
					}
				}
			})
		}
	}
}

func TestAppendCompactionAcceptsExactSummaryAndDetailsBounds(t *testing.T) {
	details := exactBoundedCompactionDetails()
	if got := compactionPathTextBytes(details); got != 64*1024 {
		t.Fatalf("test details bytes = %d", got)
	}
	if got := len(details.ReadFiles) + len(details.ModifiedFiles); got != 1_024 {
		t.Fatalf("test details paths = %d", got)
	}

	factories := map[string]func(*testing.T) compactionTestSession{
		"memory": func(t *testing.T) compactionTestSession { return createConversationMemory(t) },
		"store":  func(t *testing.T) compactionTestSession { return createConversationStore(t) },
	}
	for name, factory := range factories {
		t.Run(name, func(t *testing.T) {
			current := factory(t)
			defer current.Close()
			checkpoint := validCheckpointFor(current)
			checkpoint.Summary = strings.Repeat("s", 128*1024)
			checkpoint.Details = details
			metadata, err := current.AppendCompaction(context.Background(), checkpoint)
			if err != nil {
				t.Fatal(err)
			}
			if len(metadata.Summary) != 128*1024 || !reflect.DeepEqual(metadata.Details, details) {
				t.Fatalf("metadata exceeded/lost exact bounds: summary=%d details=%#v", len(metadata.Summary), metadata.Details)
			}
		})
	}
}

func TestOpenCompactionDetailsRejectsDuplicateKeysAndMalformedKnownFieldsLazily(t *testing.T) {
	tests := map[string]string{
		"duplicate JSON key": `{"readFiles":["first.go"],"readFiles":["second.go"]}`,
		"non-object":         `[]`,
		"read files shape":   `{"readFiles":"README.md"}`,
		"read path shape":    `{"readFiles":["README.md",1]}`,
		"modified shape":     `{"modifiedFiles":{}}`,
		"omitted shape":      `{"omittedReadFiles":"1"}`,
		"omitted range":      `{"omittedModifiedFiles":9223372036854775808}`,
	}
	for name, rawDetails := range tests {
		t.Run(name, func(t *testing.T) {
			path := externalCompactionDetailsFixture(t, rawDetails)
			store, warnings, err := Open(path)
			if err != nil {
				t.Fatalf("Open() rejected optional external details: %v", err)
			}
			defer store.Close()
			if len(warnings) != 0 {
				t.Fatalf("warnings = %#v", warnings)
			}
			metadata, ok := store.LatestCompaction()
			if !ok || !reflect.DeepEqual(metadata.Details, CompactionDetails{}) {
				t.Fatalf("LatestCompaction() = %#v, %v; want empty details", metadata, ok)
			}
		})
	}
}

func TestOpenCompactionDetailsSanitizesPathsAndClonesMetadata(t *testing.T) {
	rawDetails := `{
		"readFiles":["z.go","bad\u0000.go","dup.go","dup.go","both.go","dir/../unclean.go","a.go","bad\u007f.go","bad\u0085.go"],
		"modifiedFiles":["m.go","both.go","m.go","./unclean.go"],
		"omittedReadFiles":2,
		"omittedModifiedFiles":3,
		"unknown":{"nested":true}
	}`
	path := externalCompactionDetailsFixture(t, rawDetails)
	store, warnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	if len(warnings) != 0 {
		t.Fatalf("warnings = %#v", warnings)
	}
	want := CompactionDetails{
		ReadFiles: []string{"a.go", "dup.go", "z.go"}, ModifiedFiles: []string{"both.go", "m.go"},
		OmittedReadFiles: 2, OmittedModifiedFiles: 3,
	}
	metadata, ok := store.LatestCompaction()
	if !ok || !reflect.DeepEqual(metadata.Details, want) {
		t.Fatalf("LatestCompaction() = %#v, %v; want details %#v", metadata, ok, want)
	}
	metadata.Details.ReadFiles[0] = "mutated"
	again, ok := store.LatestCompaction()
	if !ok || !reflect.DeepEqual(again.Details, want) {
		t.Fatalf("LatestCompaction() aliases external details: %#v, %v", again, ok)
	}
}

func TestOpenCompactionDetailsBoundsOversizedExternalMetadata(t *testing.T) {
	details := CompactionDetails{OmittedReadFiles: math.MaxInt - 10, OmittedModifiedFiles: math.MaxInt - 50}
	for index := 0; index < 1_100; index++ {
		details.ModifiedFiles = append(details.ModifiedFiles, fixedWidthCompactionPath("m", index))
	}
	for index := 0; index < 20; index++ {
		details.ReadFiles = append(details.ReadFiles, fixedWidthCompactionPath("r", index))
	}
	rawDetails, err := json.Marshal(details)
	if err != nil {
		t.Fatal(err)
	}
	path := externalCompactionDetailsFixture(t, string(rawDetails))
	store, warnings, err := Open(path)
	if err != nil {
		t.Fatalf("Open() rejected oversized optional details: %v", err)
	}
	defer store.Close()
	if len(warnings) != 0 {
		t.Fatalf("warnings = %#v", warnings)
	}
	metadata, ok := store.LatestCompaction()
	if !ok {
		t.Fatal("LatestCompaction() missing")
	}
	got := metadata.Details
	if len(got.ModifiedFiles) != 1_024 || len(got.ReadFiles) != 0 || compactionPathTextBytes(got) != 64*1024 {
		t.Fatalf("unbounded external details: modified=%d read=%d bytes=%d", len(got.ModifiedFiles), len(got.ReadFiles), compactionPathTextBytes(got))
	}
	if got.OmittedModifiedFiles != math.MaxInt || got.OmittedReadFiles != math.MaxInt {
		t.Fatalf("omitted counts did not saturate: %#v", got)
	}
	if !sort.StringsAreSorted(got.ModifiedFiles) {
		t.Fatalf("modified paths are not sorted: %#v", got.ModifiedFiles[:3])
	}
}

func TestMemoryCompactionRejectsInvalidFirstPostCheckpointMessageWithoutAppending(t *testing.T) {
	tests := []struct {
		name    string
		message model.Message
	}{
		{name: "context role", message: model.Message{ID: "context-post", Role: model.RoleContext, Blocks: []model.Block{{Type: model.BlockText, Text: "context"}}}},
		{name: "malformed normal message", message: model.Message{ID: "malformed-post", Role: model.RoleUser}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			memory := createConversationMemory(t)
			defer memory.Close()
			metadata, err := memory.AppendCompaction(context.Background(), validCheckpointFor(memory))
			if err != nil {
				t.Fatal(err)
			}
			before := memory.Messages()
			test.message.CreatedAt = time.Date(2026, 8, 28, 12, 1, 0, 0, time.UTC)
			if err := memory.Append(context.Background(), test.message); !errors.Is(err, ErrInvalidSession) {
				t.Fatalf("Append() error = %v, want ErrInvalidSession", err)
			}
			if !reflect.DeepEqual(memory.Messages(), before) {
				t.Fatalf("invalid first-post message was appended: %#v", memory.Messages())
			}
			assertLatestCompaction(t, memory, metadata)

			valid := model.Message{
				ID: "real-first-post", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "valid post"}},
				CreatedAt: test.message.CreatedAt.Add(time.Second),
			}
			if err := memory.Append(context.Background(), valid); err != nil {
				t.Fatal(err)
			}
			metadata.FirstPostCheckpointMessageID = valid.ID
			assertLatestCompaction(t, memory, metadata)
			messages := memory.Messages()
			if len(messages) != len(before)+1 || messages[len(messages)-1].ID != valid.ID {
				t.Fatalf("valid first-post provenance = %#v", messages)
			}
		})
	}
}

func TestMemoryCompactionAssignsIDToValidFirstPostCheckpointMessage(t *testing.T) {
	memory := createConversationMemory(t)
	defer memory.Close()
	metadata, err := memory.AppendCompaction(context.Background(), validCheckpointFor(memory))
	if err != nil {
		t.Fatal(err)
	}
	message := model.Message{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "generated id"}}, CreatedAt: time.Date(2026, 8, 28, 12, 1, 0, 0, time.UTC)}
	if err := memory.Append(context.Background(), message); err != nil {
		t.Fatal(err)
	}
	messages := memory.Messages()
	generatedID := messages[len(messages)-1].ID
	if !piEntryIDPattern.MatchString(generatedID) {
		t.Fatalf("first post-checkpoint ID = %q, want generated stable ID", generatedID)
	}
	metadata.FirstPostCheckpointMessageID = generatedID
	assertLatestCompaction(t, memory, metadata)
}

func TestAppendCompactionRejectsInvalidCheckpointWithoutMutation(t *testing.T) {
	factories := map[string]func(*testing.T) compactionTestSession{
		"memory": func(t *testing.T) compactionTestSession { return createConversationMemory(t) },
		"store":  func(t *testing.T) compactionTestSession { return createConversationStore(t) },
	}
	for factoryName, factory := range factories {
		for caseName, mutate := range map[string]func(*CompactionCheckpoint){
			"blank summary":      func(checkpoint *CompactionCheckpoint) { checkpoint.Summary = " \n\t" },
			"missing first kept": func(checkpoint *CompactionCheckpoint) { checkpoint.FirstKeptEntryID = "" },
			"unknown first kept": func(checkpoint *CompactionCheckpoint) { checkpoint.FirstKeptEntryID = "ffffffff" },
			"negative tokens":    func(checkpoint *CompactionCheckpoint) { checkpoint.TokensBefore = -1 },
			"zero timestamp":     func(checkpoint *CompactionCheckpoint) { checkpoint.CreatedAt = time.Time{} },
			"invalid usage": func(checkpoint *CompactionCheckpoint) {
				checkpoint.Usage = &model.Usage{InputTokens: 1, CachedInputTokens: 2}
			},
		} {
			t.Run(factoryName+"/"+caseName, func(t *testing.T) {
				current := factory(t)
				defer current.Close()
				beforeMessages := current.Messages()
				beforeUsage, beforePresent := current.AggregateUsage()
				beforeFile := readOptionalFile(t, current.Path())
				checkpoint := validCheckpointFor(current)
				mutate(&checkpoint)

				metadata, err := current.AppendCompaction(context.Background(), checkpoint)
				if !errors.Is(err, ErrInvalidSession) {
					t.Fatalf("AppendCompaction() = %#v, %v; want ErrInvalidSession", metadata, err)
				}
				assertUnchangedCompactionState(t, current, beforeMessages, beforeUsage, beforePresent, beforeFile)
			})
		}
	}
}

func TestAppendCompactionRejectsProtocolUnsafeFirstKeptMessage(t *testing.T) {
	factories := map[string]func(*testing.T) compactionTestSession{
		"memory": func(t *testing.T) compactionTestSession {
			memory := NewMemory(testHeader(t))
			appendToolConversation(t, memory)
			return memory
		},
		"store": func(t *testing.T) compactionTestSession {
			store, err := Create(t.TempDir(), testHeader(t))
			if err != nil {
				t.Fatal(err)
			}
			appendToolConversation(t, store)
			return store
		},
	}
	for name, factory := range factories {
		t.Run(name, func(t *testing.T) {
			current := factory(t)
			defer current.Close()
			before := current.Messages()
			checkpoint := CompactionCheckpoint{
				Summary: testCompactionSummary, FirstKeptEntryID: before[1].ID, TokensBefore: 100,
				CreatedAt: time.Date(2026, 8, 28, 13, 0, 0, 0, time.UTC),
			}
			if _, err := current.AppendCompaction(context.Background(), checkpoint); !errors.Is(err, ErrInvalidSession) {
				t.Fatalf("AppendCompaction() error = %v, want ErrInvalidSession", err)
			}
			if !reflect.DeepEqual(current.Messages(), before) {
				t.Fatalf("messages changed: %#v", current.Messages())
			}
		})
	}
}

func TestStoreAppendCompactionValidatesCandidateBeforeEncodingOrWrite(t *testing.T) {
	store := createConversationStore(t)
	defer store.Close()
	before := store.Messages()
	beforeFile := readFile(t, store.Path())
	writer := &countingFileWriter{file: store.file}
	store.writer = writer
	// Simulate corrupt candidate source state after open. AppendCompaction must
	// rebuild and validate the complete candidate before it reaches the writer.
	store.entries[len(store.entries)-1].ParentID = stringPointer("ffffffff")

	if _, err := store.AppendCompaction(context.Background(), validCheckpointFor(store)); !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("AppendCompaction() error = %v, want ErrInvalidSession", err)
	}
	if writer.writes != 0 || !bytes.Equal(readFile(t, store.Path()), beforeFile) || !reflect.DeepEqual(store.Messages(), before) {
		t.Fatalf("candidate validation reached persistence: writes=%d messages=%#v", writer.writes, store.Messages())
	}
}

func TestStoreAppendCompactionPreservesLazyCreationOnRejection(t *testing.T) {
	root := t.TempDir()
	store, err := CreateLazy(root, testHeader(t))
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	checkpoint := CompactionCheckpoint{Summary: testCompactionSummary, FirstKeptEntryID: "ffffffff", TokensBefore: 1, CreatedAt: time.Now().UTC()}
	if _, err := store.AppendCompaction(context.Background(), checkpoint); !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("AppendCompaction() error = %v, want ErrInvalidSession", err)
	}
	if store.Path() != "" {
		t.Fatalf("Path() = %q, want lazy store without a file", store.Path())
	}
	paths, err := filepath.Glob(filepath.Join(root, "*", "*.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	if len(paths) != 0 {
		t.Fatalf("lazy rejection created files: %v", paths)
	}
}

func TestStoreAppendCompactionEnforcesRecordAndFileLimitsBeforeWrite(t *testing.T) {
	t.Run("record", func(t *testing.T) {
		store := createConversationStore(t)
		defer store.Close()
		before := store.Messages()
		beforeFile := readFile(t, store.Path())
		checkpoint := validCheckpointFor(store)
		checkpoint.Summary = strings.Repeat("x", maxSessionEntryBytes)
		if _, err := store.AppendCompaction(context.Background(), checkpoint); !errors.Is(err, ErrSessionEntryTooLarge) {
			t.Fatalf("AppendCompaction() error = %v, want ErrSessionEntryTooLarge", err)
		}
		assertUnchangedCompactionState(t, store, before, model.Usage{InputTokens: 80, CachedInputTokens: 30, OutputTokens: 9}, true, beforeFile)
	})

	t.Run("file", func(t *testing.T) {
		store := createConversationStore(t)
		defer store.Close()
		before := store.Messages()
		beforeUsage, beforePresent := store.AggregateUsage()
		beforeFile := readFile(t, store.Path())
		writer := &countingFileWriter{file: store.file}
		store.writer = writer
		store.fileBytes = maxSessionFileBytes
		if _, err := store.AppendCompaction(context.Background(), validCheckpointFor(store)); !errors.Is(err, ErrSessionFileTooLarge) {
			t.Fatalf("AppendCompaction() error = %v, want ErrSessionFileTooLarge", err)
		}
		if writer.writes != 0 {
			t.Fatalf("writes = %d, want 0", writer.writes)
		}
		assertUnchangedCompactionState(t, store, before, beforeUsage, beforePresent, beforeFile)
	})
}

func TestStoreAppendCompactionDurableFailuresPoisonWithoutAdvancingContext(t *testing.T) {
	for _, test := range []struct {
		name      string
		writeErr  error
		short     bool
		syncErr   error
		wantCause error
	}{
		{name: "write", writeErr: errors.New("checkpoint write failed")},
		{name: "short write", short: true, wantCause: io.ErrShortWrite},
		{name: "sync", syncErr: errors.New("checkpoint sync failed")},
	} {
		t.Run(test.name, func(t *testing.T) {
			store := createConversationStore(t)
			defer store.Close()
			beforeMessages := store.Messages()
			beforeUsage, beforePresent := store.AggregateUsage()
			beforeEntries, beforeLeaf, beforeBytes := len(store.entries), *store.leafID, store.fileBytes
			writer := &compactionFaultWriter{file: store.file, writeErr: test.writeErr, short: test.short, syncErr: test.syncErr}
			store.writer = writer
			cause := test.wantCause
			if cause == nil {
				cause = test.writeErr
				if cause == nil {
					cause = test.syncErr
				}
			}

			firstErr := error(nil)
			if _, err := store.AppendCompaction(context.Background(), validCheckpointFor(store)); err != nil {
				firstErr = err
			}
			if !errors.Is(firstErr, ErrFatalPersistence) || !errors.Is(firstErr, cause) {
				t.Fatalf("AppendCompaction() error = %v", firstErr)
			}
			if !reflect.DeepEqual(store.Messages(), beforeMessages) || len(store.entries) != beforeEntries || *store.leafID != beforeLeaf || store.fileBytes != beforeBytes {
				t.Fatalf("failed checkpoint advanced state: messages=%#v entries=%d leaf=%v bytes=%d", store.Messages(), len(store.entries), store.leafID, store.fileBytes)
			}
			if got, present := store.AggregateUsage(); present != beforePresent || got != beforeUsage {
				t.Fatalf("failed checkpoint usage = %#v, %v", got, present)
			}
			if _, ok := store.LatestCompaction(); ok {
				t.Fatal("failed first checkpoint became latest")
			}
			writes, syncs := writer.writes, writer.syncs
			_, secondErr := store.AppendCompaction(context.Background(), validCheckpointFor(store))
			if secondErr != firstErr {
				t.Fatalf("second error = %p, want original fatal identity %p", secondErr, firstErr)
			}
			appendErr := store.Append(context.Background(), model.Message{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "no write"}}, CreatedAt: time.Now().UTC()})
			if appendErr != firstErr || writer.writes != writes || writer.syncs != syncs {
				t.Fatalf("poisoned append = %p writes=%d syncs=%d; want %p, %d, %d", appendErr, writer.writes, writer.syncs, firstErr, writes, syncs)
			}
		})
	}
}

func TestAppendCompactionCancellationBeforeAndAfterWrite(t *testing.T) {
	t.Run("before store write", func(t *testing.T) {
		store := createConversationStore(t)
		defer store.Close()
		writer := &countingFileWriter{file: store.file}
		store.writer = writer
		before := store.Messages()
		ctx, cancel := context.WithCancel(context.Background())
		cancel()
		if _, err := store.AppendCompaction(ctx, validCheckpointFor(store)); !errors.Is(err, context.Canceled) {
			t.Fatalf("AppendCompaction() error = %v, want context.Canceled", err)
		}
		if writer.writes != 0 || !reflect.DeepEqual(store.Messages(), before) {
			t.Fatalf("canceled checkpoint wrote or advanced state")
		}
	})

	t.Run("after write begins", func(t *testing.T) {
		store := createConversationStore(t)
		defer store.Close()
		ctx, cancel := context.WithCancel(context.Background())
		writer := &compactionFaultWriter{file: store.file, cancel: cancel}
		store.writer = writer
		metadata, err := store.AppendCompaction(ctx, validCheckpointFor(store))
		if err != nil {
			t.Fatalf("AppendCompaction() after write-side cancellation = %v", err)
		}
		if ctx.Err() != context.Canceled || writer.writes != 1 || writer.syncs != 1 {
			t.Fatalf("ctx=%v writes=%d syncs=%d", ctx.Err(), writer.writes, writer.syncs)
		}
		assertLatestCompaction(t, store, metadata)
	})

	t.Run("memory", func(t *testing.T) {
		memory := createConversationMemory(t)
		defer memory.Close()
		before := memory.Messages()
		ctx, cancel := context.WithCancel(context.Background())
		cancel()
		if _, err := memory.AppendCompaction(ctx, validCheckpointFor(memory)); !errors.Is(err, context.Canceled) {
			t.Fatalf("AppendCompaction() error = %v, want context.Canceled", err)
		}
		if !reflect.DeepEqual(memory.Messages(), before) {
			t.Fatal("canceled memory checkpoint changed messages")
		}
	})
}

func TestStoreRepeatedCheckpointCrossingPriorRecordMatchesMemoryAndReopens(t *testing.T) {
	createdAt := time.Date(2026, 8, 28, 13, 30, 0, 0, time.UTC)
	memory := createConversationMemory(t)
	store := createConversationStore(t)
	defer memory.Close()

	if err := memory.UpdateRuntime(context.Background(), RuntimeMetadata{Profile: "updated", Provider: "openai-compatible", Model: "updated-model"}); err != nil {
		t.Fatal(err)
	}
	if err := store.UpdateRuntime(context.Background(), RuntimeMetadata{Profile: "updated", Provider: "openai-compatible", Model: "updated-model"}); err != nil {
		t.Fatal(err)
	}

	var storeSecond CompactionMetadata
	for name, current := range map[string]compactionTestSession{"memory": memory, "store": store} {
		t.Run(name, func(t *testing.T) {
			before := current.Messages()
			retainedID := before[2].ID
			if _, err := current.AppendCompaction(context.Background(), CompactionCheckpoint{
				Summary: testCompactionSummary + "\nfirst", FirstKeptEntryID: retainedID, TokensBefore: 200,
				Usage:   &model.Usage{InputTokens: 5, CachedInputTokens: 2, OutputTokens: 1},
				Details: CompactionDetails{ReadFiles: []string{"first.go"}}, CreatedAt: createdAt,
			}); err != nil {
				t.Fatal(err)
			}
			if err := current.Append(context.Background(), model.Message{
				ID: "memory-post", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "post checkpoint"}}, CreatedAt: createdAt.Add(time.Second),
			}); err != nil {
				t.Fatal(err)
			}
			postID := current.Messages()[len(current.Messages())-1].ID
			second, err := current.AppendCompaction(context.Background(), CompactionCheckpoint{
				Summary: testCompactionSummary + "\nsecond", FirstKeptEntryID: retainedID, TokensBefore: 300,
				Usage:   &model.Usage{InputTokens: 7, CachedInputTokens: 3, OutputTokens: 2},
				Details: CompactionDetails{ModifiedFiles: []string{"second.go"}}, CreatedAt: createdAt.Add(2 * time.Second),
			})
			if err != nil {
				t.Fatal(err)
			}
			if name == "store" {
				storeSecond = second
			}
			messages := current.Messages()
			assertMessageTexts(t, messages, []string{
				"[Compaction summary]\n" + testCompactionSummary + "\nsecond",
				"kept request", "kept answer", "post checkpoint",
			})
			if messages[0].ID != second.ID || messages[1].ID != retainedID || messages[3].ID != postID {
				t.Fatalf("active message IDs = %#v", messages)
			}
			for _, message := range messages {
				if strings.Contains(message.Text(), "\nfirst") {
					t.Fatalf("superseded checkpoint leaked into context: %#v", messages)
				}
			}
			assertLatestCompaction(t, current, second)
		})
	}

	wantMessages := store.Messages()
	wantUsage, wantUsagePresent := store.AggregateUsage()
	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	reopened, warnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	if len(warnings) != 0 || !reflect.DeepEqual(reopened.Messages(), wantMessages) {
		t.Fatalf("reopen warnings/messages = %#v / %#v; want %#v", warnings, reopened.Messages(), wantMessages)
	}
	if got, present := reopened.AggregateUsage(); present != wantUsagePresent || got != wantUsage {
		t.Fatalf("reopened usage = %#v, %v; want %#v, %v", got, present, wantUsage, wantUsagePresent)
	}
	if header := reopened.Header(); header.Profile != "updated" || header.Model != "updated-model" {
		t.Fatalf("reopened runtime settings = %#v", header)
	}
	assertLatestCompaction(t, reopened, storeSecond)
}

func TestStoreAppendCompactionRepeatedCheckpointsReopenExactly(t *testing.T) {
	store := createConversationStore(t)
	first, err := store.AppendCompaction(context.Background(), validCheckpointFor(store))
	if err != nil {
		t.Fatal(err)
	}
	postUser := model.Message{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "new work"}}, CreatedAt: time.Date(2026, 8, 28, 14, 0, 0, 0, time.UTC)}
	if err := store.Append(context.Background(), postUser); err != nil {
		t.Fatal(err)
	}
	postID := store.Messages()[len(store.Messages())-1].ID
	first.FirstPostCheckpointMessageID = postID
	assertLatestCompaction(t, store, first)
	second, err := store.AppendCompaction(context.Background(), CompactionCheckpoint{
		Summary: testCompactionSummary + "\nrepeated", FirstKeptEntryID: postID, TokensBefore: 300,
		Usage: &model.Usage{InputTokens: 9, CachedInputTokens: 4, OutputTokens: 2}, CreatedAt: postUser.CreatedAt.Add(time.Second),
	})
	if err != nil {
		t.Fatal(err)
	}
	wantMessages := store.Messages()
	wantUsage, wantPresent := store.AggregateUsage()
	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}

	reopened, warnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	if len(warnings) != 0 || !reflect.DeepEqual(reopened.Messages(), wantMessages) {
		t.Fatalf("reopen warnings/messages = %#v / %#v; want %#v", warnings, reopened.Messages(), wantMessages)
	}
	if got, present := reopened.AggregateUsage(); present != wantPresent || got != wantUsage {
		t.Fatalf("reopened usage = %#v, %v; want %#v, %v", got, present, wantUsage, wantPresent)
	}
	assertLatestCompaction(t, reopened, second)
}

func TestStoreRetainedTailRepairAdvancesToRequestedSafeRealAnchor(t *testing.T) {
	tail := []piMessage{{
		Role: "assistant", Content: json.RawMessage(`[{"type":"toolCall","id":"repair","name":"read","arguments":{"path":"a.go"}}]`),
		API: "openai-completions", Provider: "openai-compatible", Model: "test-model", Usage: &piUsage{}, StopReason: "toolUse", Timestamp: 1,
	}}
	path := writeExternalRetainedTailFixture(t, tail, nil)
	store, warnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	if len(warnings) != 1 || !strings.Contains(warnings[0].Message, "repaired dangling tool call") {
		t.Fatalf("repair warnings = %#v", warnings)
	}
	latest, ok := store.LatestCompaction()
	if !ok || !latest.RetainedTailOnly || latest.FirstPostCheckpointMessageID == "" {
		t.Fatalf("latest repair metadata = %#v, %v", latest, ok)
	}
	repairID := latest.FirstPostCheckpointMessageID
	messages := store.Messages()
	if len(messages) != 3 || messages[1].ID != "b0000002-tail-0" || messages[2].ID != repairID || messages[2].Role != model.RoleTool {
		t.Fatalf("repaired retained tail = %#v", messages)
	}

	beforeFile := append([]byte(nil), readFile(t, path)...)
	beforeMessages := store.Messages()
	for _, invalid := range []string{"b0000001", "b0000002-tail-0"} {
		if _, err := store.AppendCompaction(context.Background(), CompactionCheckpoint{
			Summary: testCompactionSummary, FirstKeptEntryID: invalid, TokensBefore: 130, CreatedAt: time.Now().UTC(),
		}); !errors.Is(err, ErrInvalidSession) {
			t.Fatalf("invalid retained-tail anchor %q error = %v, want ErrInvalidSession", invalid, err)
		}
		if !bytes.Equal(readFile(t, path), beforeFile) || !reflect.DeepEqual(store.Messages(), beforeMessages) {
			t.Fatalf("invalid anchor %q mutated store", invalid)
		}
	}

	if err := store.Append(context.Background(), model.Message{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "safe later request"}}, CreatedAt: time.Date(2026, 8, 28, 15, 30, 0, 0, time.UTC)}); err != nil {
		t.Fatal(err)
	}
	safeID := store.Messages()[len(store.Messages())-1].ID
	followOn, err := store.AppendCompaction(context.Background(), CompactionCheckpoint{
		Summary: testCompactionSummary + "\nfollow on", FirstKeptEntryID: safeID, TokensBefore: 150,
		CreatedAt: time.Date(2026, 8, 28, 15, 30, 1, 0, time.UTC),
	})
	if err != nil {
		t.Fatal(err)
	}
	if followOn.FirstKeptEntryID != safeID || strings.Contains(followOn.FirstKeptEntryID, "-tail-") {
		t.Fatalf("follow-on metadata = %#v, want requested safe ID %q", followOn, safeID)
	}
	selected := store.Messages()
	if len(selected) != 2 || selected[1].ID != safeID || selected[1].Text() != "safe later request" {
		t.Fatalf("follow-on context = %#v", selected)
	}
	if repairID == safeID {
		t.Fatal("test did not advance beyond repaired tool result")
	}
}

func TestStoreRetainedTailContextAnchorKeepsAttachedPrimary(t *testing.T) {
	contextEntry := piEntry{
		piEntryBase:   piEntryBase{Type: "custom_message", ID: "b0000003", Timestamp: "2026-08-28T15:00:01Z"},
		CustomMessage: &piCustomMessage{CustomType: "external.context", Content: json.RawMessage(`[{"type":"text","text":"attached real context"}]`), Display: true},
	}
	userEntry := piEntry{
		piEntryBase: piEntryBase{Type: "message", ID: "b0000004", Timestamp: "2026-08-28T15:00:02Z"},
		Message:     &piMessage{Role: "user", Content: json.RawMessage(`[{"type":"text","text":"following real request"}]`), Timestamp: 2},
	}
	path := writeExternalRetainedTailFixture(t, []piMessage{{Role: "user", Content: json.RawMessage(`[{"type":"text","text":"synthetic tail"}]`), Timestamp: 1}}, []piEntry{contextEntry, userEntry})
	store, warnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	if len(warnings) != 0 {
		t.Fatalf("warnings = %#v", warnings)
	}
	latest, ok := store.LatestCompaction()
	if !ok || latest.FirstPostCheckpointMessageID != contextEntry.ID {
		t.Fatalf("first real context provenance = %#v, %v", latest, ok)
	}
	followOn, err := store.AppendCompaction(context.Background(), CompactionCheckpoint{
		Summary: testCompactionSummary + "\ncontext follow on", FirstKeptEntryID: contextEntry.ID, TokensBefore: 160,
		CreatedAt: time.Date(2026, 8, 28, 15, 0, 3, 0, time.UTC),
	})
	if err != nil {
		t.Fatal(err)
	}
	if followOn.FirstKeptEntryID != contextEntry.ID {
		t.Fatalf("follow-on anchor = %q, want context %q", followOn.FirstKeptEntryID, contextEntry.ID)
	}
	messages := store.Messages()
	assertMessageTexts(t, messages, []string{"[Compaction summary]\n" + testCompactionSummary + "\ncontext follow on", "[Custom context: external.context]\nattached real context", "following real request"})
	if messages[1].ID != contextEntry.ID || messages[2].ID != userEntry.ID {
		t.Fatalf("attached context IDs = %#v", messages)
	}
}

func TestStoreEmptyRetainedTailAllowsLaterNormalRecentAnchor(t *testing.T) {
	user1 := piEntry{
		piEntryBase: piEntryBase{Type: "message", ID: "b0000003", Timestamp: "2026-08-28T16:00:01Z"},
		Message:     &piMessage{Role: "user", Content: json.RawMessage(`[{"type":"text","text":"first real request"}]`), Timestamp: 1},
	}
	assistant1 := piEntry{
		piEntryBase: piEntryBase{Type: "message", ID: "b0000004", Timestamp: "2026-08-28T16:00:02Z"},
		Message: &piMessage{Role: "assistant", Content: json.RawMessage(`[{"type":"text","text":"first real answer"}]`), API: "openai-completions",
			Provider: "openai-compatible", Model: "test-model", Usage: &piUsage{}, StopReason: "stop", Timestamp: 2},
	}
	user2 := piEntry{
		piEntryBase: piEntryBase{Type: "message", ID: "b0000005", Timestamp: "2026-08-28T16:00:03Z"},
		Message:     &piMessage{Role: "user", Content: json.RawMessage(`[{"type":"text","text":"later real request"}]`), Timestamp: 3},
	}
	path := writeExternalRetainedTailFixture(t, []piMessage{}, []piEntry{user1, assistant1, user2})
	store, warnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	if len(warnings) != 0 {
		t.Fatalf("warnings = %#v", warnings)
	}
	latest, ok := store.LatestCompaction()
	if !ok || !latest.RetainedTailOnly || latest.FirstPostCheckpointMessageID != user1.ID {
		t.Fatalf("empty-tail metadata = %#v, %v", latest, ok)
	}
	followOn, err := store.AppendCompaction(context.Background(), CompactionCheckpoint{
		Summary: testCompactionSummary + "\nnormal recent follow on", FirstKeptEntryID: user2.ID, TokensBefore: 180,
		CreatedAt: time.Date(2026, 8, 28, 16, 0, 4, 0, time.UTC),
	})
	if err != nil {
		t.Fatal(err)
	}
	if followOn.FirstKeptEntryID != user2.ID {
		t.Fatalf("later normal anchor = %q, want %q", followOn.FirstKeptEntryID, user2.ID)
	}
	messages := store.Messages()
	assertMessageTexts(t, messages, []string{"[Compaction summary]\n" + testCompactionSummary + "\nnormal recent follow on", "later real request"})
	if messages[1].ID != user2.ID {
		t.Fatalf("later real ID was not retained: %#v", messages)
	}
}

func TestOpenRetainedTailOnlyCheckpointAcceptsRequestedRealPostMessage(t *testing.T) {
	path := copyPiFixture(t, "compaction-retained-tail-only.jsonl")
	store, warnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	if len(warnings) != 0 {
		t.Fatalf("warnings = %#v", warnings)
	}
	messages := store.Messages()
	if len(messages) != 2 || messages[1].ID != "d0000003-tail-0" {
		t.Fatalf("retained-tail context = %#v", messages)
	}
	latest, ok := store.LatestCompaction()
	if !ok || !latest.RetainedTailOnly || latest.FirstKeptEntryID != "" || latest.FirstPostCheckpointMessageID != "" || !reflect.DeepEqual(latest.Details, CompactionDetails{}) {
		t.Fatalf("latest retained-tail metadata = %#v, %v", latest, ok)
	}
	if messages[0].ContextTokensBefore != 120 || messages[0].Usage == nil || *messages[0].Usage != (model.Usage{InputTokens: 12, CachedInputTokens: 3, OutputTokens: 2}) {
		t.Fatalf("retained-tail summary message = %#v", messages[0])
	}

	if _, err := store.AppendCompaction(context.Background(), CompactionCheckpoint{
		Summary: testCompactionSummary, FirstKeptEntryID: messages[1].ID, TokensBefore: 130, CreatedAt: time.Now().UTC(),
	}); !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("follow-on before real post message error = %v, want ErrInvalidSession", err)
	}
	if err := store.Append(context.Background(), model.Message{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "real post"}}, CreatedAt: time.Date(2026, 8, 28, 15, 0, 0, 0, time.UTC)}); err != nil {
		t.Fatal(err)
	}
	postID := store.Messages()[len(store.Messages())-1].ID
	latest.FirstPostCheckpointMessageID = postID
	assertLatestCompaction(t, store, latest)

	followOn, err := store.AppendCompaction(context.Background(), CompactionCheckpoint{
		Summary: testCompactionSummary + "\nfollow on", FirstKeptEntryID: postID, TokensBefore: 150,
		CreatedAt: time.Date(2026, 8, 28, 15, 0, 1, 0, time.UTC),
	})
	if err != nil {
		t.Fatal(err)
	}
	if followOn.FirstKeptEntryID != postID || followOn.RetainedTailOnly {
		t.Fatalf("follow-on metadata = %#v, want real anchor %q", followOn, postID)
	}
	selected := store.Messages()
	if len(selected) != 2 || selected[1].ID != postID || strings.Contains(selected[1].ID, "-tail-") {
		t.Fatalf("follow-on context = %#v", selected)
	}
	decoded := decodeFixture(t, store.Path())
	checkpoint := decoded.Entries[len(decoded.Entries)-1].Compaction
	if checkpoint == nil || checkpoint.FirstKeptEntryID == nil || *checkpoint.FirstKeptEntryID != postID || checkpoint.RetainedTail != nil {
		t.Fatalf("emitted follow-on checkpoint = %#v", checkpoint)
	}
	lines := readJSONLines(t, store.Path())
	var raw map[string]json.RawMessage
	if err := json.Unmarshal(lines[len(lines)-1], &raw); err != nil {
		t.Fatal(err)
	}
	if _, exists := raw["details"]; exists {
		t.Fatalf("empty details were emitted: %s", lines[len(lines)-1])
	}
}

func TestOpenDualFormCompactionPrefersRealActivePath(t *testing.T) {
	store, warnings, err := Open(filepath.Join("testdata", "pi-v3", "compaction-dual-form.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	if len(warnings) != 0 {
		t.Fatalf("warnings = %#v", warnings)
	}
	messages := store.Messages()
	assertMessageTexts(t, messages, []string{"[Compaction summary]\ndual summary", "real retained path", "after dual checkpoint"})
	for _, message := range messages {
		if strings.Contains(message.ID, "-tail-") || strings.Contains(message.Text(), "synthetic must lose") {
			t.Fatalf("dual form selected synthetic tail: %#v", messages)
		}
	}
	latest, ok := store.LatestCompaction()
	if !ok || latest.RetainedTailOnly || latest.FirstKeptEntryID != "e0000003" || latest.FirstPostCheckpointMessageID != "e0000005" ||
		!reflect.DeepEqual(latest.Details, CompactionDetails{ReadFiles: []string{"README.md"}}) {
		t.Fatalf("latest dual-form metadata = %#v, %v", latest, ok)
	}
}

func TestOpenRecoversCompleteAndIncompleteFinalCompactionRecords(t *testing.T) {
	t.Run("complete without delimiter", func(t *testing.T) {
		store := createConversationStore(t)
		metadata, err := store.AppendCompaction(context.Background(), validCheckpointFor(store))
		if err != nil {
			t.Fatal(err)
		}
		wantMessages := store.Messages()
		path := store.Path()
		if err := store.Close(); err != nil {
			t.Fatal(err)
		}
		contents := readFile(t, path)
		if err := os.WriteFile(path, bytes.TrimSuffix(contents, []byte{'\n'}), 0o600); err != nil {
			t.Fatal(err)
		}
		reopened, warnings, err := Open(path)
		if err != nil {
			t.Fatal(err)
		}
		defer reopened.Close()
		if len(warnings) != 1 || !strings.Contains(warnings[0].Message, "missing final session delimiter") || !reflect.DeepEqual(reopened.Messages(), wantMessages) {
			t.Fatalf("complete-tail recovery = warnings %#v messages %#v", warnings, reopened.Messages())
		}
		assertLatestCompaction(t, reopened, metadata)
	})

	t.Run("incomplete", func(t *testing.T) {
		store := createConversationStore(t)
		wantMessages := store.Messages()
		path := store.Path()
		if err := store.Close(); err != nil {
			t.Fatal(err)
		}
		appendFixture(t, path, `{"type":"compaction","id":"abcdef12","parentId":"`)
		reopened, warnings, err := Open(path)
		if err != nil {
			t.Fatal(err)
		}
		defer reopened.Close()
		if len(warnings) != 1 || !strings.Contains(warnings[0].Message, "incomplete final session line") || !reflect.DeepEqual(reopened.Messages(), wantMessages) {
			t.Fatalf("incomplete-tail recovery = warnings %#v messages %#v", warnings, reopened.Messages())
		}
		if _, ok := reopened.LatestCompaction(); ok {
			t.Fatal("incomplete checkpoint became latest")
		}
	})
}

func TestAppendCompactionAfterCloseFails(t *testing.T) {
	for name, current := range map[string]compactionTestSession{
		"memory": createConversationMemory(t),
		"store":  createConversationStore(t),
	} {
		t.Run(name, func(t *testing.T) {
			if err := current.Close(); err != nil {
				t.Fatal(err)
			}
			if _, err := current.AppendCompaction(context.Background(), validCheckpointFor(current)); !errors.Is(err, errSessionClosed) {
				t.Fatalf("AppendCompaction() error = %v, want errSessionClosed", err)
			}
		})
	}
}

func createConversationStore(t *testing.T) *Store {
	t.Helper()
	store, err := Create(t.TempDir(), testHeader(t))
	if err != nil {
		t.Fatal(err)
	}
	appendConversation(t, store, false)
	return store
}

func createConversationMemory(t *testing.T) *Memory {
	t.Helper()
	memory := NewMemory(testHeader(t))
	appendConversation(t, memory, true)
	return memory
}

func appendConversation(t *testing.T, current interface {
	Append(context.Context, model.Message) error
}, fixedIDs bool) {
	t.Helper()
	ids := []string{"", "", "", ""}
	if fixedIDs {
		ids = []string{"memory-user-1", "memory-assistant-1", "memory-user-2", "memory-assistant-2"}
	}
	messages := []model.Message{
		{ID: ids[0], Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "old request"}}, CreatedAt: time.Date(2026, 8, 28, 11, 0, 0, 0, time.UTC)},
		{ID: ids[1], Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "old answer"}}, CreatedAt: time.Date(2026, 8, 28, 11, 0, 1, 0, time.UTC), FinishReason: model.FinishStop, Usage: &model.Usage{InputTokens: 50, CachedInputTokens: 20, OutputTokens: 5}},
		{ID: ids[2], Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "kept request"}}, CreatedAt: time.Date(2026, 8, 28, 11, 0, 2, 0, time.UTC)},
		{ID: ids[3], Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "kept answer"}}, CreatedAt: time.Date(2026, 8, 28, 11, 0, 3, 0, time.UTC), FinishReason: model.FinishStop, Usage: &model.Usage{InputTokens: 30, CachedInputTokens: 10, OutputTokens: 4}},
	}
	for _, message := range messages {
		if err := current.Append(context.Background(), message); err != nil {
			t.Fatal(err)
		}
	}
}

func appendToolConversation(t *testing.T, current interface {
	Append(context.Context, model.Message) error
}) {
	t.Helper()
	messages := []model.Message{
		{ID: "tool-assistant", Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "read", Arguments: json.RawMessage(`{"path":"README.md"}`)}}, CreatedAt: time.Date(2026, 8, 28, 11, 0, 0, 0, time.UTC), FinishReason: model.FinishToolCalls},
		{ID: "tool-result", Role: model.RoleTool, Blocks: []model.Block{{Type: model.BlockToolResult, ToolCallID: "call-1", ToolName: "read", Text: "contents"}}, CreatedAt: time.Date(2026, 8, 28, 11, 0, 1, 0, time.UTC)},
		{ID: "tool-user", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "continue"}}, CreatedAt: time.Date(2026, 8, 28, 11, 0, 2, 0, time.UTC)},
	}
	for _, message := range messages {
		if err := current.Append(context.Background(), message); err != nil {
			t.Fatal(err)
		}
	}
}

func validCheckpointFor(current interface{ Messages() []model.Message }) CompactionCheckpoint {
	messages := current.Messages()
	return CompactionCheckpoint{
		Summary: testCompactionSummary, FirstKeptEntryID: messages[2].ID, TokensBefore: 258000,
		Usage:     &model.Usage{InputTokens: 100, CachedInputTokens: 40, OutputTokens: 20},
		Details:   CompactionDetails{ReadFiles: []string{"README.md"}},
		CreatedAt: time.Date(2026, 8, 28, 12, 0, 0, 0, time.UTC),
	}
}

func exactBoundedCompactionDetails() CompactionDetails {
	details := CompactionDetails{ReadFiles: make([]string, 1_024)}
	totalBytes := 0
	for index := range details.ReadFiles {
		details.ReadFiles[index] = fmt.Sprintf("path-%04d", index)
		totalBytes += len(details.ReadFiles[index])
	}
	details.ReadFiles[len(details.ReadFiles)-1] += strings.Repeat("x", 64*1024-totalBytes)
	return details
}

func fixedWidthCompactionPath(prefix string, index int) string {
	return fmt.Sprintf("%s%04d-%s", prefix, index, strings.Repeat("x", 58))
}

func compactionPathTextBytes(details CompactionDetails) int {
	total := 0
	for _, path := range details.ModifiedFiles {
		total += len(path)
	}
	for _, path := range details.ReadFiles {
		total += len(path)
	}
	return total
}

func writeExternalRetainedTailFixture(t *testing.T, retainedTail []piMessage, after []piEntry) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "external-retained-tail.jsonl")
	file, err := os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_RDWR, 0o600)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	created := time.Date(2026, 8, 28, 15, 0, 0, 0, time.UTC)
	if _, err := writePiRecord(file, piHeader{Type: "session", Version: PiSessionVersion, ID: "external-retained-tail", Timestamp: created.Format(time.RFC3339Nano), CWD: t.TempDir()}); err != nil {
		t.Fatal(err)
	}
	runtimeData, err := json.Marshal(RuntimeMetadata{Profile: "test", Provider: "openai-compatible", Model: "test-model"})
	if err != nil {
		t.Fatal(err)
	}
	runtime := piEntry{
		piEntryBase: piEntryBase{Type: "custom", ID: "b0000001", Timestamp: created.Format(time.RFC3339Nano)},
		Custom:      &piCustom{CustomType: ottoRuntimeCustomType, Data: runtimeData},
	}
	if _, err := writePiRecord(file, runtime); err != nil {
		t.Fatal(err)
	}
	parent := runtime.ID
	checkpoint := piEntry{
		piEntryBase: piEntryBase{Type: "compaction", ID: "b0000002", ParentID: &parent, Timestamp: created.Add(time.Second).Format(time.RFC3339Nano)},
		Compaction:  &piCompaction{Summary: testCompactionSummary, TokensBefore: 120, RetainedTail: retainedTail},
	}
	if len(retainedTail) == 0 {
		raw := fmt.Sprintf(`{"type":"compaction","id":"b0000002","parentId":"b0000001","timestamp":%q,"summary":%q,"tokensBefore":120,"retainedTail":[]}`, checkpoint.Timestamp, testCompactionSummary)
		checkpoint.Raw = json.RawMessage(raw)
	}
	if _, err := writePiRecord(file, checkpoint); err != nil {
		t.Fatal(err)
	}
	parent = checkpoint.ID
	for index := range after {
		after[index].ParentID = stringPointer(parent)
		if _, err := writePiRecord(file, after[index]); err != nil {
			t.Fatal(err)
		}
		parent = after[index].ID
	}
	return path
}

func externalCompactionDetailsFixture(t *testing.T, rawDetails string) string {
	t.Helper()
	store := createConversationStore(t)
	checkpoint := validCheckpointFor(store)
	checkpoint.Details = CompactionDetails{}
	if _, err := store.AppendCompaction(context.Background(), checkpoint); err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}

	contents := bytes.TrimSuffix(readFile(t, path), []byte{'\n'})
	lines := bytes.Split(contents, []byte{'\n'})
	last := lines[len(lines)-1]
	if len(last) == 0 || last[len(last)-1] != '}' {
		t.Fatalf("invalid generated checkpoint fixture: %s", last)
	}
	var compactDetails bytes.Buffer
	if err := json.Compact(&compactDetails, []byte(rawDetails)); err != nil {
		t.Fatalf("compact crafted external details: %v", err)
	}
	injected := make([]byte, 0, len(last)+compactDetails.Len()+11)
	injected = append(injected, last[:len(last)-1]...)
	injected = append(injected, `,"details":`...)
	injected = append(injected, compactDetails.Bytes()...)
	injected = append(injected, '}')
	if !json.Valid(injected) {
		t.Fatalf("crafted external checkpoint is invalid JSON: %s", injected)
	}
	lines[len(lines)-1] = injected
	updated := append(bytes.Join(lines, []byte{'\n'}), '\n')
	if err := os.WriteFile(path, updated, 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

func assertCompactedConversation(t *testing.T, messages []model.Message, checkpointID, keptID string, createdAt time.Time, tokensBefore int) {
	t.Helper()
	if len(messages) != 3 {
		t.Fatalf("messages = %#v, want summary plus retained suffix", messages)
	}
	if messages[0].ID != checkpointID || messages[0].Role != model.RoleContext || messages[0].ContextType != compactionContextType ||
		!messages[0].Display || messages[0].Text() != "[Compaction summary]\n"+testCompactionSummary || !messages[0].CreatedAt.Equal(createdAt) ||
		messages[0].ContextTokensBefore != tokensBefore || messages[0].Usage == nil {
		t.Fatalf("summary message = %#v", messages[0])
	}
	if messages[1].ID != keptID || messages[1].Text() != "kept request" || messages[2].Text() != "kept answer" {
		t.Fatalf("retained messages = %#v", messages[1:])
	}
}

func assertLatestCompaction(t *testing.T, current interface {
	LatestCompaction() (CompactionMetadata, bool)
}, want CompactionMetadata) {
	t.Helper()
	got, ok := current.LatestCompaction()
	if !ok || !reflect.DeepEqual(got, want) {
		t.Fatalf("LatestCompaction() = %#v, %v; want %#v, true", got, ok, want)
	}
	mutateCompactionMetadata(got)
	again, ok := current.LatestCompaction()
	if !ok || !reflect.DeepEqual(again, want) {
		t.Fatalf("LatestCompaction() aliases caller state: %#v, %v; want %#v", again, ok, want)
	}
}

func mutateCompactionMetadata(metadata CompactionMetadata) {
	if metadata.Usage != nil {
		metadata.Usage.InputTokens = math.MaxInt
	}
	if len(metadata.Details.ReadFiles) != 0 {
		metadata.Details.ReadFiles[0] = "mutated"
	}
	if len(metadata.Details.ModifiedFiles) != 0 {
		metadata.Details.ModifiedFiles[0] = "mutated"
	}
}

func assertUnchangedCompactionState(t *testing.T, current compactionTestSession, messages []model.Message, usage model.Usage, present bool, file []byte) {
	t.Helper()
	if !reflect.DeepEqual(current.Messages(), messages) {
		t.Fatalf("Messages() changed = %#v, want %#v", current.Messages(), messages)
	}
	if got, gotPresent := current.AggregateUsage(); gotPresent != present || got != usage {
		t.Fatalf("AggregateUsage() = %#v, %v; want %#v, %v", got, gotPresent, usage, present)
	}
	if _, ok := current.LatestCompaction(); ok {
		t.Fatal("rejected checkpoint became latest")
	}
	if current.Path() != "" && !bytes.Equal(readFile(t, current.Path()), file) {
		t.Fatal("rejected checkpoint changed session file")
	}
}

func readOptionalFile(t *testing.T, path string) []byte {
	t.Helper()
	if path == "" {
		return nil
	}
	return readFile(t, path)
}

type countingFileWriter struct {
	file   *os.File
	writes int
	syncs  int
}

func (writer *countingFileWriter) Write(data []byte) (int, error) {
	writer.writes++
	return writer.file.Write(data)
}

func (writer *countingFileWriter) Sync() error {
	writer.syncs++
	return writer.file.Sync()
}

type compactionFaultWriter struct {
	file     *os.File
	writeErr error
	syncErr  error
	short    bool
	cancel   context.CancelFunc
	writes   int
	syncs    int
}

func (writer *compactionFaultWriter) Write(data []byte) (int, error) {
	writer.writes++
	if writer.writeErr != nil {
		return 0, writer.writeErr
	}
	if writer.cancel != nil {
		writer.cancel()
	}
	if writer.short {
		return writer.file.Write(data[:len(data)/2])
	}
	return writer.file.Write(data)
}

func (writer *compactionFaultWriter) Sync() error {
	writer.syncs++
	if writer.syncErr != nil {
		return writer.syncErr
	}
	return writer.file.Sync()
}
