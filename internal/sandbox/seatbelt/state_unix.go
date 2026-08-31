//go:build darwin || linux

package seatbelt

import (
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"syscall"

	"github.com/baiyuqing/otto/internal/sandbox"
	"golang.org/x/sys/unix"
)

const stateLeafPrefix = "otto-sandbox-"

var (
	errStateCreate  = errors.New("seatbelt private state unavailable")
	errStateCleanup = errors.New("seatbelt private state cleanup failed")
)

type state struct {
	directories sandbox.PrivateDirectories
	profiles    string
	profilePath string
	rootParent  string
	closeOnce   sync.Once
	closeErr    error
}

type stateOperations struct {
	canonicalize func(string) (string, error)
	mkdirTemp    func(string, string) (string, error)
	mkdir        func(string, fs.FileMode) error
	openFile     func(string, int, fs.FileMode) (*os.File, error)
	lstat        func(string) (fs.FileInfo, error)
	currentUID   func() int
}

func defaultStateOperations() stateOperations {
	return stateOperations{
		canonicalize: canonicalFilesystemPath,
		mkdirTemp:    os.MkdirTemp,
		mkdir:        os.Mkdir,
		openFile:     os.OpenFile,
		lstat:        os.Lstat,
		currentUID:   os.Geteuid,
	}
}

func createState(workspace, cacheBase string) (*state, error) {
	return createStateWithOperations(workspace, cacheBase, defaultStateOperations())
}

func createStateWithOperations(workspace, cacheBase string, operations stateOperations) (*state, error) {
	if !validStateOperations(operations) {
		return nil, errStateCreate
	}
	canonicalWorkspace, err := canonicalStateDirectory(workspace, operations)
	if err != nil {
		return nil, errStateCreate
	}
	canonicalCache, err := canonicalStateDirectory(cacheBase, operations)
	if err != nil || pathWithin(canonicalWorkspace, canonicalCache) {
		return nil, errStateCreate
	}

	root, err := operations.mkdirTemp(canonicalCache, stateLeafPrefix)
	if err != nil || !safeStateLeaf(canonicalCache, root) {
		return nil, errStateCreate
	}
	complete := false
	defer func() {
		if !complete {
			_ = removeStateRoot(canonicalCache, root)
		}
	}()

	if pathWithin(canonicalWorkspace, root) || verifyCreatedStatePath(root, true, 0o700, operations) != nil {
		return nil, errStateCreate
	}
	canonicalRoot, err := operations.canonicalize(root)
	if err != nil || canonicalRoot != root {
		return nil, errStateCreate
	}

	directories := sandbox.PrivateDirectories{
		Root:  root,
		Home:  filepath.Join(root, "home"),
		Temp:  filepath.Join(root, "tmp"),
		Cache: filepath.Join(root, "cache"),
	}
	profiles := filepath.Join(root, "profiles")
	for _, directory := range []string{directories.Home, directories.Temp, directories.Cache, profiles} {
		if err := operations.mkdir(directory, 0o700); err != nil ||
			verifyCreatedStatePath(directory, true, 0o700, operations) != nil {
			return nil, errStateCreate
		}
	}

	profilePath := filepath.Join(profiles, "profile.sb")
	profile, err := operations.openFile(profilePath, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return nil, errStateCreate
	}
	openedInfo, statErr := profile.Stat()
	closeErr := profile.Close()
	pathInfo, lstatErr := operations.lstat(profilePath)
	if statErr != nil || closeErr != nil || lstatErr != nil ||
		!secureStateInfo(openedInfo, false, 0o600, operations.currentUID()) ||
		!secureStateInfo(pathInfo, false, 0o600, operations.currentUID()) ||
		!os.SameFile(openedInfo, pathInfo) {
		return nil, errStateCreate
	}

	complete = true
	return &state{
		directories: directories,
		profiles:    profiles,
		profilePath: profilePath,
		rootParent:  canonicalCache,
	}, nil
}

//lint:ignore U1000 Task 5 writes the generated policy through this no-follow boundary.
func (s *state) writeProfile(profile []byte) error {
	if s == nil || !safeStateLeaf(s.rootParent, s.directories.Root) ||
		s.profiles != filepath.Join(s.directories.Root, "profiles") ||
		s.profilePath != filepath.Join(s.profiles, "profile.sb") {
		return errStateCreate
	}

	parentFD, err := unix.Open(s.rootParent, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return errStateCreate
	}
	defer unix.Close(parentFD)
	rootFD, err := unix.Openat(parentFD, filepath.Base(s.directories.Root), unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return errStateCreate
	}
	defer unix.Close(rootFD)
	if !secureStateDirectoryDescriptor(rootFD) {
		return errStateCreate
	}

	profilesFD := -1
	for _, name := range []string{"home", "tmp", "cache", "profiles"} {
		directoryFD, openErr := unix.Openat(rootFD, name, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
		if openErr != nil || !secureStateDirectoryDescriptor(directoryFD) {
			if directoryFD >= 0 {
				_ = unix.Close(directoryFD)
			}
			return errStateCreate
		}
		if name == "profiles" {
			profilesFD = directoryFD
			continue
		}
		if err := unix.Close(directoryFD); err != nil {
			return errStateCreate
		}
	}
	defer unix.Close(profilesFD)

	profileFD, err := unix.Openat(profilesFD, "profile.sb", unix.O_WRONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return errStateCreate
	}
	file := os.NewFile(uintptr(profileFD), "")
	if file == nil {
		_ = unix.Close(profileFD)
		return errStateCreate
	}
	info, err := file.Stat()
	if err != nil || !secureStateInfo(info, false, 0o600, os.Geteuid()) {
		_ = file.Close()
		return errStateCreate
	}
	if err := file.Truncate(0); err != nil {
		_ = file.Close()
		return errStateCreate
	}
	for len(profile) > 0 {
		written, writeErr := file.Write(profile)
		if writeErr != nil || written <= 0 {
			_ = file.Close()
			return errStateCreate
		}
		profile = profile[written:]
	}
	if err := file.Close(); err != nil {
		return errStateCreate
	}
	return nil
}

func secureStateDirectoryDescriptor(fileDescriptor int) bool {
	if fileDescriptor < 0 {
		return false
	}
	var stat unix.Stat_t
	return unix.Fstat(fileDescriptor, &stat) == nil &&
		stat.Mode&unix.S_IFMT == unix.S_IFDIR && stat.Mode&0o7777 == 0o700 &&
		int(stat.Uid) == os.Geteuid()
}

func (s *state) close() error {
	if s == nil {
		return nil
	}
	s.closeOnce.Do(func() {
		if err := removeStateRoot(s.rootParent, s.directories.Root); err != nil {
			s.closeErr = errStateCleanup
		}
	})
	return s.closeErr
}

func validStateOperations(operations stateOperations) bool {
	return operations.canonicalize != nil && operations.mkdirTemp != nil &&
		operations.mkdir != nil && operations.openFile != nil &&
		operations.lstat != nil && operations.currentUID != nil
}

func canonicalStateDirectory(path string, operations stateOperations) (string, error) {
	if path == "" || !filepath.IsAbs(path) || filepath.Clean(path) != path {
		return "", errStateCreate
	}
	canonical, err := operations.canonicalize(path)
	if err != nil || canonical == "" || !filepath.IsAbs(canonical) || filepath.Clean(canonical) != canonical {
		return "", errStateCreate
	}
	info, err := operations.lstat(canonical)
	if err != nil || info.Mode()&os.ModeSymlink != 0 || !info.IsDir() {
		return "", errStateCreate
	}
	return canonical, nil
}

func verifyCreatedStatePath(path string, directory bool, permissions fs.FileMode, operations stateOperations) error {
	info, err := operations.lstat(path)
	if err != nil || !secureStateInfo(info, directory, permissions, operations.currentUID()) {
		return errStateCreate
	}
	return nil
}

func secureStateInfo(info fs.FileInfo, directory bool, permissions fs.FileMode, uid int) bool {
	if info == nil || info.Mode()&(os.ModeSymlink|os.ModeSetuid|os.ModeSetgid|os.ModeSticky) != 0 ||
		info.Mode().Perm() != permissions || (directory && !info.IsDir()) ||
		(!directory && !info.Mode().IsRegular()) {
		return false
	}
	stat, ok := info.Sys().(*syscall.Stat_t)
	return ok && int(stat.Uid) == uid
}

func safeStateLeaf(parent, root string) bool {
	if parent == "" || root == "" || !filepath.IsAbs(parent) || !filepath.IsAbs(root) ||
		filepath.Clean(parent) != parent || filepath.Clean(root) != root || filepath.Dir(root) != parent {
		return false
	}
	leaf := filepath.Base(root)
	return strings.HasPrefix(leaf, stateLeafPrefix) && len(leaf) > len(stateLeafPrefix)
}

func removeStateRoot(parent, root string) error {
	if !safeStateLeaf(parent, root) {
		return errStateCleanup
	}
	parentFD, err := unix.Open(parent, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return errStateCleanup
	}
	defer unix.Close(parentFD)
	if err := removeStateEntry(parentFD, filepath.Base(root)); err != nil {
		return errStateCleanup
	}
	return nil
}

func removeStateEntry(parentFD int, name string) error {
	if name == "" || name == "." || name == ".." || strings.ContainsRune(name, filepath.Separator) {
		return errStateCleanup
	}
	for attempts := 0; attempts < 3; attempts++ {
		var stat unix.Stat_t
		err := unix.Fstatat(parentFD, name, &stat, unix.AT_SYMLINK_NOFOLLOW)
		if errors.Is(err, unix.ENOENT) {
			return nil
		}
		if err != nil {
			return errStateCleanup
		}
		if stat.Mode&unix.S_IFMT != unix.S_IFDIR {
			err = unix.Unlinkat(parentFD, name, 0)
			if err == nil || errors.Is(err, unix.ENOENT) {
				return nil
			}
			if errors.Is(err, unix.EISDIR) || errors.Is(err, unix.EPERM) {
				continue
			}
			return errStateCleanup
		}

		directoryFD, openErr := unix.Openat(parentFD, name, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
		if openErr != nil {
			if errors.Is(openErr, unix.ENOENT) || errors.Is(openErr, unix.ENOTDIR) || errors.Is(openErr, unix.ELOOP) {
				continue
			}
			if errors.Is(openErr, unix.EACCES) {
				err = unix.Unlinkat(parentFD, name, unix.AT_REMOVEDIR)
				if err == nil || errors.Is(err, unix.ENOENT) {
					return nil
				}
			}
			return errStateCleanup
		}

		directory := os.NewFile(uintptr(directoryFD), "")
		if directory == nil {
			_ = unix.Close(directoryFD)
			return errStateCleanup
		}
		entries, readErr := directory.ReadDir(-1)
		childrenOK := true
		for _, entry := range entries {
			if err := removeStateEntry(directoryFD, entry.Name()); err != nil {
				childrenOK = false
			}
		}
		closeErr := directory.Close()
		if readErr != nil || closeErr != nil || !childrenOK {
			return errStateCleanup
		}
		err = unix.Unlinkat(parentFD, name, unix.AT_REMOVEDIR)
		if err == nil || errors.Is(err, unix.ENOENT) {
			return nil
		}
		if errors.Is(err, unix.ENOTDIR) || errors.Is(err, unix.ENOTEMPTY) || errors.Is(err, unix.EEXIST) {
			continue
		}
		return errStateCleanup
	}
	return errStateCleanup
}
