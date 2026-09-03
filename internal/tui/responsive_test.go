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
	// PgUp/PgDn and Home/End are gone from the help overlay at every size:
	// scrolling is now handled by the terminal's own mouse drag/wheel, not
	// an Otto keybinding, and Home/End were never advertised here even in
	// the full-size overlay (they just edit the composer natively).
	for _, control := range []string{
		"Help",
		"?",
		"/help",
		"Enter",
		"Shift+Enter",
		"Alt+Enter",
		"Ctrl+O",
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

func TestCompactionResponsiveCollapsedCheckpointStaysWithinBounds(t *testing.T) {
	entry := Entry{Kind: EntryCompaction, TokensBefore: 258000, TokensAfter: 23000}
	for _, width := range []int{16, 24, 40} {
		t.Run(fmt.Sprintf("width-%d", width), func(t *testing.T) {
			rendered := renderCompactionBlock(entry, width, false)
			assertRenderedBounds(t, rendered, width, 1)
		})
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
		Sandbox: app.SandboxInfo{
			Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true, Reason: app.SandboxReasonNone,
		},
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 40, 8)
	m.statusText = "status-" + long
	assertRenderedBounds(t, m.View().Content, 40, 8)

	m.overlay = overlaySession
	content := m.View().Content
	assertRenderedBounds(t, content, 40, 8)
	for _, field := range []string{"Session", "Sandbox:", "unsandboxed", "ID:", "Path:", "Provider:"} {
		if !strings.Contains(content, field) {
			t.Fatalf("session overlay = %q, want bounded %s field", content, field)
		}
	}
	if strings.Contains(content, "Profile:") {
		t.Fatalf("session overlay retained lower-priority Profile after wrapping Sandbox policy: %q", content)
	}

	footer := renderFooter(40, backend.info, model.Usage{}, "status-"+long)
	assertRenderedBounds(t, footer, 40, 1)
}

func TestResumePickerResizeClampsSelectionAndRestoresTranscriptOnClose(t *testing.T) {
	m := loadedResumeModel(t, 20)
	m.entries = []Entry{{Kind: EntryAssistant, Raw: "underlying transcript", Rendered: "underlying transcript"}}
	// dispatch (not the Update-based resizeModel/updateResumeKey helpers)
	// from here on, so pendingPrints keeps the transcript text committed
	// below instead of an auto-flush popping it into an uninspectable
	// tea.Cmd before the final assertion can see it.
	resized, _ := m.dispatch(tea.WindowSizeMsg{Width: 100, Height: 20})
	m = resized.(Model)
	m.resume.selected = 19
	m.resume.sessions[18].Current = true

	resized, _ = m.dispatch(tea.WindowSizeMsg{Width: 40, Height: 8})
	m = resized.(Model)
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
	resized, _ = m.dispatch(tea.WindowSizeMsg{Width: 100, Height: 20})
	m = resized.(Model)
	if m.resume.selected != 19 {
		t.Fatalf("resized selected = %d, want clamped 19", m.resume.selected)
	}
	content = m.View().Content
	assertResumeRowMarkers(t, content, "Session 20", true, false)
	assertResumeRowMarkers(t, content, "Session 19", false, true)
	updated, _ := m.dispatch(keyPress(tea.KeyEscape))
	m = updated.(Model)
	content = m.View().Content
	assertRenderedBounds(t, content, 100, 20)
	if printed := strings.Join(m.pendingPrints, "\n"); !strings.Contains(printed, "underlying transcript") {
		t.Fatalf("closed picker did not restore transcript: view=%q printed=%q", content, printed)
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
