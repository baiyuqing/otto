//go:build darwin || linux

package sqlite

import (
	"context"
	"database/sql"
	"errors"
	"os"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/baiyuqing/otto/internal/memory"
	"golang.org/x/sys/unix"
)

const (
	regularMode = unix.S_IFREG
	fdDirectory = "/proc/self/fd"
)

// Darwin exposes process descriptors through /dev/fd rather than procfs.
func processFDDirectory() string {
	if _, err := os.Stat(fdDirectory); err == nil {
		return fdDirectory
	}
	return "/dev/fd"
}

type syntheticMetadata struct {
	mode uint32
	uid  uint32
}

func (m syntheticMetadata) fileMode() uint32 { return m.mode }
func (m syntheticMetadata) ownerUID() uint32 { return m.uid }

type statMetadata struct{ stat unix.Stat_t }

func (m statMetadata) fileMode() uint32 { return uint32(m.stat.Mode) }
func (m statMetadata) ownerUID() uint32 { return m.stat.Uid }

type metadata interface {
	fileMode() uint32
	ownerUID() uint32
}

type inodeIdentity struct {
	device uint64
	inode  uint64
}

type securePath struct {
	canonicalPath    string
	canonicalParent  string
	baseName         string
	parent           *os.File
	database         *os.File
	parentIdentity   inodeIdentity
	databaseIdentity inodeIdentity
	created          bool
}

func currentUID() uint32 { return uint32(os.Geteuid()) }

func validateFileMetadata(value metadata, directory bool) error {
	mode := value.fileMode()
	wantType := uint32(unix.S_IFREG)
	if directory {
		wantType = unix.S_IFDIR
	}
	if mode&unix.S_IFMT != wantType || value.ownerUID() != currentUID() || mode&0o077 != 0 {
		return memory.ErrInvalidRequest
	}
	return nil
}

func openSecurePath(ctx context.Context, inputPath string) (*securePath, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	absolute, err := filepath.Abs(inputPath)
	if err != nil || filepath.Base(absolute) == "." || filepath.Base(absolute) == string(filepath.Separator) {
		return nil, memory.ErrInvalidRequest
	}
	baseName := filepath.Base(absolute)
	if baseName == "" || baseName == "." || baseName == ".." || strings.ContainsRune(baseName, filepath.Separator) {
		return nil, memory.ErrInvalidRequest
	}
	parentInput := filepath.Dir(absolute)
	canonicalAncestor, missing, err := nearestPhysicalAncestor(parentInput)
	if err != nil {
		return nil, err
	}

	ancestorFD, err := openDirectoryAbsolute(canonicalAncestor)
	if err != nil {
		return nil, memory.ErrInvalidRequest
	}
	currentFD := ancestorFD
	currentPath := canonicalAncestor
	for _, name := range missing {
		if err := ctx.Err(); err != nil {
			unix.Close(currentFD)
			return nil, err
		}
		if err := unix.Mkdirat(currentFD, name, 0o700); err != nil && !errors.Is(err, unix.EEXIST) {
			unix.Close(currentFD)
			return nil, memory.ErrInvalidRequest
		}
		nextFD, err := unix.Openat(currentFD, name, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
		unix.Close(currentFD)
		if err != nil {
			return nil, memory.ErrInvalidRequest
		}
		var stat unix.Stat_t
		if err := unix.Fstat(nextFD, &stat); err != nil || validateFileMetadata(statMetadata{stat}, true) != nil {
			unix.Close(nextFD)
			return nil, memory.ErrInvalidRequest
		}
		currentFD = nextFD
		currentPath = filepath.Join(currentPath, name)
	}

	var parentStat unix.Stat_t
	if err := unix.Fstat(currentFD, &parentStat); err != nil || validateFileMetadata(statMetadata{parentStat}, true) != nil {
		unix.Close(currentFD)
		return nil, memory.ErrInvalidRequest
	}
	parentFile := os.NewFile(uintptr(currentFD), "secure-memory-parent")
	if parentFile == nil {
		unix.Close(currentFD)
		return nil, memory.ErrUnavailable
	}

	databaseFile, created, databaseStat, err := openOrCreateDatabase(ctx, currentFD, baseName)
	if err != nil {
		parentFile.Close()
		return nil, err
	}
	path := &securePath{
		canonicalPath:    filepath.Join(currentPath, baseName),
		canonicalParent:  currentPath,
		baseName:         baseName,
		parent:           parentFile,
		database:         databaseFile,
		parentIdentity:   identityFromStat(parentStat),
		databaseIdentity: identityFromStat(databaseStat),
		created:          created,
	}
	if err := path.validateSidecarEntries(); err != nil {
		path.close()
		return nil, err
	}
	return path, nil
}

func nearestPhysicalAncestor(parent string) (string, []string, error) {
	current := filepath.Clean(parent)
	var reversed []string
	for {
		_, err := os.Lstat(current)
		if err == nil {
			physical, err := filepath.EvalSymlinks(current)
			if err != nil {
				return "", nil, memory.ErrInvalidRequest
			}
			physical, err = filepath.Abs(physical)
			if err != nil {
				return "", nil, memory.ErrInvalidRequest
			}
			missing := make([]string, len(reversed))
			for i := range reversed {
				missing[i] = reversed[len(reversed)-1-i]
			}
			return physical, missing, nil
		}
		if !errors.Is(err, os.ErrNotExist) {
			return "", nil, memory.ErrInvalidRequest
		}
		base := filepath.Base(current)
		next := filepath.Dir(current)
		if next == current || base == "." || base == string(filepath.Separator) {
			return "", nil, memory.ErrInvalidRequest
		}
		reversed = append(reversed, base)
		current = next
	}
}

func openOrCreateDatabase(ctx context.Context, parentFD int, name string) (*os.File, bool, unix.Stat_t, error) {
	for {
		if err := ctx.Err(); err != nil {
			return nil, false, unix.Stat_t{}, err
		}
		fd, err := unix.Openat(parentFD, name, unix.O_RDWR|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
		created := false
		if errors.Is(err, unix.ENOENT) {
			fd, err = unix.Openat(parentFD, name, unix.O_RDWR|unix.O_CREAT|unix.O_EXCL|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0o600)
			created = err == nil
			if errors.Is(err, unix.EEXIST) {
				continue
			}
		}
		if err != nil {
			return nil, false, unix.Stat_t{}, memory.ErrInvalidRequest
		}
		var stat unix.Stat_t
		if err := unix.Fstat(fd, &stat); err != nil || validateFileMetadata(statMetadata{stat}, false) != nil {
			unix.Close(fd)
			return nil, false, unix.Stat_t{}, memory.ErrInvalidRequest
		}
		file := os.NewFile(uintptr(fd), "secure-memory-database")
		if file == nil {
			unix.Close(fd)
			return nil, false, unix.Stat_t{}, memory.ErrUnavailable
		}
		return file, created, stat, nil
	}
}

func openDirectoryAbsolute(path string) (int, error) {
	if !filepath.IsAbs(path) {
		return -1, unix.EINVAL
	}
	fd, err := unix.Open(string(filepath.Separator), unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC, 0)
	if err != nil {
		return -1, err
	}
	for _, component := range strings.Split(strings.TrimPrefix(filepath.Clean(path), string(filepath.Separator)), string(filepath.Separator)) {
		if component == "" {
			continue
		}
		next, err := unix.Openat(fd, component, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
		unix.Close(fd)
		if err != nil {
			return -1, err
		}
		fd = next
	}
	return fd, nil
}

func (path *securePath) revalidate() error {
	parentFD, err := openDirectoryAbsolute(path.canonicalParent)
	if err != nil {
		return memory.ErrUnsupported
	}
	defer unix.Close(parentFD)
	var parentStat unix.Stat_t
	if err := unix.Fstat(parentFD, &parentStat); err != nil || identityFromStat(parentStat) != path.parentIdentity {
		return memory.ErrUnsupported
	}
	databaseFD, err := unix.Openat(parentFD, path.baseName, unix.O_RDONLY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
	if err != nil {
		return memory.ErrUnsupported
	}
	defer unix.Close(databaseFD)
	var databaseStat unix.Stat_t
	if err := unix.Fstat(databaseFD, &databaseStat); err != nil || identityFromStat(databaseStat) != path.databaseIdentity {
		return memory.ErrUnsupported
	}
	if err := validateFileMetadata(statMetadata{databaseStat}, false); err != nil {
		return err
	}
	return path.validateSidecarEntries()
}

func (path *securePath) validateSidecarEntries() error {
	for _, suffix := range []string{"-wal", "-shm"} {
		fd, err := unix.Openat(int(path.parent.Fd()), path.baseName+suffix, unix.O_RDONLY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
		if errors.Is(err, unix.ENOENT) {
			continue
		}
		if err != nil {
			return memory.ErrInvalidRequest
		}
		var stat unix.Stat_t
		statErr := unix.Fstat(fd, &stat)
		unix.Close(fd)
		if statErr != nil || validateFileMetadata(statMetadata{stat}, false) != nil {
			return memory.ErrInvalidRequest
		}
	}
	return nil
}

func (path *securePath) sidecarIdentities() (map[inodeIdentity]string, error) {
	result := make(map[inodeIdentity]string)
	for _, suffix := range []string{"-wal", "-shm"} {
		fd, err := unix.Openat(int(path.parent.Fd()), path.baseName+suffix, unix.O_RDONLY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
		if errors.Is(err, unix.ENOENT) {
			continue
		}
		if err != nil {
			return nil, memory.ErrInvalidRequest
		}
		var stat unix.Stat_t
		statErr := unix.Fstat(fd, &stat)
		unix.Close(fd)
		if statErr != nil || validateFileMetadata(statMetadata{stat}, false) != nil {
			return nil, memory.ErrInvalidRequest
		}
		result[identityFromStat(stat)] = suffix
	}
	return result, nil
}

func (path *securePath) close() {
	if path == nil {
		return
	}
	if path.database != nil {
		_ = path.database.Close()
	}
	if path.parent != nil {
		_ = path.parent.Close()
	}
}

func identityFromStat(stat unix.Stat_t) inodeIdentity {
	return inodeIdentity{device: uint64(stat.Dev), inode: uint64(stat.Ino)}
}

func snapshotProcessFDs() (map[inodeIdentity]int, error) {
	directory, err := os.Open(processFDDirectory())
	if err != nil {
		return nil, memory.ErrUnsupported
	}
	names, err := directory.Readdirnames(-1)
	_ = directory.Close()
	if err != nil {
		return nil, memory.ErrUnsupported
	}
	result := make(map[inodeIdentity]int)
	for _, name := range names {
		fd, err := strconv.Atoi(name)
		if err != nil {
			continue
		}
		var stat unix.Stat_t
		if err := unix.Fstat(fd, &stat); err != nil || stat.Mode&unix.S_IFMT != unix.S_IFREG {
			continue
		}
		result[identityFromStat(stat)]++
	}
	return result, nil
}

func proveSQLiteConnection(ctx context.Context, conn *sql.Conn, before map[inodeIdentity]int, path *securePath) error {
	after, err := snapshotProcessFDs()
	if err != nil {
		return err
	}
	if after[path.databaseIdentity] <= before[path.databaseIdentity] {
		return memory.ErrUnsupported
	}
	rows, err := conn.QueryContext(ctx, "PRAGMA database_list")
	if err != nil {
		return safeSQLiteError(ctx, err)
	}
	defer rows.Close()
	mainSeen := false
	for rows.Next() {
		var sequence int
		var name, filename string
		if err := rows.Scan(&sequence, &name, &filename); err != nil {
			return safeSQLiteError(ctx, err)
		}
		if name == "main" {
			if mainSeen || filename != path.canonicalPath {
				return memory.ErrUnsupported
			}
			mainSeen = true
		} else if name != "temp" {
			return memory.ErrUnsupported
		}
	}
	if err := rows.Err(); err != nil {
		return safeSQLiteError(ctx, err)
	}
	if !mainSeen {
		return memory.ErrUnsupported
	}
	return path.revalidate()
}

func proveSQLiteSidecarsIfPresent(path *securePath, before map[inodeIdentity]int) error {
	sidecars, err := path.sidecarIdentities()
	if err != nil {
		return err
	}
	if len(sidecars) == 0 {
		return nil
	}
	return proveSidecarIdentities(sidecars, before)
}

func proveSQLiteSidecars(path *securePath, before map[inodeIdentity]int) error {
	if err := path.revalidate(); err != nil {
		return err
	}
	sidecars, err := path.sidecarIdentities()
	if err != nil {
		return err
	}
	if len(sidecars) == 0 {
		return memory.ErrUnsupported
	}
	return proveSidecarIdentities(sidecars, before)
}

func proveSidecarIdentities(sidecars map[inodeIdentity]string, before map[inodeIdentity]int) error {
	after, err := snapshotProcessFDs()
	if err != nil {
		return err
	}
	for identity := range sidecars {
		if after[identity] <= before[identity] {
			return memory.ErrUnsupported
		}
	}
	return nil
}
