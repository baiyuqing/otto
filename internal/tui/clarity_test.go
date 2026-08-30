package tui

import (
	"strings"
	"testing"

	"github.com/charmbracelet/x/ansi"
)

func TestToolSummaryUsesLeadingStatesAndDecodedBashCommand(t *testing.T) {
	entry := Entry{Kind: EntryTool, ToolName: "bash", ToolArgs: `{"command":"git status --short"}`}
	if got := renderToolBlock(entry, 80, false); got != "… bash  git status --short" {
		t.Fatalf("running summary = %q", got)
	}
	entry.ToolDone = true
	if got := renderToolBlock(entry, 80, false); got != "✓ bash  git status --short" {
		t.Fatalf("success summary = %q", got)
	}
	entry.ToolError = true
	if got := renderToolBlock(entry, 80, false); got != "✗ bash  git status --short" {
		t.Fatalf("error summary = %q", got)
	}
}

func TestAssistantTurnShowsOttoOnceAcrossTool(t *testing.T) {
	m := Model{entries: []Entry{
		{Kind: EntryUser, Rendered: "question"},
		{Kind: EntryAssistant, Rendered: "before"},
		{Kind: EntryTool, ToolName: "read", ToolDone: true},
		{Kind: EntryAssistant, Rendered: "after"},
	}}
	got := m.transcriptContent(80)
	if strings.Count(got, "Otto") != 1 {
		t.Fatalf("transcript = %q, want one Otto label", got)
	}
	if !strings.Contains(got, "\x1b[1mOtto") {
		t.Fatalf("transcript = %q, want bold Otto label", got)
	}
	if !strings.Contains(got, "  ✓ read") || strings.Contains(got, "> read") {
		t.Fatalf("transcript = %q, want indented tool row without prompt marker", got)
	}
}

func TestAssistantTitleJoinsFirstTextWithoutBlankLine(t *testing.T) {
	m := Model{entries: []Entry{{Kind: EntryAssistant, Rendered: "first sentence"}}}
	got := m.transcriptContent(80)
	if !strings.Contains(ansi.Strip(got), "Otto\nfirst sentence") {
		t.Fatalf("transcript = %q, want title joined to first assistant text", got)
	}
}

func TestIndentedToolSummaryFitsTerminalAnd120CellLimit(t *testing.T) {
	for _, width := range []int{1, 12, 80, 240} {
		got := indentToolBlock(renderToolBlock(Entry{Kind: EntryTool, ToolName: "bash", ToolArgs: strings.Repeat("x", 300)}, width, false), width)
		for _, line := range strings.Split(got, "\n") {
			if ansi.StringWidth(line) > min(120, width) {
				t.Fatalf("width %d tool line = %d, want <= %d: %q", width, ansi.StringWidth(line), min(120, width), line)
			}
		}
	}
}

func TestEmptyTranscriptHintIncludesLogo(t *testing.T) {
	got := emptyTranscriptHint(80)
	if !strings.HasPrefix(got, "     ____") {
		t.Fatalf("empty hint = %q, want logo as first line", got)
	}
}

func TestExpandedToolWrapsIndentedDetailsWithoutDroppingTail(t *testing.T) {
	entry := Entry{Kind: EntryTool, ToolName: "write", ToolArgs: "argument-head-" + strings.Repeat("a", 60) + "-argument-tail", ToolOutput: "output-head-" + strings.Repeat("b", 60) + "-output-tail", ToolDone: true}
	got := indentToolBlock(renderToolBlock(entry, 30, true), 30)
	if !strings.Contains(got, "argument-tail") || !strings.Contains(got, "output-tail") {
		t.Fatalf("expanded details = %q, want complete argument/output tails", got)
	}
	for _, line := range strings.Split(got, "\n") {
		if ansi.StringWidth(line) > 30 {
			t.Fatalf("expanded line width = %d, want <= 30: %q", ansi.StringWidth(line), line)
		}
	}
}

func TestToolSummaryWithoutPreviewHasNoTrailingSpace(t *testing.T) {
	got := renderToolBlock(Entry{Kind: EntryTool, ToolName: "read", ToolDone: true}, 30, false)
	if got != "✓ read" {
		t.Fatalf("summary = %q, want no trailing space", got)
	}
}

func TestUserBlockFillsBandAndKeepsRailAcrossThemes(t *testing.T) {
	dark := renderUserBlock("a request", 24, true)
	light := renderUserBlock("a request", 24, false)
	if dark == light {
		t.Fatal("dark and light user bands are identical")
	}
	for name, user := range map[string]string{"dark": dark, "light": light} {
		if !strings.Contains(user, "You") || !strings.Contains(user, "│") {
			t.Fatalf("%s user block = %q, want label and rail fallback", name, user)
		}
		for _, line := range strings.Split(user, "\n") {
			if ansi.StringWidth(line) != 24 {
				t.Fatalf("%s user line width = %d, want 24: %q", name, ansi.StringWidth(line), line)
			}
		}
	}
}

func TestAssistantProseMaxWidthButCompactionUsesAvailableWidth(t *testing.T) {
	m := Model{renderer: rendererFunc(func(text string, width int) (string, error) {
		return text, nil
	})}
	m.entries = []Entry{{Kind: EntryAssistant, Raw: "assistant"}, {Kind: EntryCompaction, Raw: "summary"}}
	m.renderEntryAt(0, 160)
	if m.entries[0].RenderWidth != 100 {
		t.Fatalf("assistant render width = %d, want 100", m.entries[0].RenderWidth)
	}
	m.renderEntryAt(1, 160)
	if m.entries[1].RenderWidth != 160 {
		t.Fatalf("compaction render width = %d, want 160", m.entries[1].RenderWidth)
	}
	if width := proseWidth(160); width != 100 {
		t.Fatalf("prose width = %d, want 100", width)
	}
	if ansi.StringWidth(renderToolBlock(Entry{Kind: EntryTool, ToolName: "bash", ToolArgs: strings.Repeat("x", 300)}, 160, false)) > 120 {
		t.Fatal("tool summary exceeds 120 cells")
	}
}

func TestUserBlockWrapsWideCharactersWithinAvailableWidth(t *testing.T) {
	got := renderUserBlock(strings.Repeat("界🙂", 20), 12, true)
	for _, line := range strings.Split(got, "\n") {
		if ansi.StringWidth(line) > 12 {
			t.Fatalf("user line width = %d, want <= 12: %q", ansi.StringWidth(line), line)
		}
	}
}
