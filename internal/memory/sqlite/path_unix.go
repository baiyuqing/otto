//go:build darwin || linux

package sqlite

import (
	"context"
	"crypto/rand"
	"database/sql"
	"encoding/hex"
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

const privateDirectoryTemporaryPrefix = ".otto-memory-directory-"

var sidecarSuffixes = [...]string{"-wal", "-shm"}

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

type retainedSidecar struct {
	file     *os.File
	identity inodeIdentity
}

type securePath struct {
	canonicalPath    string
	canonicalParent  string
	baseName         string
	parent           *os.File
	database         *os.File
	parentIdentity   inodeIdentity
	databaseIdentity inodeIdentity
	sidecars         map[string]retainedSidecar
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
		nextFD, _, openErr := openOrInstallPrivateDirectory(ctx, currentFD, name)
		_ = unix.Close(currentFD)
		if openErr != nil {
			return nil, openErr
		}
		currentFD = nextFD
		currentPath = filepath.Join(currentPath, name)
	}

	var parentStat unix.Stat_t
	if err := unix.Fstat(currentFD, &parentStat); err != nil || validateFileMetadata(statMetadata{parentStat}, true) != nil {
		_ = unix.Close(currentFD)
		return nil, memory.ErrInvalidRequest
	}
	parentFile := os.NewFile(uintptr(currentFD), "secure-memory-parent")
	if parentFile == nil {
		_ = unix.Close(currentFD)
		return nil, memory.ErrUnavailable
	}

	databaseFile, created, databaseStat, err := openOrCreateDatabase(ctx, currentFD, baseName)
	if err != nil {
		_ = parentFile.Close()
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
		sidecars:         make(map[string]retainedSidecar, len(sidecarSuffixes)),
		created:          created,
	}
	if err := path.validateSidecarEntries(); err != nil {
		_ = path.close()
		return nil, err
	}
	return path, nil
}

func openOrInstallPrivateDirectory(ctx context.Context, parentFD int, name string) (int, bool, error) {
	for {
		if err := ctx.Err(); err != nil {
			return -1, false, err
		}
		fd, err := unix.Openat(parentFD, name, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
		if err == nil {
			var stat unix.Stat_t
			if err := unix.Fstat(fd, &stat); err != nil || validateFileMetadata(statMetadata{stat}, true) != nil {
				_ = unix.Close(fd)
				return -1, false, memory.ErrInvalidRequest
			}
			return fd, false, nil
		}
		if !errors.Is(err, unix.ENOENT) {
			return -1, false, memory.ErrInvalidRequest
		}

		fd, installed, err := prepareAndInstallPrivateDirectory(ctx, parentFD, name)
		if err != nil {
			return -1, false, err
		}
		if installed {
			return fd, true, nil
		}
		// A no-replace loser is never repaired. Restart by opening and
		// validating the winner as a pre-existing entry.
	}
}

func prepareAndInstallPrivateDirectory(ctx context.Context, parentFD int, targetName string) (resultFD int, installed bool, resultErr error) {
	resultFD = -1
	temporaryName := ""
	temporaryExists := false
	preparedFD := -1
	keepDescriptor := false
	defer func() {
		if preparedFD >= 0 && !keepDescriptor {
			if err := unix.Close(preparedFD); err != nil && resultErr == nil {
				resultErr = memory.ErrUnavailable
			}
		}
		if temporaryExists {
			if err := unix.Unlinkat(parentFD, temporaryName, unix.AT_REMOVEDIR); err != nil && resultErr == nil {
				resultErr = memory.ErrUnavailable
			}
		}
	}()

	for attempt := 0; attempt < 16; attempt++ {
		if err := ctx.Err(); err != nil {
			return -1, false, err
		}
		name, err := randomPrivateDirectoryName()
		if err != nil {
			return -1, false, err
		}
		if hook := loadTestHooks().mkdirat; hook != nil {
			operation := func() error { return unix.Mkdirat(parentFD, name, 0o700) }
			connectionProofMu.Unlock()
			err = func() error {
				defer connectionProofMu.Lock()
				return hook(operation)
			}()
		} else {
			err = unix.Mkdirat(parentFD, name, 0o700)
		}
		if errors.Is(err, unix.EEXIST) {
			continue
		}
		if err != nil {
			return -1, false, memory.ErrUnavailable
		}
		temporaryName = name
		temporaryExists = true
		preparedFD, resultErr = unix.Openat(parentFD, temporaryName, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
		if resultErr != nil {
			return -1, false, memory.ErrUnavailable
		}
		break
	}
	if preparedFD < 0 {
		return -1, false, memory.ErrUnavailable
	}
	if resultErr = unix.Fchmod(preparedFD, 0o700); resultErr != nil {
		return -1, false, memory.ErrUnavailable
	}
	var preparedStat unix.Stat_t
	if resultErr = unix.Fstat(preparedFD, &preparedStat); resultErr != nil || validateFileMetadata(statMetadata{preparedStat}, true) != nil || uint32(preparedStat.Mode)&0o777 != 0o700 {
		return -1, false, memory.ErrUnavailable
	}
	if err := ctx.Err(); err != nil {
		return -1, false, err
	}
	if hook := loadTestHooks().beforeDirectoryInstall; hook != nil {
		hook(targetName)
	}
	if err := ctx.Err(); err != nil {
		return -1, false, err
	}

	resultErr = renameDirectoryNoReplace(parentFD, temporaryName, parentFD, targetName)
	if errors.Is(resultErr, unix.EEXIST) {
		return -1, false, nil
	}
	if resultErr != nil {
		if noReplaceUnsupported(resultErr) {
			return -1, false, memory.ErrUnsupported
		}
		return -1, false, memory.ErrUnavailable
	}
	temporaryExists = false

	var installedStat unix.Stat_t
	if err := unix.Fstatat(parentFD, targetName, &installedStat, unix.AT_SYMLINK_NOFOLLOW); err != nil ||
		identityFromStat(installedStat) != identityFromStat(preparedStat) ||
		validateFileMetadata(statMetadata{installedStat}, true) != nil || uint32(installedStat.Mode)&0o777 != 0o700 {
		return -1, false, memory.ErrUnsupported
	}
	keepDescriptor = true
	return preparedFD, true, nil
}

func randomPrivateDirectoryName() (string, error) {
	var value [16]byte
	if _, err := rand.Read(value[:]); err != nil {
		return "", memory.ErrUnavailable
	}
	return privateDirectoryTemporaryPrefix + hex.EncodeToString(value[:]), nil
}

func noReplaceUnsupported(err error) bool {
	return errors.Is(err, unix.ENOSYS) || errors.Is(err, unix.EINVAL) || errors.Is(err, unix.ENOTSUP)
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
		if created {
			if err := unix.Fchmod(fd, 0o600); err != nil {
				_ = unix.Close(fd)
				return nil, false, unix.Stat_t{}, memory.ErrUnavailable
			}
		}
		var stat unix.Stat_t
		if err := unix.Fstat(fd, &stat); err != nil || validateFileMetadata(statMetadata{stat}, false) != nil || stat.Nlink != 1 || created && uint32(stat.Mode)&0o777 != 0o600 {
			_ = unix.Close(fd)
			return nil, false, unix.Stat_t{}, memory.ErrInvalidRequest
		}
		file := os.NewFile(uintptr(fd), "secure-memory-database")
		if file == nil {
			_ = unix.Close(fd)
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
		_ = unix.Close(fd)
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
	if err := unix.Fstat(databaseFD, &databaseStat); err != nil || identityFromStat(databaseStat) != path.databaseIdentity || databaseStat.Nlink != 1 {
		return memory.ErrUnsupported
	}
	if err := validateFileMetadata(statMetadata{databaseStat}, false); err != nil {
		return err
	}
	return path.validateSidecarEntries()
}

// validateSidecarEntries retains every verified sidecar descriptor. Once a
// sidecar has been observed, disappearance or identity replacement fails
// closed for the rest of Open and for Store lifetime.
func (path *securePath) validateSidecarEntries() error {
	identities := map[inodeIdentity]string{path.databaseIdentity: "database"}
	for suffix, sidecar := range path.sidecars {
		if previous, aliased := identities[sidecar.identity]; aliased && previous != suffix {
			return memory.ErrInvalidRequest
		}
		identities[sidecar.identity] = suffix
	}
	for _, suffix := range sidecarSuffixes {
		var entryStat unix.Stat_t
		statErr := unix.Fstatat(int(path.parent.Fd()), path.baseName+suffix, &entryStat, unix.AT_SYMLINK_NOFOLLOW)
		if errors.Is(statErr, unix.ENOENT) {
			if _, retained := path.sidecars[suffix]; retained {
				return memory.ErrUnsupported
			}
			continue
		}
		if statErr != nil || validateFileMetadata(statMetadata{entryStat}, false) != nil || entryStat.Nlink != 1 {
			return memory.ErrInvalidRequest
		}
		identity := identityFromStat(entryStat)
		if previous, aliased := identities[identity]; aliased && previous != suffix {
			return memory.ErrInvalidRequest
		}
		identities[identity] = suffix
		if retained, ok := path.sidecars[suffix]; ok {
			if retained.identity != identity {
				return memory.ErrUnsupported
			}
			continue
		}
		fd, err := unix.Openat(int(path.parent.Fd()), path.baseName+suffix, unix.O_RDONLY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
		if err != nil {
			return memory.ErrInvalidRequest
		}
		var descriptorStat unix.Stat_t
		if err := unix.Fstat(fd, &descriptorStat); err != nil || validateFileMetadata(statMetadata{descriptorStat}, false) != nil || descriptorStat.Nlink != 1 || identityFromStat(descriptorStat) != identity {
			_ = unix.Close(fd)
			return memory.ErrInvalidRequest
		}
		file := os.NewFile(uintptr(fd), "secure-memory-sidecar"+suffix)
		if file == nil {
			_ = unix.Close(fd)
			return memory.ErrUnavailable
		}
		path.sidecars[suffix] = retainedSidecar{file: file, identity: identity}
	}
	return nil
}

func (path *securePath) sidecarIdentities() map[string]inodeIdentity {
	result := make(map[string]inodeIdentity, len(path.sidecars))
	for suffix, sidecar := range path.sidecars {
		result[suffix] = sidecar.identity
	}
	return result
}

func closeSecureDescriptor(resource string, file *os.File) error {
	if file == nil {
		return nil
	}
	err := file.Close()
	if hook := loadTestHooks().closeError; hook != nil {
		err = hook(resource, err)
	}
	return err
}

func (path *securePath) close() error {
	if path == nil {
		return nil
	}
	var result error
	for _, suffix := range sidecarSuffixes {
		if sidecar, ok := path.sidecars[suffix]; ok {
			if err := closeSecureDescriptor("sidecar-descriptor"+suffix, sidecar.file); err != nil {
				result = err
			}
		}
	}
	if err := closeSecureDescriptor("database-descriptor", path.database); err != nil {
		result = err
	}
	if err := closeSecureDescriptor("parent-descriptor", path.parent); err != nil {
		result = err
	}
	return result
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
	rows, err := conn.QueryContext(ctx, "PRAGMA database_list")
	if err != nil {
		return safeSQLiteError(ctx, err)
	}
	mainSeen := false
	for rows.Next() {
		var sequence int
		var name, filename string
		if err := rows.Scan(&sequence, &name, &filename); err != nil {
			_ = rows.Close()
			return safeSQLiteError(ctx, err)
		}
		if name == "main" {
			if mainSeen || filename != path.canonicalPath {
				_ = rows.Close()
				return memory.ErrUnsupported
			}
			mainSeen = true
		} else if name != "temp" {
			_ = rows.Close()
			return memory.ErrUnsupported
		}
	}
	if err := rows.Err(); err != nil {
		_ = rows.Close()
		return safeSQLiteError(ctx, err)
	}
	if err := rows.Close(); err != nil {
		return safeSQLiteError(ctx, err)
	}
	if !mainSeen {
		return memory.ErrUnsupported
	}
	after, err := snapshotProcessFDs()
	if err != nil {
		return err
	}
	if err := path.revalidate(); err != nil {
		return err
	}
	return proveExpectedFDDelta(before, after, path, true, false)
}

func proveExpectedFDDelta(before, after map[inodeIdentity]int, path *securePath, requireDatabase, requireWAL bool) error {
	allowed := make(map[inodeIdentity]int, 1+len(path.sidecars))
	allowed[path.databaseIdentity] = 1
	for _, identity := range path.sidecarIdentities() {
		if _, duplicate := allowed[identity]; duplicate {
			return memory.ErrUnsupported
		}
		allowed[identity] = 1
	}
	for identity, afterCount := range after {
		delta := afterCount - before[identity]
		if delta <= 0 {
			continue
		}
		maximum, ok := allowed[identity]
		if !ok || delta > maximum {
			return memory.ErrUnsupported
		}
	}
	databaseDelta := after[path.databaseIdentity] - before[path.databaseIdentity]
	if requireDatabase && databaseDelta != 1 {
		return memory.ErrUnsupported
	}
	if !requireDatabase && databaseDelta > 0 {
		return memory.ErrUnsupported
	}
	if requireWAL {
		identity, ok := path.sidecarIdentities()["-wal"]
		if !ok || after[identity]-before[identity] != 1 {
			return memory.ErrUnsupported
		}
	}
	return nil
}

func proveSQLiteSidecarsIfPresent(path *securePath, before map[inodeIdentity]int) error {
	after, err := snapshotProcessFDs()
	if err != nil {
		return err
	}
	if err := path.revalidate(); err != nil {
		return err
	}
	return proveExpectedFDDelta(before, after, path, true, false)
}

func proveSQLiteSidecars(path *securePath, before map[inodeIdentity]int, requireWAL bool) error {
	after, err := snapshotProcessFDs()
	if err != nil {
		return err
	}
	if err := path.revalidate(); err != nil {
		return err
	}
	return proveExpectedFDDelta(before, after, path, false, requireWAL)
}

func proveRetainedSQLiteConnection(path *securePath, before map[inodeIdentity]int) error {
	after, err := snapshotProcessFDs()
	if err != nil {
		return err
	}
	if err := path.revalidate(); err != nil {
		return err
	}
	_, hasWAL := path.sidecarIdentities()["-wal"]
	return proveExpectedFDDelta(before, after, path, true, hasWAL)
}
