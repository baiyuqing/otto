package tool

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/model"
)

const (
	defaultGrepLimit     = 100
	maximumGrepLimit     = 1000
	maximumGrepLineBytes = 1 << 20
)

var errGrepTruncated = errors.New("grep results truncated")

type grepTool struct {
	workspace      *Workspace
	maxOutputBytes int
}

type grepArgs struct {
	Pattern    string `json:"pattern"`
	Path       string `json:"path,omitempty"`
	Glob       string `json:"glob,omitempty"`
	IgnoreCase bool   `json:"ignore_case,omitempty"`
	Limit      *int   `json:"limit,omitempty"`
}

func NewGrepTool(workspace *Workspace, maxOutputBytes int) Tool {
	return &grepTool{workspace: workspace, maxOutputBytes: maxOutputBytes}
}

func (t *grepTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{
		Name:        "grep",
		Description: "Search workspace file contents with a regular expression (read-only)",
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
	insideGit, err := searchRootInsideGit(t.workspace, searchPath, root)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if insideGit {
		return Result{}
	}

	collector := newCappedByteCollector(t.maxOutputBytes)
	matchCount := 0
	truncationMarker := ""
	err = filepath.WalkDir(root, func(filePath string, entry fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		if err := ctx.Err(); err != nil {
			return err
		}
		if filePath != root && entry.Name() == ".git" {
			if entry.IsDir() {
				return filepath.SkipDir
			}
			return nil
		}
		if entry.IsDir() {
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
		file, err := os.Open(filePath)
		if err != nil {
			return err
		}
		remainingBytes := max(0, t.maxOutputBytes-len(collector.Bytes()))
		scan, scanErr := scanGrepReader(ctx, file, expression, limit-matchCount, remainingBytes)
		closeErr := file.Close()
		if scanErr != nil {
			return scanErr
		}
		if closeErr != nil {
			return closeErr
		}
		if !scan.textFile {
			return nil
		}
		relative, err := workspaceRelativePath(t.workspace, filePath)
		if err != nil {
			return err
		}
		for _, match := range scan.matches {
			_, _ = collector.Write([]byte(relative + ":" + strconv.Itoa(match.number) + ":" + match.text + "\n"))
			matchCount++
		}
		switch {
		case scan.matchOverflow:
			truncationMarker = "[truncated: result limit reached]"
			return errGrepTruncated
		case scan.byteOverflow || collector.Discarded() > 0:
			truncationMarker = "[truncated: output limit reached]"
			return errGrepTruncated
		default:
			return nil
		}
	})
	if err != nil && !errors.Is(err, errGrepTruncated) {
		return Result{Content: err.Error(), IsError: true}
	}
	return cappedCollectorResult(collector, truncationMarker)
}

type grepLine struct {
	number int
	text   string
}

type grepScanResult struct {
	matches       []grepLine
	textFile      bool
	matchOverflow bool
	byteOverflow  bool
}

func scanGrepReader(ctx context.Context, source io.Reader, expression *regexp.Regexp, maxMatches, maxBytes int) (grepScanResult, error) {
	reader := bufio.NewReaderSize(source, 64<<10)
	result := grepScanResult{textFile: true, matches: make([]grepLine, 0, min(max(maxMatches, 0), 32))}
	lineNumber := 0
	collectedBytes := 0
	line := make([]byte, 0, 64<<10)
	for {
		if err := ctx.Err(); err != nil {
			return grepScanResult{}, err
		}
		fragment, more, err := reader.ReadLine()
		if err != nil {
			if errors.Is(err, io.EOF) {
				return result, nil
			}
			return grepScanResult{}, err
		}
		if err := ctx.Err(); err != nil {
			return grepScanResult{}, err
		}
		if len(line)+len(fragment) > maximumGrepLineBytes {
			return grepScanResult{}, nil
		}
		line = append(line, fragment...)
		if more {
			continue
		}
		lineNumber++
		if bytes.IndexByte(line, 0) >= 0 || !utf8.Valid(line) {
			return grepScanResult{}, nil
		}
		if expression.Match(line) {
			switch {
			case len(result.matches) >= maxMatches:
				result.matchOverflow = true
			case collectedBytes+len(line) > maxBytes:
				result.byteOverflow = true
			default:
				text := string(append([]byte(nil), line...))
				result.matches = append(result.matches, grepLine{number: lineNumber, text: text})
				collectedBytes += len(line)
			}
		}
		line = line[:0]
	}
}

var _ Tool = (*grepTool)(nil)
