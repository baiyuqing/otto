//go:build darwin || linux

package seatbelt

import (
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"syscall"
	"testing"

	"golang.org/x/sys/unix"
)

func TestStateCreatesOwnerOnlyCanonicalTreeOutsideWorkspace(t *testing.T) {
	base := t.TempDir()
	workspace := makeStateTestDirectory(t, filepath.Join(base, "workspace"), 0o700)
	cache := makeStateTestDirectory(t, filepath.Join(base, "user-cache"), 0o700)

	cacheAlias := filepath.Join(base, "cache-alias")
	if err := os.Symlink(cache, cacheAlias); err != nil {
		t.Fatal(err)
	}
	workspaceAlias := filepath.Join(base, "workspace-alias")
	if err := os.Symlink(workspace, workspaceAlias); err != nil {
		t.Fatal(err)
	}

	state, err := createState(workspaceAlias, cacheAlias)
	if err != nil {
		t.Fatalf("createState() error = %v", err)
	}
	t.Cleanup(func() {
		if err := state.close(); err != nil {
			t.Errorf("state.close() error = %v", err)
		}
	})

	if state.rootParent != cache {
		t.Fatalf("rootParent = %q, want canonical cache base", state.rootParent)
	}
	if filepath.Dir(state.directories.Root) != cache ||
		!strings.HasPrefix(filepath.Base(state.directories.Root), "otto-sandbox-") {
		t.Fatalf("state root is not one otto-sandbox- leaf beneath cache base")
	}
	if pathContains(workspace, state.directories.Root) {
		t.Fatal("state root is inside the canonical workspace")
	}

	expectedDirectories := map[string]string{
		"root":     state.directories.Root,
		"home":     state.directories.Home,
		"tmp":      state.directories.Temp,
		"cache":    state.directories.Cache,
		"profiles": state.profiles,
	}
	for name, path := range expectedDirectories {
		t.Run(name, func(t *testing.T) {
			assertCanonicalStatePath(t, path)
			assertStateEntry(t, path, true, 0o700)
		})
	}

	if state.directories.Home != filepath.Join(state.directories.Root, "home") ||
		state.directories.Temp != filepath.Join(state.directories.Root, "tmp") ||
		state.directories.Cache != filepath.Join(state.directories.Root, "cache") ||
		state.profiles != filepath.Join(state.directories.Root, "profiles") ||
		state.profilePath != filepath.Join(state.profiles, "profile.sb") {
		t.Fatal("private state children are not fixed canonical descendants")
	}
	assertCanonicalStatePath(t, state.profilePath)
	assertStateEntry(t, state.profilePath, false, 0o600)
}

func TestStateRejectsCacheBaseInsideCanonicalWorkspaceWithoutFallback(t *testing.T) {
	base := t.TempDir()
	workspace := makeStateTestDirectory(t, filepath.Join(base, "workspace"), 0o700)
	cache := makeStateTestDirectory(t, filepath.Join(workspace, "user-cache"), 0o700)

	alias := filepath.Join(base, "cache-alias")
	if err := os.Symlink(cache, alias); err != nil {
		t.Fatal(err)
	}
	if state, err := createState(workspace, alias); err == nil || state != nil {
		t.Fatalf("createState() = (%v, %v), want fail-closed rejection", state, err)
	}
	assertNoStateLeaves(t, cache)
}

func TestStateRejectsCaseAliasOfCacheInsideWorkspace(t *testing.T) {
	if runtime.GOOS != "darwin" {
		t.Skip("case-insensitive macOS filesystem regression")
	}
	base := t.TempDir()
	workspace := makeStateTestDirectory(t, filepath.Join(base, "CaseWorkspace"), 0o700)
	cache := makeStateTestDirectory(t, filepath.Join(workspace, "CaseCache"), 0o700)
	alias := strings.Replace(cache, "CaseWorkspace", "caseworkspace", 1)
	alias = strings.Replace(alias, "CaseCache", "casecache", 1)
	if _, err := os.Stat(alias); err != nil {
		t.Skip("test volume is case-sensitive")
	}

	if state, err := createState(workspace, alias); err == nil || state != nil {
		t.Fatalf("createState() accepted a differently-cased cache alias inside workspace")
	}
	assertNoStateLeaves(t, cache)
}

func TestStateRejectsSymlinkCandidateWithoutFollowing(t *testing.T) {
	base := t.TempDir()
	workspace := makeStateTestDirectory(t, filepath.Join(base, "workspace"), 0o700)
	cache := makeStateTestDirectory(t, filepath.Join(base, "user-cache"), 0o700)
	outside := makeStateTestDirectory(t, filepath.Join(base, "outside"), 0o700)
	marker := filepath.Join(outside, "keep")
	if err := os.WriteFile(marker, []byte("keep"), 0o600); err != nil {
		t.Fatal(err)
	}

	operations := defaultStateOperations()
	mkdirat := operations.mkdirat
	operations.mkdirat = func(parentFD int, name string, mode uint32) error {
		if strings.HasPrefix(name, stateLeafPrefix) {
			return unix.Symlinkat(outside, parentFD, name)
		}
		return mkdirat(parentFD, name, mode)
	}

	if state, err := createStateWithOperations(workspace, cache, operations); err == nil || state != nil {
		t.Fatalf("createStateWithOperations() = (%v, %v), want rejection", state, err)
	}
	if data, err := os.ReadFile(marker); err != nil || string(data) != "keep" {
		t.Fatalf("outside marker = %q, %v; cleanup followed candidate symlink", data, err)
	}
	assertNoStateLeaves(t, cache)
}

func TestStateFailsClosedWhenCreationStripsOwnerBits(t *testing.T) {
	tests := []struct {
		name   string
		mutate func(*stateOperations)
	}{
		{
			name: "root",
			mutate: func(operations *stateOperations) {
				operations.event = func(event stateEvent) {
					if event.kind == stateEventRootCreated {
						_ = os.Chmod(event.path, 0o600)
					}
				}
			},
		},
		{
			name: "root special mode bits",
			mutate: func(operations *stateOperations) {
				operations.event = func(event stateEvent) {
					if event.kind == stateEventRootCreated {
						_ = os.Chmod(event.path, fs.ModeSticky|0o700)
					}
				}
			},
		},
		{
			name: "child directory",
			mutate: func(operations *stateOperations) {
				operations.event = func(event stateEvent) {
					if event.kind == stateEventDirectoryCreated && event.name == "home" {
						_ = os.Chmod(event.path, 0o600)
					}
				}
			},
		},
		{
			name: "profile",
			mutate: func(operations *stateOperations) {
				operations.event = func(event stateEvent) {
					if event.kind == stateEventProfileCreated {
						_ = os.Chmod(event.path, 0o400)
					}
				}
			},
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			base := t.TempDir()
			workspace := makeStateTestDirectory(t, filepath.Join(base, "workspace"), 0o700)
			cache := makeStateTestDirectory(t, filepath.Join(base, "user-cache"), 0o700)
			operations := defaultStateOperations()
			test.mutate(&operations)

			if state, err := createStateWithOperations(workspace, cache, operations); err == nil || state != nil {
				t.Fatalf("createStateWithOperations() = (%v, %v), want rejection", state, err)
			}
			assertNoStateLeaves(t, cache)
		})
	}
}

func TestStateCleansPartialConstructionFailure(t *testing.T) {
	base := t.TempDir()
	workspace := makeStateTestDirectory(t, filepath.Join(base, "workspace"), 0o700)
	cache := makeStateTestDirectory(t, filepath.Join(base, "user-cache"), 0o700)
	operations := defaultStateOperations()
	mkdirat := operations.mkdirat
	operations.mkdirat = func(parentFD int, name string, mode uint32) error {
		if name == "cache" {
			return fs.ErrPermission
		}
		return mkdirat(parentFD, name, mode)
	}

	if state, err := createStateWithOperations(workspace, cache, operations); err == nil || state != nil {
		t.Fatalf("createStateWithOperations() = (%v, %v), want partial failure", state, err)
	}
	assertNoStateLeaves(t, cache)
}

func TestStateWriteProfileRevalidatesOwnerOnlyTree(t *testing.T) {
	base := t.TempDir()
	workspace := makeStateTestDirectory(t, filepath.Join(base, "workspace"), 0o700)
	cache := makeStateTestDirectory(t, filepath.Join(base, "user-cache"), 0o700)
	state, err := createState(workspace, cache)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		_ = os.Chmod(state.profiles, 0o700)
		if err := state.close(); err != nil {
			t.Errorf("state.close() error = %v", err)
		}
	})

	if err := os.Chmod(state.profiles, 0o755); err != nil {
		t.Fatal(err)
	}
	privateProfileText := []byte("(version 1)\n(deny default)\n")
	if err := state.writeProfile(privateProfileText); err == nil {
		t.Fatal("writeProfile() accepted a non-owner-only profiles directory")
	}
	contents, err := os.ReadFile(state.profilePath)
	if err != nil {
		t.Fatal(err)
	}
	if len(contents) != 0 {
		t.Fatal("writeProfile() modified the profile before validating its directory chain")
	}

	if err := os.Chmod(state.profiles, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := state.writeProfile(privateProfileText); err != nil {
		t.Fatalf("writeProfile() error = %v", err)
	}
	contents, err = os.ReadFile(state.profilePath)
	if err != nil || string(contents) != string(privateProfileText) {
		t.Fatalf("profile contents = %q, %v", contents, err)
	}
	assertStateEntry(t, state.profilePath, false, 0o600)
}

func TestStateWriteProfileValidatesFileBeforeTruncating(t *testing.T) {
	base := t.TempDir()
	workspace := makeStateTestDirectory(t, filepath.Join(base, "workspace"), 0o700)
	cache := makeStateTestDirectory(t, filepath.Join(base, "user-cache"), 0o700)
	state, err := createState(workspace, cache)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		_ = os.Chmod(state.profilePath, 0o600)
		if err := state.close(); err != nil {
			t.Errorf("state.close() error = %v", err)
		}
	})
	original := []byte("original private profile")
	if err := os.WriteFile(state.profilePath, original, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(state.profilePath, 0o644); err != nil {
		t.Fatal(err)
	}

	if err := state.writeProfile([]byte("replacement")); err == nil {
		t.Fatal("writeProfile() accepted a non-owner-only profile")
	}
	contents, err := os.ReadFile(state.profilePath)
	if err != nil || string(contents) != string(original) {
		t.Fatalf("profile contents = %q, %v; file was truncated before validation", contents, err)
	}
}

func TestStateWriteProfileDoesNotFollowReplacementSymlink(t *testing.T) {
	base := t.TempDir()
	workspace := makeStateTestDirectory(t, filepath.Join(base, "workspace"), 0o700)
	cache := makeStateTestDirectory(t, filepath.Join(base, "user-cache"), 0o700)
	outside := makeProfileTestFile(t, filepath.Join(base, "outside", "keep"), 0o600)
	state, err := createState(workspace, cache)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if err := state.close(); err != nil {
			t.Errorf("state.close() error = %v", err)
		}
	})
	if err := os.Remove(state.profilePath); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(outside, state.profilePath); err != nil {
		t.Fatal(err)
	}

	if err := state.writeProfile([]byte("replace")); err == nil {
		t.Fatal("writeProfile() followed a replacement symlink")
	}
	contents, err := os.ReadFile(outside)
	if err != nil || string(contents) != "fixture" {
		t.Fatalf("outside file = %q, %v; replacement symlink was followed", contents, err)
	}
}

func TestStateCloseIsIdempotentAndNeverFollowsSymlinks(t *testing.T) {
	base := t.TempDir()
	workspace := makeStateTestDirectory(t, filepath.Join(base, "workspace"), 0o700)
	cache := makeStateTestDirectory(t, filepath.Join(base, "user-cache"), 0o700)
	outside := makeStateTestDirectory(t, filepath.Join(base, "outside"), 0o700)
	marker := filepath.Join(outside, "keep")
	if err := os.WriteFile(marker, []byte("keep"), 0o600); err != nil {
		t.Fatal(err)
	}

	state, err := createState(workspace, cache)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(state.directories.Home); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(outside, state.directories.Home); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(outside, filepath.Join(state.directories.Cache, "outside-link")); err != nil {
		t.Fatal(err)
	}

	firstErr := state.close()
	secondErr := state.close()
	if firstErr != nil || secondErr != nil {
		t.Fatalf("close errors = (%v, %v), want nil", firstErr, secondErr)
	}
	if _, err := os.Lstat(state.directories.Root); !errors.Is(err, fs.ErrNotExist) {
		t.Fatalf("state root still exists: %v", err)
	}
	if data, err := os.ReadFile(marker); err != nil || string(data) != "keep" {
		t.Fatalf("outside marker = %q, %v; close followed a symlink", data, err)
	}
}

func TestStateConstructionDoesNotFollowRootReplacementAfterValidation(t *testing.T) {
	base := t.TempDir()
	workspace := makeStateTestDirectory(t, filepath.Join(base, "workspace"), 0o700)
	cache := makeStateTestDirectory(t, filepath.Join(base, "user-cache"), 0o700)
	outside := makeStateTestDirectory(t, filepath.Join(base, "outside"), 0o700)

	operations := defaultStateOperations()
	movedRoot := ""
	operations.event = func(event stateEvent) {
		if event.kind != stateEventRootValidated || movedRoot != "" {
			return
		}
		movedRoot = event.path + ".validated"
		if err := os.Rename(event.path, movedRoot); err != nil {
			t.Fatal(err)
		}
		if err := os.Symlink(outside, event.path); err != nil {
			t.Fatal(err)
		}
	}
	t.Cleanup(func() {
		_ = os.RemoveAll(movedRoot)
	})

	created, err := createStateWithOperations(workspace, cache, operations)
	if created != nil {
		_ = created.close()
	}
	if err == nil || created != nil {
		t.Fatalf("createStateWithOperations() = (%v, %v), want replacement rejection", created, err)
	}
	for _, relative := range []string{"home", "tmp", "cache", "profiles", filepath.Join("profiles", "profile.sb")} {
		if _, statErr := os.Lstat(filepath.Join(outside, relative)); !errors.Is(statErr, fs.ErrNotExist) {
			t.Fatalf("external state entry %q was created or followed: %v", relative, statErr)
		}
	}
}

func TestStateFinalConstructionRejectsReplacedProfilesAndProfile(t *testing.T) {
	tests := []struct {
		name    string
		replace func(*testing.T, string, string)
	}{
		{
			name: "profiles directory",
			replace: func(t *testing.T, base, root string) {
				profiles := filepath.Join(root, "profiles")
				if err := os.Rename(profiles, filepath.Join(base, "created-profiles")); err != nil {
					t.Fatal(err)
				}
				if err := os.Mkdir(profiles, 0o700); err != nil {
					t.Fatal(err)
				}
				if err := os.WriteFile(filepath.Join(profiles, "profile.sb"), []byte("substituted"), 0o600); err != nil {
					t.Fatal(err)
				}
			},
		},
		{
			name: "profile file",
			replace: func(t *testing.T, base, root string) {
				profile := filepath.Join(root, "profiles", "profile.sb")
				if err := os.Rename(profile, filepath.Join(base, "created-profile.sb")); err != nil {
					t.Fatal(err)
				}
				if err := os.WriteFile(profile, []byte("substituted"), 0o600); err != nil {
					t.Fatal(err)
				}
			},
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			base := t.TempDir()
			workspace := makeStateTestDirectory(t, filepath.Join(base, "workspace"), 0o700)
			cache := makeStateTestDirectory(t, filepath.Join(base, "user-cache"), 0o700)
			operations := defaultStateOperations()
			injected := false
			operations.event = func(event stateEvent) {
				if event.kind != stateEventFinalValidation || injected {
					return
				}
				injected = true
				test.replace(t, base, event.path)
			}

			created, err := createStateWithOperations(workspace, cache, operations)
			if created != nil {
				_ = created.close()
			}
			if !injected {
				t.Fatal("createStateWithOperations() did not emit the final-validation event")
			}
			if err == nil || created != nil {
				t.Fatalf("createStateWithOperations() accepted a final replacement (state non-nil=%v, error=%v)", created != nil, err)
			}
		})
	}
}

func TestStatePartialCleanupDoesNotTraverseUnknownOrSubstitutedDirectory(t *testing.T) {
	tests := []struct {
		name   string
		insert func(*testing.T, string, string) string
	}{
		{
			name: "unknown directory",
			insert: func(t *testing.T, _, root string) string {
				unknown := filepath.Join(root, "unknown", "nested")
				if err := os.MkdirAll(unknown, 0o700); err != nil {
					t.Fatal(err)
				}
				marker := filepath.Join("unknown", "nested", "keep")
				if err := os.WriteFile(filepath.Join(root, marker), []byte("keep"), 0o600); err != nil {
					t.Fatal(err)
				}
				return marker
			},
		},
		{
			name: "substituted directory",
			insert: func(t *testing.T, base, root string) string {
				home := filepath.Join(root, "home")
				if err := os.Rename(home, filepath.Join(base, "created-home")); err != nil {
					t.Fatal(err)
				}
				if err := os.MkdirAll(filepath.Join(home, "nested"), 0o700); err != nil {
					t.Fatal(err)
				}
				marker := filepath.Join("home", "nested", "keep")
				if err := os.WriteFile(filepath.Join(root, marker), []byte("keep"), 0o600); err != nil {
					t.Fatal(err)
				}
				return marker
			},
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			base := t.TempDir()
			workspace := makeStateTestDirectory(t, filepath.Join(base, "workspace"), 0o700)
			cache := makeStateTestDirectory(t, filepath.Join(base, "user-cache"), 0o700)
			operations := defaultStateOperations()
			injected := false
			detachedRoot := ""
			markerPath := ""
			operations.event = func(event stateEvent) {
				if event.kind != stateEventFinalValidation || injected {
					return
				}
				injected = true
				marker := test.insert(t, base, event.path)
				detachedRoot = event.path + ".detached"
				if err := os.Rename(event.path, detachedRoot); err != nil {
					t.Fatal(err)
				}
				markerPath = filepath.Join(detachedRoot, marker)
			}
			t.Cleanup(func() { _ = os.RemoveAll(detachedRoot) })

			created, err := createStateWithOperations(workspace, cache, operations)
			if created != nil {
				_ = created.close()
			}
			if !injected || err == nil || created != nil {
				t.Fatalf("createStateWithOperations() did not fail after the injected replacement (event=%v, state non-nil=%v, error=%v)", injected, created != nil, err)
			}
			contents, readErr := os.ReadFile(markerPath)
			if readErr != nil || string(contents) != "keep" {
				t.Fatalf("partial cleanup touched untrusted directory contents: contents=%q error=%v", contents, readErr)
			}
		})
	}
}

func TestStateProfileCloseErrorsRetireDescriptorsBeforeDeferredCleanup(t *testing.T) {
	t.Run("construction", func(t *testing.T) {
		base := t.TempDir()
		workspace := makeStateTestDirectory(t, filepath.Join(base, "workspace"), 0o700)
		cache := makeStateTestDirectory(t, filepath.Join(base, "user-cache"), 0o700)
		probe := newStateCloseReuseProbe(t, makeProfileTestFile(t, filepath.Join(base, "sentinel"), 0o600))
		probe.enabled = true
		operations := defaultStateOperations()
		operations.closeFD = probe.closeFD

		created, err := createStateWithOperations(workspace, cache, operations)
		if created != nil {
			_ = created.close()
		}
		if err == nil || created != nil {
			t.Fatalf("createStateWithOperations() accepted a synthetic profile close error (state non-nil=%v, error=%v)", created != nil, err)
		}
		probe.assertReusedFDOpen(t)
		assertNoStateLeaves(t, cache)
	})

	t.Run("profile write", func(t *testing.T) {
		base := t.TempDir()
		workspace := makeStateTestDirectory(t, filepath.Join(base, "workspace"), 0o700)
		cache := makeStateTestDirectory(t, filepath.Join(base, "user-cache"), 0o700)
		probe := newStateCloseReuseProbe(t, makeProfileTestFile(t, filepath.Join(base, "sentinel"), 0o600))
		operations := defaultStateOperations()
		operations.closeFD = probe.closeFD
		state, err := createStateWithOperations(workspace, cache, operations)
		if err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() {
			if err := state.close(); err != nil {
				t.Errorf("state.close() error = %v", err)
			}
		})

		probe.enabled = true
		if err := state.writeProfile([]byte("private profile")); err == nil {
			t.Fatal("writeProfile() accepted a synthetic profile close error")
		}
		probe.assertReusedFDOpen(t)
	})
}

func TestStateWriteProfileDoesNotUseSubstitutedRootInode(t *testing.T) {
	base := t.TempDir()
	workspace := makeStateTestDirectory(t, filepath.Join(base, "workspace"), 0o700)
	cache := makeStateTestDirectory(t, filepath.Join(base, "user-cache"), 0o700)
	outsideProfile := makeProfileTestFile(t, filepath.Join(base, "outside", "profile.sb"), 0o600)
	operations := defaultStateOperations()
	movedRoot := ""
	operations.event = func(event stateEvent) {
		if event.kind != stateEventProfileWrite || movedRoot != "" {
			return
		}
		movedRoot = event.path + ".validated"
		if err := os.Rename(event.path, movedRoot); err != nil {
			t.Fatal(err)
		}
		makeStateReplacementTree(t, event.path, outsideProfile)
	}
	state, err := createStateWithOperations(workspace, cache, operations)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		_ = os.RemoveAll(state.directories.Root)
		_ = os.Rename(movedRoot, state.directories.Root)
		_ = state.close()
		_ = os.RemoveAll(movedRoot)
	})

	writeErr := state.writeProfile([]byte("substituted write"))
	contents, readErr := os.ReadFile(outsideProfile)
	if writeErr == nil || readErr != nil || string(contents) != "fixture" {
		t.Fatalf("writeProfile() error = %v, external profile = %q, %v; want rejection without external write", writeErr, contents, readErr)
	}
}

func TestStateCloseDoesNotRemoveSubstitutedRootInode(t *testing.T) {
	base := t.TempDir()
	workspace := makeStateTestDirectory(t, filepath.Join(base, "workspace"), 0o700)
	cache := makeStateTestDirectory(t, filepath.Join(base, "user-cache"), 0o700)
	operations := defaultStateOperations()
	movedRoot := ""
	operations.event = func(event stateEvent) {
		if event.kind != stateEventCleanup || movedRoot != "" {
			return
		}
		movedRoot = event.path + ".validated"
		if err := os.Rename(event.path, movedRoot); err != nil {
			t.Fatal(err)
		}
		makeStateReplacementTree(t, event.path, "")
		if err := os.WriteFile(filepath.Join(event.path, "replacement-marker"), []byte("keep"), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	state, err := createStateWithOperations(workspace, cache, operations)
	if err != nil {
		t.Fatal(err)
	}

	outsideAlias := filepath.Join(base, "outside-root-alias")
	if err := os.Symlink(state.directories.Root, outsideAlias); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		_ = os.RemoveAll(state.directories.Root)
		_ = os.RemoveAll(movedRoot)
		_ = os.Remove(outsideAlias)
	})

	firstErr := state.close()
	secondErr := state.close()
	contents, readErr := os.ReadFile(filepath.Join(outsideAlias, "replacement-marker"))
	if firstErr == nil || secondErr == nil || firstErr.Error() != secondErr.Error() ||
		readErr != nil || string(contents) != "keep" {
		t.Fatalf("close errors = (%v, %v), substituted marker = %q, %v; want stable rejection without cleanup escape", firstErr, secondErr, contents, readErr)
	}
}

func TestStateCloseReturnsStableBoundedPathFreeError(t *testing.T) {
	base := t.TempDir()
	workspace := makeStateTestDirectory(t, filepath.Join(base, "workspace-private-name"), 0o700)
	cache := makeStateTestDirectory(t, filepath.Join(base, "cache-private-name"), 0o700)
	state, err := createState(workspace, cache)
	if err != nil {
		t.Fatal(err)
	}

	movedCache := cache + "-moved"
	if err := os.Rename(cache, movedCache); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(movedCache) })

	firstErr := state.close()
	secondErr := state.close()
	if firstErr == nil || secondErr == nil || firstErr.Error() != secondErr.Error() {
		t.Fatalf("close errors = (%v, %v), want one stable error", firstErr, secondErr)
	}
	message := firstErr.Error()
	if len(message) > 128 || strings.Contains(message, base) ||
		strings.Contains(message, filepath.Base(state.directories.Root)) ||
		strings.Contains(message, "profile.sb") {
		t.Fatalf("close error is not bounded and path-free: %q", message)
	}
}

type stateCloseReuseProbe struct {
	t        *testing.T
	sentinel string
	enabled  bool
	reusedFD int
	calls    int
}

func newStateCloseReuseProbe(t *testing.T, sentinel string) *stateCloseReuseProbe {
	t.Helper()
	probe := &stateCloseReuseProbe{t: t, sentinel: sentinel, reusedFD: -1}
	t.Cleanup(func() {
		if probe.reusedFD >= 0 {
			_ = unix.Close(probe.reusedFD)
			probe.reusedFD = -1
		}
	})
	return probe
}

func (p *stateCloseReuseProbe) closeFD(fd int) error {
	if !p.enabled {
		return unix.Close(fd)
	}
	p.calls++
	if p.reusedFD >= 0 {
		p.t.Fatalf("close hook called more than once after descriptor reuse")
	}
	if err := unix.Close(fd); err != nil {
		p.t.Fatalf("real close in injected hook failed: %v", err)
	}
	reusedFD, err := unix.Open(p.sentinel, unix.O_RDONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		p.t.Fatalf("open sentinel in injected hook: %v", err)
	}
	if reusedFD != fd {
		_ = unix.Close(reusedFD)
		p.t.Fatalf("sentinel descriptor = %d, want immediate reuse of %d", reusedFD, fd)
	}
	p.reusedFD = reusedFD
	return errors.New("synthetic descriptor close failure")
}

func (p *stateCloseReuseProbe) assertReusedFDOpen(t *testing.T) {
	t.Helper()
	if p.calls != 1 || p.reusedFD < 0 {
		t.Fatalf("injected close calls = %d, reused descriptor = %d; want one reuse", p.calls, p.reusedFD)
	}
	var descriptorStat unix.Stat_t
	var sentinelStat unix.Stat_t
	if err := unix.Fstat(p.reusedFD, &descriptorStat); err != nil {
		t.Fatalf("reused sentinel descriptor was closed: %v", err)
	}
	if err := unix.Stat(p.sentinel, &sentinelStat); err != nil ||
		!sameStateIdentity(stateIdentityFromStat(descriptorStat), stateIdentityFromStat(sentinelStat)) {
		t.Fatalf("reused descriptor no longer identifies the sentinel: %v", err)
	}
}

func makeStateReplacementTree(t *testing.T, root, externalProfile string) {
	t.Helper()
	if err := os.Mkdir(root, 0o700); err != nil {
		t.Fatal(err)
	}
	for _, name := range []string{"home", "tmp", "cache", "profiles"} {
		if err := os.Mkdir(filepath.Join(root, name), 0o700); err != nil {
			t.Fatal(err)
		}
	}
	profilePath := filepath.Join(root, "profiles", "profile.sb")
	if externalProfile != "" {
		if err := os.Link(externalProfile, profilePath); err != nil {
			t.Fatal(err)
		}
		return
	}
	if err := os.WriteFile(profilePath, nil, 0o600); err != nil {
		t.Fatal(err)
	}
}

func makeStateTestDirectory(t *testing.T, path string, mode fs.FileMode) string {
	t.Helper()
	if err := os.Mkdir(path, mode); err != nil {
		t.Fatal(err)
	}
	canonical, err := filepath.EvalSymlinks(path)
	if err != nil {
		t.Fatal(err)
	}
	return canonical
}

func assertCanonicalStatePath(t *testing.T, path string) {
	t.Helper()
	canonical, err := filepath.EvalSymlinks(path)
	if err != nil {
		t.Fatal(err)
	}
	if canonical != path || !filepath.IsAbs(path) || filepath.Clean(path) != path {
		t.Fatalf("path %q is not canonical", path)
	}
}

func assertStateEntry(t *testing.T, path string, directory bool, mode fs.FileMode) {
	t.Helper()
	info, err := os.Lstat(path)
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode()&(os.ModeSymlink|os.ModeSetuid|os.ModeSetgid|os.ModeSticky) != 0 ||
		info.Mode().Perm() != mode || (directory && !info.IsDir()) ||
		(!directory && !info.Mode().IsRegular()) {
		t.Fatalf("entry mode = %v, want directory=%v permissions=%#o", info.Mode(), directory, mode)
	}
	stat, ok := info.Sys().(*syscall.Stat_t)
	if !ok || int(stat.Uid) != os.Geteuid() {
		t.Fatalf("entry UID is not effective UID")
	}
}

func assertNoStateLeaves(t *testing.T, cache string) {
	t.Helper()
	matches, err := filepath.Glob(filepath.Join(cache, "otto-sandbox-*"))
	if err != nil {
		t.Fatal(err)
	}
	if len(matches) != 0 {
		t.Fatalf("partial state remains beneath cache base")
	}
}

func pathContains(parent, child string) bool {
	relative, err := filepath.Rel(parent, child)
	return err == nil && relative != ".." && !strings.HasPrefix(relative, ".."+string(filepath.Separator))
}
