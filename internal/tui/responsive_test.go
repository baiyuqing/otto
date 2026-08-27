package tui

import (
	"fmt"
	"strings"
	"testing"

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
