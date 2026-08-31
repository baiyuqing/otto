//go:build darwin || linux

package seatbelt

import (
	"crypto/rand"
	"encoding/hex"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"sync"

	"github.com/baiyuqing/otto/internal/sandbox"
	"golang.org/x/sys/unix"
)

const (
	stateLeafPrefix       = "otto-sandbox-"
	stateLeafRandomBytes  = 16
	stateLeafMaxAttempts  = 128
	stateDirectoryMode    = 0o700
	stateProfileFileMode  = 0o600
	stateRootChildrenSize = 4
)

var (
	errStateCreate  = errors.New("seatbelt private state unavailable")
	errStateCleanup = errors.New("seatbelt private state cleanup failed")
)

type stateEventKind uint8

const (
	stateEventRootCreated stateEventKind = iota + 1
	stateEventRootValidated
	stateEventDirectoryCreated
	stateEventProfileCreated
	stateEventProfileWrite
	stateEventCleanup
)

type stateEvent struct {
	kind stateEventKind
	name string
	path string
}

type stateIdentity struct {
	device uint64
	inode  uint64
}

type state struct {
	directories sandbox.PrivateDirectories
	profiles    string
	profilePath string
	rootParent  string

	parentFile      *os.File
	rootFile        *os.File
	rootName        string
	rootIdentity    stateIdentity
	childIdentities map[string]stateIdentity
	profileIdentity stateIdentity
	event           func(stateEvent)
	lstat           func(string) (fs.FileInfo, error)
	mu              sync.Mutex
	closed          bool
	closeOnce       sync.Once
	closeErr        error
}

type stateOperations struct {
	canonicalize func(string) (string, error)
	lstat        func(string) (fs.FileInfo, error)
	currentUID   func() int
	randomBytes  func([]byte) error
	mkdirat      func(int, string, uint32) error
	event        func(stateEvent)
}

func defaultStateOperations() stateOperations {
	return stateOperations{
		canonicalize: canonicalFilesystemPath,
		lstat:        os.Lstat,
		currentUID:   os.Geteuid,
		randomBytes: func(destination []byte) error {
			_, err := rand.Read(destination)
			return err
		},
		mkdirat: unix.Mkdirat,
	}
}

func createState(workspace, cacheBase string) (*state, error) {
	return createStateWithOperations(workspace, cacheBase, defaultStateOperations())
}

func createStateWithOperations(workspace, cacheBase string, operations stateOperations) (*state, error) {
	if !validStateOperations(operations) {
		return nil, errStateCreate
	}
	canonicalWorkspace, _, err := canonicalStateDirectory(workspace, operations)
	if err != nil {
		return nil, errStateCreate
	}
	canonicalCache, cacheInfo, err := canonicalStateDirectory(cacheBase, operations)
	if err != nil || pathWithin(canonicalWorkspace, canonicalCache) {
		return nil, errStateCreate
	}

	parentFile, err := openStateDirectoryPath(canonicalCache)
	if err != nil || !stateDescriptorMatchesInfo(parentFile, cacheInfo) {
		if parentFile != nil {
			_ = parentFile.Close()
		}
		return nil, errStateCreate
	}
	parentOwned := true
	defer func() {
		if parentOwned {
			_ = parentFile.Close()
		}
	}()

	rootName, rootPath, rootEdge, err := createRandomStateRoot(parentFile, canonicalCache, operations)
	if err != nil {
		return nil, errStateCreate
	}
	var rootFile *os.File
	var rootIdentity stateIdentity
	rootIdentityKnown := false
	constructionComplete := false
	defer func() {
		if constructionComplete {
			return
		}
		if rootFile != nil && rootIdentityKnown {
			_ = cleanupStateRoot(parentFile, rootFile, rootName, rootIdentity, nil)
		} else {
			_ = removeExpectedStateEdge(int(parentFile.Fd()), rootName, rootEdge)
		}
		if rootFile != nil {
			_ = rootFile.Close()
		}
	}()

	emitStateEvent(operations.event, stateEvent{kind: stateEventRootCreated, name: rootName, path: rootPath})
	rootFile, err = openStateDirectoryAt(int(parentFile.Fd()), rootName)
	if err != nil {
		return nil, errStateCreate
	}
	rootStat, err := stateDescriptorStat(rootFile)
	if err != nil || !secureStateStat(rootStat, true, stateDirectoryMode, operations.currentUID()) ||
		!sameStateIdentity(stateIdentityFromStat(rootStat), stateIdentityFromStat(rootEdge)) ||
		!stateEdgeMatches(int(parentFile.Fd()), rootName, stateIdentityFromStat(rootStat), true, stateDirectoryMode, operations.currentUID()) {
		return nil, errStateCreate
	}
	rootIdentity = stateIdentityFromStat(rootStat)
	rootIdentityKnown = true
	emitStateEvent(operations.event, stateEvent{kind: stateEventRootValidated, name: rootName, path: rootPath})

	directories := sandbox.PrivateDirectories{
		Root:  rootPath,
		Home:  filepath.Join(rootPath, "home"),
		Temp:  filepath.Join(rootPath, "tmp"),
		Cache: filepath.Join(rootPath, "cache"),
	}
	profiles := filepath.Join(rootPath, "profiles")
	childIdentities := make(map[string]stateIdentity, stateRootChildrenSize)
	var profilesFile *os.File
	for _, child := range []struct {
		name string
		path string
	}{
		{name: "home", path: directories.Home},
		{name: "tmp", path: directories.Temp},
		{name: "cache", path: directories.Cache},
		{name: "profiles", path: profiles},
	} {
		childFile, identity, createErr := createStateDirectoryAt(rootFile, child.name, child.path, operations)
		if createErr != nil {
			return nil, errStateCreate
		}
		childIdentities[child.name] = identity
		if child.name == "profiles" {
			profilesFile = childFile
			continue
		}
		if closeErr := childFile.Close(); closeErr != nil {
			return nil, errStateCreate
		}
	}
	if profilesFile == nil {
		return nil, errStateCreate
	}

	profilePath := filepath.Join(profiles, "profile.sb")
	profileIdentity, err := createStateProfileAt(profilesFile, profilePath, operations)
	profilesCloseErr := profilesFile.Close()
	if err != nil || profilesCloseErr != nil {
		return nil, errStateCreate
	}
	if !stateEdgeMatches(int(parentFile.Fd()), rootName, rootIdentity, true, stateDirectoryMode, operations.currentUID()) ||
		!stateDescriptorPathMatches(canonicalCache, parentFile, operations) ||
		!stateDescriptorPathMatches(rootPath, rootFile, operations) ||
		pathWithin(canonicalWorkspace, rootPath) {
		return nil, errStateCreate
	}

	constructionComplete = true
	parentOwned = false
	return &state{
		directories:     directories,
		profiles:        profiles,
		profilePath:     profilePath,
		rootParent:      canonicalCache,
		parentFile:      parentFile,
		rootFile:        rootFile,
		rootName:        rootName,
		rootIdentity:    rootIdentity,
		childIdentities: childIdentities,
		profileIdentity: profileIdentity,
		event:           operations.event,
		lstat:           operations.lstat,
	}, nil
}

//lint:ignore U1000 Task 5 writes the generated policy through this no-follow boundary.
func (s *state) writeProfile(profile []byte) error {
	if s == nil {
		return errStateCreate
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed || !s.validShape() || !s.validRetainedRoot() {
		return errStateCreate
	}

	emitStateEvent(s.event, stateEvent{kind: stateEventProfileWrite, name: s.rootName, path: s.directories.Root})
	if !stateEdgeMatches(int(s.parentFile.Fd()), s.rootName, s.rootIdentity, true, stateDirectoryMode, os.Geteuid()) ||
		!stateDescriptorPathMatchesWithLstat(s.rootParent, s.parentFile, s.lstat) ||
		!stateDescriptorPathMatchesWithLstat(s.directories.Root, s.rootFile, s.lstat) {
		return errStateCreate
	}

	profilesFile := (*os.File)(nil)
	for _, name := range []string{"home", "tmp", "cache", "profiles"} {
		identity, ok := s.childIdentities[name]
		if !ok {
			if profilesFile != nil {
				_ = profilesFile.Close()
			}
			return errStateCreate
		}
		childFile, err := openStateDirectoryAt(int(s.rootFile.Fd()), name)
		if err != nil || !stateDescriptorMatchesExpected(childFile, identity, true, stateDirectoryMode, os.Geteuid()) ||
			!stateEdgeMatches(int(s.rootFile.Fd()), name, identity, true, stateDirectoryMode, os.Geteuid()) {
			if childFile != nil {
				_ = childFile.Close()
			}
			if profilesFile != nil {
				_ = profilesFile.Close()
			}
			return errStateCreate
		}
		if name == "profiles" {
			profilesFile = childFile
			continue
		}
		if err := childFile.Close(); err != nil {
			return errStateCreate
		}
	}
	if profilesFile == nil {
		return errStateCreate
	}
	defer profilesFile.Close()

	profileFD, err := unix.Openat(int(profilesFile.Fd()), "profile.sb", unix.O_WRONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return errStateCreate
	}
	profileOpen := true
	defer func() {
		if profileOpen {
			_ = unix.Close(profileFD)
		}
	}()
	profileStat, err := stateFDStat(profileFD)
	if err != nil || !secureStateStat(profileStat, false, stateProfileFileMode, os.Geteuid()) ||
		profileStat.Nlink != 1 || !sameStateIdentity(stateIdentityFromStat(profileStat), s.profileIdentity) ||
		!stateEdgeMatches(int(profilesFile.Fd()), "profile.sb", s.profileIdentity, false, stateProfileFileMode, os.Geteuid()) ||
		!stateEdgeMatches(int(s.parentFile.Fd()), s.rootName, s.rootIdentity, true, stateDirectoryMode, os.Geteuid()) {
		return errStateCreate
	}
	if err := unix.Ftruncate(profileFD, 0); err != nil {
		return errStateCreate
	}
	for len(profile) > 0 {
		written, writeErr := unix.Write(profileFD, profile)
		if errors.Is(writeErr, unix.EINTR) {
			continue
		}
		if writeErr != nil || written <= 0 {
			return errStateCreate
		}
		profile = profile[written:]
	}
	if err := unix.Close(profileFD); err != nil {
		return errStateCreate
	}
	profileOpen = false
	return nil
}

func (s *state) validShape() bool {
	return s.parentFile != nil && s.rootFile != nil && s.lstat != nil && safeStateLeaf(s.rootParent, s.directories.Root) &&
		s.rootName == filepath.Base(s.directories.Root) &&
		s.directories.Home == filepath.Join(s.directories.Root, "home") &&
		s.directories.Temp == filepath.Join(s.directories.Root, "tmp") &&
		s.directories.Cache == filepath.Join(s.directories.Root, "cache") &&
		s.profiles == filepath.Join(s.directories.Root, "profiles") &&
		s.profilePath == filepath.Join(s.profiles, "profile.sb") &&
		len(s.childIdentities) == stateRootChildrenSize
}

func (s *state) validRetainedRoot() bool {
	return stateDescriptorMatchesExpected(s.rootFile, s.rootIdentity, true, stateDirectoryMode, os.Geteuid())
}

func (s *state) close() error {
	if s == nil {
		return nil
	}
	s.closeOnce.Do(func() {
		s.mu.Lock()
		defer s.mu.Unlock()
		s.closed = true

		rootUsable := s.validShape() && s.validRetainedRoot()
		cleanupFailed := !rootUsable
		emitStateEvent(s.event, stateEvent{kind: stateEventCleanup, name: s.rootName, path: s.directories.Root})
		if rootUsable {
			if !stateDescriptorPathMatchesWithLstat(s.rootParent, s.parentFile, s.lstat) ||
				!stateDescriptorPathMatchesWithLstat(s.directories.Root, s.rootFile, s.lstat) {
				cleanupFailed = true
			}
			if err := cleanupStateRoot(s.parentFile, s.rootFile, s.rootName, s.rootIdentity, s.childIdentities); err != nil {
				cleanupFailed = true
			}
		}
		if s.rootFile != nil {
			if err := s.rootFile.Close(); err != nil {
				cleanupFailed = true
			}
			s.rootFile = nil
		}
		if s.parentFile != nil {
			if err := s.parentFile.Close(); err != nil {
				cleanupFailed = true
			}
			s.parentFile = nil
		}
		if cleanupFailed {
			s.closeErr = errStateCleanup
		}
	})
	return s.closeErr
}

func validStateOperations(operations stateOperations) bool {
	return operations.canonicalize != nil && operations.lstat != nil && operations.currentUID != nil &&
		operations.randomBytes != nil && operations.mkdirat != nil
}

func canonicalStateDirectory(path string, operations stateOperations) (string, fs.FileInfo, error) {
	if path == "" || !filepath.IsAbs(path) || filepath.Clean(path) != path {
		return "", nil, errStateCreate
	}
	canonical, err := operations.canonicalize(path)
	if err != nil || canonical == "" || !filepath.IsAbs(canonical) || filepath.Clean(canonical) != canonical {
		return "", nil, errStateCreate
	}
	info, err := operations.lstat(canonical)
	if err != nil || info.Mode()&os.ModeSymlink != 0 || !info.IsDir() {
		return "", nil, errStateCreate
	}
	return canonical, info, nil
}

func openStateDirectoryPath(path string) (*os.File, error) {
	if path == "" || !filepath.IsAbs(path) || filepath.Clean(path) != path {
		return nil, errStateCreate
	}
	rootFD, err := unix.Open(string(filepath.Separator), unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return nil, errStateCreate
	}
	current := os.NewFile(uintptr(rootFD), "")
	if current == nil {
		_ = unix.Close(rootFD)
		return nil, errStateCreate
	}
	if path == string(filepath.Separator) {
		return current, nil
	}
	for _, component := range strings.Split(strings.TrimPrefix(path, string(filepath.Separator)), string(filepath.Separator)) {
		next, openErr := openStateDirectoryAt(int(current.Fd()), component)
		closeErr := current.Close()
		if openErr != nil || closeErr != nil {
			if next != nil {
				_ = next.Close()
			}
			return nil, errStateCreate
		}
		current = next
	}
	return current, nil
}

func openStateDirectoryAt(parentFD int, name string) (*os.File, error) {
	if !validStateEntryName(name) {
		return nil, errStateCreate
	}
	fd, err := unix.Openat(parentFD, name, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return nil, err
	}
	file := os.NewFile(uintptr(fd), "")
	if file == nil {
		_ = unix.Close(fd)
		return nil, errStateCreate
	}
	return file, nil
}

func createRandomStateRoot(parentFile *os.File, parentPath string, operations stateOperations) (string, string, unix.Stat_t, error) {
	for attempt := 0; attempt < stateLeafMaxAttempts; attempt++ {
		random := make([]byte, stateLeafRandomBytes)
		if err := operations.randomBytes(random); err != nil {
			return "", "", unix.Stat_t{}, errStateCreate
		}
		name := stateLeafPrefix + hex.EncodeToString(random)
		err := operations.mkdirat(int(parentFile.Fd()), name, stateDirectoryMode)
		if errors.Is(err, unix.EEXIST) {
			continue
		}
		if err != nil {
			return "", "", unix.Stat_t{}, errStateCreate
		}
		var edge unix.Stat_t
		if err := unix.Fstatat(int(parentFile.Fd()), name, &edge, unix.AT_SYMLINK_NOFOLLOW); err != nil {
			if removeErr := unix.Unlinkat(int(parentFile.Fd()), name, unix.AT_REMOVEDIR); removeErr != nil {
				_ = unix.Unlinkat(int(parentFile.Fd()), name, 0)
			}
			return "", "", unix.Stat_t{}, errStateCreate
		}
		return name, filepath.Join(parentPath, name), edge, nil
	}
	return "", "", unix.Stat_t{}, errStateCreate
}

func createStateDirectoryAt(parentFile *os.File, name, path string, operations stateOperations) (*os.File, stateIdentity, error) {
	if !validStateEntryName(name) || operations.mkdirat(int(parentFile.Fd()), name, stateDirectoryMode) != nil {
		return nil, stateIdentity{}, errStateCreate
	}
	var createdEdge unix.Stat_t
	if err := unix.Fstatat(int(parentFile.Fd()), name, &createdEdge, unix.AT_SYMLINK_NOFOLLOW); err != nil {
		return nil, stateIdentity{}, errStateCreate
	}
	emitStateEvent(operations.event, stateEvent{kind: stateEventDirectoryCreated, name: name, path: path})
	file, err := openStateDirectoryAt(int(parentFile.Fd()), name)
	if err != nil {
		return nil, stateIdentity{}, errStateCreate
	}
	stat, err := stateDescriptorStat(file)
	identity := stateIdentityFromStat(stat)
	if err != nil || !secureStateStat(stat, true, stateDirectoryMode, operations.currentUID()) ||
		!sameStateIdentity(identity, stateIdentityFromStat(createdEdge)) ||
		!stateEdgeMatches(int(parentFile.Fd()), name, identity, true, stateDirectoryMode, operations.currentUID()) {
		_ = file.Close()
		return nil, stateIdentity{}, errStateCreate
	}
	return file, identity, nil
}

func createStateProfileAt(profilesFile *os.File, path string, operations stateOperations) (stateIdentity, error) {
	fd, err := unix.Openat(int(profilesFile.Fd()), "profile.sb", unix.O_WRONLY|unix.O_CREAT|unix.O_EXCL|unix.O_CLOEXEC|unix.O_NOFOLLOW, stateProfileFileMode)
	if err != nil {
		return stateIdentity{}, errStateCreate
	}
	open := true
	defer func() {
		if open {
			_ = unix.Close(fd)
		}
	}()
	emitStateEvent(operations.event, stateEvent{kind: stateEventProfileCreated, name: "profile.sb", path: path})
	stat, err := stateFDStat(fd)
	identity := stateIdentityFromStat(stat)
	if err != nil || !secureStateStat(stat, false, stateProfileFileMode, operations.currentUID()) || stat.Nlink != 1 ||
		!stateEdgeMatches(int(profilesFile.Fd()), "profile.sb", identity, false, stateProfileFileMode, operations.currentUID()) {
		return stateIdentity{}, errStateCreate
	}
	if err := unix.Close(fd); err != nil {
		return stateIdentity{}, errStateCreate
	}
	open = false
	return identity, nil
}

func stateDescriptorStat(file *os.File) (unix.Stat_t, error) {
	if file == nil {
		return unix.Stat_t{}, errStateCreate
	}
	return stateFDStat(int(file.Fd()))
}

func stateFDStat(fd int) (unix.Stat_t, error) {
	if fd < 0 {
		return unix.Stat_t{}, errStateCreate
	}
	var stat unix.Stat_t
	if err := unix.Fstat(fd, &stat); err != nil {
		return unix.Stat_t{}, err
	}
	return stat, nil
}

func secureStateStat(stat unix.Stat_t, directory bool, permissions fs.FileMode, uid int) bool {
	expectedType := uint32(unix.S_IFREG)
	if directory {
		expectedType = unix.S_IFDIR
	}
	return uint32(stat.Mode)&uint32(unix.S_IFMT) == expectedType &&
		uint32(stat.Mode)&0o7777 == uint32(permissions) && int(stat.Uid) == uid
}

func stateIdentityFromStat(stat unix.Stat_t) stateIdentity {
	return stateIdentity{device: uint64(stat.Dev), inode: uint64(stat.Ino)}
}

func sameStateIdentity(left, right stateIdentity) bool {
	return left == right && left != (stateIdentity{})
}

func stateEdgeMatches(parentFD int, name string, identity stateIdentity, directory bool, permissions fs.FileMode, uid int) bool {
	if parentFD < 0 || !validStateEntryName(name) {
		return false
	}
	var stat unix.Stat_t
	return unix.Fstatat(parentFD, name, &stat, unix.AT_SYMLINK_NOFOLLOW) == nil &&
		secureStateStat(stat, directory, permissions, uid) &&
		sameStateIdentity(stateIdentityFromStat(stat), identity)
}

func stateEdgeIdentityMatches(parentFD int, name string, identity stateIdentity, directory bool) bool {
	if parentFD < 0 || !validStateEntryName(name) {
		return false
	}
	var stat unix.Stat_t
	if unix.Fstatat(parentFD, name, &stat, unix.AT_SYMLINK_NOFOLLOW) != nil {
		return false
	}
	expectedType := uint32(unix.S_IFREG)
	if directory {
		expectedType = unix.S_IFDIR
	}
	return uint32(stat.Mode)&uint32(unix.S_IFMT) == expectedType &&
		sameStateIdentity(stateIdentityFromStat(stat), identity)
}

func stateDescriptorMatchesExpected(file *os.File, identity stateIdentity, directory bool, permissions fs.FileMode, uid int) bool {
	stat, err := stateDescriptorStat(file)
	return err == nil && secureStateStat(stat, directory, permissions, uid) &&
		sameStateIdentity(stateIdentityFromStat(stat), identity)
}

func stateDescriptorMatchesInfo(file *os.File, expected fs.FileInfo) bool {
	if file == nil || expected == nil {
		return false
	}
	actual, err := file.Stat()
	return err == nil && actual.IsDir() && os.SameFile(actual, expected)
}

func stateDescriptorPathMatches(path string, file *os.File, operations stateOperations) bool {
	return stateDescriptorPathMatchesWithLstat(path, file, operations.lstat)
}

func stateDescriptorPathMatchesWithLstat(path string, file *os.File, lstat func(string) (fs.FileInfo, error)) bool {
	if file == nil || lstat == nil {
		return false
	}
	pathInfo, err := lstat(path)
	if err != nil || pathInfo.Mode()&os.ModeSymlink != 0 {
		return false
	}
	descriptorInfo, err := file.Stat()
	return err == nil && os.SameFile(pathInfo, descriptorInfo)
}

func emitStateEvent(hook func(stateEvent), event stateEvent) {
	if hook != nil {
		hook(event)
	}
}

func safeStateLeaf(parent, root string) bool {
	if parent == "" || root == "" || !filepath.IsAbs(parent) || !filepath.IsAbs(root) ||
		filepath.Clean(parent) != parent || filepath.Clean(root) != root || filepath.Dir(root) != parent {
		return false
	}
	leaf := filepath.Base(root)
	return strings.HasPrefix(leaf, stateLeafPrefix) && len(leaf) > len(stateLeafPrefix)
}

func cleanupStateRoot(parentFile, rootFile *os.File, rootName string, rootIdentity stateIdentity, expectedChildren map[string]stateIdentity) error {
	if parentFile == nil || rootFile == nil || !validStateEntryName(rootName) ||
		!stateDescriptorMatchesExpected(rootFile, rootIdentity, true, stateDirectoryMode, os.Geteuid()) {
		return errStateCleanup
	}
	edgeMatches := stateEdgeMatches(int(parentFile.Fd()), rootName, rootIdentity, true, stateDirectoryMode, os.Geteuid())
	contentsErr := removeStateDirectoryContents(int(rootFile.Fd()), expectedChildren)
	if contentsErr != nil || !edgeMatches ||
		!stateEdgeMatches(int(parentFile.Fd()), rootName, rootIdentity, true, stateDirectoryMode, os.Geteuid()) {
		return errStateCleanup
	}
	if err := unix.Unlinkat(int(parentFile.Fd()), rootName, unix.AT_REMOVEDIR); err != nil && !errors.Is(err, unix.ENOENT) {
		return errStateCleanup
	}
	return nil
}

func removeStateDirectoryContents(directoryFD int, expectedChildren map[string]stateIdentity) error {
	readFD, err := unix.Openat(directoryFD, ".", unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return errStateCleanup
	}
	directory := os.NewFile(uintptr(readFD), "")
	if directory == nil {
		_ = unix.Close(readFD)
		return errStateCleanup
	}
	entries, readErr := directory.ReadDir(-1)
	closeErr := directory.Close()
	if readErr != nil || closeErr != nil {
		return errStateCleanup
	}
	childrenOK := true
	for _, entry := range entries {
		var expected *stateIdentity
		if identity, tracked := expectedChildren[entry.Name()]; tracked {
			expected = &identity
		}
		if err := removeStateEntry(directoryFD, entry.Name(), expected); err != nil {
			childrenOK = false
		}
	}
	if !childrenOK {
		return errStateCleanup
	}
	return nil
}

func removeExpectedStateEdge(parentFD int, name string, expected unix.Stat_t) error {
	if parentFD < 0 || !validStateEntryName(name) {
		return errStateCleanup
	}
	var current unix.Stat_t
	if err := unix.Fstatat(parentFD, name, &current, unix.AT_SYMLINK_NOFOLLOW); errors.Is(err, unix.ENOENT) {
		return nil
	} else if err != nil || !sameStateIdentity(stateIdentityFromStat(current), stateIdentityFromStat(expected)) ||
		uint32(current.Mode)&uint32(unix.S_IFMT) != uint32(expected.Mode)&uint32(unix.S_IFMT) {
		return errStateCleanup
	}
	flags := 0
	if uint32(current.Mode)&uint32(unix.S_IFMT) == uint32(unix.S_IFDIR) {
		flags = unix.AT_REMOVEDIR
	}
	if err := unix.Unlinkat(parentFD, name, flags); err != nil && !errors.Is(err, unix.ENOENT) {
		return errStateCleanup
	}
	return nil
}

func removeStateEntry(parentFD int, name string, expected *stateIdentity) error {
	if parentFD < 0 || !validStateEntryName(name) {
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
		statIdentity := stateIdentityFromStat(stat)
		isDirectory := uint32(stat.Mode)&uint32(unix.S_IFMT) == uint32(unix.S_IFDIR)
		if expected != nil && !sameStateIdentity(statIdentity, *expected) && isDirectory {
			return errStateCleanup
		}
		if !isDirectory {
			err = unix.Unlinkat(parentFD, name, 0)
			if err == nil || errors.Is(err, unix.ENOENT) {
				return nil
			}
			if errors.Is(err, unix.EISDIR) || errors.Is(err, unix.EPERM) {
				continue
			}
			return errStateCleanup
		}

		directoryFile, openErr := openStateDirectoryAt(parentFD, name)
		if openErr != nil {
			if errors.Is(openErr, unix.ENOENT) || errors.Is(openErr, unix.ENOTDIR) || errors.Is(openErr, unix.ELOOP) {
				continue
			}
			if errors.Is(openErr, unix.EACCES) && stateEdgeIdentityMatches(parentFD, name, statIdentity, true) {
				err = unix.Unlinkat(parentFD, name, unix.AT_REMOVEDIR)
				if err == nil || errors.Is(err, unix.ENOENT) {
					return nil
				}
			}
			return errStateCleanup
		}
		openedStat, openedStatErr := stateDescriptorStat(directoryFile)
		openedIdentity := stateIdentityFromStat(openedStat)
		if openedStatErr != nil || !sameStateIdentity(openedIdentity, statIdentity) ||
			expected != nil && !sameStateIdentity(openedIdentity, *expected) {
			_ = directoryFile.Close()
			continue
		}
		removeErr := removeStateDirectoryContents(int(directoryFile.Fd()), nil)
		closeErr := directoryFile.Close()
		if closeErr != nil {
			return errStateCleanup
		}
		if removeErr != nil {
			if !stateEdgeIdentityMatches(parentFD, name, openedIdentity, true) {
				return errStateCleanup
			}
			err = unix.Unlinkat(parentFD, name, unix.AT_REMOVEDIR)
			if err == nil || errors.Is(err, unix.ENOENT) {
				return nil
			}
			return errStateCleanup
		}
		if !stateEdgeIdentityMatches(parentFD, name, openedIdentity, true) {
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

func validStateEntryName(name string) bool {
	return name != "" && name != "." && name != ".." && !strings.ContainsRune(name, filepath.Separator)
}
