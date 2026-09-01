//go:build darwin

package seatbelt

import (
	"crypto/rand"
	"encoding/hex"
	"errors"
	"os"
	"path/filepath"
	"strconv"

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
	selfTestFixtureBeforeDispatch
	selfTestFixtureBeforeReadback
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

	retainedFD   int
	retainedOpen bool
	closeFD      func(int) error
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
	}{
		{kind: selfTestAllowedReadFixture, parentPath: driver.state.directories.Home, contents: allowedReadContents},
		{kind: selfTestAllowedWorkspaceWriteFixture, parentPath: driver.workspace},
		{kind: selfTestAllowedPrivateWriteFixture, parentPath: driver.state.directories.Temp},
		{kind: selfTestDeniedReadFixture, parentPath: driver.state.profiles, contents: deniedContents},
		{kind: selfTestDeniedWriteFixture, parentPath: driver.state.profiles, contents: deniedContents},
	}
	for _, definition := range definitions {
		fixture, err := prepareSelfTestFixture(
			definition.kind,
			definition.parentPath,
			definition.contents,
			dependencies.selfTestRandomBytes,
			dependencies.selfTestCloseFD,
			dependencies.selfTestEvent,
		)
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

func prepareSelfTestFixture(kind selfTestFixtureKind, parentPath, contents string, randomBytes func(selfTestFixtureKind, []byte) error, closeFD func(int) error, event func(selfTestFixtureEvent)) (*selfTestFixture, error) {
	parent, err := openStateDirectoryPath(parentPath)
	if err != nil || !stateDescriptorPathMatchesWithLstat(parentPath, parent, os.Lstat) {
		if parent != nil {
			_ = parent.Close()
		}
		return nil, sandboxSelfTestFixtureError()
	}
	fixture := &selfTestFixture{
		kind:       kind,
		parentPath: parentPath,
		parent:     parent,
		retainedFD: -1,
		closeFD:    closeFD,
	}
	for attempt := 0; attempt < selfTestFixtureMaxAttempts; attempt++ {
		random := make([]byte, selfTestFixtureRandomBytes)
		if err := randomBytes(kind, random); err != nil {
			return fixture, sandboxSelfTestFixtureError()
		}
		fixture.name = selfTestFixturePrefix + hex.EncodeToString(random)
		fixture.path = filepath.Join(parentPath, fixture.name)
		emitSelfTestFixtureEvent(event, selfTestFixtureEvent{stage: selfTestFixtureCandidate, kind: kind, path: fixture.path})
		created, createErr := createHostSelfTestFixture(fixture, []byte(contents))
		if errors.Is(createErr, unix.EEXIST) {
			continue
		}
		if createErr != nil || !created {
			return fixture, sandboxSelfTestFixtureError()
		}
		return fixture, nil
	}
	return fixture, sandboxSelfTestFixtureError()
}

func createHostSelfTestFixture(fixture *selfTestFixture, contents []byte) (bool, error) {
	if fixture == nil || fixture.parent == nil || fixture.closeFD == nil || fixture.retainedOpen {
		return false, sandboxSelfTestFixtureError()
	}
	fd, err := unix.Openat(int(fixture.parent.Fd()), fixture.name, unix.O_RDWR|unix.O_CREAT|unix.O_EXCL|unix.O_CLOEXEC|unix.O_NOFOLLOW, selfTestFixtureMode)
	if err != nil {
		return false, err
	}
	fixture.retainedFD = fd
	fixture.retainedOpen = true

	stat, statErr := stateFDStat(fd)
	if statErr != nil {
		return false, sandboxSelfTestFixtureError()
	}
	fixture.identity = stateIdentityFromStat(stat)
	fixture.owned = validSelfTestFixtureIdentityStat(stat, fixture.identity) && fixture.edgeIdentityMatches()
	if !fixture.owned || unix.Fchmod(fd, selfTestFixtureMode) != nil {
		return false, sandboxSelfTestFixtureError()
	}
	if !fixture.descriptorMatches() || !fixture.edgeMatches() || !writeSelfTestFixture(fd, contents) ||
		!fixture.descriptorMatches() || !fixture.edgeMatches() {
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

func (fixtures *selfTestFixtures) writeProbeArguments() ([]string, error) {
	if fixtures == nil {
		return nil, sandboxSelfTestFixtureError()
	}
	arguments := make([]string, 0, 14)
	for _, kind := range []selfTestFixtureKind{selfTestAllowedWorkspaceWriteFixture, selfTestAllowedPrivateWriteFixture} {
		fixture := fixtures.fixture(kind)
		values, err := fixture.writeProbeArguments()
		if err != nil {
			return nil, err
		}
		arguments = append(arguments, values...)
	}
	return arguments, nil
}

func (fixture *selfTestFixture) writeProbeArguments() ([]string, error) {
	if fixture == nil || !fixture.owned || !fixture.descriptorMatches() || !fixture.edgeMatches() {
		return nil, sandboxSelfTestFixtureError()
	}
	return []string{
		fixture.path,
		strconv.FormatUint(fixture.identity.device, 10),
		strconv.FormatUint(fixture.identity.inode, 10),
		strconv.FormatUint(uint64(unix.S_IFREG), 10),
		strconv.Itoa(os.Geteuid()),
		strconv.FormatUint(uint64(selfTestFixtureMode), 10),
		"1",
	}, nil
}

func (fixtures *selfTestFixtures) validateBeforeWriteDispatch() error {
	if fixtures == nil {
		return sandboxSelfTestFixtureError()
	}
	writeFixtures := []*selfTestFixture{
		fixtures.fixture(selfTestAllowedWorkspaceWriteFixture),
		fixtures.fixture(selfTestAllowedPrivateWriteFixture),
	}
	for _, fixture := range writeFixtures {
		if fixture == nil {
			return sandboxSelfTestFixtureError()
		}
		emitSelfTestFixtureEvent(fixtures.event, selfTestFixtureEvent{stage: selfTestFixtureBeforeDispatch, kind: fixture.kind, path: fixture.path})
	}
	for _, fixture := range writeFixtures {
		if !fixture.descriptorMatches() || !fixture.edgeMatches() {
			return sandboxSelfTestFixtureError()
		}
	}
	return nil
}

func (fixtures *selfTestFixtures) validateWrittenContents(contents string) error {
	if fixtures == nil {
		return sandboxSelfTestFixtureError()
	}
	writeFixtures := []*selfTestFixture{
		fixtures.fixture(selfTestAllowedWorkspaceWriteFixture),
		fixtures.fixture(selfTestAllowedPrivateWriteFixture),
	}
	for _, fixture := range writeFixtures {
		if fixture == nil {
			return sandboxSelfTestFixtureError()
		}
		emitSelfTestFixtureEvent(fixtures.event, selfTestFixtureEvent{stage: selfTestFixtureBeforeReadback, kind: fixture.kind, path: fixture.path})
	}
	for _, fixture := range writeFixtures {
		if err := fixture.validateContents(contents); err != nil {
			return err
		}
	}
	return nil
}

func (fixture *selfTestFixture) validateContents(contents string) error {
	if fixture == nil || !fixture.owned || !fixture.descriptorMatches() || !fixture.edgeMatches() {
		return sandboxSelfTestFixtureError()
	}
	data, err := readSelfTestFixture(fixture.retainedFD, len(contents)+1)
	if err != nil || string(data) != contents || !fixture.descriptorMatches() || !fixture.edgeMatches() {
		return sandboxSelfTestFixtureError()
	}
	return nil
}

func readSelfTestFixture(fd, limit int) ([]byte, error) {
	if fd < 0 || limit < 0 {
		return nil, sandboxSelfTestFixtureError()
	}
	data := make([]byte, 0, limit)
	buffer := make([]byte, limit)
	for len(data) < limit {
		read, err := unix.Pread(fd, buffer[len(data):], int64(len(data)))
		if errors.Is(err, unix.EINTR) {
			continue
		}
		if err != nil {
			return nil, err
		}
		if read == 0 {
			break
		}
		data = append(data, buffer[len(data):len(data)+read]...)
	}
	return data, nil
}

func (fixture *selfTestFixture) descriptorMatches() bool {
	if fixture == nil || !fixture.retainedOpen || fixture.retainedFD < 0 || fixture.identity == (stateIdentity{}) {
		return false
	}
	stat, err := stateFDStat(fixture.retainedFD)
	return err == nil && validSelfTestFixtureStat(stat, fixture.identity)
}

func (fixture *selfTestFixture) edgeIdentityMatches() bool {
	if fixture == nil || fixture.parent == nil || fixture.identity == (stateIdentity{}) ||
		!stateDescriptorPathMatchesWithLstat(fixture.parentPath, fixture.parent, os.Lstat) {
		return false
	}
	var stat unix.Stat_t
	return unix.Fstatat(int(fixture.parent.Fd()), fixture.name, &stat, unix.AT_SYMLINK_NOFOLLOW) == nil &&
		validSelfTestFixtureIdentityStat(stat, fixture.identity)
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

func validSelfTestFixtureIdentityStat(stat unix.Stat_t, identity stateIdentity) bool {
	return uint32(stat.Mode)&uint32(unix.S_IFMT) == uint32(unix.S_IFREG) && int(stat.Uid) == os.Geteuid() && stat.Nlink == 1 &&
		sameStateIdentity(stateIdentityFromStat(stat), identity)
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
			if !fixture.descriptorMatches() || !fixture.edgeMatches() {
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
		if err := fixture.retireRetainedFD(); err != nil {
			failed = true
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

func (fixture *selfTestFixture) retireRetainedFD() error {
	if fixture == nil || !fixture.retainedOpen {
		return nil
	}
	fd := fixture.retainedFD
	fixture.retainedFD = -1
	fixture.retainedOpen = false
	if fixture.closeFD == nil || fd < 0 {
		return sandboxSelfTestFixtureError()
	}
	return fixture.closeFD(fd)
}

func emitSelfTestFixtureEvent(event func(selfTestFixtureEvent), value selfTestFixtureEvent) {
	if event != nil {
		event(value)
	}
}

func sandboxSelfTestFixtureError() error {
	return errSelfTestFixture
}
