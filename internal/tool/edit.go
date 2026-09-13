package tool

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"sort"
	"strings"
	"sync"
	"unicode"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/model"
)

type editTool struct {
	workspace *Workspace
}

type editArgs struct {
	Path  string
	Edits []editReplacement
}

type editReplacement struct {
	OldText string
	NewText string
}

type resolvedEdit struct {
	start int
	end   int
	text  string
}

func NewEditTool(workspace *Workspace) Tool {
	return &editTool{workspace: workspace}
}

func (t *editTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{
		Name:        "edit",
		Description: "Replace exactly one matching text fragment in a workspace file",
		Parameters: map[string]any{
			"type":                 "object",
			"additionalProperties": false,
			"properties": map[string]any{
				"path": map[string]any{
					"type":        "string",
					"description": "Workspace-relative file path to edit",
				},
				"old_text": map[string]any{
					"type":        "string",
					"description": "Exact existing text to replace",
				},
				"new_text": map[string]any{
					"type":        "string",
					"description": "Replacement text, kept for compatibility",
				},
				"oldText": map[string]any{
					"type":        "string",
					"description": "Exact existing text to replace, kept for compatibility",
				},
				"newText": map[string]any{
					"type":        "string",
					"description": "Replacement text, kept for compatibility",
				},
				"edits": map[string]any{
					"description": "One edit object, an array of edit objects, or a JSON string containing either shape",
				},
			},
			"required": []string{"path"},
		},
	}
}

func (t *editTool) Execute(_ context.Context, arguments json.RawMessage) Result {
	args, err := prepareEditArguments(arguments)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	key, err := t.workspace.ResolveExisting(args.Path)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	return withFileMutationQueue(key, func() Result {
		return t.executeLocked(args)
	})
}

func (t *editTool) executeLocked(args editArgs) Result {
	file, err := t.workspace.Open(args.Path)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	text, err := readValidatedTextFile(file, args.Path)
	closeErr := file.Close()
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if closeErr != nil {
		return Result{Content: closeErr.Error(), IsError: true}
	}

	replaced, err := applyTextEdits(text, args.Path, args.Edits)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}

	path, err := t.workspace.writeRelative(args.Path)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if err := writeFileAtomic(t.workspace, path, []byte(replaced)); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	return Result{Content: fmt.Sprintf("edited %s\n%s", args.Path, editDiff(displayEditText(text), displayEditText(replaced)))}
}

func prepareEditArguments(arguments json.RawMessage) (editArgs, error) {
	var raw map[string]json.RawMessage
	decoder := json.NewDecoder(bytes.NewReader(arguments))
	if err := decoder.Decode(&raw); err != nil {
		return editArgs{}, fmt.Errorf("invalid JSON: %w", err)
	}
	if err := decoder.Decode(&struct{}{}); err != io.EOF {
		return editArgs{}, fmt.Errorf("trailing JSON tokens after arguments")
	}

	allowed := map[string]bool{
		"path": true, "old_text": true, "new_text": true, "oldText": true, "newText": true, "edits": true,
	}
	for key := range raw {
		if !allowed[key] {
			return editArgs{}, fmt.Errorf("json: unknown field %q", key)
		}
	}

	path, ok, err := readStringField(raw, "path")
	if err != nil {
		return editArgs{}, err
	}
	if !ok || path == "" {
		return editArgs{}, fmt.Errorf("missing required argument: path")
	}

	if editsRaw, ok := raw["edits"]; ok {
		edits, err := parseEditList(editsRaw)
		if err != nil {
			return editArgs{}, err
		}
		return editArgs{Path: path, Edits: edits}, nil
	}

	edit, err := parseEditObject(raw, true)
	if err != nil {
		return editArgs{}, err
	}
	return editArgs{Path: path, Edits: []editReplacement{edit}}, nil
}

func parseEditList(raw json.RawMessage) ([]editReplacement, error) {
	var encoded string
	if err := json.Unmarshal(raw, &encoded); err == nil {
		raw = json.RawMessage(encoded)
	}

	trimmed := bytes.TrimSpace(raw)
	if len(trimmed) == 0 {
		return nil, fmt.Errorf("missing required argument: edits")
	}
	if trimmed[0] == '{' {
		var object map[string]json.RawMessage
		if err := json.Unmarshal(trimmed, &object); err != nil {
			return nil, fmt.Errorf("invalid JSON: %w", err)
		}
		edit, err := parseEditObject(object, false)
		if err != nil {
			return nil, err
		}
		return []editReplacement{edit}, nil
	}

	var objects []map[string]json.RawMessage
	if err := json.Unmarshal(trimmed, &objects); err != nil {
		return nil, fmt.Errorf("invalid JSON: %w", err)
	}
	if len(objects) == 0 {
		return nil, fmt.Errorf("missing required argument: edits")
	}
	edits := make([]editReplacement, 0, len(objects))
	for _, object := range objects {
		edit, err := parseEditObject(object, false)
		if err != nil {
			return nil, err
		}
		edits = append(edits, edit)
	}
	return edits, nil
}

func parseEditObject(raw map[string]json.RawMessage, topLevel bool) (editReplacement, error) {
	allowed := map[string]bool{"old_text": true, "new_text": true, "oldText": true, "newText": true}
	if topLevel {
		allowed["path"] = true
	}
	for key := range raw {
		if !allowed[key] {
			return editReplacement{}, fmt.Errorf("json: unknown field %q", key)
		}
	}

	oldText, ok, err := readStringField(raw, "old_text", "oldText")
	if err != nil {
		return editReplacement{}, err
	}
	if !ok || oldText == "" {
		return editReplacement{}, fmt.Errorf("missing required argument: old_text")
	}
	newText, ok, err := readStringField(raw, "new_text", "newText")
	if err != nil {
		return editReplacement{}, err
	}
	if !ok {
		return editReplacement{}, fmt.Errorf("missing required argument: new_text")
	}
	return editReplacement{OldText: oldText, NewText: newText}, nil
}

func readStringField(raw map[string]json.RawMessage, names ...string) (string, bool, error) {
	for _, name := range names {
		value, ok := raw[name]
		if !ok {
			continue
		}
		var text string
		if err := json.Unmarshal(value, &text); err != nil {
			return "", false, fmt.Errorf("invalid argument %s: must be a string", name)
		}
		return text, true, nil
	}
	return "", false, nil
}

func applyTextEdits(text, path string, edits []editReplacement) (string, error) {
	body := strings.TrimPrefix(text, "\ufeff")
	hasBOM := len(body) != len(text)
	content, contentToBody := normalizeLineEndingsWithMap(body)
	newline := detectNewline(body)

	resolved := make([]resolvedEdit, 0, len(edits))
	for i, edit := range edits {
		oldText := normalizeLineEndings(edit.OldText)
		newText := restoreLineEndings(normalizeLineEndings(edit.NewText), newline)
		start, end, err := findUniqueEditMatch(content, oldText, path, i, len(edits))
		if err != nil {
			return "", err
		}
		resolved = append(resolved, resolvedEdit{
			start: contentToBody[start],
			end:   contentToBody[end],
			text:  newText,
		})
	}

	sort.Slice(resolved, func(i, j int) bool {
		return resolved[i].start < resolved[j].start
	})
	for i := 1; i < len(resolved); i++ {
		if resolved[i].start < resolved[i-1].end {
			return "", fmt.Errorf("edit failed: edits overlap in %s; combine them or provide non-overlapping old_text values", path)
		}
	}

	replaced := applyResolvedEdits(body, resolved)
	if hasBOM {
		replaced = "\ufeff" + replaced
	}
	return replaced, nil
}

func findUniqueEditMatch(content, oldText, path string, index, total int) (int, int, error) {
	count := strings.Count(content, oldText)
	if count == 1 {
		start := strings.Index(content, oldText)
		return start, start + len(oldText), nil
	}
	if count > 1 {
		return 0, 0, editMatchError(path, index, total, "old_text matched %d locations in %s; include more surrounding context to make it unique", count)
	}

	fuzzyContent, fuzzyToContent := normalizeForFuzzyMatchWithMap(content)
	fuzzyOldText, _ := normalizeForFuzzyMatchWithMap(oldText)
	if fuzzyOldText == "" {
		return 0, 0, editMatchError(path, index, total, "old_text was not found in %s")
	}
	count = strings.Count(fuzzyContent, fuzzyOldText)
	if count == 0 {
		return 0, 0, editMatchError(path, index, total, "old_text was not found in %s")
	}
	if count > 1 {
		return 0, 0, editMatchError(path, index, total, "old_text matched %d locations in %s; include more surrounding context to make it unique", count)
	}
	start := strings.Index(fuzzyContent, fuzzyOldText)
	return fuzzyToContent[start], fuzzyToContent[start+len(fuzzyOldText)], nil
}

func editMatchError(path string, index, total int, format string, args ...any) error {
	values := append(args, path)
	message := fmt.Sprintf(format, values...)
	if total == 1 {
		return fmt.Errorf("edit failed: %s", message)
	}
	return fmt.Errorf("edit %d failed: %s", index+1, message)
}

func applyResolvedEdits(text string, edits []resolvedEdit) string {
	for i := len(edits) - 1; i >= 0; i-- {
		edit := edits[i]
		text = text[:edit.start] + edit.text + text[edit.end:]
	}
	return text
}

func normalizeLineEndings(text string) string {
	text = strings.ReplaceAll(text, "\r\n", "\n")
	return strings.ReplaceAll(text, "\r", "\n")
}

func normalizeLineEndingsWithMap(text string) (string, []int) {
	var builder strings.Builder
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

func detectNewline(text string) string {
	if strings.Contains(text, "\r\n") {
		return "\r\n"
	}
	if strings.Contains(text, "\r") {
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

func displayEditText(text string) string {
	return normalizeLineEndings(strings.TrimPrefix(text, "\ufeff"))
}

func normalizeForFuzzyMatchWithMap(text string) (string, []int) {
	var builder strings.Builder
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

var fileMutationQueues = struct {
	sync.Mutex
	locks map[string]*sync.Mutex
}{locks: map[string]*sync.Mutex{}}

func withFileMutationQueue(path string, fn func() Result) Result {
	// ponytail: locks live for the process lifetime; add ref-count cleanup if edit churn matters.
	fileMutationQueues.Lock()
	lock := fileMutationQueues.locks[path]
	if lock == nil {
		lock = &sync.Mutex{}
		fileMutationQueues.locks[path] = lock
	}
	fileMutationQueues.Unlock()

	lock.Lock()
	defer lock.Unlock()
	return fn()
}

const (
	diffContextLines = 3
	maxDiffBytes     = 4096
)

// editDiff renders a unified-style hunk for a single-match replacement.
// The edit is one contiguous region, so trimming common prefix and suffix
// lines between the two file versions yields the exact changed range.
func editDiff(before, after string) string {
	if before == after {
		return "(no textual changes)"
	}
	oldLines := strings.Split(before, "\n")
	newLines := strings.Split(after, "\n")

	prefix := 0
	for prefix < len(oldLines) && prefix < len(newLines) && oldLines[prefix] == newLines[prefix] {
		prefix++
	}
	suffix := 0
	for suffix < len(oldLines)-prefix && suffix < len(newLines)-prefix &&
		oldLines[len(oldLines)-1-suffix] == newLines[len(newLines)-1-suffix] {
		suffix++
	}

	contextStart := prefix - diffContextLines
	if contextStart < 0 {
		contextStart = 0
	}
	contextEnd := len(oldLines) - suffix + diffContextLines
	if contextEnd > len(oldLines) {
		contextEnd = len(oldLines)
	}

	oldCount := contextEnd - contextStart
	newCount := oldCount - (len(oldLines) - suffix - prefix) + (len(newLines) - suffix - prefix)

	var builder strings.Builder
	fmt.Fprintf(&builder, "@@ -%d,%d +%d,%d @@\n", contextStart+1, oldCount, contextStart+1, newCount)
	for _, line := range oldLines[contextStart:prefix] {
		builder.WriteString(" " + line + "\n")
	}
	for _, line := range oldLines[prefix : len(oldLines)-suffix] {
		builder.WriteString("-" + line + "\n")
	}
	for _, line := range newLines[prefix : len(newLines)-suffix] {
		builder.WriteString("+" + line + "\n")
	}
	for _, line := range oldLines[len(oldLines)-suffix : contextEnd] {
		builder.WriteString(" " + line + "\n")
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
