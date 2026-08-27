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
	got, err := (GlamourRenderer{}).Render("**hello**", 40)
	if err != nil {
		t.Fatalf("Render() error = %v", err)
	}
	if !strings.Contains(got, "hello") {
		t.Fatalf("Render() = %q, want rendered content", got)
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
