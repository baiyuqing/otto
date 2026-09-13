//go:build rustinterop

// Package session's Rust interoperability gate. It is excluded from the
// default build: run it through `make rust-interop`, which first produces the
// bundle with `cargo test -p otto --test interop`.
package session

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"testing"

	"github.com/baiyuqing/otto/internal/model"
)

// rustMessage is the shape both implementations agree on for one message.
type rustMessage struct {
	Role        string `json:"role"`
	ContextType string `json:"contextType"`
	Text        string `json:"text"`
	ToolCallID  string `json:"toolCallId"`
	ToolName    string `json:"toolName"`
	ToolText    string `json:"toolText"`
}

type rustUsage struct {
	InputTokens       int `json:"inputTokens"`
	OutputTokens      int `json:"outputTokens"`
	CachedInputTokens int `json:"cachedInputTokens"`
}

type rustExpectation struct {
	SessionPath string `json:"sessionPath"`
	Workspace   string `json:"workspace"`
	Header      struct {
		Version  int    `json:"version"`
		ID       string `json:"id"`
		Provider string `json:"provider"`
		Profile  string `json:"profile"`
		Model    string `json:"model"`
	} `json:"header"`
	Name                      string        `json:"name"`
	Messages                  []rustMessage `json:"messages"`
	AggregateUsage            rustUsage     `json:"aggregateUsage"`
	AggregateUsagePresent     bool          `json:"aggregateUsagePresent"`
	ContextInputTokens        int           `json:"contextInputTokens"`
	ContextInputTokensPresent bool          `json:"contextInputTokensPresent"`
	ContextInputTokensPending bool          `json:"contextInputTokensPending"`
	Compaction                struct {
		Summary          string `json:"summary"`
		FirstKeptEntryID string `json:"firstKeptEntryId"`
		TokensBefore     int    `json:"tokensBefore"`
		RetainedTailOnly bool   `json:"retainedTailOnly"`
	} `json:"compaction"`
}

type rustFixture struct {
	Messages              []rustMessage `json:"messages"`
	Warnings              []string      `json:"warnings"`
	AggregateUsage        rustUsage     `json:"aggregateUsage"`
	AggregateUsagePresent bool          `json:"aggregateUsagePresent"`
}

// TestRustInterop checks both directions of the session file contract: Go
// reads the session the Rust store wrote, and Go's own decoding of every Pi
// v3 fixture matches what Rust decoded from the same bytes.
func TestRustInterop(t *testing.T) {
	directory := os.Getenv("OTTO_RUST_INTEROP_DIR")
	if directory == "" {
		t.Skip("OTTO_RUST_INTEROP_DIR is unset; run make rust-interop")
	}

	t.Run("go reads the rust session", func(t *testing.T) {
		var want rustExpectation
		readJSON(t, filepath.Join(directory, "expectation.json"), &want)

		header, err := ReadHeader(want.SessionPath)
		if err != nil {
			t.Fatalf("ReadHeader() = %v", err)
		}
		if header.Version != want.Header.Version || header.ID != want.Header.ID ||
			header.Provider != want.Header.Provider || header.Profile != want.Header.Profile ||
			header.Model != want.Header.Model || header.Workspace != want.Workspace {
			t.Fatalf("ReadHeader() = %#v, want %#v", header, want.Header)
		}

		store, warnings, err := Open(want.SessionPath)
		if err != nil {
			t.Fatalf("Open() = %v", err)
		}
		defer store.Close()
		if len(warnings) != 0 {
			t.Fatalf("Open() warnings = %#v, want none: the Rust store must write repair-free files", warnings)
		}
		if got := store.Name(); got != want.Name {
			t.Fatalf("Name() = %q, want %q", got, want.Name)
		}
		if got := goMessages(store.Messages()); !reflect.DeepEqual(got, want.Messages) {
			t.Fatalf("Messages() = %#v, want %#v", got, want.Messages)
		}

		usage, present := store.AggregateUsage()
		if present != want.AggregateUsagePresent || !sameUsage(usage, want.AggregateUsage) {
			t.Fatalf("AggregateUsage() = %#v/%v, want %#v/%v", usage, present, want.AggregateUsage, want.AggregateUsagePresent)
		}

		snapshot := store.Snapshot()
		if snapshot.ContextInputTokens != want.ContextInputTokens ||
			snapshot.ContextInputTokensPresent != want.ContextInputTokensPresent ||
			snapshot.ContextInputTokensPending != want.ContextInputTokensPending {
			t.Fatalf("Snapshot() = %#v, want context tokens %d/%v/%v", snapshot,
				want.ContextInputTokens, want.ContextInputTokensPresent, want.ContextInputTokensPending)
		}

		latest, ok := store.LatestCompaction()
		if !ok {
			t.Fatal("LatestCompaction() reported no checkpoint")
		}
		if latest.Summary != want.Compaction.Summary ||
			latest.FirstKeptEntryID != want.Compaction.FirstKeptEntryID ||
			latest.TokensBefore != want.Compaction.TokensBefore ||
			latest.RetainedTailOnly != want.Compaction.RetainedTailOnly {
			t.Fatalf("LatestCompaction() = %#v, want %#v", latest, want.Compaction)
		}
	})

	t.Run("rust decodes the go fixtures", func(t *testing.T) {
		var want map[string]rustFixture
		readJSON(t, filepath.Join(directory, "fixtures.json"), &want)
		if len(want) == 0 {
			t.Fatal("fixtures.json listed no fixtures")
		}

		for name, fixture := range want {
			t.Run(name, func(t *testing.T) {
				// Open repairs in place, so decode from a copy.
				source, err := os.ReadFile(filepath.Join("testdata", "pi-v3", name))
				if err != nil {
					t.Fatal(err)
				}
				path := filepath.Join(t.TempDir(), name)
				if err := os.WriteFile(path, source, 0o600); err != nil {
					t.Fatal(err)
				}

				store, warnings, err := Open(path)
				if err != nil {
					t.Fatalf("Open() = %v", err)
				}
				defer store.Close()
				if got := warningMessages(warnings); !reflect.DeepEqual(got, fixture.Warnings) {
					t.Fatalf("Open() warnings = %#v, want %#v", got, fixture.Warnings)
				}
				if got := goMessages(store.Messages()); !reflect.DeepEqual(got, fixture.Messages) {
					t.Fatalf("Messages() = %#v, want %#v", got, fixture.Messages)
				}
				usage, present := store.AggregateUsage()
				if present != fixture.AggregateUsagePresent || !sameUsage(usage, fixture.AggregateUsage) {
					t.Fatalf("AggregateUsage() = %#v/%v, want %#v/%v", usage, present, fixture.AggregateUsage, fixture.AggregateUsagePresent)
				}
			})
		}
	})
}

func readJSON(t *testing.T, path string, target any) {
	t.Helper()
	contents, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %s: %v (run make rust-interop)", path, err)
	}
	if err := json.Unmarshal(contents, target); err != nil {
		t.Fatalf("decode %s: %v", path, err)
	}
}

func goMessages(messages []model.Message) []rustMessage {
	converted := make([]rustMessage, 0, len(messages))
	for _, message := range messages {
		row := rustMessage{
			Role:        string(message.Role),
			ContextType: message.ContextType,
			Text:        message.Text(),
		}
		for _, block := range message.Blocks {
			if block.Type == model.BlockToolCall || block.Type == model.BlockToolResult {
				row.ToolCallID = block.ToolCallID
				row.ToolName = block.ToolName
				row.ToolText = block.Text
				break
			}
		}
		converted = append(converted, row)
	}
	return converted
}

func warningMessages(warnings []Warning) []string {
	messages := make([]string, 0, len(warnings))
	for _, warning := range warnings {
		messages = append(messages, warning.Message)
	}
	return messages
}

func sameUsage(usage model.Usage, want rustUsage) bool {
	return usage.InputTokens == want.InputTokens &&
		usage.OutputTokens == want.OutputTokens &&
		usage.CachedInputTokens == want.CachedInputTokens
}
