package skill

import (
	"errors"
	"fmt"
	"regexp"
	"strings"
)

// frontmatterKeyPattern matches a top-level frontmatter key.
var frontmatterKeyPattern = regexp.MustCompile(`^[A-Za-z0-9_-]+$`)

// blockScalarPattern matches a block-scalar value indicator: "|" or ">",
// optionally followed by a chomping indicator ("-" or "+"), which is
// accepted but has no effect on the parsed value.
var blockScalarPattern = regexp.MustCompile(`^[|>][-+]?$`)

// parseFrontmatter splits data into its YAML-subset frontmatter fields and
// the Markdown body that follows. See the package doc for the supported
// subset. Any construct outside that subset returns an error rather than
// guessing.
func parseFrontmatter(data []byte) (map[string]string, string, error) {
	lines := strings.Split(string(data), "\n")
	if len(lines) == 0 || !isFrontmatterDelimiter(lines[0]) {
		return nil, "", errors.New("missing frontmatter")
	}

	end := -1
	for i := 1; i < len(lines); i++ {
		if isFrontmatterDelimiter(lines[i]) {
			end = i
			break
		}
	}
	if end == -1 {
		return nil, "", errors.New("unterminated frontmatter")
	}

	fields, err := parseFrontmatterFields(lines[1:end])
	if err != nil {
		return nil, "", err
	}

	body := strings.Join(lines[end+1:], "\n")
	body = strings.TrimPrefix(body, "\n")
	return fields, body, nil
}

func isFrontmatterDelimiter(line string) bool {
	return strings.TrimSuffix(line, "\r") == "---"
}

// parseFrontmatterFields parses the lines strictly between the opening and
// closing "---" delimiters. lines[i] is file line i+2 (1-based).
func parseFrontmatterFields(lines []string) (map[string]string, error) {
	fields := make(map[string]string)
	i := 0
	for i < len(lines) {
		line := lines[i]
		trimmed := strings.TrimSpace(line)
		lineNo := i + 2

		switch {
		case trimmed == "":
			i++
			continue
		case trimmed[0] == '#':
			i++
			continue
		case len(line) > 0 && (line[0] == ' ' || line[0] == '\t'):
			return nil, fmt.Errorf("unsupported frontmatter line %d", lineNo)
		}

		key, rest, ok := splitFrontmatterKey(line)
		if !ok {
			return nil, fmt.Errorf("unsupported frontmatter line %d", lineNo)
		}

		value, consumed, err := parseFrontmatterValue(rest, lines, i+1, lineNo)
		if err != nil {
			return nil, err
		}
		fields[key] = value
		i += 1 + consumed
	}
	return fields, nil
}

// splitFrontmatterKey splits a "key: value" line into its key and the
// (possibly empty) remainder after the required separator. ok is false when
// the line does not match the key grammar.
func splitFrontmatterKey(line string) (key, rest string, ok bool) {
	idx := strings.IndexByte(line, ':')
	if idx < 0 {
		return "", "", false
	}
	key = line[:idx]
	if !frontmatterKeyPattern.MatchString(key) {
		return "", "", false
	}
	after := line[idx+1:]
	if after != "" && !strings.HasPrefix(after, " ") {
		return "", "", false
	}
	return key, strings.TrimPrefix(after, " "), true
}

// parseFrontmatterValue parses the value that follows "key:" on a line,
// consuming any continuation lines it owns. lines[start:] are the lines
// after the key line; lineNo is the key line's 1-based file line number,
// used only for error messages.
func parseFrontmatterValue(rest string, lines []string, start, lineNo int) (value string, consumed int, err error) {
	switch {
	case rest == "":
		_, consumed := collectIndentedBlock(lines, start)
		return "", consumed, nil
	case blockScalarPattern.MatchString(rest):
		raw, consumed := collectIndentedBlock(lines, start)
		if rest[0] == '|' {
			return literalBlockContent(raw), consumed, nil
		}
		return foldedBlockContent(raw), consumed, nil
	case rest[0] == '"':
		value, err := parseDoubleQuoted(rest)
		if err != nil {
			return "", 0, fmt.Errorf("frontmatter line %d: %w", lineNo, err)
		}
		return value, 0, nil
	case rest[0] == '\'':
		value, err := parseSingleQuoted(rest)
		if err != nil {
			return "", 0, fmt.Errorf("frontmatter line %d: %w", lineNo, err)
		}
		return value, 0, nil
	default:
		continuation, consumed := collectPlainContinuation(lines, start)
		parts := append([]string{strings.TrimSpace(rest)}, continuation...)
		return strings.Join(parts, " "), consumed, nil
	}
}

// collectIndentedBlock collects the raw lines starting at lines[start] that
// belong to a nested block or block scalar: blank lines and lines indented
// with leading space/tab. It stops at the first blank-or-indented run ends,
// i.e. the first non-blank line at column 0, or end of input.
func collectIndentedBlock(lines []string, start int) (collected []string, consumed int) {
	i := start
	for i < len(lines) {
		line := lines[i]
		if strings.TrimSpace(line) == "" {
			collected = append(collected, "")
			i++
			continue
		}
		if line[0] == ' ' || line[0] == '\t' {
			collected = append(collected, line)
			i++
			continue
		}
		break
	}
	return collected, i - start
}

// collectPlainContinuation collects continuation lines for a multi-line
// plain scalar: only non-blank lines indented with leading space/tab. A
// blank line ends the scalar without being consumed.
func collectPlainContinuation(lines []string, start int) (collected []string, consumed int) {
	i := start
	for i < len(lines) {
		line := lines[i]
		if strings.TrimSpace(line) == "" {
			break
		}
		if line[0] == ' ' || line[0] == '\t' {
			collected = append(collected, strings.TrimSpace(line))
			i++
			continue
		}
		break
	}
	return collected, i - start
}

// stripBlockIndent removes the leading whitespace of the first non-blank
// line from every line, per the YAML rule that a block scalar's indentation
// is set by its first content line.
func stripBlockIndent(lines []string) []string {
	indent := -1
	for _, line := range lines {
		if strings.TrimSpace(line) == "" {
			continue
		}
		indent = len(line) - len(strings.TrimLeft(line, " \t"))
		break
	}
	if indent < 0 {
		return lines
	}
	out := make([]string, len(lines))
	for i, line := range lines {
		if len(line) >= indent {
			out[i] = line[indent:]
		} else {
			out[i] = strings.TrimLeft(line, " \t")
		}
	}
	return out
}

func trimTrailingBlank(lines []string) []string {
	end := len(lines)
	for end > 0 && strings.TrimSpace(lines[end-1]) == "" {
		end--
	}
	return lines[:end]
}

func literalBlockContent(raw []string) string {
	lines := trimTrailingBlank(stripBlockIndent(raw))
	if len(lines) == 0 {
		return ""
	}
	return strings.Join(lines, "\n") + "\n"
}

func foldedBlockContent(raw []string) string {
	lines := trimTrailingBlank(stripBlockIndent(raw))
	if len(lines) == 0 {
		return ""
	}
	var b strings.Builder
	prevBlank := false
	for i, line := range lines {
		if line == "" {
			b.WriteString("\n")
			prevBlank = true
			continue
		}
		if i > 0 && !prevBlank {
			b.WriteString(" ")
		}
		b.WriteString(line)
		prevBlank = false
	}
	b.WriteString("\n")
	return b.String()
}

func parseDoubleQuoted(s string) (string, error) {
	var b strings.Builder
	for i := 1; i < len(s); i++ {
		c := s[i]
		if c == '"' {
			return b.String(), nil
		}
		if c == '\\' && i+1 < len(s) {
			i++
			switch s[i] {
			case '"':
				b.WriteByte('"')
			case '\\':
				b.WriteByte('\\')
			case 'n':
				b.WriteByte('\n')
			case 't':
				b.WriteByte('\t')
			default:
				b.WriteByte(s[i])
			}
			continue
		}
		b.WriteByte(c)
	}
	return "", errors.New("unterminated double-quoted value")
}

func parseSingleQuoted(s string) (string, error) {
	var b strings.Builder
	for i := 1; i < len(s); i++ {
		c := s[i]
		if c == '\'' {
			if i+1 < len(s) && s[i+1] == '\'' {
				b.WriteByte('\'')
				i++
				continue
			}
			return b.String(), nil
		}
		b.WriteByte(c)
	}
	return "", errors.New("unterminated single-quoted value")
}
