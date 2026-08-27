package tui

import (
	"errors"
	"strings"
	"testing"
	"unicode/utf8"
)

func TestMarkdownStripsOnlyTrailingLayoutNewline(t *testing.T) {
	renderer := &stubMarkdownRenderer{rendered: "hello\n\n"}
	got, err := renderMarkdown(renderer, "**hello**", 18)
	if err != nil {
		t.Fatalf("renderMarkdown() error = %v", err)
	}
	if got != "hello\n" {
		t.Fatalf("renderMarkdown() = %q, want %q", got, "hello\n")
	}
	if renderer.markdown != "**hello**" || renderer.width != 18 {
		t.Fatalf("renderer saw markdown=%q width=%d", renderer.markdown, renderer.width)
	}
}

func TestMarkdownFallsBackToSafePlainTextOnFailure(t *testing.T) {
	renderer := &stubMarkdownRenderer{err: errors.New("boom")}
	got, err := renderMarkdown(renderer, "hello\x1b[31m\n\tworld", 24)
	if !errors.Is(err, renderer.err) {
		t.Fatalf("renderMarkdown() error = %v, want %v", err, renderer.err)
	}
	if strings.Contains(got, "\x1b[31m") {
		t.Fatalf("fallback should escape terminal control sequences: %q", got)
	}
	if !strings.Contains(got, `hello\x1b[31m`) {
		t.Fatalf("fallback = %q, want escaped plain text", got)
	}
	if !strings.Contains(got, markdownFallbackMarker) {
		t.Fatalf("fallback = %q, want marker %q", got, markdownFallbackMarker)
	}
}

func TestMarkdownEscapesUntrustedControlsBeforeRenderer(t *testing.T) {
	renderer := &stubMarkdownRenderer{rendered: "ignored"}
	input := "line\x1b]52;c;owned\a\u009b31m\x7f\n\tnext"
	_, err := renderMarkdown(renderer, input, 40)
	if err != nil {
		t.Fatal(err)
	}
	for _, forbidden := range []string{"\x1b", "\a", "\u009b", "\x7f"} {
		if strings.Contains(renderer.markdown, forbidden) {
			t.Fatalf("renderer input contains raw control %q: %q", forbidden, renderer.markdown)
		}
	}
	if !strings.Contains(renderer.markdown, `\x1b]52;c;owned\x07\x9b31m\x7f`) {
		t.Fatalf("renderer input = %q, want escaped OSC/CSI payload", renderer.markdown)
	}
	if !strings.Contains(renderer.markdown, "\n\tnext") {
		t.Fatalf("renderer input = %q, want newline and tab preserved", renderer.markdown)
	}
}

func TestMarkdownFallbackDoesNotDoubleEscapeSanitizedInput(t *testing.T) {
	renderer := &stubMarkdownRenderer{err: errors.New("boom")}
	got, err := renderMarkdown(renderer, "entity &#27; and bad\x1b[31m", 40)
	if !errors.Is(err, renderer.err) {
		t.Fatalf("error = %v, want %v", err, renderer.err)
	}
	if strings.Count(got, `\x1b`) != 1 || strings.Contains(got, "\x1b") {
		t.Fatalf("fallback = %q, want one escaped control representation", got)
	}
	if !strings.Contains(got, "entity &#27;") || strings.Contains(got, "&amp;#27;") {
		t.Fatalf("fallback = %q, want original safe entity text", got)
	}
}

func TestMarkdownFallsBackWhenRendererIsNil(t *testing.T) {
	got, err := renderMarkdown(nil, "plain", 12)
	if err == nil {
		t.Fatal("renderMarkdown() error = nil, want nonfatal fallback error")
	}
	if !strings.Contains(got, "plain") || !strings.Contains(got, markdownFallbackMarker) {
		t.Fatalf("renderMarkdown() = %q, want plain fallback with marker", got)
	}
}

func TestMarkdownGlamourRendererRendersMarkdown(t *testing.T) {
	got, err := newGlamourRenderer(true).Render("**hello**", 40)
	if err != nil {
		t.Fatalf("Render() error = %v", err)
	}
	if !strings.Contains(got, "hello") {
		t.Fatalf("Render() = %q, want rendered content", got)
	}
}

func TestTerminalOutputFilterPreservesOnlySafeFormatting(t *testing.T) {
	tests := []struct {
		name  string
		input string
		want  string
	}{
		{name: "plain whitespace", input: "plain\n\ttext", want: "plain\n\ttext"},
		{name: "SGR", input: "\x1b[31;1mred\x1b[m \x1b[38:5:252mgray\x1b[0m", want: "\x1b[31;1mred\x1b[m \x1b[38:5:252mgray\x1b[0m"},
		{name: "OSC BEL", input: "before\x1b]52;c;owned\aafter", want: "beforeafter"},
		{name: "OSC ST", input: "before\x1b]52;c;owned\x1b\\after", want: "beforeafter"},
		{name: "OSC multiline whitespace", input: "before\x1b]52;c;hidden\n\tmore\aafter", want: "before\n\tafter"},
		{name: "C1 OSC ST", input: "before\u009d52;c;owned\u009cafter", want: "beforeafter"},
		{name: "raw C1 OSC ST", input: "before" + string([]byte{0x9d}) + "52;c;owned" + string([]byte{0x9c}) + "after", want: "beforeafter"},
		{name: "DCS", input: "before\x1bP1;2|owned\x1b\\after", want: "beforeafter"},
		{name: "SOS", input: "before\x1bXowned\aafter", want: "beforeafter"},
		{name: "PM", input: "before\x1b^owned\u009cafter", want: "beforeafter"},
		{name: "C1 APC", input: "before\u009fowned\x1b\\after", want: "beforeafter"},
		{name: "non-SGR CSI", input: "before\x1b[2Jafter\x1b[?25l", want: "beforeafter"},
		{name: "8-bit CSI SGR", input: "before\u009b31mred", want: "beforered"},
		{name: "ESC family", input: "before\x1bcafter", want: "beforeafter"},
		{name: "truncated OSC", input: "before\x1b]52;c;owned", want: "before"},
		{name: "truncated CSI", input: "before\x1b[31", want: "before"},
		{name: "standalone controls", input: "a\ab\rc\x7fd\u0085e", want: `a\x07b\x0dc\x7fd\x85e`},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if got := filterTerminalOutput(test.input); got != test.want {
				t.Fatalf("filterTerminalOutput() = %q, want %q", got, test.want)
			}
		})
	}

	overlong := "before\x1b[" + strings.Repeat("1", 256) + "mafter"
	if got := filterTerminalOutput(overlong); got != "beforeafter" {
		t.Fatalf("overlong SGR survived filtering: %q", got)
	}
}

func TestMarkdownFiltersControlsSynthesizedByFormatting(t *testing.T) {
	input := strings.Join([]string{
		"**safe bold**",
		"direct \x1b]52;c;direct\a and \u009d52;c;c1-direct\u009c",
		"decimal &#27;]52;c;decimal&#7;",
		"hex &#x1b;]52;c;hex&#x07;",
		"C1 CSI &#155;31mdecimal and &#x9b;31mhex",
		"C1 OSC &#157;52;c;c1-entity&#156;",
		"~~&**#27;**]52;c;owned&**#7;**~~",
	}, "\n")

	got, err := renderMarkdown(newGlamourRenderer(true), input, 100)
	if err != nil {
		t.Fatalf("renderMarkdown() error = %v", err)
	}
	if !strings.Contains(got, "safe bold") {
		t.Fatalf("rendered markdown lost ordinary content: %q", got)
	}
	assertOnlySGRControls(t, got)
}

func TestMarkdownPreservesEntityCodeAndLinkSemantics(t *testing.T) {
	const link = "https://example.com/docs?a=1&copy;=2"
	input := "Copyright &copy;. Code `&copy;` and `&#27;`. **bold** [docs](" + link + ")"
	got, err := renderMarkdown(newGlamourRenderer(true), input, 100)
	if err != nil {
		t.Fatalf("renderMarkdown() error = %v", err)
	}
	if strings.Count(got, "©") < 2 || !strings.Contains(got, `\x1b`) {
		t.Fatalf("rendered entities/code changed semantics: %q", got)
	}
	if !strings.Contains(got, "bold") || !strings.Contains(got, "docs") || !strings.Contains(got, link) {
		t.Fatalf("rendered markdown/link is not useful: %q", got)
	}
	if !strings.Contains(got, "\x1b[") {
		t.Fatalf("Glamour SGR formatting was blanket-stripped: %q", got)
	}
	if strings.Contains(got, "\x1b]8;") {
		t.Fatalf("Glamour OSC 8 hyperlink survived filtering: %q", got)
	}
	assertOnlySGRControls(t, got)
}

func assertOnlySGRControls(t *testing.T, rendered string) {
	t.Helper()
	for index := 0; index < len(rendered); {
		value := rendered[index]
		if value == '\x1b' {
			end := index + 2
			if end > len(rendered) || rendered[index+1] != '[' {
				t.Fatalf("non-CSI ESC at byte %d: %q", index, rendered)
			}
			for end < len(rendered) && ((rendered[end] >= '0' && rendered[end] <= '9') || rendered[end] == ';' || rendered[end] == ':') {
				end++
			}
			if end >= len(rendered) || rendered[end] != 'm' {
				t.Fatalf("non-SGR CSI at byte %d: %q", index, rendered)
			}
			index = end + 1
			continue
		}
		if value < 0x20 || value == 0x7f {
			if value != '\n' && value != '\t' {
				t.Fatalf("C0 control 0x%02x at byte %d: %q", value, index, rendered)
			}
		}
		if value >= utf8.RuneSelf {
			_, size := utf8.DecodeRuneInString(rendered[index:])
			index += size
			continue
		}
		index++
	}
	if !utf8.ValidString(rendered) {
		t.Fatalf("invalid UTF-8 survived filtering: %q", rendered)
	}
	for _, value := range rendered {
		if value >= 0x80 && value <= 0x9f {
			t.Fatalf("C1 control %U: %q", value, rendered)
		}
	}
}

func TestGlamourRendererUsesCachedExplicitBackgroundStyle(t *testing.T) {
	dark := newGlamourRenderer(true)
	light := newGlamourRenderer(false)
	if dark.styleName != "dark" || light.styleName != "light" {
		t.Fatalf("style names = %q/%q, want dark/light", dark.styleName, light.styleName)
	}
	for name, renderer := range map[string]GlamourRenderer{"dark": dark, "light": light} {
		got, err := renderer.Render("plain text", 40)
		if err != nil || got == "" {
			t.Fatalf("%s Render() = %q, %v", name, got, err)
		}
	}
}

type stubMarkdownRenderer struct {
	rendered string
	err      error
	markdown string
	width    int
}

func (s *stubMarkdownRenderer) Render(markdown string, width int) (string, error) {
	s.markdown = markdown
	s.width = width
	if s.err != nil {
		return "", s.err
	}
	return s.rendered, nil
}
