package tool

import (
	"fmt"
	"io"
	"path"
	"path/filepath"
	"strings"
)

func matchRecursiveGlob(pattern, name string) (bool, error) {
	segments, err := validatedGlobSegments(pattern)
	if err != nil {
		return false, err
	}
	return matchGlobSegments(segments, name), nil
}

func matchGlobSegments(segments []string, name string) bool {
	name = strings.TrimPrefix(name, "./")
	nameSegments := strings.Split(name, "/")
	type state struct{ pattern, name int }
	memo := make(map[state]bool)
	seen := make(map[state]bool)
	var match func(int, int) bool
	match = func(patternIndex, nameIndex int) bool {
		current := state{pattern: patternIndex, name: nameIndex}
		if seen[current] {
			return memo[current]
		}
		seen[current] = true

		var matched bool
		switch {
		case patternIndex == len(segments):
			matched = nameIndex == len(nameSegments)
		case segments[patternIndex] == "**":
			matched = match(patternIndex+1, nameIndex) || nameIndex < len(nameSegments) && match(patternIndex, nameIndex+1)
		case nameIndex < len(nameSegments):
			segmentMatched, _ := path.Match(segments[patternIndex], nameSegments[nameIndex])
			matched = segmentMatched && match(patternIndex+1, nameIndex+1)
		}
		memo[current] = matched
		return matched
	}
	return match(0, 0)
}

func cappedTextResult(content string, maxOutputBytes int) Result {
	collector := newCappedByteCollector(maxOutputBytes)
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

func cappedCollectorResult(collector *cappedByteCollector, marker string) Result {
	safe := validUTF8Prefix(collector.Bytes())
	content := string(safe)
	if marker != "" {
		if content != "" && !strings.HasSuffix(content, "\n") {
			content += "\n"
		}
		content += marker + "\n"
	}
	return Result{Content: content}
}

func workspaceRelativePath(workspace *Workspace, filePath string) (string, error) {
	relative, err := filepath.Rel(workspace.root, filePath)
	if err != nil {
		return "", err
	}
	if relative == ".." || strings.HasPrefix(relative, ".."+string(filepath.Separator)) {
		return "", fmt.Errorf("path escapes workspace: %s", filePath)
	}
	return filepath.ToSlash(relative), nil
}

func searchRelativePath(root, filePath string) (string, error) {
	relative, err := filepath.Rel(root, filePath)
	if err != nil {
		return "", err
	}
	if relative == "." {
		return filepath.Base(filePath), nil
	}
	return filepath.ToSlash(relative), nil
}

func resolveSearchLimit(value *int, defaultValue, maximum int) (int, error) {
	if value == nil {
		return defaultValue, nil
	}
	if *value < 1 {
		return 0, fmt.Errorf("limit must be >= 1")
	}
	if *value > maximum {
		return 0, fmt.Errorf("limit must be <= %d", maximum)
	}
	return *value, nil
}

func searchRootInsideGit(workspace *Workspace, requestedPath, resolvedRoot string) (bool, error) {
	resolvedRelative, err := workspaceRelativePath(workspace, resolvedRoot)
	if err != nil {
		return false, err
	}
	if pathHasGitSegment(resolvedRelative) {
		return true, nil
	}
	requestedCandidate := requestedPath
	if !filepath.IsAbs(requestedCandidate) {
		requestedCandidate = filepath.Join(workspace.lexicalRoot, requestedCandidate)
	}
	requestedRelative, err := filepath.Rel(workspace.lexicalRoot, filepath.Clean(requestedCandidate))
	if err != nil {
		return false, err
	}
	if requestedRelative == ".." || strings.HasPrefix(requestedRelative, ".."+string(filepath.Separator)) {
		return false, nil
	}
	return pathHasGitSegment(filepath.ToSlash(requestedRelative)), nil
}

func pathHasGitSegment(relative string) bool {
	for _, segment := range strings.Split(filepath.ToSlash(relative), "/") {
		if segment == ".git" {
			return true
		}
	}
	return false
}

func validatedGlobSegments(pattern string) ([]string, error) {
	if pattern == "" {
		return nil, fmt.Errorf("pattern must not be empty")
	}
	if strings.HasPrefix(pattern, "/") {
		return nil, fmt.Errorf("pattern must be relative")
	}
	segments := strings.Split(pattern, "/")
	for _, segment := range segments {
		if segment == "" || segment == ".." {
			return nil, fmt.Errorf("invalid glob pattern %q", pattern)
		}
		if segment == "**" {
			continue
		}
		if _, err := path.Match(segment, ""); err != nil {
			return nil, fmt.Errorf("invalid glob pattern %q: %w", pattern, err)
		}
	}
	return segments, nil
}
