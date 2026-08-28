package session

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sync"

	"golang.org/x/sys/unix"
)

// Prepared pins one verified session file descriptor and its runtime metadata.
// Preparing is read-only. Activate consumes the handle and transfers that exact
// descriptor to a Store, which may then perform the normal documented repairs.
type Prepared struct {
	mu             sync.Mutex
	path           string
	file           *os.File
	fileInfo       os.FileInfo
	info           SessionInfo
	verifyIdentity func(os.FileInfo) error
}

func Prepare(ctx context.Context, path string) (*Prepared, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	file, fileInfo, err := openPreparedSessionFileNoFollow(path)
	if err != nil {
		return nil, err
	}
	return prepareOpenedSession(ctx, path, file, fileInfo, func(prepared os.FileInfo) error {
		return verifyPreparedPathIdentity(path, prepared)
	})
}

// PrepareListed opens only a candidate returned for the expected session root
// and workspace. Every attacker-controlled component below root is traversed
// with openat and O_NOFOLLOW before the exact regular-file descriptor is pinned.
func PrepareListed(ctx context.Context, root, workspace, path string) (*Prepared, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	rootPath, workspaceCanonical, basename, candidatePath, err := validateListedCandidatePath(root, workspace, path)
	if err != nil {
		return nil, err
	}
	file, fileInfo, err := openListedPreparedSessionFile(rootPath, workspaceCanonical, basename)
	if err != nil {
		return nil, err
	}
	prepared, err := prepareOpenedSession(ctx, candidatePath, file, fileInfo, func(expected os.FileInfo) error {
		current, currentInfo, openErr := openListedPreparedSessionFile(rootPath, workspaceCanonical, basename)
		if openErr != nil {
			return fmt.Errorf("%w: prepared listed session path identity changed before activation", ErrInvalidSession)
		}
		defer current.Close()
		if !os.SameFile(expected, currentInfo) {
			return fmt.Errorf("%w: prepared listed session path identity changed before activation", ErrInvalidSession)
		}
		return nil
	})
	if err != nil {
		return nil, err
	}
	candidateWorkspace, err := canonicalWorkspace(prepared.info.CWD)
	if err != nil || candidateWorkspace != workspaceCanonical {
		_ = prepared.Close()
		return nil, fmt.Errorf("%w: listed session workspace does not match expected workspace", ErrInvalidSession)
	}
	return prepared, nil
}

func prepareOpenedSession(ctx context.Context, path string, file *os.File, fileInfo os.FileInfo, verifyIdentity func(os.FileInfo) error) (*Prepared, error) {
	closeOnError := func(prepareErr error) (*Prepared, error) {
		if closeErr := file.Close(); closeErr != nil {
			prepareErr = errors.Join(prepareErr, fmt.Errorf("close prepared session file: %w", closeErr))
		}
		return nil, prepareErr
	}

	info, _, err := inspectOpenedSession(ctx, path, file, fileInfo)
	if err != nil {
		return closeOnError(err)
	}
	if err := ctx.Err(); err != nil {
		return closeOnError(err)
	}
	return &Prepared{path: path, file: file, fileInfo: fileInfo, info: info, verifyIdentity: verifyIdentity}, nil
}

func (p *Prepared) Info() SessionInfo {
	if p == nil {
		return SessionInfo{}
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.info
}

// Activate consumes the prepared handle. On success the returned Store owns
// the descriptor; on failure the descriptor is closed.
func (p *Prepared) Activate(ctx context.Context) (*Store, []Warning, error) {
	if p == nil {
		return nil, nil, errors.New("prepared session is no longer available")
	}
	p.mu.Lock()
	file := p.file
	p.file = nil
	path := p.path
	fileInfo := p.fileInfo
	preparedInfo := p.info
	verifyIdentity := p.verifyIdentity
	p.mu.Unlock()
	if file == nil {
		return nil, nil, errors.New("prepared session is no longer available")
	}
	closeOnError := func(activateErr error) (*Store, []Warning, error) {
		if closeErr := file.Close(); closeErr != nil {
			activateErr = errors.Join(activateErr, fmt.Errorf("close prepared session file: %w", closeErr))
		}
		return nil, nil, activateErr
	}
	if err := ctx.Err(); err != nil {
		return closeOnError(err)
	}
	if verifyIdentity != nil {
		if err := verifyIdentity(fileInfo); err != nil {
			return closeOnError(err)
		}
	}
	currentFileInfo, err := file.Stat()
	if err != nil {
		return closeOnError(fmt.Errorf("stat prepared session file: %w", err))
	}
	if !os.SameFile(fileInfo, currentFileInfo) {
		return closeOnError(fmt.Errorf("%w: prepared session file identity changed", ErrInvalidSession))
	}
	currentInfo, _, err := inspectOpenedSession(ctx, path, file, currentFileInfo)
	if err != nil {
		return closeOnError(err)
	}
	if !samePreparedMetadata(preparedInfo, currentInfo) {
		return closeOnError(fmt.Errorf("%w: prepared session metadata changed before activation", ErrInvalidSession))
	}

	// openStoreFromFile consumes file on both success and failure.
	return openStoreFromFile(file, path)
}

// Close abandons a prepared handle. It is safe to call after activation and is
// idempotent.
func (p *Prepared) Close() error {
	if p == nil {
		return nil
	}
	p.mu.Lock()
	file := p.file
	p.file = nil
	p.mu.Unlock()
	if file == nil {
		return nil
	}
	if err := file.Close(); err != nil {
		return fmt.Errorf("close prepared session file: %w", err)
	}
	return nil
}

func validateListedCandidatePath(root, workspace, path string) (string, string, string, string, error) {
	rootPath, err := filepath.Abs(root)
	if err != nil {
		return "", "", "", "", fmt.Errorf("resolve session root path: %w", err)
	}
	rootPath = filepath.Clean(rootPath)
	workspaceCanonical, err := canonicalWorkspace(workspace)
	if err != nil {
		return "", "", "", "", err
	}
	key, err := workspaceKey(workspaceCanonical)
	if err != nil {
		return "", "", "", "", err
	}
	candidatePath, err := filepath.Abs(path)
	if err != nil {
		return "", "", "", "", fmt.Errorf("resolve listed session path: %w", err)
	}
	candidatePath = filepath.Clean(candidatePath)
	basename := filepath.Base(candidatePath)
	expected := filepath.Join(rootPath, key, basename)
	if candidatePath != expected || basename == "." || basename == ".." || filepath.Ext(basename) != ".jsonl" {
		return "", "", "", "", fmt.Errorf("%w: listed session candidate is outside the expected workspace directory", ErrInvalidSession)
	}
	return rootPath, workspaceCanonical, basename, candidatePath, nil
}

func openListedPreparedSessionFile(root, workspace, basename string) (*os.File, os.FileInfo, error) {
	rootDir, err := openSessionRootNoFollow(root)
	if err != nil {
		return nil, nil, err
	}
	defer rootDir.Close()
	workspaceDir, _, exists, err := openWorkspaceSessionDirectoryNoFollow(rootDir, root, workspace)
	if err != nil {
		return nil, nil, err
	}
	if !exists {
		return nil, nil, fmt.Errorf("%w: listed session directory no longer exists", ErrInvalidSession)
	}
	defer workspaceDir.Close()

	file, err := openPathAtNoFollow(workspaceDir, basename, unix.O_RDWR, 0)
	if err != nil {
		if err == unix.ELOOP {
			return nil, nil, fmt.Errorf("%w: listed session file is a symlink", ErrInvalidSession)
		}
		return nil, nil, fmt.Errorf("open listed session file: %w", err)
	}
	info, err := file.Stat()
	if err != nil {
		_ = file.Close()
		return nil, nil, fmt.Errorf("stat listed session file: %w", err)
	}
	if !info.Mode().IsRegular() {
		_ = file.Close()
		return nil, nil, fmt.Errorf("%w: listed session file is not a regular file", ErrInvalidSession)
	}
	return file, info, nil
}

func openPreparedSessionFileNoFollow(path string) (*os.File, os.FileInfo, error) {
	file, err := openPathNoFollow(path, unix.O_RDWR, 0)
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

func verifyPreparedPathIdentity(path string, prepared os.FileInfo) error {
	current, err := os.Lstat(path)
	if err != nil || !current.Mode().IsRegular() || !os.SameFile(prepared, current) {
		return fmt.Errorf("%w: prepared session path identity changed before activation", ErrInvalidSession)
	}
	return nil
}

func samePreparedMetadata(prepared, current SessionInfo) bool {
	return prepared.ID == current.ID &&
		prepared.CWD == current.CWD &&
		prepared.Profile == current.Profile &&
		prepared.Provider == current.Provider &&
		prepared.Model == current.Model &&
		prepared.Created.Equal(current.Created)
}
