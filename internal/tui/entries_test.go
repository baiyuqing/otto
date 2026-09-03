package tui

import (
	"encoding/json"
	"fmt"
	"slices"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/model"
)

func TestEntriesFromHistoryCreatesFoldedCompactionEntry(t *testing.T) {
	history := []model.Message{{
		ID:                  "deadbeef",
		Role:                model.RoleContext,
		ContextType:         "compaction",
		Display:             true,
		ContextTokensBefore: 258000,
		Blocks:              []model.Block{{Type: model.BlockText, Text: "[Compaction summary]\n## Goal\nship"}},
	}}
	entries, _ := EntriesFromHistory(history)
	if len(entries) != 1 || entries[0].Kind != EntryCompaction || entries[0].CheckpointID != "deadbeef" {
		t.Fatalf("entries=%#v", entries)
	}
	if entries[0].TokensBefore != 258000 {
		t.Fatalf("tokens before = %d, want 258000", entries[0].TokensBefore)
	}
	if strings.Contains(entries[0].Raw, "[Compaction summary]") || entries[0].Raw != "## Goal\nship" {
		t.Fatalf("raw=%q", entries[0].Raw)
	}
}

func TestEntriesFromHistoryKeepsFoldedCompactionBeforeRetainedTail(t *testing.T) {
	history := []model.Message{
		{ID: "checkpoint", Role: model.RoleContext, ContextType: "compaction", Display: true, ContextTokensBefore: 64000, Blocks: []model.Block{{Type: model.BlockText, Text: "[Compaction summary]\nsummary"}}},
		{ID: "tail-user", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "retained request"}}},
		{ID: "tail-assistant", Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "retained answer"}}},
	}

	entries, _ := EntriesFromHistory(history)
	if len(entries) != 3 {
		t.Fatalf("entries=%#v", entries)
	}
	if entries[0].Kind != EntryCompaction || entries[0].CheckpointID != "checkpoint" || entries[0].Raw != "summary" {
		t.Fatalf("compaction entry=%#v", entries[0])
	}
	if entries[1].Kind != EntryUser || entries[1].Raw != "retained request" {
		t.Fatalf("tail user=%#v", entries[1])
	}
	if entries[2].Kind != EntryAssistant || entries[2].Raw != "retained answer" {
		t.Fatalf("tail assistant=%#v", entries[2])
	}
}

func TestNotificationEntryTextTruncatesLongBody(t *testing.T) {
	header := "[task-notification] task t1 (explorer) succeeded · 42s · 7 tool calls · 12,310 tokens"
	lines := make([]string, 30)
	for i := range lines {
		lines[i] = fmt.Sprintf("line %d", i+1)
	}
	text := header + "\n" + strings.Join(lines, "\n")

	got := notificationEntryText("t1", text)

	want := header + "\n" + strings.Join(lines[:20], "\n") + "\n… (10 more lines; /task t1)"
	if got != want {
		t.Fatalf("got = %q, want %q", got, want)
	}
}

func TestNotificationEntryTextKeepsShortBodyIntact(t *testing.T) {
	text := "[task-notification] task t2 (reviewer) failed · 12s · 3 tool calls\nerror text"
	if got := notificationEntryText("t2", text); got != text {
		t.Fatalf("got = %q, want unchanged %q", got, text)
	}
}

func TestEntriesFromHistoryTruncatesTaskNotificationBody(t *testing.T) {
	header := "[task-notification] task t1 (explorer) succeeded · 42s · 7 tool calls · 12,310 tokens"
	lines := make([]string, 30)
	for i := range lines {
		lines[i] = fmt.Sprintf("line %d", i+1)
	}
	text := header + "\n" + strings.Join(lines, "\n")
	history := []model.Message{{
		ID: "note-1", Role: model.RoleContext, ContextType: "task_notification", Display: true,
		Blocks: []model.Block{{Type: model.BlockText, Text: text}},
	}}

	entries, _ := EntriesFromHistory(history)
	if len(entries) != 1 || entries[0].Kind != EntrySystem {
		t.Fatalf("entries = %#v", entries)
	}
	if entries[0].Raw != notificationEntryText("t1", text) {
		t.Fatalf("raw = %q, want %q", entries[0].Raw, notificationEntryText("t1", text))
	}
	if strings.Contains(entries[0].Raw, "line 21") {
		t.Fatalf("raw = %q, should not contain lines beyond 20", entries[0].Raw)
	}
}

func TestEntriesFromHistoryKeepsShortTaskNotificationIntact(t *testing.T) {
	text := "[task-notification] task t2 (reviewer) failed · 12s · 3 tool calls\nerror text"
	history := []model.Message{{
		ID: "note-2", Role: model.RoleContext, ContextType: "task_notification", Display: true,
		Blocks: []model.Block{{Type: model.BlockText, Text: text}},
	}}

	entries, _ := EntriesFromHistory(history)
	if len(entries) != 1 || entries[0].Kind != EntrySystem || entries[0].Raw != text {
		t.Fatalf("entries = %#v", entries)
	}
}

func TestEntriesFromHistoryPairsToolResults(t *testing.T) {
	history := []model.Message{
		{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "inspect"}}},
		{Role: model.RoleAssistant, Usage: &model.Usage{InputTokens: 10, OutputTokens: 4}, Blocks: []model.Block{
			{Type: model.BlockText, Text: "checking"},
			{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "read", Arguments: json.RawMessage(`{"path":"README.md"}`)},
		}},
		{Role: model.RoleTool, Blocks: []model.Block{{Type: model.BlockToolResult, ToolCallID: "call-1", ToolName: "read", Text: "file contents"}}},
	}
	entries, usage := EntriesFromHistory(history)
	if len(entries) != 3 || entries[2].Kind != EntryTool || entries[2].ToolOutput != "file contents" || !entries[2].ToolDone {
		t.Fatalf("entries = %#v", entries)
	}
	if usage.InputTokens != 10 || usage.OutputTokens != 4 {
		t.Fatalf("usage = %#v", usage)
	}
}

func TestEntriesFromHistoryHidesHiddenContextAndRendersVisibleContextNeutrally(t *testing.T) {
	history := []model.Message{
		{ID: "hidden", Role: model.RoleContext, ContextType: "hidden", Display: false, Blocks: []model.Block{{Type: model.BlockText, Text: "provider only"}}},
		{ID: "visible", Role: model.RoleContext, ContextType: "visible", Display: true, Blocks: []model.Block{{Type: model.BlockText, Text: "visible context"}}},
		{ID: "user", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "prompt"}}},
	}

	entries, _ := EntriesFromHistory(history)
	if len(entries) != 2 {
		t.Fatalf("entries = %#v, want visible context and user only", entries)
	}
	if entries[0].Kind != EntrySystem || entries[0].Raw != "visible context" {
		t.Fatalf("visible context entry = %#v, want neutral system entry", entries[0])
	}
	if entries[1].Kind != EntryUser || entries[1].Raw != "prompt" {
		t.Fatalf("user entry = %#v", entries[1])
	}
}

func TestEntriesFromHistoryPreservesZeroBlockToolMessages(t *testing.T) {
	history := []model.Message{
		{ID: "tool-empty", Role: model.RoleTool},
	}

	entries, _ := EntriesFromHistory(history)
	if len(entries) != 1 {
		t.Fatalf("len(entries) = %d, want 1 (%#v)", len(entries), entries)
	}
	if entries[0].Kind != EntryTool || entries[0].Raw != "" || entries[0].ToolDone {
		t.Fatalf("entries[0] = %#v, want empty pending tool entry", entries[0])
	}
}

func TestEntriesFromHistoryMapsErrorRoleToEntryError(t *testing.T) {
	history := []model.Message{
		{ID: "role-error", Role: model.Role("error")},
		{ID: "role-unknown", Role: model.Role("alien")},
	}

	entries, _ := EntriesFromHistory(history)
	if len(entries) != 2 {
		t.Fatalf("len(entries) = %d, want 2 (%#v)", len(entries), entries)
	}
	if entries[0].Kind != EntryError {
		t.Fatalf("entries[0] = %#v, want error entry", entries[0])
	}
	if entries[1].Kind != EntrySystem {
		t.Fatalf("entries[1] = %#v, want safe system fallback", entries[1])
	}
}

func TestEntriesFromHistoryHandlesZeroBlocksOrphansAndToolErrors(t *testing.T) {
	history := []model.Message{
		{ID: "assistant-empty", Role: model.RoleAssistant},
		{ID: "assistant-call", Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "read", Arguments: json.RawMessage(`{"path":"README.md"}`)}}},
		{ID: "orphan-result", Role: model.RoleTool, Blocks: []model.Block{{Type: model.BlockToolResult, ToolCallID: "missing", ToolName: "write", Text: "permission denied", IsError: true}}},
		{ID: "matched-result", Role: model.RoleTool, Blocks: []model.Block{{Type: model.BlockToolResult, ToolCallID: "call-1", ToolName: "read", Text: "README"}}},
	}

	entries, _ := EntriesFromHistory(history)
	if len(entries) != 3 {
		t.Fatalf("len(entries) = %d, want 3 (%#v)", len(entries), entries)
	}
	if entries[0].Kind != EntryAssistant || entries[0].Raw != "" {
		t.Fatalf("entries[0] = %#v, want empty assistant entry", entries[0])
	}
	if entries[1].Kind != EntryTool || entries[1].ToolCallID != "call-1" || entries[1].ToolArgs != `{"path":"README.md"}` || entries[1].ToolOutput != "README" || !entries[1].ToolDone || entries[1].ToolError {
		t.Fatalf("entries[1] = %#v", entries[1])
	}
	if entries[2].Kind != EntryTool || entries[2].ToolCallID != "missing" || entries[2].ToolOutput != "permission denied" || !entries[2].ToolDone || !entries[2].ToolError {
		t.Fatalf("entries[2] = %#v", entries[2])
	}
}

func TestEntriesFromHistoryPreservesMultipleTextBlocksAndStableIDs(t *testing.T) {
	history := []model.Message{
		{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "one"}, {Type: model.BlockText, Text: "two"}}},
		{Role: model.RoleAssistant, Blocks: []model.Block{
			{Type: model.BlockText, Text: "alpha"},
			{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "read"},
			{Type: model.BlockText, Text: "omega"},
		}},
	}

	first, _ := EntriesFromHistory(history)
	second, _ := EntriesFromHistory(history)
	if len(first) != 4 {
		t.Fatalf("len(entries) = %d, want 4 (%#v)", len(first), first)
	}
	if first[0].Kind != EntryUser || first[0].Raw != "onetwo" {
		t.Fatalf("first[0] = %#v, want merged user text", first[0])
	}
	if first[1].Kind != EntryAssistant || first[1].Raw != "alpha" {
		t.Fatalf("first[1] = %#v, want assistant text before tool", first[1])
	}
	if first[2].Kind != EntryTool || first[2].ToolCallID != "call-1" || first[2].ToolDone {
		t.Fatalf("first[2] = %#v, want pending tool entry", first[2])
	}
	if first[3].Kind != EntryAssistant || first[3].Raw != "omega" {
		t.Fatalf("first[3] = %#v, want assistant text after tool", first[3])
	}

	ids := []string{first[0].ID, first[1].ID, first[2].ID, first[3].ID}
	if slices.Contains(ids, "") {
		t.Fatalf("IDs must be non-empty: %#v", first)
	}
	if ids[0] == ids[1] || ids[0] == ids[2] || ids[0] == ids[3] || ids[1] == ids[2] || ids[1] == ids[3] || ids[2] == ids[3] {
		t.Fatalf("IDs must be unique: %#v", ids)
	}
	if got := []string{second[0].ID, second[1].ID, second[2].ID, second[3].ID}; !slices.Equal(got, ids) {
		t.Fatalf("IDs changed between runs: first=%v second=%v", ids, got)
	}
}

func TestEntriesFromHistoryDefensivelyCopiesAndSaturatesUsageTotals(t *testing.T) {
	maxInt := int(^uint(0) >> 1)
	args := json.RawMessage(`{"path":"README.md"}`)
	history := []model.Message{
		{Role: model.RoleUser, Usage: &model.Usage{InputTokens: maxInt - 1, OutputTokens: 3, CachedInputTokens: 2}, Blocks: []model.Block{{Type: model.BlockText, Text: "before"}}},
		{Role: model.RoleAssistant, Usage: &model.Usage{InputTokens: 10, OutputTokens: -5, CachedInputTokens: 4}, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "read", Arguments: args}}},
	}

	entries, usage := EntriesFromHistory(history)
	history[0].Blocks[0].Text = "after"
	history[1].Blocks[0].Arguments[0] = '{'
	history[0].Usage.InputTokens = -99
	history[0].Usage.OutputTokens = -99

	if entries[0].Raw != "before" {
		t.Fatalf("entries[0].Raw = %q, want defensive copy", entries[0].Raw)
	}
	if entries[1].ToolArgs != `{"path":"README.md"}` {
		t.Fatalf("entries[1].ToolArgs = %q, want defensive copy", entries[1].ToolArgs)
	}
	if usage.InputTokens != maxInt || usage.OutputTokens != 3 || usage.CachedInputTokens != 6 {
		t.Fatalf("usage = %#v, want saturated nonnegative totals", usage)
	}
}

func TestEntriesFromHistorySaturatesCachedUsageTotals(t *testing.T) {
	maxInt := int(^uint(0) >> 1)
	history := []model.Message{
		{Role: model.RoleAssistant, Usage: &model.Usage{InputTokens: maxInt - 1, OutputTokens: maxInt - 2, CachedInputTokens: maxInt - 3}, Blocks: []model.Block{{Type: model.BlockText, Text: "one"}}},
		{Role: model.RoleAssistant, Usage: &model.Usage{InputTokens: 10, OutputTokens: 10, CachedInputTokens: 10}, Blocks: []model.Block{{Type: model.BlockText, Text: "two"}}},
	}

	_, usage := EntriesFromHistory(history)
	if usage != (model.Usage{InputTokens: maxInt, OutputTokens: maxInt, CachedInputTokens: maxInt}) {
		t.Fatalf("usage = %#v, want all totals saturated", usage)
	}
}
