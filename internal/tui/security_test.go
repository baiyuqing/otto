package tui

import (
	"regexp"
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

func TestResumePickerSanitizesAndClipsExternalMetadataWithoutEntityInterpretation(t *testing.T) {
	m := resizeModel(t, loadedResumeModel(t, 1), 40, 8)
	payload := "会話🙂" + terminalInjectionPayload + "\nProvider: forged &#27;]52;c;entity&#7; " + strings.Repeat("界🙂", 100)
	m.resume.sessions[0].Name = payload
	m.resume.sessions[0].LastUserText = payload
	m.resume.sessions[0].Profile = payload
	m.resume.sessions[0].Provider = payload
	m.resume.sessions[0].Model = payload
	m.resume.sessions[0].Path = "/secret/" + payload
	m.resume.errText = payload

	content := m.View().Content
	assertRenderedBounds(t, content, 40, 8)
	assertNoRawTerminalControls(t, content)
	if !strings.Contains(content, "会話🙂") || !strings.Contains(content, `\x1b]52;c;owned`) {
		t.Fatalf("resume picker = %q, want safe CJK/emoji and escaped controls", content)
	}
	if strings.Contains(content, "Provider: forged") {
		t.Fatalf("resume picker allowed field spoof: %q", content)
	}
	if strings.Contains(content, "/secret/") {
		t.Fatalf("resume picker exposed path despite higher-priority title: %q", content)
	}
	if !strings.Contains(content, "…") {
		t.Fatalf("resume picker did not ellipsis-clip long display-width text: %q", content)
	}

	m = resizeModel(t, m, 240, 12)
	content = m.View().Content
	assertRenderedBounds(t, content, 240, 12)
	assertNoRawTerminalControls(t, content)
	if !strings.Contains(content, "&#27;]52;c;entity&#7;") {
		t.Fatalf("resume picker interpreted or lost inert entity text: %q", content)
	}
}

func TestResumePickerSanitizesInjectionInRenderedErrorModesAndPathFallback(t *testing.T) {
	payload := "safe" + terminalInjectionPayload + "\n> * forged-row"
	assertSafePicker := func(t *testing.T, m Model, want string) {
		t.Helper()
		content := m.View().Content
		assertRenderedBounds(t, content, m.width, m.height)
		assertNoRawTerminalControls(t, content)
		if !strings.Contains(content, want) || !strings.Contains(content, `\x1b]52;c;owned`) || !strings.Contains(content, `\x0a> * forged-row`) {
			t.Fatalf("resume picker = %q, want escaped single-line payload and %q", content, want)
		}
		for _, line := range strings.Split(content, "\n") {
			if strings.HasPrefix(strings.TrimSpace(line), "> * forged-row") {
				t.Fatalf("resume picker created spoofed row: %q", content)
			}
		}
	}

	for _, tc := range []struct {
		name string
		mode resumeMode
		want string
	}{
		{name: "load error", mode: resumeLoadError, want: "Unable to load sessions"},
		{name: "resume error", mode: resumeResumeError, want: "Error: safe"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			m := resizeModel(t, loadedResumeModel(t, 1), 200, 12)
			m.resume.mode = tc.mode
			m.resume.errText = payload
			assertSafePicker(t, m, tc.want)
		})
	}

	t.Run("canonical path fallback", func(t *testing.T) {
		m := resizeModel(t, loadedResumeModel(t, 1), 200, 12)
		info := &m.resume.sessions[0]
		info.Name = ""
		info.LastUserText = ""
		info.ID = ""
		info.CWD = ""
		info.Path = "/fallback/" + payload
		assertSafePicker(t, m, "/fallback/safe")
	})
}

func TestCompactionSecurityExpandedDetailsSanitizeTerminalControlsAndPreserveWhitespace(t *testing.T) {
	raw := "## Goal\n\tkeep" + terminalInjectionPayload + "\n  exact trailing space  \n"
	rendered, err := renderMarkdown(rendererFunc(func(text string, _ int) (string, error) { return text, nil }), raw, 80)
	if err != nil {
		t.Fatalf("renderMarkdown() error = %v", err)
	}
	entry := Entry{Kind: EntryCompaction, Raw: raw, Rendered: rendered, TokensBefore: 258000}
	expanded := renderCompactionBlock(entry, 80, true)
	assertNoRawTerminalControls(t, expanded)
	if !strings.Contains(expanded, "[context] compacted 258k tokens") || strings.Contains(expanded, "[Compaction summary]") {
		t.Fatalf("expanded compaction = %q", expanded)
	}
	if !strings.Contains(expanded, "## Goal\n\tkeep") || !strings.Contains(expanded, "\n  exact trailing space  ") {
		t.Fatalf("expanded compaction = %q, want copy-friendly whitespace preserved", expanded)
	}
	if !strings.Contains(expanded, `\x1b]52;c;owned\x07\x9b31m\x7f`) {
		t.Fatalf("expanded compaction = %q, want escaped terminal payload", expanded)
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

// sgrRe matches ANSI SGR sequences (\x1b[...m) that lipgloss produces for
// color/style. Only these are stripped before checking for injected terminal
// controls \u2014 other escapes (OSC, CSI non-SGR) remain so the check catches them.
var sgrRe = regexp.MustCompile("\x1b\\[[0-9;]*m")

func assertNoRawTerminalControls(t *testing.T, rendered string) {
	t.Helper()
	stripped := sgrRe.ReplaceAllString(rendered, "")
	for _, forbidden := range []string{"\x1b", "\a", "\u009b", "\x7f"} {
		if strings.Contains(stripped, forbidden) {
			t.Fatalf("rendered text contains raw terminal control %q: %q", forbidden, rendered)
		}
	}
}
