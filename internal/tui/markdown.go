package tui

import (
	"errors"
	"fmt"
	"strings"
	"unicode"

	glamour "charm.land/glamour/v2"
	glamourstyles "charm.land/glamour/v2/styles"
)

const (
	markdownFallbackMarker = "[markdown rendering unavailable]"
	minimumMarkdownWidth   = 20
)

var errNilMarkdownRenderer = errors.New("markdown renderer is nil")

type MarkdownRenderer interface {
	Render(markdown string, width int) (string, error)
}

type GlamourRenderer struct {
	styleName string
}

var _ MarkdownRenderer = GlamourRenderer{}

func newGlamourRenderer(darkBackground bool) GlamourRenderer {
	styleName := glamourstyles.LightStyle
	if darkBackground {
		styleName = glamourstyles.DarkStyle
	}
	return GlamourRenderer{styleName: styleName}
}

func (g GlamourRenderer) Render(markdown string, width int) (string, error) {
	styleName := g.styleName
	if styleName == "" {
		// Keep standalone zero-value use deterministic and free of terminal I/O.
		styleName = glamourstyles.DarkStyle
	}
	renderer, err := glamour.NewTermRenderer(
		glamour.WithStandardStyle(styleName),
		glamour.WithWordWrap(markdownWidth(width)),
	)
	if err != nil {
		return "", err
	}
	return renderer.Render(markdown)
}

func renderMarkdown(renderer MarkdownRenderer, markdown string, width int) (string, error) {
	safeMarkdown := escapePlainText(markdown)
	if renderer == nil {
		return fallbackMarkdown(safeMarkdown), errNilMarkdownRenderer
	}
	rendered, err := renderer.Render(safeMarkdown, width)
	if err != nil {
		return fallbackMarkdown(safeMarkdown), err
	}
	return strings.TrimSuffix(rendered, "\n"), nil
}

func markdownWidth(width int) int {
	if width < minimumMarkdownWidth {
		return minimumMarkdownWidth
	}
	return width
}

// fallbackMarkdown receives text already escaped by renderMarkdown. Keeping the
// sanitization at that boundary avoids escaping the same untrusted input twice.
func fallbackMarkdown(safeMarkdown string) string {
	if safeMarkdown == "" {
		return markdownFallbackMarker
	}
	return safeMarkdown + "\n\n" + markdownFallbackMarker
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
