package tui

import (
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/session"
	"github.com/charmbracelet/x/ansi"
)

// Inline mode: a frame as tall as the terminal scrolls the terminal by the
// number of rows above the frame, so modals render at content height.
func TestModalOverlaysRenderAtContentHeight(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 100, 30)
	m.overlay = overlayHelp
	assertContentHeightBox(t, "help", m.View().Content, 30)
	m.overlay = overlaySession
	assertContentHeightBox(t, "session", m.View().Content, 30)
	m.overlay = overlayNone
	m.resume = resumePickerState{mode: resumeLoaded, sessions: []session.SessionInfo{{ID: "other", Path: "/sessions/other.jsonl"}}}
	assertContentHeightBox(t, "resume", m.View().Content, 30)
}

// Inline mode: when a frame shrinks, the renderer moves the cursor up from
// its row in the previous frame, clamped to the new height. A hidden cursor
// stays on the last row of the modal, so closing a 25-row modal into a 7-row
// frame would leave 18 rows on screen. Modals keep a visible cursor on the
// title row (row 1) instead.
func TestModalOverlaysPlaceCursorOnTitleRow(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 100, 30)
	cases := []struct {
		name  string
		apply func(m Model) Model
		title string
	}{
		{"help", func(m Model) Model { m.overlay = overlayHelp; return m }, "Help (? or /help)"},
		{"session", func(m Model) Model { m.overlay = overlaySession; return m }, "Session"},
		{"resume", func(m Model) Model {
			m.resume = resumePickerState{mode: resumeLoaded, sessions: []session.SessionInfo{{ID: "other", Path: "/sessions/other.jsonl"}}}
			return m
		}, "Resume Session  1/1"},
		{"archive", func(m Model) Model { m.archive = archivePickerState{mode: archiveLoading}; return m }, "Archive Session"},
		{"profile", func(m Model) Model { m.profilePicker = profilePickerState{profiles: []string{"a"}}; return m }, "Select Model Profile  1/1"},
	}
	for _, tc := range cases {
		view := tc.apply(m).View()
		lines := strings.Split(view.Content, "\n")
		row := ansi.Strip(lines[1])
		index := strings.Index(row, tc.title)
		if index < 0 {
			t.Fatalf("%s row 1 = %q, want title %q", tc.name, row, tc.title)
		}
		want := ansi.StringWidth(row[:index]) + ansi.StringWidth(tc.title)
		if view.Cursor == nil || view.Cursor.Y != 1 || view.Cursor.X != want {
			t.Fatalf("%s cursor = %+v, want (%d,1) after the title", tc.name, view.Cursor, want)
		}
	}
}

func assertContentHeightBox(t *testing.T, name, content string, height int) {
	t.Helper()
	lines := strings.Split(content, "\n")
	if len(lines) >= height {
		t.Fatalf("%s frame is %d rows, want fewer than the %d-row terminal:\n%s", name, len(lines), height, content)
	}
	first, last := ansi.Strip(lines[0]), ansi.Strip(lines[len(lines)-1])
	if !strings.Contains(first, "╭") || !strings.Contains(last, "╰") {
		t.Fatalf("%s frame is padded around the box: first=%q last=%q", name, first, last)
	}
}
