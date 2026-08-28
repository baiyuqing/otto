package tool

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/model"
)

const (
	defaultGrepLimit = 100
	maximumGrepLimit = 1000
)

var errGrepLimitReached = errors.New("grep result limit reached")

type grepTool struct {
	workspace      *Workspace
	maxOutputBytes int
}

type grepArgs struct {
	Pattern    string `json:"pattern"`
	Path       string `json:"path,omitempty"`
	Glob       string `json:"glob,omitempty"`
	IgnoreCase bool   `json:"ignore_case,omitempty"`
	Limit      int    `json:"limit,omitempty"`
}

func NewGrepTool(workspace *Workspace, maxOutputBytes int) Tool {
	return &grepTool{workspace: workspace, maxOutputBytes: maxOutputBytes}
}

func (t *grepTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{
		Name:        "grep",
		Description: "Search workspace file contents with a regular expression",
		Parameters: map[string]any{
			"type":                 "object",
			"additionalProperties": false,
			"properties": map[string]any{
				"pattern":     map[string]any{"type": "string", "description": "Go RE2 regular expression"},
				"path":        map[string]any{"type": "string", "description": "Workspace-relative directory or file to search; defaults to ."},
				"glob":        map[string]any{"type": "string", "description": "Optional relative file glob; supports recursive ** segments"},
				"ignore_case": map[string]any{"type": "boolean", "description": "Match without case sensitivity; defaults to false"},
				"limit":       map[string]any{"type": "integer", "minimum": 1, "maximum": maximumGrepLimit, "description": "Maximum matching lines; defaults to 100"},
			},
			"required": []string{"pattern"},
		},
	}
}

func (t *grepTool) Execute(ctx context.Context, arguments json.RawMessage) Result {
	var args grepArgs
	if err := decodeStrictJSON(arguments, &args, "pattern"); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if args.Pattern == "" {
		return Result{Content: "pattern must not be empty", IsError: true}
	}
	pattern := args.Pattern
	if args.IgnoreCase {
		pattern = "(?i:" + pattern + ")"
	}
	expression, err := regexp.Compile(pattern)
	if err != nil {
		return Result{Content: "invalid regular expression: " + err.Error(), IsError: true}
	}
	var globSegments []string
	if args.Glob != "" {
		globSegments, err = validatedGlobSegments(args.Glob)
		if err != nil {
			return Result{Content: err.Error(), IsError: true}
		}
	}
	limit, err := resolveSearchLimit(args.Limit, defaultGrepLimit, maximumGrepLimit)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if err := ctx.Err(); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	searchPath := args.Path
	if searchPath == "" {
		searchPath = "."
	}
	root, err := t.workspace.ResolveExisting(searchPath)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}

	matches := make([]string, 0, min(limit, 128))
	truncated := false
	err = filepath.WalkDir(root, func(filePath string, entry fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		if err := ctx.Err(); err != nil {
			return err
		}
		if entry.IsDir() {
			if entry.Name() == ".git" {
				return filepath.SkipDir
			}
			return nil
		}
		if entry.Type()&os.ModeSymlink != 0 {
			return nil
		}
		info, err := entry.Info()
		if err != nil {
			return err
		}
		if !info.Mode().IsRegular() {
			return nil
		}
		candidate, err := searchRelativePath(root, filePath)
		if err != nil {
			return err
		}
		if globSegments != nil && !matchGlobSegments(globSegments, candidate) {
			return nil
		}
		text, textFile, err := readSearchTextFile(filePath)
		if err != nil {
			return err
		}
		if !textFile {
			return nil
		}
		relative, err := workspaceRelativePath(t.workspace, filePath)
		if err != nil {
			return err
		}
		for lineIndex, line := range splitLinesPreservingNewlines(text) {
			if err := ctx.Err(); err != nil {
				return err
			}
			line = strings.TrimSuffix(line, "\n")
			line = strings.TrimSuffix(line, "\r")
			if !expression.MatchString(line) {
				continue
			}
			if len(matches) >= limit {
				truncated = true
				return errGrepLimitReached
			}
			matches = append(matches, relative+":"+strconv.Itoa(lineIndex+1)+":"+line)
		}
		return nil
	})
	if err != nil && !errors.Is(err, errGrepLimitReached) {
		return Result{Content: err.Error(), IsError: true}
	}

	var output strings.Builder
	for _, match := range matches {
		output.WriteString(match)
		output.WriteByte('\n')
	}
	if truncated {
		output.WriteString("[truncated: result limit reached]\n")
	}
	return cappedTextResult(output.String(), t.maxOutputBytes)
}

func readSearchTextFile(filePath string) (string, bool, error) {
	data, err := os.ReadFile(filePath)
	if err != nil {
		return "", false, err
	}
	if bytes.IndexByte(data, 0) >= 0 || !utf8.Valid(data) {
		return "", false, nil
	}
	return string(data), true, nil
}

var _ Tool = (*grepTool)(nil)
