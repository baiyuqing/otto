package tui

import (
	"errors"
	"fmt"
	"strings"
	"unicode"
	"unicode/utf8"

	glamour "charm.land/glamour/v2"
	glamourstyles "charm.land/glamour/v2/styles"
)

const (
	markdownFallbackMarker      = "[markdown rendering unavailable]"
	minimumMarkdownWidth        = 20
	maximumSGRParameterBytes    = 64
	maximumTerminalStringBytes  = 4096
	safeTerminalFormattingReset = "\x1b[0m"
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
	rendered, err := renderer.Render(safePlainText, width)
	if err != nil {
		return fallbackMarkdown(safePlainText), err
	}
	// Goldmark and Glamour can decode character references after the direct-input
	// sanitizer runs. Trust only the SGR sequences Glamour uses for visual style.
	return filterTerminalOutput(strings.TrimSuffix(rendered, "\n")), nil
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

func filterTerminalOutput(output string) string {
	var builder strings.Builder
	builder.Grow(len(output))
	retainedSGR := false
	for index := 0; index < len(output); {
		value := output[index]
		switch {
		case value == 0x1b:
			end, safeSGR := scanEscapeSequence(output, index)
			if safeSGR {
				builder.WriteString(output[index:end])
				retainedSGR = true
			} else if end == index+1 {
				writeEscapedTerminalControl(&builder, rune(value))
			} else {
				writePreservedTerminalWhitespace(&builder, output[index:end])
			}
			index = end
		case value == '\n' || value == '\t':
			builder.WriteByte(value)
			index++
		case value < 0x20 || value == 0x7f:
			writeEscapedTerminalControl(&builder, rune(value))
			index++
		case value < utf8.RuneSelf:
			builder.WriteByte(value)
			index++
		default:
			if value >= 0x80 && value <= 0x9f {
				if isTerminalStringIntroducer(rune(value)) {
					end := scanTerminalString(output, index+1, value == 0x9d)
					writePreservedTerminalWhitespace(&builder, output[index:end])
					index = end
					continue
				}
				if value == 0x9b {
					index, _ = scanCSISequence(output, index+1, false)
					continue
				}
				writeEscapedTerminalControl(&builder, rune(value))
				index++
				continue
			}
			r, size := utf8.DecodeRuneInString(output[index:])
			if r == utf8.RuneError && size == 1 {
				builder.WriteRune(utf8.RuneError)
				index++
				continue
			}
			if r >= 0x80 && r <= 0x9f {
				if isTerminalStringIntroducer(r) {
					end := scanTerminalString(output, index+size, r == 0x9d)
					writePreservedTerminalWhitespace(&builder, output[index:end])
					index = end
					continue
				}
				if r == 0x9b {
					index, _ = scanCSISequence(output, index+size, false)
					continue
				}
				writeEscapedTerminalControl(&builder, r)
				index += size
				continue
			}
			builder.WriteString(output[index : index+size])
			index += size
		}
	}
	if retainedSGR {
		builder.WriteString(safeTerminalFormattingReset)
	}
	return builder.String()
}

func scanEscapeSequence(output string, start int) (int, bool) {
	if start+1 >= len(output) {
		return start + 1, false
	}
	switch output[start+1] {
	case '[':
		return scanCSISequence(output, start+2, true)
	case ']':
		return scanTerminalString(output, start+2, true), false
	case 'P', 'X', '^', '_':
		return scanTerminalString(output, start+2, false), false
	}

	next := output[start+1]
	if next >= 0x30 && next <= 0x7e {
		return start + 2, false
	}
	if next < 0x20 || next >= 0x80 {
		return start + 1, false
	}
	index := start + 1
	for index < len(output) && output[index] >= 0x20 && output[index] <= 0x2f {
		index++
	}
	if index < len(output) && output[index] >= 0x30 && output[index] <= 0x7e {
		return index + 1, false
	}
	if index == len(output) {
		return index, false
	}
	return start + 1, false
}

func scanCSISequence(output string, bodyStart int, allowSGR bool) (int, bool) {
	validSGR := allowSGR
	for index := bodyStart; index < len(output); index++ {
		value := output[index]
		if value >= 0x40 && value <= 0x7e {
			return index + 1, validSGR && value == 'm' && index-bodyStart <= maximumSGRParameterBytes
		}
		if !((value >= '0' && value <= '9') || value == ';' || value == ':') {
			validSGR = false
		}
		if value < 0x20 || value >= 0x80 {
			return index, false
		}
	}
	return len(output), false
}

func scanTerminalString(output string, bodyStart int, allowBEL bool) int {
	limit := min(len(output), bodyStart+maximumTerminalStringBytes)
	for index := bodyStart; index < limit; index++ {
		switch output[index] {
		case 0x07:
			if allowBEL {
				return index + 1
			}
		case 0x9c:
			return index + 1
		case 0x18, 0x1a:
			return index + 1
		case 0x1b:
			if index+1 < limit && output[index+1] == '\\' {
				return index + 2
			}
			// A nested ESC that is not ST marks the outer string as malformed.
			// Resume at the ESC so the normal filter can safely process it.
			return index
		case 0xc2:
			if index+1 < limit && output[index+1] == 0x9c {
				return index + 2
			}
		}
	}
	return limit
}

func isTerminalStringIntroducer(r rune) bool {
	switch r {
	case 0x90, 0x98, 0x9d, 0x9e, 0x9f:
		return true
	default:
		return false
	}
}

func writePreservedTerminalWhitespace(builder *strings.Builder, sequence string) {
	for index := 0; index < len(sequence); index++ {
		if sequence[index] == '\n' || sequence[index] == '\t' {
			builder.WriteByte(sequence[index])
		}
	}
}

func writeEscapedTerminalControl(builder *strings.Builder, value rune) {
	if value < 0x100 {
		_, _ = fmt.Fprintf(builder, "\\x%02x", value)
		return
	}
	_, _ = fmt.Fprintf(builder, "\\u%04x", value)
}
