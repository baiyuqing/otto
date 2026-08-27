package tui

import (
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
)

const terminalInjectionPayload = "safe\x1b]52;c;owned\a\u009b31m\x7f"

func TestTranscriptEscapesTerminalInjectionFromUserAndAssistant(t *testing.T) {
	backend := &fakeBackend{history: []model.Message{
		{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: terminalInjectionPayload}}},
		{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: terminalInjectionPayload}}},
	}}
	m := resizeModel(t, newTestModelWithBackend(t, backend), 80, 12)
	if len(m.entries) != 2 {
		t.Fatalf("entries = %#v", m.entries)
	}
	for _, entry := range m.entries {
		assertNoRawTerminalControls(t, entry.Rendered)
		if !strings.Contains(entry.Rendered, `\x1b]52;c;owned\x07\x9b31m\x7f`) {
			t.Fatalf("rendered entry = %q, want escaped terminal payload", entry.Rendered)
		}
	}
}

func TestToolRenderingEscapesNameArgumentsAndOutput(t *testing.T) {
	entry := Entry{
		Kind:       EntryTool,
		ToolName:   "name-" + terminalInjectionPayload,
		ToolArgs:   "args-" + terminalInjectionPayload,
		ToolOutput: "output-" + terminalInjectionPayload + "\n\tkept",
		ToolDone:   true,
	}
	rendered := renderToolBlock(entry, 80, true)
	assertNoRawTerminalControls(t, rendered)
	if got := strings.Count(rendered, `\x1b]52;c;owned`); got != 3 {
		t.Fatalf("escaped OSC count = %d, want 3 in %q", got, rendered)
	}
	if !strings.Contains(rendered, "\n\tkept") {
		t.Fatalf("rendered tool = %q, want newline and tab preserved", rendered)
	}
}

func TestFooterAndSessionOverlayEscapeExternalMetadata(t *testing.T) {
	info := app.Info{
		SessionID:   "id-" + terminalInjectionPayload,
		SessionPath: "/tmp/path-" + terminalInjectionPayload,
		Workspace:   "/tmp/workspace-" + terminalInjectionPayload,
		Provider:    "provider-" + terminalInjectionPayload,
		Profile:     "profile-" + terminalInjectionPayload,
		Model:       "model-" + terminalInjectionPayload,
	}
	footer := renderFooter(300, info, model.Usage{}, "status-"+terminalInjectionPayload)
	overlay := sessionOverlayContent(info)
	for name, rendered := range map[string]string{"footer": footer, "session overlay": overlay} {
		assertNoRawTerminalControls(t, rendered)
		if !strings.Contains(rendered, `\x1b]52;c;owned`) {
			t.Fatalf("%s = %q, want escaped payload", name, rendered)
		}
	}
}

func assertNoRawTerminalControls(t *testing.T, rendered string) {
	t.Helper()
	for _, forbidden := range []string{"\x1b", "\a", "\u009b", "\x7f"} {
		if strings.Contains(rendered, forbidden) {
			t.Fatalf("rendered text contains raw terminal control %q: %q", forbidden, rendered)
		}
	}
}
