package tool

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"strings"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/model"
)

type readTool struct {
	workspace      *Workspace
	maxOutputBytes int
}

type readArgs struct {
	Path   string `json:"path"`
	Offset int    `json:"offset,omitempty"`
	Limit  int    `json:"limit,omitempty"`
}

func NewReadTool(workspace *Workspace, maxOutputBytes int) Tool {
	return &readTool{workspace: workspace, maxOutputBytes: maxOutputBytes}
}

func (t *readTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{
		Name:        "read",
		Description: "Read a UTF-8 text file from the workspace",
		Parameters: map[string]any{
			"type":                 "object",
			"additionalProperties": false,
			"properties": map[string]any{
				"path": map[string]any{
					"type":        "string",
					"description": "Workspace-relative file path to read",
				},
				"offset": map[string]any{
					"type":        "integer",
					"description": "One-based starting line number",
				},
				"limit": map[string]any{
					"type":        "integer",
					"description": "Maximum number of lines to return; 0 means all remaining lines",
				},
			},
			"required": []string{"path"},
		},
	}
}

func (t *readTool) Execute(_ context.Context, arguments json.RawMessage) Result {
	var args readArgs
	if err := DecodeStrictJSON(arguments, &args, "path"); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if args.Path == "" {
		return Result{Content: "missing required argument: path", IsError: true}
	}
	if args.Offset < 0 {
		return Result{Content: "offset must be >= 0", IsError: true}
	}
	if args.Limit < 0 {
		return Result{Content: "limit must be >= 0", IsError: true}
	}

	path, err := t.workspace.ResolveExisting(args.Path)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	text, err := readValidatedTextFile(path)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}

	startLine := args.Offset
	if startLine == 0 {
		startLine = 1
	}
	content := selectLines(text, startLine, args.Limit)
	collector := newCappedByteCollector(t.maxOutputBytes)
	_, _ = io.WriteString(collector, content)
	if collector.Discarded() == 0 {
		return Result{Content: content}
	}

	raw := collector.Bytes()
	safe := validUTF8Prefix(raw)
	omitted := collector.Discarded() + len(raw) - len(safe)
	if len(safe) == 0 {
		return Result{Content: fmt.Sprintf("[truncated: %d bytes omitted]", omitted)}
	}
	return Result{Content: fmt.Sprintf("%s\n[truncated: %d bytes omitted]", string(safe), omitted)}
}

func validUTF8Prefix(data []byte) []byte {
	if utf8.Valid(data) {
		return append([]byte(nil), data...)
	}
	prefix := append([]byte(nil), data...)
	for len(prefix) > 0 && !utf8.Valid(prefix) {
		prefix = prefix[:len(prefix)-1]
	}
	return prefix
}

// DecodeStrictJSON decodes tool arguments, rejecting unknown fields, trailing
// tokens, and missing required keys.
// DecodeStrictJSON decodes tool arguments, rejecting unknown fields, trailing
// tokens, and missing required keys.
func DecodeStrictJSON(arguments json.RawMessage, destination any, required ...string) error {
	decoder := json.NewDecoder(bytes.NewReader(arguments))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(destination); err != nil {
		if strings.Contains(err.Error(), "unknown field") {
			return err
		}
		return fmt.Errorf("invalid JSON: %w", err)
	}
	if _, err := decoder.Token(); err != io.EOF {
		if err != nil {
			return fmt.Errorf("invalid JSON: %w", err)
		}
		return fmt.Errorf("trailing JSON tokens after arguments")
	}

	if len(required) == 0 {
		return nil
	}
	var provided map[string]json.RawMessage
	if err := json.Unmarshal(arguments, &provided); err != nil {
		return fmt.Errorf("invalid JSON: %w", err)
	}
	for _, key := range required {
		if _, ok := provided[key]; !ok {
			return fmt.Errorf("missing required argument: %s", key)
		}
	}
	return nil
}

const maxReadFileBytes = 64 << 20

func readValidatedTextFile(path string) (string, error) {
	info, err := os.Stat(path)
	if err != nil {
		return "", err
	}
	if info.Size() > maxReadFileBytes {
		return "", fmt.Errorf("file is too large (%d bytes); maximum readable size is %d bytes", info.Size(), maxReadFileBytes)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return "", err
	}
	if bytes.IndexByte(data, 0) >= 0 {
		return "", fmt.Errorf("binary file not supported: %s", path)
	}
	if !utf8.Valid(data) {
		return "", fmt.Errorf("file is not valid UTF-8: %s", path)
	}
	return string(data), nil
}

func selectLines(text string, offset, limit int) string {
	lines := splitLinesPreservingNewlines(text)
	if offset <= 0 {
		offset = 1
	}
	start := offset - 1
	if start >= len(lines) {
		return ""
	}
	end := len(lines)
	if limit > 0 && start+limit < end {
		end = start + limit
	}
	return strings.Join(lines[start:end], "")
}

func splitLinesPreservingNewlines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.SplitAfter(text, "\n")
	if lines[len(lines)-1] == "" {
		lines = lines[:len(lines)-1]
	}
	return lines
}
