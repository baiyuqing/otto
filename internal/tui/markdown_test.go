package tui

import (
	"errors"
	"strings"
	"testing"
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

func TestMarkdownNeutralizesCharacterReferencesBeforeActualGlamour(t *testing.T) {
	input := strings.Join([]string{
		"**safe bold**",
		"direct \x1b]52;c;direct\a and \u009d52;c;c1-direct\u009c",
		"decimal &#27;]52;c;decimal&#7;",
		"hex &#x1b;]52;c;hex&#x07;",
		"uppercase hex &#X1B;]52;c;upper&#X07;",
		"C1 CSI &#155;31mdecimal and &#x9b;31mhex",
		"C1 OSC &#157;52;c;c1-entity&#156;",
		"nested &amp;#27;]52;c;nested&amp;#7;",
		"double nested &amp;amp;#x1b;]52;c;double&amp;amp;#x07;",
	}, "\n")

	got, err := renderMarkdown(newGlamourRenderer(true), input, 100)
	if err != nil {
		t.Fatalf("renderMarkdown() error = %v", err)
	}
	if !strings.Contains(got, "safe bold") {
		t.Fatalf("rendered markdown lost ordinary content: %q", got)
	}
	if strings.ContainsRune(got, '\a') {
		t.Fatalf("actual Glamour output regenerated BEL: %q", got)
	}
	for _, value := range got {
		if value >= 0x80 && value <= 0x9f {
			t.Fatalf("actual Glamour output regenerated C1 control %U: %q", value, got)
		}
	}
	for index := 0; index < len(got); index++ {
		if got[index] == 0x1b && (index+1 >= len(got) || got[index+1] != '[') {
			t.Fatalf("actual Glamour output contains non-CSI attacker ESC at byte %d: %q", index, got)
		}
	}
}

func TestMarkdownCharacterReferenceBoundaryPreservesFormattingAndLinks(t *testing.T) {
	const link = "https://example.com/docs?a=1&b=2"
	got, err := renderMarkdown(newGlamourRenderer(true), "**bold** [docs]("+link+")", 80)
	if err != nil {
		t.Fatalf("renderMarkdown() error = %v", err)
	}
	if !strings.Contains(got, "bold") || !strings.Contains(got, "docs") || !strings.Contains(got, link) {
		t.Fatalf("rendered markdown/link is not useful: %q", got)
	}
	if !strings.Contains(got, "\x1b[") || !strings.Contains(got, "\x1b]8;") {
		t.Fatalf("Glamour formatting or hyperlink ANSI was blanket-stripped: %q", got)
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
