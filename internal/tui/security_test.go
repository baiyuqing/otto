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
		ToolName:   "name-" + terminalInjectionPayload + "\nforged\r\tfield",
		ToolArgs:   "args-" + terminalInjectionPayload,
		ToolOutput: "output-" + terminalInjectionPayload + "\n\tkept",
		ToolDone:   true,
	}
	rendered := renderToolBlock(entry, 80, true)
	assertNoRawTerminalControls(t, rendered)
	if got := strings.Count(rendered, `\x1b]52;c;owned`); got != 3 {
		t.Fatalf("escaped OSC count = %d, want 3 in %q", got, rendered)
	}
	if !strings.Contains(rendered, `name-`) || !strings.Contains(rendered, `\x0aforged\x0d\x09field`) {
		t.Fatalf("rendered tool name = %q, want single-line escaped metadata", rendered)
	}
	if !strings.Contains(rendered, "\n\tkept") {
		t.Fatalf("rendered tool = %q, want output newline and tab preserved", rendered)
	}
}

func TestFooterAndSessionOverlayEscapeExternalMetadata(t *testing.T) {
	spoof := "\nProvider: forged\r\tfield"
	entityPayload := "&#27;]52;c;entity&#7; &amp;#x9b;31m"
	info := app.Info{
		SessionID:   "id-" + terminalInjectionPayload + spoof + entityPayload,
		SessionPath: "/tmp/path-" + terminalInjectionPayload + spoof + entityPayload,
		Workspace:   "/tmp/workspace-" + terminalInjectionPayload + spoof + entityPayload,
		Provider:    "provider-" + terminalInjectionPayload + spoof + entityPayload,
		Profile:     "profile-" + terminalInjectionPayload + spoof + entityPayload,
		Model:       "model-" + terminalInjectionPayload + spoof + entityPayload,
	}
	footer := renderFooter(1200, info, model.Usage{}, "status-"+terminalInjectionPayload+spoof+entityPayload)
	overlay := sessionOverlayContent(info)
	for name, rendered := range map[string]string{"footer": footer, "session overlay": overlay} {
		assertNoRawTerminalControls(t, rendered)
		if name == "footer" && strings.ContainsAny(rendered, "\n\r\t") {
			t.Fatalf("%s contains raw single-line controls: %q", name, rendered)
		}
		if name == "session overlay" && strings.ContainsAny(rendered, "\r\t") {
			t.Fatalf("%s contains raw field controls: %q", name, rendered)
		}
		if !strings.Contains(rendered, `\x1b]52;c;owned`) || !strings.Contains(rendered, `\x0aProvider: forged\x0d\x09field`) {
			t.Fatalf("%s = %q, want escaped direct controls and field spoof", name, rendered)
		}
		if !strings.Contains(rendered, entityPayload) {
			t.Fatalf("%s = %q, want inert entity text preserved", name, rendered)
		}
	}
	if got, want := strings.Count(overlay, "\n"), 5; got != want {
		t.Fatalf("session overlay line separators = %d, want %d structural lines only: %q", got, want, overlay)
	}
}

func TestSingleLineSanitizerEscapesAllLineBreakingControls(t *testing.T) {
	if got, want := escapeSingleLineText("one\ntwo\rthree\tfour"), `one\x0atwo\x0dthree\x09four`; got != want {
		t.Fatalf("escapeSingleLineText() = %q, want %q", got, want)
	}
	if got, want := escapePlainText("one\ntwo\tthree"), "one\ntwo\tthree"; got != want {
		t.Fatalf("escapePlainText() = %q, want multiline content preserved", got)
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
