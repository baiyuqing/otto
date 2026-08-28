package session

import (
	"context"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"unicode"

	"github.com/baiyuqing/otto/internal/model"
	"golang.org/x/sys/unix"
)

const (
	maxListSessions        = 20
	maxSessionPreviewRunes = 120
	maxSkippedSessionCount = int(^uint(0) >> 1)
)

type listCandidate struct {
	name string
	path string
	info os.FileInfo
}

func Inspect(ctx context.Context, path string) (SessionInfo, []Warning, error) {
	if err := ctx.Err(); err != nil {
		return SessionInfo{}, nil, err
	}

	file, info, err := openSessionFileReadOnlyNoFollow(path)
	if err != nil {
		return SessionInfo{}, nil, err
	}
	defer file.Close()

	return inspectOpenedSession(ctx, path, file, info)
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

	rootDir, err := openSessionRootNoFollow(root)
	if err != nil {
		return ListResult{}, err
	}
	defer rootDir.Close()

	sessionDir, directory, exists, err := openWorkspaceSessionDirectoryNoFollow(rootDir, root, workspaceCanonical)
	if err != nil {
		return ListResult{}, err
	}
	if !exists {
		return ListResult{}, nil
	}
	defer sessionDir.Close()

	entries, err := sessionDir.ReadDir(-1)
	if err != nil {
		return ListResult{}, fmt.Errorf("read session directory: %w", err)
	}

	currentCanonical := ""
	var currentInfo os.FileInfo
	haveCurrentInfo := false
	if currentPath != "" {
		currentCanonical, err = canonicalWorkspace(currentPath)
		if err != nil {
			return ListResult{}, err
		}
		currentInfo, err = os.Stat(currentPath)
		if err == nil {
			haveCurrentInfo = true
		}
	}

	result := ListResult{}
	candidates := make([]listCandidate, 0, len(entries))
	for _, entry := range entries {
		if err := ctx.Err(); err != nil {
			return ListResult{}, err
		}
		if filepath.Ext(entry.Name()) != ".jsonl" {
			continue
		}
		file, info, path, err := openSessionFileReadOnlyNoFollowAt(sessionDir, directory, entry.Name())
		if err != nil {
			result.Skipped = incrementSkipped(result.Skipped)
			continue
		}
		if closeErr := file.Close(); closeErr != nil {
			result.Skipped = incrementSkipped(result.Skipped)
			continue
		}
		candidates = append(candidates, listCandidate{name: entry.Name(), path: path, info: info})
	}

	sort.Slice(candidates, func(i, j int) bool {
		left, right := candidates[i], candidates[j]
		if left.info.ModTime().Equal(right.info.ModTime()) {
			return left.path > right.path
		}
		return left.info.ModTime().After(right.info.ModTime())
	})

	sessions := make([]SessionInfo, 0, min(limit, len(candidates)))
	for _, candidate := range candidates {
		if len(sessions) == limit {
			break
		}
		if err := ctx.Err(); err != nil {
			return ListResult{}, err
		}
		file, info, path, err := openSessionFileReadOnlyNoFollowAt(sessionDir, directory, candidate.name)
		if err != nil {
			result.Skipped = incrementSkipped(result.Skipped)
			continue
		}
		if !sameListCandidateMetadata(candidate.info, info) {
			_ = file.Close()
			result.Skipped = incrementSkipped(result.Skipped)
			continue
		}

		sessionInfo, _, inspectErr := inspectOpenedSession(ctx, path, file, info)
		closeErr := file.Close()
		if inspectErr != nil || closeErr != nil {
			result.Skipped = incrementSkipped(result.Skipped)
			continue
		}

		candidateWorkspace, err := canonicalWorkspace(sessionInfo.CWD)
		if err != nil || candidateWorkspace != workspaceCanonical {
			result.Skipped = incrementSkipped(result.Skipped)
			continue
		}

		if currentCanonical != "" {
			if haveCurrentInfo {
				sessionInfo.Current = os.SameFile(info, currentInfo)
			} else {
				candidateCanonical, err := canonicalWorkspace(path)
				if err != nil {
					result.Skipped = incrementSkipped(result.Skipped)
					continue
				}
				sessionInfo.Current = candidateCanonical == currentCanonical
			}
		}
		sessions = append(sessions, sessionInfo)
	}

	result.Sessions = sessions
	return result, nil
}

func sessionDirectory(root, workspace string) (string, error) {
	key, err := workspaceKey(workspace)
	if err != nil {
		return "", err
	}
	return filepath.Join(root, key), nil
}

func openSessionRootNoFollow(root string) (*os.File, error) {
	file, err := openPathNoFollow(root, unix.O_RDONLY|unix.O_DIRECTORY, 0)
	if err != nil {
		switch {
		case err == unix.ELOOP:
			return nil, fmt.Errorf("%w: session root is a symlink", ErrInvalidSession)
		case err == unix.ENOTDIR:
			return nil, fmt.Errorf("%w: session root is not a directory", ErrInvalidSession)
		default:
			return nil, fmt.Errorf("open session root: %w", err)
		}
	}
	return file, nil
}

func openWorkspaceSessionDirectoryNoFollow(rootDir *os.File, root, workspace string) (*os.File, string, bool, error) {
	key, err := workspaceKey(workspace)
	if err != nil {
		return nil, "", false, err
	}
	path := filepath.Join(root, key)
	file, err := openPathAtNoFollow(rootDir, key, unix.O_RDONLY|unix.O_DIRECTORY, 0)
	if err != nil {
		switch {
		case err == unix.ENOENT:
			return nil, path, false, nil
		case err == unix.ELOOP:
			return nil, "", false, fmt.Errorf("%w: session directory is a symlink", ErrInvalidSession)
		case err == unix.ENOTDIR:
			return nil, "", false, fmt.Errorf("%w: session directory is not a directory", ErrInvalidSession)
		default:
			return nil, "", false, fmt.Errorf("open session directory: %w", err)
		}
	}
	return file, path, true, nil
}

func openSessionFileReadOnlyNoFollow(path string) (*os.File, os.FileInfo, error) {
	file, err := openPathNoFollow(path, unix.O_RDONLY, 0)
	if err != nil {
		if err == unix.ELOOP {
			return nil, nil, fmt.Errorf("%w: session file is a symlink", ErrInvalidSession)
		}
		return nil, nil, fmt.Errorf("open session file: %w", err)
	}
	info, err := file.Stat()
	if err != nil {
		_ = file.Close()
		return nil, nil, fmt.Errorf("stat session file: %w", err)
	}
	if !info.Mode().IsRegular() {
		_ = file.Close()
		return nil, nil, fmt.Errorf("%w: session file is not a regular file", ErrInvalidSession)
	}
	return file, info, nil
}

func openSessionFileReadOnlyNoFollowAt(directory *os.File, directoryPath, name string) (*os.File, os.FileInfo, string, error) {
	path := filepath.Join(directoryPath, name)
	file, err := openPathAtNoFollow(directory, name, unix.O_RDONLY, 0)
	if err != nil {
		if err == unix.ELOOP {
			return nil, nil, "", fmt.Errorf("%w: session file is a symlink", ErrInvalidSession)
		}
		return nil, nil, "", fmt.Errorf("open session file: %w", err)
	}
	info, err := file.Stat()
	if err != nil {
		_ = file.Close()
		return nil, nil, "", fmt.Errorf("stat session file: %w", err)
	}
	if !info.Mode().IsRegular() {
		_ = file.Close()
		return nil, nil, "", fmt.Errorf("%w: session file is not a regular file", ErrInvalidSession)
	}
	return file, info, path, nil
}

func openPathNoFollow(path string, flags int, perm uint32) (*os.File, error) {
	fd, err := unix.Open(path, flags|unix.O_CLOEXEC|unix.O_NOFOLLOW, perm)
	if err != nil {
		return nil, err
	}
	return fileFromFD(fd, path)
}

func openPathAtNoFollow(directory *os.File, name string, flags int, perm uint32) (*os.File, error) {
	fd, err := unix.Openat(int(directory.Fd()), name, flags|unix.O_CLOEXEC|unix.O_NOFOLLOW, perm)
	if err != nil {
		return nil, err
	}
	return fileFromFD(fd, filepath.Join(directory.Name(), name))
}

func fileFromFD(fd int, name string) (*os.File, error) {
	file := os.NewFile(uintptr(fd), name)
	if file == nil {
		_ = unix.Close(fd)
		return nil, fmt.Errorf("create file handle for %s: invalid descriptor", name)
	}
	return file, nil
}

func inspectOpenedSession(ctx context.Context, path string, file *os.File, info os.FileInfo) (SessionInfo, []Warning, error) {
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
	var builder strings.Builder
	builder.Grow(maxSessionPreviewRunes*6 + 3)
	used := 0
	wroteContent := false
	pendingSpace := false
	truncated := false

	for _, r := range text {
		if unicode.IsSpace(r) {
			pendingSpace = wroteContent
			continue
		}

		needed := previewRuneWidth(r)
		if pendingSpace {
			needed++
		}
		if used+needed > maxSessionPreviewRunes {
			truncated = true
			break
		}
		if pendingSpace {
			builder.WriteByte(' ')
			used++
			pendingSpace = false
		}
		writePreviewRune(&builder, r)
		used += previewRuneWidth(r)
		wroteContent = true
	}

	if !wroteContent {
		return ""
	}
	if !truncated {
		return builder.String()
	}
	return builder.String() + "..."
}

func previewRuneWidth(r rune) int {
	switch {
	case r < 0x100 && unicode.IsControl(r):
		return 4
	case unicode.IsControl(r):
		return 2 + max(4, hexWidth(uint32(r)))
	default:
		return 1
	}
}

func writePreviewRune(builder *strings.Builder, r rune) {
	switch {
	case r < 0x100 && unicode.IsControl(r):
		builder.WriteString(`\x`)
		writeHex(builder, uint32(r), 2)
	case unicode.IsControl(r):
		builder.WriteString(`\u`)
		writeHex(builder, uint32(r), max(4, hexWidth(uint32(r))))
	default:
		builder.WriteRune(r)
	}
}

func hexWidth(value uint32) int {
	width := 1
	for value >= 16 {
		value /= 16
		width++
	}
	return width
}

func writeHex(builder *strings.Builder, value uint32, width int) {
	const digits = "0123456789abcdef"
	var buffer [8]byte
	for index := width - 1; index >= 0; index-- {
		buffer[index] = digits[value&0xf]
		value >>= 4
	}
	for _, digit := range buffer[:width] {
		builder.WriteByte(digit)
	}
}

func sameListCandidateMetadata(expected, current os.FileInfo) bool {
	return expected != nil && current != nil &&
		os.SameFile(expected, current) &&
		expected.Mode() == current.Mode() &&
		expected.Size() == current.Size() &&
		expected.ModTime().Equal(current.ModTime())
}

func incrementSkipped(count int) int {
	if count < maxSkippedSessionCount {
		return count + 1
	}
	return count
}
