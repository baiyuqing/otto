package tui

import (
	"strings"
	"testing"

	"github.com/charmbracelet/x/ansi"
)

func TestToolSummaryUsesLeadingStatesAndDecodedBashCommand(t *testing.T) {
	entry := Entry{Kind: EntryTool, ToolName: "bash", ToolArgs: `{"command":"git status --short"}`}
	if got := strings.TrimRight(ansi.Strip(renderToolBlock(entry, 80, false)), " "); got != "… bash  git status --short" {
		t.Fatalf("running summary = %q", got)
	}
	entry.ToolDone = true
	if got := strings.TrimRight(ansi.Strip(renderToolBlock(entry, 80, false)), " "); got != "✓ bash  git status --short" {
		t.Fatalf("success summary = %q", got)
	}
	entry.ToolError = true
	if got := strings.TrimRight(ansi.Strip(renderToolBlock(entry, 80, false)), " "); got != "✗ bash  git status --short" {
		t.Fatalf("error summary = %q", got)
	}

	styled := renderToolBlock(Entry{Kind: EntryTool, ToolName: "bash", ToolArgs: `{"command":"ls"}`, ToolDone: true}, 80, false)
	if styled == ansi.Strip(styled) {
		t.Fatal("tool summary should contain ANSI styling")
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
	stripped := ansi.Strip(got)
	if !strings.Contains(stripped, "  ✓ read") || strings.Contains(stripped, "> read") {
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

func TestToolArgumentPreviewExtractsHumanReadableSummary(t *testing.T) {
	tests := []struct {
		name string
		tool string
		args string
		want string
	}{
		{"grep shows pattern and glob", "grep", `{"pattern":"TODO","path":"src","glob":"**/*.go"}`, "TODO src **/*.go"},
		{"grep omits default path", "grep", `{"pattern":"TODO","path":"."}`, "TODO"},
		{"read shows path", "read", `{"path":"internal/tui/model.go"}`, "internal/tui/model.go"},
		{"write shows path", "write", `{"path":"foo.go","content":"package foo"}`, "foo.go"},
		{"ls shows path", "ls", `{"path":"internal/tui"}`, "internal/tui"},
		{"find shows pattern and dir", "find", `{"pattern":"*.go","path":"internal"}`, "*.go in internal"},
		{"find omits default path", "find", `{"pattern":"*.go","path":"."}`, "*.go"},
		{"edit shows path and old_string prefix", "edit", `{"path":"a.go","old_string":"func main() {\n\tfmt.Println(\"hello world long string here\")","new_string":"x"}`, `a.go func main() { fmt.Println("hello world l… → …`},
		{"memory_search shows query", "memory_search", `{"query":"deployment config"}`, "deployment config"},
		{"bash unchanged", "bash", `{"command":"go test ./..."}`, "go test ./..."},
		{"unknown tool falls back to raw", "custom_tool", `{"x":1}`, `{"x":1}`},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := toolArgumentPreview(tt.tool, tt.args)
			if got != tt.want {
				t.Fatalf("toolArgumentPreview(%q, ...) = %q, want %q", tt.tool, got, tt.want)
			}
		})
	}
}

func TestToolSummaryWithoutPreviewPadsToWidth(t *testing.T) {
	got := ansi.Strip(renderToolBlock(Entry{Kind: EntryTool, ToolName: "read", ToolDone: true}, 30, false))
	if strings.TrimRight(got, " ") != "✓ read" {
		t.Fatalf("summary = %q, want '✓ read' (possibly padded)", got)
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
