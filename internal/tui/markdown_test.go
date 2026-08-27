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
	got, err := renderMarkdown(renderer, "bad\x1b[31m", 40)
	if !errors.Is(err, renderer.err) {
		t.Fatalf("error = %v, want %v", err, renderer.err)
	}
	if strings.Count(got, `\x1b`) != 1 || strings.Contains(got, "\x1b") {
		t.Fatalf("fallback = %q, want one escaped control representation", got)
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
