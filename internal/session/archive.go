package session

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"

	"golang.org/x/sys/unix"
)

const archiveDirectoryName = "archive"

// ArchiveResult describes a completed archive move.
type ArchiveResult struct {
	Path string
	ID   string
}

// Archive moves one active Pi v3 session file into the workspace's archive/
// directory. The source must be a direct child of the workspace-key directory
// and a valid Pi v3 session whose recorded workspace matches the current
// canonical workspace. On success the file exists only under archive/.
func Archive(ctx context.Context, root, workspace, path string) (ArchiveResult, error) {
	if err := ctx.Err(); err != nil {
		return ArchiveResult{}, err
	}
	_, workspaceCanonical, basename, _, err := validateListedCandidatePath(root, workspace, path)
	if err != nil {
		return ArchiveResult{}, err
	}

	rootDir, err := openSessionRootNoFollow(root)
	if err != nil {
		return ArchiveResult{}, err
	}
	defer rootDir.Close()

	workspaceDir, directory, exists, err := openWorkspaceSessionDirectoryNoFollow(rootDir, root, workspace)
	if err != nil {
		return ArchiveResult{}, err
	}
	if !exists {
		return ArchiveResult{}, fmt.Errorf("%w: listed session directory no longer exists", ErrInvalidSession)
	}
	defer workspaceDir.Close()

	file, info, candidatePath, err := openSessionFileReadOnlyNoFollowAt(workspaceDir, directory, basename)
	if err != nil {
		return ArchiveResult{}, err
	}
	defer file.Close()

	sessionInfo, _, err := inspectOpenedSession(ctx, candidatePath, file, info)
	if err != nil {
		return ArchiveResult{}, err
	}
	recordedWorkspace, err := canonicalWorkspace(sessionInfo.CWD)
	if err != nil || recordedWorkspace != workspaceCanonical {
		return ArchiveResult{}, fmt.Errorf("%w: session workspace does not match expected workspace", ErrInvalidSession)
	}
	if err := ctx.Err(); err != nil {
		return ArchiveResult{}, err
	}

	currentInfo, err := os.Lstat(candidatePath)
	if err != nil || !currentInfo.Mode().IsRegular() || !os.SameFile(info, currentInfo) {
		return ArchiveResult{}, fmt.Errorf("%w: session path identity changed before archive", ErrInvalidSession)
	}

	archiveDir, err := ensureArchiveDirectory(workspaceDir)
	if err != nil {
		return ArchiveResult{}, err
	}
	defer archiveDir.Close()

	destination := filepath.Join(directory, archiveDirectoryName, basename)
	if err := ensureArchiveDestinationAbsent(archiveDir, basename); err != nil {
		return ArchiveResult{}, err
	}
	if err := ctx.Err(); err != nil {
		return ArchiveResult{}, err
	}

	// RENAME_EXCL makes the move atomic and refuses to replace an existing
	// destination, closing the check-then-rename race.
	if err := unix.RenamexNp(candidatePath, destination, unix.RENAME_EXCL); err != nil {
		return ArchiveResult{}, fmt.Errorf("archive session file: %w", err)
	}
	return ArchiveResult{Path: destination, ID: sessionInfo.ID}, nil
}

// ensureArchiveDirectory returns an opened, verified archive/ directory under
// the workspace session directory, creating it with mode 0700 when missing.
func ensureArchiveDirectory(workspaceDir *os.File) (*os.File, error) {
	err := unix.Mkdirat(int(workspaceDir.Fd()), archiveDirectoryName, 0o700)
	if err != nil && !errors.Is(err, unix.EEXIST) {
		return nil, fmt.Errorf("create archive directory: %w", err)
	}
	archiveDir, err := openPathAtNoFollow(workspaceDir, archiveDirectoryName, unix.O_RDONLY|unix.O_DIRECTORY, 0)
	if err != nil {
		switch err {
		case unix.ELOOP:
			return nil, fmt.Errorf("%w: archive directory is a symlink", ErrInvalidSession)
		case unix.ENOTDIR:
			return nil, fmt.Errorf("%w: archive path is not a directory", ErrInvalidSession)
		default:
			return nil, fmt.Errorf("open archive directory: %w", err)
		}
	}
	info, statErr := archiveDir.Stat()
	if statErr != nil {
		_ = archiveDir.Close()
		return nil, fmt.Errorf("stat archive directory: %w", statErr)
	}
	if !info.IsDir() {
		_ = archiveDir.Close()
		return nil, fmt.Errorf("%w: archive path is not a directory", ErrInvalidSession)
	}
	if chmodErr := archiveDir.Chmod(0o700); chmodErr != nil {
		_ = archiveDir.Close()
		return nil, fmt.Errorf("chmod archive directory: %w", chmodErr)
	}
	return archiveDir, nil
}

// ensureArchiveDestinationAbsent fails when the destination file already exists
// so an archive move never replaces an existing archived session.
func ensureArchiveDestinationAbsent(archiveDir *os.File, basename string) error {
	var stat unix.Stat_t
	err := unix.Fstatat(int(archiveDir.Fd()), basename, &stat, unix.AT_SYMLINK_NOFOLLOW)
	if err == nil {
		return fmt.Errorf("%w: archive destination already exists", ErrInvalidSession)
	}
	if err == unix.ENOENT {
		return nil
	}
	return fmt.Errorf("stat archive destination: %w", err)
}
