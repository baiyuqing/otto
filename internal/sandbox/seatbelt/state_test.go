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
	operations.mkdirTemp = func(parent, pattern string) (string, error) {
		candidate := filepath.Join(parent, "otto-sandbox-symlink")
		if err := os.Symlink(outside, candidate); err != nil {
			return "", err
		}
		return candidate, nil
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
				mkdirTemp := operations.mkdirTemp
				operations.mkdirTemp = func(parent, pattern string) (string, error) {
					path, err := mkdirTemp(parent, pattern)
					if err == nil {
						err = os.Chmod(path, 0o600)
					}
					return path, err
				}
			},
		},
		{
			name: "root special mode bits",
			mutate: func(operations *stateOperations) {
				mkdirTemp := operations.mkdirTemp
				operations.mkdirTemp = func(parent, pattern string) (string, error) {
					path, err := mkdirTemp(parent, pattern)
					if err == nil {
						err = os.Chmod(path, fs.ModeSticky|0o700)
					}
					return path, err
				}
			},
		},
		{
			name: "child directory",
			mutate: func(operations *stateOperations) {
				mkdir := operations.mkdir
				operations.mkdir = func(path string, mode fs.FileMode) error {
					if err := mkdir(path, mode); err != nil {
						return err
					}
					if filepath.Base(path) == "home" {
						return os.Chmod(path, 0o600)
					}
					return nil
				}
			},
		},
		{
			name: "profile",
			mutate: func(operations *stateOperations) {
				openFile := operations.openFile
				operations.openFile = func(path string, flags int, mode fs.FileMode) (*os.File, error) {
					file, err := openFile(path, flags, mode)
					if err == nil {
						err = file.Chmod(0o400)
					}
					return file, err
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
	mkdir := operations.mkdir
	operations.mkdir = func(path string, mode fs.FileMode) error {
		if filepath.Base(path) == "cache" {
			return fs.ErrPermission
		}
		return mkdir(path, mode)
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
