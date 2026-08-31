//go:build darwin

package seatbelt

import (
	"bytes"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"io"
	"os"
	"path/filepath"

	"golang.org/x/sys/unix"
)

var errSelfTestFixture = errors.New("seatbelt self-test fixture unavailable")

const (
	selfTestFixturePrefix      = ".otto-self-test-"
	selfTestFixtureRandomBytes = 16
	selfTestFixtureMaxAttempts = 128
	selfTestFixtureMode        = 0o600
)

type selfTestFixtureKind uint8

const (
	selfTestAllowedReadFixture selfTestFixtureKind = iota + 1
	selfTestAllowedWorkspaceWriteFixture
	selfTestAllowedPrivateWriteFixture
	selfTestDeniedReadFixture
	selfTestDeniedWriteFixture
)

type selfTestFixtureEventStage uint8

const (
	selfTestFixtureCandidate selfTestFixtureEventStage = iota + 1
	selfTestFixtureBeforeCleanup
)

type selfTestFixtureEvent struct {
	stage selfTestFixtureEventStage
	kind  selfTestFixtureKind
	path  string
}

type selfTestFixture struct {
	kind       selfTestFixtureKind
	parentPath string
	parent     *os.File
	name       string
	path       string
	identity   stateIdentity
	owned      bool
}

type selfTestFixtures struct {
	values []*selfTestFixture
	event  func(selfTestFixtureEvent)
}

func productionSelfTestRandomBytes(_ selfTestFixtureKind, destination []byte) error {
	_, err := rand.Read(destination)
	return err
}

func prepareSelfTestFixtures(driver *Driver, dependencies driverDependencies, allowedReadContents, deniedContents string) (*selfTestFixtures, error) {
	fixtures := &selfTestFixtures{event: dependencies.selfTestEvent}
	definitions := []struct {
		kind       selfTestFixtureKind
		parentPath string
		contents   string
		hostCreate bool
	}{
		{kind: selfTestAllowedReadFixture, parentPath: driver.state.directories.Home, contents: allowedReadContents, hostCreate: true},
		{kind: selfTestAllowedWorkspaceWriteFixture, parentPath: driver.workspace},
		{kind: selfTestAllowedPrivateWriteFixture, parentPath: driver.state.directories.Temp},
		{kind: selfTestDeniedReadFixture, parentPath: driver.state.profiles, contents: deniedContents, hostCreate: true},
		{kind: selfTestDeniedWriteFixture, parentPath: driver.state.profiles, contents: deniedContents, hostCreate: true},
	}
	for _, definition := range definitions {
		fixture, err := prepareSelfTestFixture(definition.kind, definition.parentPath, definition.contents, definition.hostCreate, dependencies.selfTestRandomBytes, dependencies.selfTestEvent)
		if fixture != nil {
			fixtures.values = append(fixtures.values, fixture)
		}
		if err != nil {
			_ = fixtures.cleanup()
			return nil, err
		}
	}
	return fixtures, nil
}

func prepareSelfTestFixture(kind selfTestFixtureKind, parentPath, contents string, hostCreate bool, randomBytes func(selfTestFixtureKind, []byte) error, event func(selfTestFixtureEvent)) (*selfTestFixture, error) {
	parent, err := openStateDirectoryPath(parentPath)
	if err != nil || !stateDescriptorPathMatchesWithLstat(parentPath, parent, os.Lstat) {
		if parent != nil {
			_ = parent.Close()
		}
		return nil, sandboxSelfTestFixtureError()
	}
	fixture := &selfTestFixture{kind: kind, parentPath: parentPath, parent: parent}
	for attempt := 0; attempt < selfTestFixtureMaxAttempts; attempt++ {
		random := make([]byte, selfTestFixtureRandomBytes)
		if err := randomBytes(kind, random); err != nil {
			return fixture, sandboxSelfTestFixtureError()
		}
		fixture.name = selfTestFixturePrefix + hex.EncodeToString(random)
		fixture.path = filepath.Join(parentPath, fixture.name)
		emitSelfTestFixtureEvent(event, selfTestFixtureEvent{stage: selfTestFixtureCandidate, kind: kind, path: fixture.path})
		if hostCreate {
			created, createErr := createHostSelfTestFixture(fixture, []byte(contents))
			if errors.Is(createErr, unix.EEXIST) {
				continue
			}
			if createErr != nil || !created {
				return fixture, sandboxSelfTestFixtureError()
			}
			return fixture, nil
		}

		var stat unix.Stat_t
		err := unix.Fstatat(int(parent.Fd()), fixture.name, &stat, unix.AT_SYMLINK_NOFOLLOW)
		if errors.Is(err, unix.ENOENT) {
			return fixture, nil
		}
		if err != nil {
			return fixture, sandboxSelfTestFixtureError()
		}
	}
	return fixture, sandboxSelfTestFixtureError()
}

func createHostSelfTestFixture(fixture *selfTestFixture, contents []byte) (bool, error) {
	fd, err := unix.Openat(int(fixture.parent.Fd()), fixture.name, unix.O_WRONLY|unix.O_CREAT|unix.O_EXCL|unix.O_CLOEXEC|unix.O_NOFOLLOW, selfTestFixtureMode)
	if err != nil {
		return false, err
	}
	open := true
	defer func() {
		if open {
			_ = unix.Close(fd)
		}
	}()

	stat, statErr := stateFDStat(fd)
	if statErr == nil {
		fixture.identity = stateIdentityFromStat(stat)
	}
	if statErr != nil || unix.Fchmod(fd, selfTestFixtureMode) != nil {
		return false, sandboxSelfTestFixtureError()
	}
	stat, statErr = stateFDStat(fd)
	fixture.identity = stateIdentityFromStat(stat)
	fixture.owned = statErr == nil && validSelfTestFixtureStat(stat, fixture.identity)
	if !fixture.owned || !writeSelfTestFixture(fd, contents) {
		return false, sandboxSelfTestFixtureError()
	}
	open = false
	if err := unix.Close(fd); err != nil {
		return false, sandboxSelfTestFixtureError()
	}
	if !fixture.edgeMatches() {
		return false, sandboxSelfTestFixtureError()
	}
	return true, nil
}

func writeSelfTestFixture(fd int, contents []byte) bool {
	for len(contents) > 0 {
		written, err := unix.Write(fd, contents)
		if errors.Is(err, unix.EINTR) {
			continue
		}
		if err != nil || written <= 0 {
			return false
		}
		contents = contents[written:]
	}
	return true
}

func (fixtures *selfTestFixtures) fixture(kind selfTestFixtureKind) *selfTestFixture {
	if fixtures == nil {
		return nil
	}
	for _, fixture := range fixtures.values {
		if fixture.kind == kind {
			return fixture
		}
	}
	return nil
}

func (fixture *selfTestFixture) proveAbsent() error {
	if fixture == nil || fixture.parent == nil || fixture.owned || !stateDescriptorPathMatchesWithLstat(fixture.parentPath, fixture.parent, os.Lstat) {
		return sandboxSelfTestFixtureError()
	}
	var stat unix.Stat_t
	err := unix.Fstatat(int(fixture.parent.Fd()), fixture.name, &stat, unix.AT_SYMLINK_NOFOLLOW)
	if !errors.Is(err, unix.ENOENT) {
		return sandboxSelfTestFixtureError()
	}
	return nil
}

func (fixture *selfTestFixture) adoptChildCreated(contents string) error {
	if fixture == nil || fixture.parent == nil || fixture.owned || !stateDescriptorPathMatchesWithLstat(fixture.parentPath, fixture.parent, os.Lstat) {
		return sandboxSelfTestFixtureError()
	}
	var edge unix.Stat_t
	if err := unix.Fstatat(int(fixture.parent.Fd()), fixture.name, &edge, unix.AT_SYMLINK_NOFOLLOW); err != nil {
		return sandboxSelfTestFixtureError()
	}
	identity := stateIdentityFromStat(edge)
	if !validSelfTestFixtureStat(edge, identity) {
		return sandboxSelfTestFixtureError()
	}
	fd, err := unix.Openat(int(fixture.parent.Fd()), fixture.name, unix.O_RDONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return sandboxSelfTestFixtureError()
	}
	file := os.NewFile(uintptr(fd), "")
	if file == nil {
		_ = unix.Close(fd)
		return sandboxSelfTestFixtureError()
	}
	data, readErr := io.ReadAll(io.LimitReader(file, int64(len(contents)+1)))
	stat, statErr := stateDescriptorStat(file)
	closeErr := file.Close()
	if readErr != nil || statErr != nil || closeErr != nil || !bytes.Equal(data, []byte(contents)) ||
		!validSelfTestFixtureStat(stat, identity) {
		return sandboxSelfTestFixtureError()
	}
	fixture.identity = identity
	fixture.owned = true
	if !fixture.edgeMatches() {
		fixture.owned = false
		return sandboxSelfTestFixtureError()
	}
	return nil
}

func (fixture *selfTestFixture) validateContents(contents string) error {
	if fixture == nil || !fixture.owned || !fixture.edgeMatches() {
		return sandboxSelfTestFixtureError()
	}
	fd, err := unix.Openat(int(fixture.parent.Fd()), fixture.name, unix.O_RDONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return sandboxSelfTestFixtureError()
	}
	file := os.NewFile(uintptr(fd), "")
	if file == nil {
		_ = unix.Close(fd)
		return sandboxSelfTestFixtureError()
	}
	data, readErr := io.ReadAll(io.LimitReader(file, int64(len(contents)+1)))
	stat, statErr := stateDescriptorStat(file)
	closeErr := file.Close()
	if readErr != nil || statErr != nil || closeErr != nil || !bytes.Equal(data, []byte(contents)) ||
		!validSelfTestFixtureStat(stat, fixture.identity) || !fixture.edgeMatches() {
		return sandboxSelfTestFixtureError()
	}
	return nil
}

func (fixture *selfTestFixture) edgeMatches() bool {
	if fixture == nil || fixture.parent == nil || fixture.identity == (stateIdentity{}) ||
		!stateDescriptorPathMatchesWithLstat(fixture.parentPath, fixture.parent, os.Lstat) {
		return false
	}
	var stat unix.Stat_t
	return unix.Fstatat(int(fixture.parent.Fd()), fixture.name, &stat, unix.AT_SYMLINK_NOFOLLOW) == nil &&
		validSelfTestFixtureStat(stat, fixture.identity)
}

func validSelfTestFixtureStat(stat unix.Stat_t, identity stateIdentity) bool {
	return secureStateStat(stat, false, selfTestFixtureMode, os.Geteuid()) && stat.Nlink == 1 &&
		sameStateIdentity(stateIdentityFromStat(stat), identity)
}

func (fixtures *selfTestFixtures) cleanup() error {
	if fixtures == nil {
		return nil
	}
	failed := false
	for _, fixture := range fixtures.values {
		if fixture == nil {
			failed = true
			continue
		}
		if fixture.owned {
			emitSelfTestFixtureEvent(fixtures.event, selfTestFixtureEvent{stage: selfTestFixtureBeforeCleanup, kind: fixture.kind, path: fixture.path})
			if !fixture.edgeMatches() {
				failed = true
			} else {
				unlinkErr := unix.Unlinkat(int(fixture.parent.Fd()), fixture.name, 0)
				var remaining unix.Stat_t
				statErr := unix.Fstatat(int(fixture.parent.Fd()), fixture.name, &remaining, unix.AT_SYMLINK_NOFOLLOW)
				if unlinkErr != nil && !errors.Is(unlinkErr, unix.ENOENT) || !errors.Is(statErr, unix.ENOENT) {
					failed = true
				}
			}
		}
		if fixture.parent != nil {
			if err := fixture.parent.Close(); err != nil {
				failed = true
			}
			fixture.parent = nil
		}
	}
	if failed {
		return sandboxSelfTestFixtureError()
	}
	return nil
}

func emitSelfTestFixtureEvent(event func(selfTestFixtureEvent), value selfTestFixtureEvent) {
	if event != nil {
		event(value)
	}
}

func sandboxSelfTestFixtureError() error {
	return errSelfTestFixture
}
