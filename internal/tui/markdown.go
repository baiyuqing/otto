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
	safePlainText := escapePlainText(markdown)
	if renderer == nil {
		return fallbackMarkdown(safePlainText), errNilMarkdownRenderer
	}
	// Glamour HTML-unescapes Markdown text while rendering. Neutralize character
	// references at this boundary so they cannot recreate terminal controls after
	// the plain-text sanitizer has run.
	safeMarkdown := escapeMarkdownCharacterReferences(safePlainText)
	rendered, err := renderer.Render(safeMarkdown, width)
	if err != nil {
		return fallbackMarkdown(safePlainText), err
	}
	return strings.TrimSuffix(rendered, "\n"), nil
}

func markdownWidth(width int) int {
	if width < minimumMarkdownWidth {
		return minimumMarkdownWidth
	}
	return width
}

// fallbackMarkdown receives plain text already escaped by renderMarkdown.
// Keeping sanitization at that boundary avoids double-escaping untrusted input.
func fallbackMarkdown(safeMarkdown string) string {
	if safeMarkdown == "" {
		return markdownFallbackMarker
	}
	return safeMarkdown + "\n\n" + markdownFallbackMarker
}

func escapePlainText(text string) string {
	return escapeTextControls(text, true)
}

func escapeSingleLineText(text string) string {
	return escapeTextControls(text, false)
}

func escapeTextControls(text string, preserveMultilineWhitespace bool) string {
	var builder strings.Builder
	for _, r := range text {
		switch {
		case preserveMultilineWhitespace && (r == '\n' || r == '\t'):
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

func escapeMarkdownCharacterReferences(markdown string) string {
	var builder strings.Builder
	for index := 0; index < len(markdown); {
		if markdown[index] == '&' && startsHTMLCharacterReference(markdown[index:]) {
			builder.WriteString("&amp;")
			index++
			continue
		}
		builder.WriteByte(markdown[index])
		index++
	}
	return builder.String()
}

func startsHTMLCharacterReference(text string) bool {
	if len(text) < 2 || text[0] != '&' {
		return false
	}
	if text[1] == '#' {
		index := 2
		hexadecimal := false
		if index < len(text) && (text[index] == 'x' || text[index] == 'X') {
			hexadecimal = true
			index++
		}
		start := index
		for index < len(text) && isCharacterReferenceDigit(text[index], hexadecimal) {
			index++
		}
		return index > start
	}

	index := 1
	for index < len(text) && isASCIIAlphaNumeric(text[index]) {
		index++
	}
	if index == 1 {
		return false
	}
	if index < len(text) && text[index] == ';' {
		return true
	}
	name := text[1:index]
	return (name == "amp" || name == "AMP") && (index == len(text) || !isASCIIAlphaNumeric(text[index]))
}

func isCharacterReferenceDigit(value byte, hexadecimal bool) bool {
	if value >= '0' && value <= '9' {
		return true
	}
	return hexadecimal && ((value >= 'a' && value <= 'f') || (value >= 'A' && value <= 'F'))
}

func isASCIIAlphaNumeric(value byte) bool {
	return (value >= 'a' && value <= 'z') || (value >= 'A' && value <= 'Z') || (value >= '0' && value <= '9')
}
