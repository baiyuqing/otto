package seatbelt

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"testing"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/sandbox"
)

func TestProfileIsClosedByDefaultAndLimitsPrivateWrites(t *testing.T) {
	fixture := newProfileFixture(t)
	externalDirectory := makeProfileTestDirectory(t, filepath.Join(fixture.base, "external", "tools"))
	externalFile := makeProfileTestFile(t, filepath.Join(fixture.base, "external", "runtime.conf"), 0o600)
	fixture.options.ReadPaths = []string{externalDirectory, externalFile}

	profile := renderProfileForTest(t, fixture.options, fixture.dependencies)
	if !strings.HasPrefix(profile, "(version 1)\n(deny default)\n") {
		t.Fatalf("profile does not begin closed by default:\n%s", profile)
	}
	for _, unfiltered := range []string{
		"(allow file-read*)",
		"(allow file-write*)",
		"(allow network-outbound)",
		"(allow network-inbound)",
	} {
		if strings.Contains(profile, unfiltered) {
			t.Fatalf("profile contains unfiltered grant %q", unfiltered)
		}
	}

	readSection := profileTestSection(t, profile, "READ")
	writeSection := profileTestSection(t, profile, "WRITE")
	for _, path := range []string{
		fixture.options.Workspace,
		fixture.options.Directories.Home,
		fixture.options.Directories.Temp,
		fixture.options.Directories.Cache,
	} {
		filter := profileTestFilter("subpath", path)
		if !strings.Contains(readSection, filter) || !strings.Contains(writeSection, filter) {
			t.Fatalf("writable path %q is not read/write", filepath.Base(path))
		}
	}
	for _, path := range []string{externalDirectory, externalFile} {
		if strings.Contains(writeSection, profileTestQuote(path)) {
			t.Fatalf("external path %q appears in write rules", filepath.Base(path))
		}
	}
	if !strings.Contains(readSection, profileTestFilter("subpath", externalDirectory)) ||
		!strings.Contains(readSection, profileTestFilter("literal", externalFile)) {
		t.Fatal("external directory/file did not become read-only subpath/literal grants")
	}

	if strings.Contains(readSection, profileTestFilter("subpath", fixture.options.Directories.Root)) ||
		strings.Contains(writeSection, profileTestFilter("subpath", fixture.options.Directories.Root)) {
		t.Fatal("private state parent received a subpath grant")
	}
	if strings.Contains(profile, fixture.state.profiles) || strings.Contains(profile, fixture.state.profilePath) {
		t.Fatal("profiles directory or generated profile leaked into child policy")
	}
}

func TestProfileRejectsEveryExternalRootOverlappingPrivateState(t *testing.T) {
	tests := []struct {
		name   string
		mutate func(*profileFixture)
	}{
		{
			name: "explicit ancestor",
			mutate: func(fixture *profileFixture) {
				fixture.options.ReadPaths = []string{fixture.state.rootParent}
			},
		},
		{
			name: "explicit profiles child",
			mutate: func(fixture *profileFixture) {
				fixture.options.ReadPaths = []string{fixture.state.profiles}
			},
		},
		{
			name: "fixed automatic ancestor",
			mutate: func(fixture *profileFixture) {
				fixture.dependencies.fixedPaths = []profileAutomaticPath{{path: fixture.state.rootParent, kind: profilePathDirectory}}
			},
		},
		{
			name: "PATH automatic ancestor",
			mutate: func(fixture *profileFixture) {
				fixture.options.HostEntries = []string{"PATH=" + fixture.state.rootParent}
			},
		},
		{
			name: "developer root ancestor",
			mutate: func(fixture *profileFixture) {
				fixture.dependencies.developerRoot = func() (string, bool, error) {
					return fixture.state.rootParent, true, nil
				}
			},
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			fixture := newProfileFixture(t)
			test.mutate(&fixture)
			if profile, err := generateProfileWithDependencies(fixture.options, fixture.dependencies); err == nil || profile != nil {
				t.Fatalf("generateProfileWithDependencies() = (%d bytes, %v), want fail closed", len(profile), err)
			}
		})
	}
}

func TestProfileRejectsCaseAliasContainingPrivateState(t *testing.T) {
	if runtime.GOOS != "darwin" {
		t.Skip("case-insensitive macOS filesystem regression")
	}
	fixture := newProfileFixture(t)
	alias := filepath.Join(filepath.Dir(fixture.state.rootParent), strings.ToUpper(filepath.Base(fixture.state.rootParent)))
	if _, err := os.Stat(alias); err != nil {
		t.Skip("test volume is case-sensitive")
	}
	fixture.options.ReadPaths = []string{alias}
	if profile, err := generateProfileWithDependencies(fixture.options, fixture.dependencies); err == nil || profile != nil {
		t.Fatalf("differently-cased state ancestor generated %d profile bytes", len(profile))
	}
}

func TestProfileNetworkRulesAreIPOnly(t *testing.T) {
	fixture := newProfileFixture(t)
	fixture.options.Network = sandbox.NetworkAllow
	allow := renderProfileForTest(t, fixture.options, fixture.dependencies)
	if !strings.Contains(allow, "(remote ip)") || !strings.Contains(allow, "(local ip)") {
		t.Fatal("NetworkAllow is missing filtered remote/local IP grants")
	}
	for _, forbidden := range []string{
		"(allow network-outbound)",
		"(allow network-inbound)",
		"(allow network-bind)",
		"(remote unix-socket)",
		"(local unix-socket)",
	} {
		if strings.Contains(allow, forbidden) {
			t.Fatalf("NetworkAllow contains unfiltered or Unix-socket grant %q", forbidden)
		}
	}

	fixture.options.Network = sandbox.NetworkDeny
	deny := renderProfileForTest(t, fixture.options, fixture.dependencies)
	if strings.Contains(deny, "(remote ip)") || strings.Contains(deny, "(local ip)") ||
		strings.Contains(deny, "allow network-") {
		t.Fatal("NetworkDeny emitted a network grant")
	}

	fixture.options.Network = sandbox.NetworkMode(99)
	if profile, err := generateProfileWithDependencies(fixture.options, fixture.dependencies); err == nil || profile != nil {
		t.Fatalf("invalid network mode generated %d bytes", len(profile))
	}
}

func TestProfileTemplateHasOnlyReviewedIPCAndProcessRules(t *testing.T) {
	fixture := newProfileFixture(t)
	profile := renderProfileForTest(t, fixture.options, fixture.dependencies)

	for _, required := range []string{
		"(allow process-fork)",
		"(allow process-exec)",
		"(allow signal (target same-sandbox))",
		"(sysctl-name ",
		"com.apple.system.opendirectoryd.libinfo",
	} {
		if !strings.Contains(profile, required) {
			t.Fatalf("profile is missing reviewed rule %q", required)
		}
	}
	for _, forbidden := range []string{
		"appleevent",
		"lsopen",
		"launchservices",
		"com.apple.lsd",
		"com.apple.coreservices",
		"com.apple.securityd",
		"com.apple.tccd",
		"com.apple.pboard",
		"process-info",
		"(target self)",
		"mach-task",
		"ipc-posix",
		"sysctl-name-prefix",
		"(with no-sandbox)",
	} {
		if strings.Contains(strings.ToLower(profile), forbidden) {
			t.Fatalf("profile contains forbidden IPC/process rule %q", forbidden)
		}
	}
}

func TestProfileAutomaticPATHSkipsBroadAndSensitiveAnchors(t *testing.T) {
	fixture := newProfileFixture(t)
	broad := []string{
		"/", "/Users", fixture.options.Home, "/Applications", "/Library", "/Network", "/Volumes",
		"/dev", "/private", "/private/etc", "/private/tmp", "/private/var", "/usr", "/opt",
		"/opt/homebrew", "/usr/local", "/opt/homebrew/etc", "/opt/homebrew/etc/private",
		"/opt/homebrew/var", "/usr/local/etc", "/usr/local/var/cache",
	}
	narrow := []string{
		"/opt/homebrew/bin",
		"/usr/local/bin",
		"/Applications/Xcode.app/Contents/Developer/usr/bin",
		"/Library/Developer/CommandLineTools/usr/bin",
		filepath.Join(fixture.options.Home, ".local", "bin"),
	}
	fake := make(map[string]resolvedProfilePath, len(broad)+len(narrow))
	for _, path := range append(slices.Clone(broad), narrow...) {
		fake[path] = resolvedProfilePath{path: path, kind: profilePathDirectory}
	}
	baseResolve := fixture.dependencies.resolve
	fixture.dependencies.resolve = func(path string) (resolvedProfilePath, error) {
		if resolved, ok := fake[path]; ok {
			return resolved, nil
		}
		return baseResolve(path)
	}
	fixture.options.HostEntries = []string{"LANG=C", "PATH=" + strings.Join(append(slices.Clone(broad), narrow...), string(os.PathListSeparator))}

	profile := renderProfileForTest(t, fixture.options, fixture.dependencies)
	readSection := profileTestSection(t, profile, "READ")
	for _, path := range broad {
		if strings.Contains(readSection, profileTestFilter("subpath", path)) {
			t.Fatalf("broad/sensitive PATH entry %q was granted", path)
		}
	}
	for _, path := range narrow {
		if !strings.Contains(readSection, profileTestFilter("subpath", path)) {
			t.Fatalf("narrow PATH entry %q was not granted", path)
		}
		if strings.Contains(profileTestSection(t, profile, "WRITE"), profileTestQuote(path)) {
			t.Fatalf("narrow PATH entry %q was writable", path)
		}
	}
}

func TestProfileAutomaticFixedRootsRequireDeclaredTypes(t *testing.T) {
	fixture := newProfileFixture(t)
	regularTarget := makeProfileTestFile(t, filepath.Join(fixture.base, "automatic-types", "regular"), 0o600)
	directoryTarget := makeProfileTestDirectory(t, filepath.Join(fixture.base, "automatic-types", "directory"))

	for _, test := range []struct {
		name     string
		source   string
		expected profilePathKind
		resolved resolvedProfilePath
	}{
		{
			name:     "fixed directory resolved to regular file",
			source:   "/usr/bin",
			expected: profilePathDirectory,
			resolved: resolvedProfilePath{path: regularTarget, kind: profilePathRegular},
		},
		{
			name:     "exact runtime file resolved to directory",
			source:   "/private/etc/hosts",
			expected: profilePathRegular,
			resolved: resolvedProfilePath{path: directoryTarget, kind: profilePathDirectory},
		},
	} {
		t.Run(test.name, func(t *testing.T) {
			options := fixture.options
			dependencies := fixture.dependencies
			dependencies.fixedPaths = []profileAutomaticPath{{path: test.source, kind: test.expected}}
			baseResolve := dependencies.resolve
			dependencies.resolve = func(path string) (resolvedProfilePath, error) {
				if path == test.source {
					return test.resolved, nil
				}
				return baseResolve(path)
			}
			if profile, err := generateProfileWithDependencies(options, dependencies); err == nil || profile != nil {
				t.Fatalf("mismatched automatic type generated %d profile bytes", len(profile))
			}
		})
	}
}

func TestProfileCanonicalAutomaticTargetsCannotGrantForbiddenAnchors(t *testing.T) {
	for _, class := range []string{"fixed", "developer", "PATH"} {
		t.Run(class, func(t *testing.T) {
			fixture := newProfileFixture(t)
			forbiddenTarget := fixture.options.Home
			if class != "PATH" {
				forbiddenTarget = makeProfileTestDirectory(t, filepath.Join(fixture.options.Home, "sensitive", class))
			}
			alias := filepath.Join(fixture.base, "automatic-alias-"+class)
			if err := os.Symlink(forbiddenTarget, alias); err != nil {
				t.Fatal(err)
			}
			switch class {
			case "fixed":
				fixture.dependencies.fixedPaths = []profileAutomaticPath{{path: alias, kind: profilePathDirectory}}
			case "developer":
				fixture.dependencies.developerRoot = func() (string, bool, error) {
					return alias, true, nil
				}
			case "PATH":
				fixture.options.HostEntries = []string{"PATH=" + alias}
			}

			profile := renderProfileForTest(t, fixture.options, fixture.dependencies)
			readData := profileTestSection(t, profile, "READ-DATA")
			if strings.Contains(readData, profileTestQuote(forbiddenTarget)) || strings.Contains(readData, profileTestQuote(alias)) {
				t.Fatalf("canonical automatic %s target inside the user home was granted", class)
			}
		})
	}
}

func TestProfileCanonicalAutomaticSymlinksToBroadAnchorAreSkipped(t *testing.T) {
	for _, class := range []string{"fixed", "developer", "PATH"} {
		t.Run(class, func(t *testing.T) {
			fixture := newProfileFixture(t)
			alias := filepath.Join(fixture.base, "broad-root-alias-"+class)
			if err := os.Symlink(string(filepath.Separator), alias); err != nil {
				t.Fatal(err)
			}
			switch class {
			case "fixed":
				fixture.dependencies.fixedPaths = []profileAutomaticPath{{path: alias, kind: profilePathDirectory}}
			case "developer":
				fixture.dependencies.developerRoot = func() (string, bool, error) {
					return alias, true, nil
				}
			case "PATH":
				fixture.options.HostEntries = []string{"PATH=" + alias}
			}

			profile := renderProfileForTest(t, fixture.options, fixture.dependencies)
			if strings.Contains(profileTestSection(t, profile, "READ-DATA"), profileTestFilter("subpath", string(filepath.Separator))) {
				t.Fatalf("canonical automatic %s symlink target granted the filesystem root", class)
			}
		})
	}
}

func TestProfileConfiguredShellIsOneCanonicalExecutableLiteral(t *testing.T) {
	fixture := newProfileFixture(t)
	shellTarget := makeProfileTestFile(t, filepath.Join(fixture.base, "shells", "real shell"), 0o700)
	shellAlias := filepath.Join(fixture.base, "shell alias")
	if err := os.Symlink(shellTarget, shellAlias); err != nil {
		t.Fatal(err)
	}
	fixture.options.Shell = shellAlias

	profile := renderProfileForTest(t, fixture.options, fixture.dependencies)
	shellSection := profileTestSection(t, profile, "SHELL")
	if strings.Count(shellSection, profileTestFilter("literal", shellTarget)) != 1 {
		t.Fatalf("canonical shell is not exactly one literal grant:\n%s", shellSection)
	}
	if strings.Contains(shellSection, shellAlias) ||
		strings.Contains(shellSection, profileTestFilter("subpath", filepath.Dir(shellTarget))) {
		t.Fatal("shell alias or shell parent received a grant")
	}
}

func TestProfileRejectsConfiguredShellOverlappingPrivateState(t *testing.T) {
	for _, test := range []struct {
		name      string
		shellPath func(profileFixture) string
	}{
		{
			name: "state root executable",
			shellPath: func(fixture profileFixture) string {
				return filepath.Join(fixture.options.Directories.Root, "private-shell")
			},
		},
		{
			name: "profiles executable",
			shellPath: func(fixture profileFixture) string {
				return filepath.Join(fixture.state.profiles, "private-shell")
			},
		},
	} {
		t.Run(test.name, func(t *testing.T) {
			fixture := newProfileFixture(t)
			privateShell := makeProfileTestFile(t, test.shellPath(fixture), 0o700)
			fixture.options.Shell = privateShell

			profile, err := generateProfileWithDependencies(fixture.options, fixture.dependencies)
			if err == nil || profile != nil {
				t.Fatalf("private-state shell generated %d bytes containing a shell/profile grant", len(profile))
			}
		})
	}
}

func TestProfileRejectsInvalidConfiguredShell(t *testing.T) {
	fixture := newProfileFixture(t)
	nonExecutable := makeProfileTestFile(t, filepath.Join(fixture.base, "shells", "not-executable"), 0o600)
	directory := makeProfileTestDirectory(t, filepath.Join(fixture.base, "shells", "directory"))
	missing := filepath.Join(fixture.base, "shells", "missing")

	for _, shell := range []string{"relative-shell", nonExecutable, directory, missing, "bad\nname", string([]byte{'/', 'b', 'a', 'd', 0xff})} {
		t.Run(profileTestName(shell), func(t *testing.T) {
			options := fixture.options
			options.Shell = shell
			if profile, err := generateProfileWithDependencies(options, fixture.dependencies); err == nil || profile != nil {
				t.Fatalf("invalid shell generated %d bytes", len(profile))
			}
		})
	}

	options := fixture.options
	options.Shell = "/injected/special"
	dependencies := fixture.dependencies
	baseResolve := dependencies.resolve
	dependencies.resolve = func(path string) (resolvedProfilePath, error) {
		if path == options.Shell {
			return resolvedProfilePath{path: path, kind: profilePathSpecial, executable: true}, nil
		}
		return baseResolve(path)
	}
	if profile, err := generateProfileWithDependencies(options, dependencies); err == nil || profile != nil {
		t.Fatalf("special shell generated %d bytes", len(profile))
	}
}

func TestProfileExpandsOnlyResolvedHomeTildePrefix(t *testing.T) {
	fixture := newProfileFixture(t)
	toolDirectory := makeProfileTestDirectory(t, filepath.Join(fixture.options.Home, "tool roots", "bin"))
	fixture.options.ReadPaths = []string{"~/tool roots/bin"}
	profile := renderProfileForTest(t, fixture.options, fixture.dependencies)
	if !strings.Contains(profileTestSection(t, profile, "READ"), profileTestFilter("subpath", toolDirectory)) {
		t.Fatal("~/ was not expanded from the resolved home")
	}

	for _, syntax := range []string{"$OTTO_ROOT", "${OTTO_ROOT}", "$(whoami)", "`whoami`", "*.tools"} {
		t.Run(profileTestName(syntax), func(t *testing.T) {
			literal := filepath.Join(fixture.base, syntax)
			options := fixture.options
			options.ReadPaths = []string{literal}
			seen := ""
			dependencies := fixture.dependencies
			baseResolve := dependencies.resolve
			dependencies.resolve = func(path string) (resolvedProfilePath, error) {
				if path == literal {
					seen = path
					return resolvedProfilePath{}, fs.ErrNotExist
				}
				return baseResolve(path)
			}
			if profile, err := generateProfileWithDependencies(options, dependencies); err == nil || profile != nil {
				t.Fatalf("shell syntax generated %d bytes", len(profile))
			}
			if seen != literal {
				t.Fatal("shell syntax was transformed instead of checked literally")
			}
		})
	}
}

func TestProfileCanonicalizesReadPathsAndRejectsSpecialOrMissingEntries(t *testing.T) {
	fixture := newProfileFixture(t)
	directory := makeProfileTestDirectory(t, filepath.Join(fixture.base, "canonical", "directory"))
	file := makeProfileTestFile(t, filepath.Join(fixture.base, "canonical", "file"), 0o600)
	directoryAlias := filepath.Join(fixture.base, "directory-alias")
	fileAlias := filepath.Join(fixture.base, "file-alias")
	if err := os.Symlink(directory, directoryAlias); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(file, fileAlias); err != nil {
		t.Fatal(err)
	}
	fixture.options.ReadPaths = []string{directoryAlias, fileAlias}
	profile := renderProfileForTest(t, fixture.options, fixture.dependencies)
	readSection := profileTestSection(t, profile, "READ")
	if !strings.Contains(readSection, profileTestFilter("subpath", directory)) ||
		!strings.Contains(readSection, profileTestFilter("literal", file)) ||
		strings.Contains(readSection, directoryAlias) || strings.Contains(readSection, fileAlias) {
		t.Fatal("read path aliases were not canonicalized to typed grants")
	}

	for _, test := range []struct {
		name     string
		path     string
		resolved resolvedProfilePath
		err      error
	}{
		{name: "relative", path: "relative/path"},
		{name: "missing", path: filepath.Join(fixture.base, "missing"), err: fs.ErrNotExist},
		{name: "special", path: "/injected/special", resolved: resolvedProfilePath{path: "/injected/special", kind: profilePathSpecial}},
	} {
		t.Run(test.name, func(t *testing.T) {
			options := fixture.options
			options.ReadPaths = []string{test.path}
			dependencies := fixture.dependencies
			baseResolve := dependencies.resolve
			dependencies.resolve = func(path string) (resolvedProfilePath, error) {
				if path == test.path {
					return test.resolved, test.err
				}
				return baseResolve(path)
			}
			if generated, err := generateProfileWithDependencies(options, dependencies); err == nil || generated != nil {
				t.Fatalf("invalid read path generated %d bytes", len(generated))
			}
		})
	}
}

func TestProfileEscapesDynamicLiteralsWithoutInjection(t *testing.T) {
	fixture := newProfileFixture(t)
	name := "tools 空格 \\\" ) (allow network-outbound) ; #"
	path := makeProfileTestDirectory(t, filepath.Join(fixture.base, name))
	fixture.options.ReadPaths = []string{path}

	profile := renderProfileForTest(t, fixture.options, fixture.dependencies)
	if !utf8.ValidString(profile) || !strings.Contains(profile, profileTestFilter("subpath", path)) {
		t.Fatal("spaces, Unicode, quotes, backslashes, or profile punctuation were not safely represented")
	}
	if strings.Count(profile, "(deny default)") != 1 || strings.Contains(profile, "(allow network-outbound)\n") {
		t.Fatal("dynamic path injected an SBPL form")
	}

	invalid := []string{
		filepath.Join(fixture.base, "line\nbreak"),
		filepath.Join(fixture.base, "tab\tcontrol"),
		filepath.Join(fixture.base, "line\u2028separator"),
		filepath.Join(fixture.base, "paragraph\u2029separator"),
		filepath.Join(fixture.base, string([]byte{'n', 'u', 'l', 0, 'x'})),
		filepath.Join(fixture.base, string([]byte{'b', 'a', 'd', 0xff})),
	}
	for index, path := range invalid {
		t.Run(fmt.Sprintf("invalid-%d", index), func(t *testing.T) {
			options := fixture.options
			options.ReadPaths = []string{path}
			dependencies := withSyntheticDirectories(fixture.dependencies, []string{path})
			generated, err := generateProfileWithDependencies(options, dependencies)
			if err == nil || generated != nil {
				t.Fatalf("control/invalid path generated %d bytes", len(generated))
			}
			if len(err.Error()) > 128 || strings.Contains(err.Error(), fixture.base) || strings.Contains(err.Error(), "allow") {
				t.Fatalf("profile error is not bounded and source-free: %q", err)
			}
		})
	}
}

func TestProfileMarkerLikePathTextRendersWithoutTemplateReprocessing(t *testing.T) {
	fixture := newProfileFixture(t)
	markers := []string{profileReadMarker, profileWriteMarker, profileNetworkMarker, profileShellMarker}
	markerName := strings.Join(markers, "-")
	readDirectory := makeProfileTestDirectory(t, filepath.Join(fixture.base, "marker-paths", markerName))
	shell := makeProfileTestFile(t, filepath.Join(fixture.base, "marker-shells", markerName), 0o700)
	fixture.options.ReadPaths = []string{readDirectory}
	fixture.options.Shell = shell

	first := renderProfileForTest(t, fixture.options, fixture.dependencies)
	second := renderProfileForTest(t, fixture.options, fixture.dependencies)
	if first != second {
		t.Fatal("marker-like path text rendered nondeterministically")
	}
	if !strings.Contains(profileTestSection(t, first, "READ-DATA"), profileTestFilter("subpath", readDirectory)) ||
		!strings.Contains(profileTestSection(t, first, "SHELL"), profileTestFilter("literal", shell)) {
		t.Fatal("marker-like filenames were not rendered as escaped literal path text")
	}
	for _, marker := range markers {
		if strings.Count(first, marker) != 2 {
			t.Fatalf("marker-like text %q count = %d, want only the two dynamic path occurrences", marker, strings.Count(first, marker))
		}
	}
	for _, section := range []string{"READ", "WRITE", "NETWORK", "SHELL"} {
		if strings.Count(first, "; OTTO-DYNAMIC-"+section+"-BEGIN") != 1 ||
			strings.Count(first, "; OTTO-DYNAMIC-"+section+"-END") != 1 {
			t.Fatalf("marker-like text changed the %s section structure", section)
		}
	}
	if strings.Count(first, "(version 1)") != 1 || strings.Count(first, "(deny default)") != 1 ||
		strings.Contains(first, "(allow network-outbound)\n") {
		t.Fatal("marker-like path text injected or changed fixed profile forms")
	}
}

func TestProfileAncestorMetadataDoesNotGrantAncestorContents(t *testing.T) {
	fixture := newProfileFixture(t)
	ancestor := makeProfileTestDirectory(t, filepath.Join(fixture.base, "metadata-only"))
	approved := makeProfileTestDirectory(t, filepath.Join(ancestor, "approved", "bin"))
	fixture.options.ReadPaths = []string{approved}
	profile := renderProfileForTest(t, fixture.options, fixture.dependencies)

	metadata := profileTestSection(t, profile, "METADATA")
	readData := profileTestSection(t, profile, "READ-DATA")
	if !strings.Contains(metadata, profileTestFilter("literal", ancestor)) {
		t.Fatal("approved root ancestor lacks traversal metadata")
	}
	if strings.Contains(readData, profileTestQuote(ancestor)) ||
		!strings.Contains(readData, profileTestFilter("subpath", approved)) {
		t.Fatal("ancestor content was granted or approved root content was omitted")
	}
}

func TestProfileDynamicRootLimitsApplyAfterDeterministicCollapse(t *testing.T) {
	t.Run("root count rejected without truncation", func(t *testing.T) {
		fixture := newProfileFixture(t)
		paths := make([]string, 129)
		for index := range paths {
			paths[index] = fmt.Sprintf("/dynamic/tool-%03d", index)
		}
		fixture.options.ReadPaths = paths
		fixture.dependencies = withSyntheticDirectories(fixture.dependencies, paths)
		if profile, err := generateProfileWithDependencies(fixture.options, fixture.dependencies); err == nil || profile != nil {
			t.Fatalf("129 effective roots generated %d bytes", len(profile))
		}
	})

	t.Run("aggregate canonical bytes rejected without truncation", func(t *testing.T) {
		fixture := newProfileFixture(t)
		paths := []string{
			"/dynamic/a-" + strings.Repeat("a", 11_000),
			"/dynamic/b-" + strings.Repeat("b", 11_000),
			"/dynamic/c-" + strings.Repeat("c", 11_000),
		}
		fixture.options.ReadPaths = paths
		fixture.dependencies = withSyntheticDirectories(fixture.dependencies, paths)
		if profile, err := generateProfileWithDependencies(fixture.options, fixture.dependencies); err == nil || profile != nil {
			t.Fatalf("oversized roots generated %d bytes", len(profile))
		}
	})

	t.Run("duplicates and nested roots collapse before limits", func(t *testing.T) {
		fixture := newProfileFixture(t)
		parent := "/dynamic/tools"
		paths := []string{parent, parent}
		for index := 0; index < 129; index++ {
			paths = append(paths, fmt.Sprintf("%s/tool-%03d/bin", parent, index))
		}
		fixture.options.ReadPaths = slices.Clone(paths)
		fixture.dependencies = withSyntheticDirectories(fixture.dependencies, paths)
		profile := renderProfileForTest(t, fixture.options, fixture.dependencies)
		readData := profileTestSection(t, profile, "READ-DATA")
		if strings.Count(readData, profileTestFilter("subpath", parent)) != 1 || strings.Contains(readData, "tool-000") {
			t.Fatal("nested/duplicate roots were not collapsed to their parent")
		}
	})

	t.Run("128 effective roots accepted", func(t *testing.T) {
		fixture := newProfileFixture(t)
		paths := make([]string, 128)
		for index := range paths {
			paths[index] = fmt.Sprintf("/dynamic-%03d/bin", index)
		}
		fixture.options.ReadPaths = paths
		fixture.dependencies = withSyntheticDirectories(fixture.dependencies, paths)
		_ = renderProfileForTest(t, fixture.options, fixture.dependencies)
	})
}

func TestProfileEquivalentSemanticInputsRenderByteIdentically(t *testing.T) {
	fixture := newProfileFixture(t)
	parent := makeProfileTestDirectory(t, filepath.Join(fixture.base, "deterministic", "tools"))
	child := makeProfileTestDirectory(t, filepath.Join(parent, "nested", "bin"))
	alias := filepath.Join(fixture.base, "deterministic-alias")
	if err := os.Symlink(parent, alias); err != nil {
		t.Fatal(err)
	}
	pathOne := makeProfileTestDirectory(t, filepath.Join(fixture.base, "path-one"))
	pathTwo := makeProfileTestDirectory(t, filepath.Join(fixture.base, "path-two"))

	firstOptions := fixture.options
	firstOptions.ReadPaths = []string{child, alias, parent, child}
	firstOptions.HostEntries = []string{"PATH=" + pathOne + string(os.PathListSeparator) + pathTwo}
	firstDependencies := fixture.dependencies
	firstDependencies.fixedPaths = []profileAutomaticPath{
		{path: pathTwo, kind: profilePathDirectory},
		{path: pathOne, kind: profilePathDirectory},
	}

	secondOptions := fixture.options
	secondOptions.ReadPaths = []string{parent}
	secondOptions.HostEntries = []string{"LANG=C", "PATH=" + pathTwo + string(os.PathListSeparator) + pathOne}
	secondDependencies := fixture.dependencies
	secondDependencies.fixedPaths = []profileAutomaticPath{
		{path: pathOne, kind: profilePathDirectory},
		{path: pathTwo, kind: profilePathDirectory},
		{path: pathOne, kind: profilePathDirectory},
	}

	first := renderProfileForTest(t, firstOptions, firstDependencies)
	second := renderProfileForTest(t, secondOptions, secondDependencies)
	if first != second {
		t.Fatal("semantically equal roots rendered different profile bytes")
	}
}

func TestProfileReviewedAutomaticRootsAreNarrow(t *testing.T) {
	for _, required := range []string{
		"/bin", "/sbin", "/usr/bin", "/usr/sbin", "/usr/lib", "/usr/libexec", "/usr/share", "/usr/include",
		"/System", "/Library/Apple", "/Library/Developer", "/opt/homebrew/bin", "/opt/homebrew/lib",
		"/opt/homebrew/Cellar", "/opt/homebrew/opt", "/usr/local/bin", "/usr/local/lib", "/usr/local/Homebrew",
	} {
		if !slices.Contains(reviewedAutomaticPaths, required) {
			t.Fatalf("reviewed automatic roots omit %q", required)
		}
	}
	for _, forbidden := range []string{
		"/", "/Users", "/Applications", "/Library", "/Network", "/Volumes", "/dev", "/private", "/private/etc",
		"/private/tmp", "/private/var", "/usr", "/opt", "/opt/homebrew", "/opt/homebrew/etc", "/opt/homebrew/var",
		"/usr/local", "/usr/local/etc", "/usr/local/var",
	} {
		if slices.Contains(reviewedAutomaticPaths, forbidden) {
			t.Fatalf("reviewed automatic roots contain broad/sensitive %q", forbidden)
		}
	}
	for _, path := range reviewedRuntimeFiles {
		if !strings.HasPrefix(path, "/private/etc/") || path == "/private/etc" {
			t.Fatalf("runtime compatibility path is not one exact /private/etc file: %q", path)
		}
	}
}

func TestProfileEmbeddedTemplateHasFourUniqueMarkers(t *testing.T) {
	markers := []string{profileReadMarker, profileWriteMarker, profileNetworkMarker, profileShellMarker}
	for _, marker := range markers {
		if strings.Count(profileTemplate, marker) != 1 {
			t.Fatalf("template marker %q count = %d, want 1", marker, strings.Count(profileTemplate, marker))
		}
	}
}

type profileFixture struct {
	base         string
	state        *state
	options      profileOptions
	dependencies profileDependencies
}

func newProfileFixture(t *testing.T) profileFixture {
	t.Helper()
	base := t.TempDir()
	workspace := makeProfileTestDirectory(t, filepath.Join(base, "workspace"))
	cacheBase := makeProfileTestDirectory(t, filepath.Join(base, "user-cache"))
	privateState, err := createState(workspace, cacheBase)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if err := privateState.close(); err != nil {
			t.Errorf("state.close() error = %v", err)
		}
	})
	home := makeProfileTestDirectory(t, filepath.Join(base, "resolved-home"))
	shell := makeProfileTestFile(t, filepath.Join(base, "shells", "shell"), 0o700)
	return profileFixture{
		base:  base,
		state: privateState,
		options: profileOptions{
			Workspace:   workspace,
			Directories: privateState.directories,
			Shell:       shell,
			Home:        home,
			HostEntries: []string{"PATH="},
			Network:     sandbox.NetworkDeny,
		},
		dependencies: profileDependencies{
			resolve: resolveProfilePath,
			developerRoot: func() (string, bool, error) {
				return "", false, nil
			},
		},
	}
}

func renderProfileForTest(t *testing.T, options profileOptions, dependencies profileDependencies) string {
	t.Helper()
	profile, err := generateProfileWithDependencies(options, dependencies)
	if err != nil {
		t.Fatalf("generateProfileWithDependencies() error = %v", err)
	}
	return string(profile)
}

func withSyntheticDirectories(dependencies profileDependencies, paths []string) profileDependencies {
	resolved := make(map[string]resolvedProfilePath, len(paths))
	for _, path := range paths {
		resolved[path] = resolvedProfilePath{path: path, kind: profilePathDirectory}
	}
	baseResolve := dependencies.resolve
	dependencies.resolve = func(path string) (resolvedProfilePath, error) {
		if result, ok := resolved[path]; ok {
			return result, nil
		}
		return baseResolve(path)
	}
	return dependencies
}

func makeProfileTestDirectory(t *testing.T, path string) string {
	t.Helper()
	if err := os.MkdirAll(path, 0o700); err != nil {
		t.Fatal(err)
	}
	canonical, err := filepath.EvalSymlinks(path)
	if err != nil {
		t.Fatal(err)
	}
	return canonical
}

func makeProfileTestFile(t *testing.T, path string, mode fs.FileMode) string {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte("fixture"), mode); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(path, mode); err != nil {
		t.Fatal(err)
	}
	canonical, err := filepath.EvalSymlinks(path)
	if err != nil {
		t.Fatal(err)
	}
	return canonical
}

func profileTestSection(t *testing.T, profile, name string) string {
	t.Helper()
	begin := "; OTTO-DYNAMIC-" + name + "-BEGIN"
	end := "; OTTO-DYNAMIC-" + name + "-END"
	start := strings.Index(profile, begin)
	finish := strings.Index(profile, end)
	if start < 0 || finish < 0 || finish < start {
		t.Fatalf("profile section %s not found", name)
	}
	return profile[start : finish+len(end)]
}

func profileTestQuote(path string) string {
	return `"` + strings.NewReplacer(`\`, `\\`, `"`, `\"`).Replace(path) + `"`
}

func profileTestFilter(kind, path string) string {
	return "(" + kind + " " + profileTestQuote(path) + ")"
}

func profileTestName(value string) string {
	value = strings.Map(func(r rune) rune {
		if r >= 'a' && r <= 'z' || r >= 'A' && r <= 'Z' || r >= '0' && r <= '9' {
			return r
		}
		return '-'
	}, value)
	value = strings.Trim(value, "-")
	if value == "" || !utf8.ValidString(value) {
		return "invalid"
	}
	return value
}

func TestProfileDiscoveryErrorsRemainBounded(t *testing.T) {
	fixture := newProfileFixture(t)
	privateDiagnostic := filepath.Join(fixture.base, "private-discovery-value")
	fixture.dependencies.developerRoot = func() (string, bool, error) {
		return "", false, errors.New(privateDiagnostic)
	}
	profile, err := generateProfileWithDependencies(fixture.options, fixture.dependencies)
	if err == nil || profile != nil {
		t.Fatalf("discovery failure generated %d bytes", len(profile))
	}
	if len(err.Error()) > 128 || strings.Contains(err.Error(), privateDiagnostic) || strings.Contains(err.Error(), fixture.base) {
		t.Fatalf("profile error leaks discovery details: %q", err)
	}
}
