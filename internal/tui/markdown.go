package tui

import (
	"errors"
	"fmt"
	"os"
	"strings"
	"unicode"

	glamour "charm.land/glamour/v2"
	glamourstyles "charm.land/glamour/v2/styles"
	"charm.land/lipgloss/v2"
)

const (
	markdownFallbackMarker = "[markdown rendering unavailable]"
	minimumMarkdownWidth   = 20
)

var errNilMarkdownRenderer = errors.New("markdown renderer is nil")

type MarkdownRenderer interface {
	Render(markdown string, width int) (string, error)
}

type GlamourRenderer struct{}

var _ MarkdownRenderer = GlamourRenderer{}

func (GlamourRenderer) Render(markdown string, width int) (string, error) {
	renderer, err := glamour.NewTermRenderer(
		glamour.WithStandardStyle(glamourStyleName()),
		glamour.WithWordWrap(markdownWidth(width)),
	)
	if err != nil {
		return "", err
	}
	return renderer.Render(markdown)
}

func renderMarkdown(renderer MarkdownRenderer, markdown string, width int) (string, error) {
	if renderer == nil {
		return fallbackMarkdown(markdown), errNilMarkdownRenderer
	}
	rendered, err := renderer.Render(markdown, width)
	if err != nil {
		return fallbackMarkdown(markdown), err
	}
	return strings.TrimSuffix(rendered, "\n"), nil
}

func glamourStyleName() string {
	if lipgloss.HasDarkBackground(os.Stdin, os.Stdout) {
		return glamourstyles.DarkStyle
	}
	return glamourstyles.LightStyle
}

func markdownWidth(width int) int {
	if width < minimumMarkdownWidth {
		return minimumMarkdownWidth
	}
	return width
}

func fallbackMarkdown(markdown string) string {
	escaped := escapePlainText(markdown)
	if escaped == "" {
		return markdownFallbackMarker
	}
	return escaped + "\n\n" + markdownFallbackMarker
}

func escapePlainText(markdown string) string {
	var builder strings.Builder
	for _, r := range markdown {
		switch {
		case r == '\n' || r == '\t':
			builder.WriteRune(r)
		case r < 0x100 && unicode.IsControl(r):
			builder.WriteString(fmt.Sprintf("\\x%02x", r))
		case unicode.IsControl(r):
			builder.WriteString(fmt.Sprintf("\\u%04x", r))
		default:
			builder.WriteRune(r)
		}
	}
	return builder.String()
}
