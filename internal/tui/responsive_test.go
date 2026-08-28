package tui

import (
	"fmt"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
)

func TestVerySmallTerminalViewsStayWithinBounds(t *testing.T) {
	for _, size := range []struct{ width, height int }{
		{width: 1, height: 1},
		{width: 2, height: 1},
		{width: 10, height: 3},
		{width: 39, height: 7},
	} {
		t.Run(fmt.Sprintf("%dx%d", size.width, size.height), func(t *testing.T) {
			m := resizeModel(t, newTestModel(t), size.width, size.height)
			assertRenderedBounds(t, m.View().Content, size.width, size.height)
		})
	}
}

func TestHelpOverlayAtMinimumTerminalShowsEveryControlWithinBounds(t *testing.T) {
	m := resizeModel(t, newTestModel(t), 40, 8)
	m.overlay = overlayHelp
	content := m.View().Content
	assertRenderedBounds(t, content, 40, 8)
	for _, control := range []string{
		"Help",
		"?",
		"/help",
		"Enter",
		"Shift+Enter",
		"Alt+Enter",
		"Ctrl+O",
		"PgUp/PgDn",
		"Home/End",
		"Esc",
		"Ctrl+C",
		"/session",
		"/new",
		"/exit",
	} {
		if !strings.Contains(content, control) {
			t.Fatalf("help overlay = %q, want accessible %q control", content, control)
		}
	}
}

func TestLongSessionOverlayAndFooterStayWithinBounds(t *testing.T) {
	long := strings.Repeat("very-long-metadata/", 20)
	backend := &fakeBackend{info: app.Info{
		SessionID:   "session-" + long,
		SessionPath: "/tmp/" + long + "session.jsonl",
		Workspace:   "/workspace/" + long,
		Provider:    "openai-compatible-" + long,
		Profile:     "profile-" + long,
		Model:       "model-" + long,
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 40, 8)
	m.statusText = "status-" + long
	assertRenderedBounds(t, m.View().Content, 40, 8)

	m.overlay = overlaySession
	content := m.View().Content
	assertRenderedBounds(t, content, 40, 8)
	for _, field := range []string{"Session", "ID:", "Path:", "Provider:", "Profile:", "Model:"} {
		if !strings.Contains(content, field) {
			t.Fatalf("session overlay = %q, want bounded %s field", content, field)
		}
	}

	footer := renderFooter(40, backend.info, model.Usage{}, "status-"+long)
	assertRenderedBounds(t, footer, 40, 1)
}

func TestResumePickerResizeClampsSelectionAndRestoresTranscriptOnClose(t *testing.T) {
	m := loadedResumeModel(t, 20)
	m.entries = []Entry{{Kind: EntryAssistant, Raw: "underlying transcript", Rendered: "underlying transcript"}}
	m = resizeModel(t, m, 100, 20)
	m.resume.selected = 19
	m.resume.sessions[18].Current = true

	m = resizeModel(t, m, 40, 8)
	start, end := resumeVisibleRange(len(m.resume.sessions), m.resume.selected, resumeVisibleRows(m.width, m.height))
	if m.resume.selected != 19 || m.resume.selected < start || m.resume.selected >= end {
		t.Fatalf("shrunk range=%d:%d selected=%d", start, end, m.resume.selected)
	}
	content := m.View().Content
	assertRenderedBounds(t, content, 40, 8)
	assertResumeRowMarkers(t, content, "Session 20", true, false)
	assertResumeRowMarkers(t, content, "Session 19", false, true)
	if strings.Contains(content, "underlying transcript") {
		t.Fatalf("modal leaked transcript: %q", content)
	}

	m.resume.selected = 99
	m = resizeModel(t, m, 100, 20)
	if m.resume.selected != 19 {
		t.Fatalf("resized selected = %d, want clamped 19", m.resume.selected)
	}
	content = m.View().Content
	assertResumeRowMarkers(t, content, "Session 20", true, false)
	assertResumeRowMarkers(t, content, "Session 19", false, true)
	m, _ = updateResumeKey(t, m, tea.KeyEscape)
	content = m.View().Content
	assertRenderedBounds(t, content, 100, 20)
	if !strings.Contains(content, "underlying transcript") {
		t.Fatalf("closed picker did not restore transcript: %q", content)
	}
}

func assertRenderedBounds(t *testing.T, content string, width, height int) {
	t.Helper()
	lines := strings.Split(content, "\n")
	if content == "" {
		lines = nil
	}
	if len(lines) > height {
		t.Fatalf("rendered line count = %d, want <= %d: %q", len(lines), height, content)
	}
	for i, line := range lines {
		if got := lipgloss.Width(line); got > width {
			t.Fatalf("rendered line %d width = %d, want <= %d: %q", i, got, width, line)
		}
	}
}
