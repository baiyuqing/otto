package session

import (
	"context"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"
	"unicode"

	"github.com/baiyuqing/otto/internal/model"
)

const (
	maxListSessions        = 20
	maxSessionPreviewRunes = 120
	maxSkippedSessionCount = int(^uint(0) >> 1)
)

type listCandidate struct {
	path     string
	modified time.Time
}

func Inspect(ctx context.Context, path string) (SessionInfo, []Warning, error) {
	if err := ctx.Err(); err != nil {
		return SessionInfo{}, nil, err
	}

	file, err := os.Open(path)
	if err != nil {
		return SessionInfo{}, nil, fmt.Errorf("open session file: %w", err)
	}
	defer file.Close()

	info, err := file.Stat()
	if err != nil {
		return SessionInfo{}, nil, fmt.Errorf("stat session file: %w", err)
	}
	if !info.Mode().IsRegular() {
		return SessionInfo{}, nil, fmt.Errorf("%w: session file is not a regular file", ErrInvalidSession)
	}
	if err := rejectOversizedSessionFile(file); err != nil {
		return SessionInfo{}, nil, err
	}

	decoded, err := decodePiFileReadOnlyContext(ctx, file)
	if err != nil {
		return SessionInfo{}, nil, err
	}
	if err := ctx.Err(); err != nil {
		return SessionInfo{}, nil, err
	}

	createdAt, err := validatePiHeader(decoded.Header)
	if err != nil {
		return SessionInfo{}, nil, err
	}
	leafID := ""
	if len(decoded.Entries) > 0 {
		leafID = decoded.Entries[len(decoded.Entries)-1].ID
	}
	resolved, warnings, err := buildContext(decoded.Entries, leafID)
	if err != nil {
		return SessionInfo{}, nil, err
	}
	preview := previewText(lastUserText(resolved.Messages))
	name := previewText(resolved.SessionName)
	if name == "" {
		name = preview
	}

	return SessionInfo{
		Path:         path,
		ID:           decoded.Header.ID,
		CWD:          decoded.Header.CWD,
		Name:         name,
		Created:      createdAt,
		Modified:     info.ModTime(),
		MessageCount: len(resolved.Messages),
		LastUserText: preview,
		Profile:      resolved.Runtime.Profile,
		Provider:     resolved.Runtime.Provider,
		Model:        resolved.Runtime.Model,
	}, warnings, nil
}

func List(ctx context.Context, root, workspace, currentPath string, limit int) (ListResult, error) {
	if limit < 1 || limit > maxListSessions {
		return ListResult{}, fmt.Errorf("list limit must be between 1 and %d", maxListSessions)
	}
	if err := ctx.Err(); err != nil {
		return ListResult{}, err
	}

	workspaceCanonical, err := canonicalWorkspace(workspace)
	if err != nil {
		return ListResult{}, err
	}
	directory, err := sessionDirectory(root, workspaceCanonical)
	if err != nil {
		return ListResult{}, err
	}
	entries, err := os.ReadDir(directory)
	if err != nil {
		if os.IsNotExist(err) {
			return ListResult{}, nil
		}
		return ListResult{}, fmt.Errorf("read session directory: %w", err)
	}

	currentCanonical := ""
	if currentPath != "" {
		currentCanonical, err = canonicalWorkspace(currentPath)
		if err != nil {
			return ListResult{}, err
		}
	}

	candidates := make([]listCandidate, 0, len(entries))
	result := ListResult{}
	for _, entry := range entries {
		if err := ctx.Err(); err != nil {
			return ListResult{}, err
		}
		if filepath.Ext(entry.Name()) != ".jsonl" {
			continue
		}
		path := filepath.Join(directory, entry.Name())
		info, err := os.Lstat(path)
		if err != nil {
			result.Skipped = incrementSkipped(result.Skipped)
			continue
		}
		if info.Mode()&os.ModeSymlink != 0 || !info.Mode().IsRegular() {
			result.Skipped = incrementSkipped(result.Skipped)
			continue
		}
		candidates = append(candidates, listCandidate{path: path, modified: info.ModTime()})
	}

	sort.Slice(candidates, func(i, j int) bool {
		if candidates[i].modified.Equal(candidates[j].modified) {
			return candidates[i].path > candidates[j].path
		}
		return candidates[i].modified.After(candidates[j].modified)
	})

	result.Sessions = make([]SessionInfo, 0, min(limit, len(candidates)))
	for _, candidate := range candidates {
		if err := ctx.Err(); err != nil {
			return ListResult{}, err
		}
		info, _, err := Inspect(ctx, candidate.path)
		if err != nil {
			result.Skipped = incrementSkipped(result.Skipped)
			continue
		}
		candidateWorkspace, err := canonicalWorkspace(info.CWD)
		if err != nil || candidateWorkspace != workspaceCanonical {
			result.Skipped = incrementSkipped(result.Skipped)
			continue
		}
		info.Modified = candidate.modified
		if currentCanonical != "" {
			candidateCanonical, err := canonicalWorkspace(candidate.path)
			if err != nil {
				result.Skipped = incrementSkipped(result.Skipped)
				continue
			}
			info.Current = candidateCanonical == currentCanonical
		}
		result.Sessions = append(result.Sessions, info)
		if len(result.Sessions) == limit {
			break
		}
	}
	return result, nil
}

func sessionDirectory(root, workspace string) (string, error) {
	key, err := workspaceKey(workspace)
	if err != nil {
		return "", err
	}
	return filepath.Join(root, key), nil
}

func decodePiFileReadOnlyContext(ctx context.Context, file *os.File) (piFile, error) {
	if err := ctx.Err(); err != nil {
		return piFile{}, err
	}
	finalStart, _, incomplete, err := finalPiRecordState(file)
	if err != nil {
		return piFile{}, err
	}
	if err := ctx.Err(); err != nil {
		return piFile{}, err
	}
	if incomplete && finalStart > 0 {
		decoded, _, err := decodePiFile(&contextReader{ctx: ctx, reader: io.NewSectionReader(file, 0, finalStart)})
		return decoded, err
	}
	if _, err := file.Seek(0, io.SeekStart); err != nil {
		return piFile{}, fmt.Errorf("seek session file: %w", err)
	}
	decoded, _, err := decodePiFile(&contextReader{ctx: ctx, reader: file})
	return decoded, err
}

type contextReader struct {
	ctx    context.Context
	reader io.Reader
}

func (r *contextReader) Read(buffer []byte) (int, error) {
	if err := r.ctx.Err(); err != nil {
		return 0, err
	}
	count, err := r.reader.Read(buffer)
	if err == nil && r.ctx.Err() != nil {
		return count, r.ctx.Err()
	}
	return count, err
}

func lastUserText(messages []model.Message) string {
	for index := len(messages) - 1; index >= 0; index-- {
		if messages[index].Role == model.RoleUser {
			return messages[index].Text()
		}
	}
	return ""
}

func previewText(text string) string {
	text = strings.TrimSpace(text)
	if text == "" {
		return ""
	}
	var builder strings.Builder
	builder.Grow(len(text))
	lastSpace := false
	for _, r := range text {
		switch {
		case unicode.IsSpace(r):
			if builder.Len() > 0 && !lastSpace {
				builder.WriteByte(' ')
				lastSpace = true
			}
		case r < 0x100 && unicode.IsControl(r):
			builder.WriteString(fmt.Sprintf("\\x%02x", r))
			lastSpace = false
		case unicode.IsControl(r):
			builder.WriteString(fmt.Sprintf("\\u%04x", r))
			lastSpace = false
		default:
			builder.WriteRune(r)
			lastSpace = false
		}
	}
	preview := strings.TrimSpace(builder.String())
	if preview == "" {
		return ""
	}
	count := 0
	for index := range preview {
		if count == maxSessionPreviewRunes {
			return preview[:index] + "..."
		}
		count++
	}
	return preview
}

func incrementSkipped(count int) int {
	if count < maxSkippedSessionCount {
		return count + 1
	}
	return count
}
