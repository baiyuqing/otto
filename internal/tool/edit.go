package tool

import (
	"context"
	"encoding/json"
	"fmt"
	"sort"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/model"
)

type editTool struct {
	workspace *Workspace
}

// editRequest is the wire shape of edit arguments. Pointer fields distinguish
// an absent key from an empty string so that "new_text": "" stays a valid
// deletion. Exactly one of old_text/new_text or edits must be present.
type editRequest struct {
	Path    string            `json:"path"`
	OldText *string           `json:"old_text"`
	NewText *string           `json:"new_text"`
	Edits   []editRequestItem `json:"edits"`
}

type editRequestItem struct {
	OldText *string `json:"old_text"`
	NewText *string `json:"new_text"`
}

type editReplacement struct {
	OldText string
	NewText string
}

// resolvedEdit is a replacement located in the LF-normalized file content.
// text uses LF line endings; the file's own newline style is restored on write.
type resolvedEdit struct {
	start int
	end   int
	text  string
}

func NewEditTool(workspace *Workspace) Tool {
	return &editTool{workspace: workspace}
}

func (t *editTool) Definition() model.ToolDefinition {
	oldText := map[string]any{
		"type":        "string",
		"description": "Existing text to replace; it must occur exactly once in the file",
	}
	newText := map[string]any{
		"type":        "string",
		"description": "Replacement text",
	}
	return model.ToolDefinition{
		Name: "edit",
		Description: "Replace unique text fragments in a workspace file. Pass old_text and new_text for one " +
			"replacement, or edits for several applied together against the original file. When old_text has " +
			"no exact match, a match that ignores trailing whitespace and treats curly quotes, dashes, and " +
			"non-breaking spaces as ASCII is used, and only the part of old_text that new_text changes is rewritten.",
		Parameters: map[string]any{
			"type":                 "object",
			"additionalProperties": false,
			"properties": map[string]any{
				"path": map[string]any{
					"type":        "string",
					"description": "Workspace-relative file path to edit",
				},
				"old_text": oldText,
				"new_text": newText,
				"edits": map[string]any{
					"type":        "array",
					"description": "Non-overlapping replacements, each matched against the original file",
					"items": map[string]any{
						"type":                 "object",
						"additionalProperties": false,
						"properties":           map[string]any{"old_text": oldText, "new_text": newText},
						"required":             []string{"old_text", "new_text"},
					},
				},
			},
			"required": []string{"path"},
		},
	}
}

func (t *editTool) Execute(_ context.Context, arguments json.RawMessage) Result {
	var request editRequest
	if err := DecodeStrictJSON(arguments, &request, "path"); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if request.Path == "" {
		return Result{Content: "missing required argument: path", IsError: true}
	}
	edits, err := request.replacements()
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	key, err := t.workspace.writeRelative(request.Path)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	unlock := t.workspace.lockPath(key)
	defer unlock()
	return t.executeLocked(request.Path, edits)
}

func (r editRequest) replacements() ([]editReplacement, error) {
	if r.Edits != nil && (r.OldText != nil || r.NewText != nil) {
		return nil, fmt.Errorf("invalid arguments: pass either old_text and new_text or edits, not both")
	}
	if r.Edits == nil {
		edit, err := editRequestItem{OldText: r.OldText, NewText: r.NewText}.replacement()
		if err != nil {
			return nil, err
		}
		return []editReplacement{edit}, nil
	}
	if len(r.Edits) == 0 {
		return nil, fmt.Errorf("invalid argument edits: must contain at least one replacement")
	}
	edits := make([]editReplacement, 0, len(r.Edits))
	for _, item := range r.Edits {
		edit, err := item.replacement()
		if err != nil {
			return nil, err
		}
		edits = append(edits, edit)
	}
	return edits, nil
}

func (item editRequestItem) replacement() (editReplacement, error) {
	if item.OldText == nil || *item.OldText == "" {
		return editReplacement{}, fmt.Errorf("missing required argument: old_text")
	}
	if item.NewText == nil {
		return editReplacement{}, fmt.Errorf("missing required argument: new_text")
	}
	return editReplacement{OldText: *item.OldText, NewText: *item.NewText}, nil
}

func (t *editTool) executeLocked(relPath string, edits []editReplacement) Result {
	file, err := t.workspace.Open(relPath)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	text, err := readValidatedTextFile(file, relPath)
	closeErr := file.Close()
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if closeErr != nil {
		return Result{Content: closeErr.Error(), IsError: true}
	}

	replaced, diff, err := applyTextEdits(text, relPath, edits)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}

	path, err := t.workspace.writeRelative(relPath)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if err := writeFileAtomic(t.workspace, path, []byte(replaced)); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	return Result{Content: fmt.Sprintf("edited %s\n%s", relPath, diff)}
}

// applyTextEdits returns the edited file text and a diff of the change. Every
// old_text is matched against the original content; a leading BOM and the
// file's newline style are preserved.
func applyTextEdits(text, path string, edits []editReplacement) (string, string, error) {
	body := strings.TrimPrefix(text, "\ufeff")
	hasBOM := len(body) != len(text)
	content, contentToBody := normalizeLineEndingsWithMap(body)
	newline := detectNewline(body)

	matcher := editMatcher{content: content}
	resolved := make([]resolvedEdit, 0, len(edits))
	for i, edit := range edits {
		oldText := normalizeLineEndings(edit.OldText)
		newText := normalizeLineEndings(edit.NewText)
		if strings.HasPrefix(oldText, "\ufeff") {
			// read reports the BOM as part of line 1, so models copy it into old_text.
			oldText = strings.TrimPrefix(oldText, "\ufeff")
			newText = strings.TrimPrefix(newText, "\ufeff")
		}
		match, err := matcher.resolve(oldText, newText, path, i, len(edits))
		if err != nil {
			return "", "", err
		}
		resolved = append(resolved, match)
	}

	sort.Slice(resolved, func(i, j int) bool {
		return resolved[i].start < resolved[j].start
	})
	for i := 1; i < len(resolved); i++ {
		if resolved[i].start < resolved[i-1].end {
			return "", "", fmt.Errorf("edit failed: edits overlap in %s; combine them or provide non-overlapping old_text values", path)
		}
	}

	replaced := spliceEdits(body, resolved, contentToBody, newline)
	if hasBOM {
		replaced = "\ufeff" + replaced
	}
	return replaced, editDiff(content, resolved), nil
}

// editMatcher locates old_text in LF-normalized content. The fuzzy form of the
// content is built on the first inexact lookup and reused for later edits.
type editMatcher struct {
	content      string
	fuzzy        string
	fuzzyOffsets []int
	fuzzyReady   bool
}

func (m *editMatcher) resolve(oldText, newText, path string, index, total int) (resolvedEdit, error) {
	count := strings.Count(m.content, oldText)
	if count == 1 {
		start := strings.Index(m.content, oldText)
		return resolvedEdit{start: start, end: start + len(oldText), text: newText}, nil
	}
	if count > 1 {
		return resolvedEdit{}, editMatchError(path, index, total, "old_text matched %d locations in %s; include more surrounding context to make it unique", count)
	}

	if !m.fuzzyReady {
		m.fuzzy, m.fuzzyOffsets = normalizeForFuzzyMatchWithMap(m.content)
		m.fuzzyReady = true
	}
	fuzzyOld, oldOffsets := normalizeForFuzzyMatchWithMap(oldText)
	if strings.TrimSpace(fuzzyOld) == "" {
		return resolvedEdit{}, editMatchError(path, index, total, "old_text was not found in %s")
	}
	count = strings.Count(m.fuzzy, fuzzyOld)
	if count == 0 {
		return resolvedEdit{}, editMatchError(path, index, total, "old_text was not found in %s")
	}
	if count > 1 {
		return resolvedEdit{}, editMatchError(path, index, total, "old_text matched %d locations in %s; include more surrounding context to make it unique", count)
	}
	start := strings.Index(m.fuzzy, fuzzyOld)

	// The file's bytes differ from old_text inside the match (quotes, dashes,
	// trailing whitespace). Keep them wherever new_text leaves old_text
	// unchanged and rewrite only the span between the common prefix and suffix.
	prefix, suffix := commonAffixes(oldText, newText)
	oldOffsets = oldOffsets[:len(fuzzyOld)]
	first := start + sort.SearchInts(oldOffsets, prefix)
	last := start + sort.SearchInts(oldOffsets, len(oldText)-suffix)
	contentStart := m.fuzzyOffsets[first]
	contentEnd := contentStart
	if last > first {
		lastRune := m.fuzzyOffsets[last-1]
		_, size := utf8.DecodeRuneInString(m.content[lastRune:])
		contentEnd = lastRune + size
	}
	return resolvedEdit{start: contentStart, end: contentEnd, text: newText[prefix : len(newText)-suffix]}, nil
}

// commonAffixes returns the byte lengths of the longest common prefix and
// suffix of a and b, cut at rune boundaries and never overlapping.
func commonAffixes(a, b string) (prefix, suffix int) {
	limit := min(len(a), len(b))
	for prefix < limit && a[prefix] == b[prefix] {
		prefix++
	}
	for prefix > 0 && prefix < len(a) && !utf8.RuneStart(a[prefix]) {
		prefix--
	}
	limit -= prefix
	for suffix < limit && a[len(a)-1-suffix] == b[len(b)-1-suffix] {
		suffix++
	}
	for suffix > 0 && !utf8.RuneStart(a[len(a)-suffix]) {
		suffix--
	}
	return prefix, suffix
}

func editMatchError(path string, index, total int, format string, args ...any) error {
	values := append(args, path)
	message := fmt.Sprintf(format, values...)
	if total == 1 {
		return fmt.Errorf("edit failed: %s", message)
	}
	return fmt.Errorf("edit %d failed: %s", index+1, message)
}

// spliceEdits applies sorted, non-overlapping edits in one pass. Edit offsets
// are in normalized content; offsets maps them to positions in text and may
// be nil when the two coincide.
func spliceEdits(text string, edits []resolvedEdit, offsets []int, newline string) string {
	var builder strings.Builder
	builder.Grow(len(text))
	last := 0
	for _, edit := range edits {
		builder.WriteString(text[last:mapOffset(offsets, edit.start)])
		builder.WriteString(restoreLineEndings(edit.text, newline))
		last = mapOffset(offsets, edit.end)
	}
	builder.WriteString(text[last:])
	return builder.String()
}

func mapOffset(offsets []int, i int) int {
	if offsets == nil {
		return i
	}
	return offsets[i]
}

func normalizeLineEndings(text string) string {
	text = strings.ReplaceAll(text, "\r\n", "\n")
	return strings.ReplaceAll(text, "\r", "\n")
}

// normalizeLineEndingsWithMap converts CRLF and CR to LF. The returned map is
// nil when text needs no change, meaning offsets are identical.
func normalizeLineEndingsWithMap(text string) (string, []int) {
	if !strings.Contains(text, "\r") {
		return text, nil
	}
	var builder strings.Builder
	builder.Grow(len(text))
	offsets := make([]int, 0, len(text)+1)
	for i := 0; i < len(text); {
		offsets = append(offsets, i)
		if text[i] == '\r' {
			builder.WriteByte('\n')
			if i+1 < len(text) && text[i+1] == '\n' {
				i += 2
			} else {
				i++
			}
			continue
		}
		builder.WriteByte(text[i])
		i++
	}
	offsets = append(offsets, len(text))
	return builder.String(), offsets
}

// detectNewline returns the file's line terminator. A lone CR only counts when
// the file has no LF at all, so a stray CR inside a line does not change it.
func detectNewline(text string) string {
	switch {
	case strings.Contains(text, "\r\n"):
		return "\r\n"
	case strings.Contains(text, "\n"):
		return "\n"
	case strings.Contains(text, "\r"):
		return "\r"
	}
	return "\n"
}

func restoreLineEndings(text, newline string) string {
	if newline == "\n" {
		return text
	}
	return strings.ReplaceAll(text, "\n", newline)
}

// normalizeForFuzzyMatchWithMap drops trailing whitespace on every line and
// maps typographic quotes, dashes, and spaces to ASCII. offsets[i] is the
// source offset of output byte i, with one extra entry for len(text).
func normalizeForFuzzyMatchWithMap(text string) (string, []int) {
	var builder strings.Builder
	builder.Grow(len(text))
	offsets := make([]int, 0, len(text)+1)
	for pos := 0; pos < len(text); {
		lineEnd := strings.IndexByte(text[pos:], '\n')
		end := len(text)
		if lineEnd >= 0 {
			end = pos + lineEnd
		}

		trimmedEnd := trimTrailingFuzzyWhitespace(text, pos, end)
		for i := pos; i < trimmedEnd; {
			r, size := utf8.DecodeRuneInString(text[i:trimmedEnd])
			replacement := fuzzyRune(r)
			for range len(replacement) {
				offsets = append(offsets, i)
			}
			builder.WriteString(replacement)
			i += size
		}
		if lineEnd < 0 {
			break
		}
		offsets = append(offsets, end)
		builder.WriteByte('\n')
		pos = end + 1
	}
	offsets = append(offsets, len(text))
	return builder.String(), offsets
}

func trimTrailingFuzzyWhitespace(text string, start, end int) int {
	for end > start {
		r, size := utf8.DecodeLastRuneInString(text[start:end])
		if !unicode.IsSpace(r) || r == '\n' || r == '\r' {
			break
		}
		end -= size
	}
	return end
}

func fuzzyRune(r rune) string {
	switch r {
	case '‘', '’', '‚', '‛':
		return "'"
	case '“', '”', '„', '‟':
		return `"`
	case '‐', '‑', '‒', '–', '—', '―', '−':
		return "-"
	case '\u00a0', '\u2000', '\u2001', '\u2002', '\u2003', '\u2004', '\u2005', '\u2006', '\u2007', '\u2008', '\u2009', '\u200a', '\u202f', '\u205f', '\u3000':
		return " "
	default:
		return string(r)
	}
}

const (
	diffContextLines = 3
	maxDiffBytes     = 4096
)

// diffHunk is one changed line range: oldLines[oldStart:oldEnd] becomes newLines.
type diffHunk struct {
	oldStart int
	oldEnd   int
	newLines []string
}

// editDiff renders unified-style hunks for sorted, non-overlapping edits in
// LF-normalized content. Edits touching the same lines form one hunk, and
// hunks whose context lines meet are printed together.
func editDiff(content string, edits []resolvedEdit) string {
	oldLines := strings.Split(content, "\n")
	lineStarts := make([]int, 0, len(oldLines))
	lineStarts = append(lineStarts, 0)
	for i := 0; i < len(content); i++ {
		if content[i] == '\n' {
			lineStarts = append(lineStarts, i+1)
		}
	}
	lineOf := func(offset int) int { return sort.SearchInts(lineStarts, offset+1) - 1 }
	lastLineOf := func(edit resolvedEdit) int {
		if edit.end == edit.start {
			return lineOf(edit.start)
		}
		line := lineOf(edit.end - 1)
		if content[edit.end-1] == '\n' {
			// Removing or keeping this newline decides whether the next line joins.
			line++
		}
		return line
	}

	var hunks []diffHunk
	for i := 0; i < len(edits); {
		first := lineOf(edits[i].start)
		last := lastLineOf(edits[i])
		j := i + 1
		for j < len(edits) && lineOf(edits[j].start) <= last {
			last = max(last, lastLineOf(edits[j]))
			j++
		}
		regionStart := lineStarts[first]
		regionEnd := len(content)
		if last+1 < len(lineStarts) {
			regionEnd = lineStarts[last+1] - 1
		}
		region := make([]resolvedEdit, 0, j-i)
		for _, edit := range edits[i:j] {
			region = append(region, resolvedEdit{start: edit.start - regionStart, end: edit.end - regionStart, text: edit.text})
		}
		before := oldLines[first : last+1]
		after := strings.Split(spliceEdits(content[regionStart:regionEnd], region, nil, "\n"), "\n")
		prefix, suffix := commonLines(before, after)
		if prefix+suffix < len(before) || prefix+suffix < len(after) {
			hunks = append(hunks, diffHunk{oldStart: first + prefix, oldEnd: last + 1 - suffix, newLines: after[prefix : len(after)-suffix]})
		}
		i = j
	}
	if len(hunks) == 0 {
		return "(no textual changes)"
	}

	var builder strings.Builder
	delta := 0
	for g := 0; g < len(hunks); {
		contextStart := max(hunks[g].oldStart-diffContextLines, 0)
		contextEnd := min(hunks[g].oldEnd+diffContextLines, len(oldLines))
		h := g + 1
		for h < len(hunks) && hunks[h].oldStart-diffContextLines <= contextEnd {
			contextEnd = min(hunks[h].oldEnd+diffContextLines, len(oldLines))
			h++
		}
		oldCount := contextEnd - contextStart
		newCount := oldCount
		for _, hunk := range hunks[g:h] {
			newCount += len(hunk.newLines) - (hunk.oldEnd - hunk.oldStart)
		}
		fmt.Fprintf(&builder, "@@ -%d,%d +%d,%d @@\n", contextStart+1, oldCount, contextStart+1+delta, newCount)
		cursor := contextStart
		for _, hunk := range hunks[g:h] {
			writeDiffLines(&builder, " ", oldLines[cursor:hunk.oldStart])
			writeDiffLines(&builder, "-", oldLines[hunk.oldStart:hunk.oldEnd])
			writeDiffLines(&builder, "+", hunk.newLines)
			cursor = hunk.oldEnd
		}
		writeDiffLines(&builder, " ", oldLines[cursor:contextEnd])
		delta += newCount - oldCount
		g = h
	}

	diff := strings.TrimSuffix(builder.String(), "\n")
	if len(diff) > maxDiffBytes {
		cut := strings.LastIndexByte(diff[:maxDiffBytes], '\n')
		if cut < 0 {
			cut = maxDiffBytes
		}
		diff = diff[:cut] + "\n... (diff truncated)"
	}
	return diff
}

func writeDiffLines(builder *strings.Builder, marker string, lines []string) {
	for _, line := range lines {
		builder.WriteString(marker)
		builder.WriteString(line)
		builder.WriteByte('\n')
	}
}

func commonLines(a, b []string) (prefix, suffix int) {
	limit := min(len(a), len(b))
	for prefix < limit && a[prefix] == b[prefix] {
		prefix++
	}
	limit -= prefix
	for suffix < limit && a[len(a)-1-suffix] == b[len(b)-1-suffix] {
		suffix++
	}
	return prefix, suffix
}
