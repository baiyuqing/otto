package tui

import (
	"context"
	"strings"
	"testing"
	"time"

	"charm.land/bubbles/v2/textarea"
	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

// renderTranscript grouping

func TestRenderTranscriptGroupsAcrossCommitBoundaryAndResetsOnUser(t *testing.T) {
	assistant := []Entry{{Kind: EntryAssistant, Rendered: "answer"}}
	firstChunk, afterAssistant := renderTranscript(assistant, false, 80, false, true)
	if !afterAssistant {
		t.Fatalf("assistantTurn after assistant entry = false, want true")
	}
	if got := strings.Count(firstChunk, "Otto"); got != 1 {
		t.Fatalf("Otto headers in first chunk = %d, want 1", got)
	}

	tool := []Entry{{Kind: EntryTool, ToolName: "read", ToolDone: true}}
	secondChunk, afterTool := renderTranscript(tool, afterAssistant, 80, false, true)
	if !afterTool {
		t.Fatalf("assistantTurn after tool entry = false, want true (grouping continues)")
	}
	if got := strings.Count(secondChunk, "Otto"); got != 0 {
		t.Fatalf("Otto headers in second chunk = %d, want 0 (grouping already open)", got)
	}

	user := []Entry{{Kind: EntryUser, Rendered: "hi"}}
	_, afterUser := renderTranscript(user, afterTool, 80, false, true)
	if afterUser {
		t.Fatal("assistantTurn after user entry = true, want false (user resets grouping)")
	}
}

// isEntryFinal rule table

func TestIsEntryFinalRules(t *testing.T) {
	tests := []struct {
		name            string
		entry           Entry
		activeAssistant int
		running         bool
		want            bool
	}{
		{"user always final", Entry{Kind: EntryUser}, -1, true, true},
		{"system always final", Entry{Kind: EntrySystem}, -1, true, true},
		{"error always final", Entry{Kind: EntryError}, -1, true, true},
		{"tool not done", Entry{Kind: EntryTool, ToolDone: false}, -1, true, false},
		{"tool done", Entry{Kind: EntryTool, ToolDone: true}, -1, true, true},
		{"assistant is the active entry", Entry{Kind: EntryAssistant}, 0, true, false},
		{"assistant is not the active entry", Entry{Kind: EntryAssistant}, 1, true, true},
		{"compaction while running", Entry{Kind: EntryCompaction}, -1, true, false},
		{"compaction once stopped", Entry{Kind: EntryCompaction}, -1, false, true},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			entries := []Entry{tc.entry}
			if got := isEntryFinal(entries, 0, tc.activeAssistant, tc.running); got != tc.want {
				t.Fatalf("isEntryFinal() = %v, want %v", got, tc.want)
			}
		})
	}
}

// commitFinalEntries

func TestCommitFinalEntriesStopsAtFirstNonFinalEntry(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 20)
	m.entries = []Entry{
		{ID: "u1", Kind: EntryUser, Rendered: "hello"},
		{ID: "t1", Kind: EntryTool, ToolName: "read", ToolDone: true},
		{ID: "a1", Kind: EntryAssistant, Rendered: "answer"},
		{ID: "u2", Kind: EntryUser, Rendered: "followup"},
	}
	m.activeAssistant = 2
	m.committed = 0
	m.pendingPrints = nil

	m.commitFinalEntries(80)

	if m.committed != 2 {
		t.Fatalf("committed = %d, want 2", m.committed)
	}
	if len(m.pendingPrints) != 1 {
		t.Fatalf("pendingPrints = %d, want 1", len(m.pendingPrints))
	}
	chunk := m.pendingPrints[0]
	if !strings.Contains(chunk, "hello") || !strings.Contains(chunk, "read") {
		t.Fatalf("committed chunk = %q, want user text and tool block", chunk)
	}
	if strings.Contains(chunk, "answer") || strings.Contains(chunk, "followup") {
		t.Fatalf("committed chunk = %q, want later entries excluded", chunk)
	}
	if m.liveLines() <= 0 {
		t.Fatalf("liveLines() = %d, want > 0 (tail still live)", m.liveLines())
	}
}

func TestCommitFinalEntriesNoopWhenWidthUnset(t *testing.T) {
	m := newTestModel(t) // never resized, so m.width == 0
	m.entries = []Entry{{ID: "u1", Kind: EntryUser, Rendered: "hello"}}
	m.committed = 0
	m.pendingPrints = nil

	m.commitFinalEntries(80)

	if m.committed != 0 {
		t.Fatalf("committed = %d, want 0 (gated on m.width > 0)", m.committed)
	}
	if len(m.pendingPrints) != 0 {
		t.Fatalf("pendingPrints = %d, want 0", len(m.pendingPrints))
	}
}

// Print ordering handshake

func TestPrintOrderingHandshakeSerializesQueuedChunks(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 20)
	m.pendingPrints = []string{"chunk1\n", "chunk2\n"}
	m.printInFlight = false

	inert := renderStreamingMsg{generation: m.turnGeneration} // running is false, so dispatch returns early untouched

	updated, cmd := m.Update(inert)
	got := updated.(Model)
	if cmd == nil {
		t.Fatal("Update() cmd = nil, want a flush command for the first chunk")
	}
	if !got.printInFlight {
		t.Fatal("printInFlight = false after first flush, want true")
	}
	if len(got.pendingPrints) != 1 {
		t.Fatalf("pendingPrints = %d, want 1 (second chunk still queued)", len(got.pendingPrints))
	}

	updated, cmd = got.Update(inert)
	got = updated.(Model)
	if cmd != nil {
		t.Fatalf("Update() cmd = %v, want nil while a print is in flight", cmd)
	}
	if len(got.pendingPrints) != 1 || !got.printInFlight {
		t.Fatalf("state changed while in flight: pendingPrints=%d printInFlight=%v", len(got.pendingPrints), got.printInFlight)
	}

	updated, cmd = got.Update(commitFlushedMsg{})
	got = updated.(Model)
	if cmd == nil {
		t.Fatal("Update(commitFlushedMsg{}) cmd = nil, want a flush command for the second chunk")
	}
	if !got.printInFlight {
		t.Fatal("printInFlight = false after dequeuing second chunk, want true")
	}
	if len(got.pendingPrints) != 0 {
		t.Fatalf("pendingPrints = %d, want 0", len(got.pendingPrints))
	}

	updated, cmd = got.Update(commitFlushedMsg{})
	got = updated.(Model)
	if cmd != nil {
		t.Fatalf("Update(commitFlushedMsg{}) cmd = %v, want nil when pendingPrints is empty", cmd)
	}
	if got.printInFlight {
		t.Fatal("printInFlight = true with nothing queued, want false")
	}
}

// Dynamic layout

func TestCalculateLayoutLiveLinesDrivesTranscriptHeightOnly(t *testing.T) {
	editor := textarea.New()
	width, height := 80, 24

	zero := calculateLayout(width, height, editor, 0, 0)
	if zero.transcriptHeight != 0 {
		t.Fatalf("transcriptHeight with liveLines=0 = %d, want 0", zero.transcriptHeight)
	}

	huge := calculateLayout(width, height, editor, 0, 1000)
	availableHeight := height - zero.inputBoxHeight - zero.footerHeight - zero.editorSpacing - zero.suggestionHeight
	if huge.transcriptHeight != availableHeight {
		t.Fatalf("transcriptHeight with liveLines=1000 = %d, want clamped to available height %d", huge.transcriptHeight, availableHeight)
	}

	if zero.editorHeight != huge.editorHeight || zero.footerHeight != huge.footerHeight ||
		zero.inputBoxHeight != huge.inputBoxHeight || zero.inputBoxed != huge.inputBoxed {
		t.Fatalf("input box / footer layout changed with liveLines: zero=%#v huge=%#v", zero, huge)
	}
}

func TestCalculateLayoutTooSmallRuleUnchanged(t *testing.T) {
	editor := textarea.New()
	if got := calculateLayout(minTerminalWidth-1, 24, editor, 0, 5); !got.tooSmall {
		t.Fatal("tooSmall = false for width below minimum, want true")
	}
	if got := calculateLayout(80, minTerminalHeight-1, editor, 0, 5); !got.tooSmall {
		t.Fatal("tooSmall = false for height below minimum, want true")
	}
	if got := calculateLayout(minTerminalWidth, minTerminalHeight, editor, 0, 5); got.tooSmall {
		t.Fatal("tooSmall = true at the minimum bounds, want false")
	}
}

// History commit on startup and /resume / /new

func TestStartupCommitsFullHistoryOnFirstWindowSize(t *testing.T) {
	backend := &fakeBackend{history: []model.Message{
		{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "hi"}}},
		{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "hello there"}}},
	}}
	m := newTestModelWithBackend(t, backend)

	updated, _ := m.dispatch(tea.WindowSizeMsg{Width: 80, Height: 20})
	got := updated.(Model)

	if got.committed != len(got.entries) {
		t.Fatalf("committed = %d, want len(entries) = %d", got.committed, len(got.entries))
	}
	if got.liveLines() != 0 {
		t.Fatalf("liveLines() = %d, want 0", got.liveLines())
	}
	printed := strings.Join(got.pendingPrints, "\n")
	if !strings.Contains(printed, "hi") || !strings.Contains(printed, "hello there") {
		t.Fatalf("printed = %q, want both history entries queued", printed)
	}
}

func TestResumeReplacesHistoryRestartsCommitCounter(t *testing.T) {
	backend := &resumeBackend{
		info:    app.Info{Profile: "old-profile", Model: "old-model", SessionID: "session-old"},
		history: []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "old transcript"}}}},
	}
	backend.resumeSession = func(_ context.Context, path string) (app.ResumeResult, error) {
		backend.info = app.Info{Profile: "new-profile", Model: "new-model", SessionID: "session-new"}
		backend.history = []model.Message{{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "fresh transcript"}}}}
		return app.ResumeResult{SessionPath: path}, nil
	}
	m := loadResumePicker(t, backend, session.ListResult{Sessions: []session.SessionInfo{{ID: "fresh", Path: "/sessions/fresh.jsonl"}}})
	if m.committed == 0 || m.committed != len(m.entries) {
		t.Fatalf("pre-resume committed = %d, entries = %d, want fully committed old history", m.committed, len(m.entries))
	}

	updated, cmd := m.Update(keyPress(tea.KeyEnter))
	resuming := updated.(Model)
	if cmd == nil {
		t.Fatal("resume cmd = nil")
	}
	result := runCommandWithin(t, cmd, time.Second)
	updated, _ = resuming.dispatch(result)
	got := updated.(Model)

	if got.committed != len(got.entries) {
		t.Fatalf("committed = %d, want len(entries) = %d", got.committed, len(got.entries))
	}
	printed := strings.Join(got.pendingPrints, "\n")
	if strings.Contains(printed, "old transcript") || !strings.Contains(printed, "fresh transcript") {
		t.Fatalf("printed = %q, want old history dropped and replacement queued", printed)
	}
}

func TestNewSessionResetRequeuesBannerForEmptyHistory(t *testing.T) {
	backend := &fakeBackend{history: []model.Message{
		{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "old"}}},
	}}
	backend.newSession = func() error {
		backend.history = nil
		return nil
	}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 20)
	if m.committed == 0 || m.committed != len(m.entries) {
		t.Fatalf("pre-reset committed = %d, entries = %d, want fully committed old history", m.committed, len(m.entries))
	}

	m.editor.SetValue("/new")
	updated, cmd := m.dispatch(keyPress(tea.KeyEnter))
	pending := updated.(Model)
	if cmd == nil {
		t.Fatal("/new cmd = nil")
	}
	msg := runCommandWithin(t, cmd, time.Second)
	updated, _ = pending.dispatch(msg)
	got := updated.(Model)

	if got.committed != 0 || len(got.entries) != 0 {
		t.Fatalf("committed = %d, entries = %d, want both 0 after reset to empty history", got.committed, len(got.entries))
	}
	printed := strings.Join(got.pendingPrints, "\n")
	if !strings.Contains(printed, "Ask Otto anything") {
		t.Fatalf("printed = %q, want the empty-session banner requeued", printed)
	}
}

// Reconciliation after commit

func TestReconciliationAfterCommitDoesNotReprintOrRegressCommit(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 80, 20)
	m.expandedDetails = true
	m.entries = []Entry{{ID: "tool-1", Kind: EntryTool, ToolCallID: "call-1", ToolName: "read", ToolOutput: "first output", ToolDone: true}}
	m.committed = 0
	m.pendingPrints = nil

	m.commitFinalEntries(80)
	if m.committed != 1 {
		t.Fatalf("committed = %d, want 1", m.committed)
	}
	if len(m.pendingPrints) != 1 || !strings.Contains(m.pendingPrints[0], "first output") {
		t.Fatalf("pendingPrints = %#v, want one chunk containing the tool output", m.pendingPrints)
	}

	// Simulate reconcilePersistedToolResults rewriting an already-committed
	// tool entry in place, as it does when the persisted session message for
	// the same ToolCallID arrives after the live event already committed.
	m.entries[0].ToolOutput = "second output"
	m.commitFinalEntries(80)

	if m.committed != 1 {
		t.Fatalf("committed = %d after reconciliation, want unchanged at 1 (no regression)", m.committed)
	}
	if len(m.pendingPrints) != 1 {
		t.Fatalf("pendingPrints = %d after reconciliation, want unchanged at 1 (no re-print)", len(m.pendingPrints))
	}
	if strings.Contains(m.pendingPrints[0], "second output") {
		t.Fatalf("pendingPrints = %#v, want the rewritten output not to reach scrollback", m.pendingPrints)
	}
}
