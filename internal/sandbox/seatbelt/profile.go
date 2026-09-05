package seatbelt

import (
	_ "embed"
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/sandbox"
)

const (
	maxProfileDynamicRoots     = 128
	maxProfileDynamicPathBytes = 32 * 1024

	profileReadMarker    = "@@OTTO_PROFILE_READ_RULES@@"
	profileWriteMarker   = "@@OTTO_PROFILE_WRITE_RULES@@"
	profileNetworkMarker = "@@OTTO_PROFILE_NETWORK_RULES@@"
	profileShellMarker   = "@@OTTO_PROFILE_SHELL_RULE@@"
)

var errProfileRejected = errors.New("seatbelt profile rejected")

//go:embed profile_v1.sb
var profileTemplate string

var reviewedAutomaticPaths = []string{
	"/bin",
	"/sbin",
	"/usr/bin",
	"/usr/sbin",
	"/usr/lib",
	"/usr/libexec",
	"/usr/share",
	"/usr/include",
	"/System",
	"/Library/Apple",
	"/Library/Developer",
	"/opt/homebrew/bin",
	"/opt/homebrew/sbin",
	"/opt/homebrew/lib",
	"/opt/homebrew/share",
	"/opt/homebrew/include",
	"/opt/homebrew/Cellar",
	"/opt/homebrew/opt",
	"/usr/local/bin",
	"/usr/local/sbin",
	"/usr/local/lib",
	"/usr/local/share",
	"/usr/local/include",
	"/usr/local/Cellar",
	"/usr/local/opt",
	"/usr/local/Homebrew",
}

var reviewedRuntimeFiles = []string{
	"/private/etc/hosts",
	"/private/etc/protocols",
	"/private/etc/resolv.conf",
	"/private/etc/services",
	"/private/etc/ssl/cert.pem",
}

type profileOptions struct {
	Workspace   string
	Directories sandbox.PrivateDirectories
	Shell       string
	Home        string
	HostEntries []string
	ReadPaths   []string
	Network     sandbox.NetworkMode
}

type profilePathKind uint8

const (
	profilePathDirectory profilePathKind = iota + 1
	profilePathRegular
	profilePathSpecial
)

type resolvedProfilePath struct {
	path       string
	kind       profilePathKind
	executable bool
}

type profileAutomaticPath struct {
	path string
	kind profilePathKind
}

type profileDependencies struct {
	resolve       func(string) (resolvedProfilePath, error)
	fixedPaths    []profileAutomaticPath
	developerRoot func() (string, bool, error)
}

type resolvedProfileOptions struct {
	workspace   string
	directories sandbox.PrivateDirectories
	shell       string
	home        string
	network     sandbox.NetworkMode
}

//lint:ignore U1000 Task 5's Darwin Driver consumes this production profile boundary.
func generateProfile(options profileOptions) ([]byte, error) {
	fixedPaths := make([]profileAutomaticPath, 0, len(reviewedAutomaticPaths)+len(reviewedRuntimeFiles))
	for _, path := range reviewedAutomaticPaths {
		fixedPaths = append(fixedPaths, profileAutomaticPath{path: path, kind: profilePathDirectory})
	}
	for _, path := range reviewedRuntimeFiles {
		fixedPaths = append(fixedPaths, profileAutomaticPath{path: path, kind: profilePathRegular})
	}
	return generateProfileWithDependencies(options, profileDependencies{
		resolve:       resolveProfilePath,
		fixedPaths:    fixedPaths,
		developerRoot: discoverDeveloperRoot,
	})
}

func generateProfileWithDependencies(options profileOptions, dependencies profileDependencies) ([]byte, error) {
	if dependencies.resolve == nil || dependencies.developerRoot == nil {
		return nil, errProfileRejected
	}
	resolved, err := resolveRequiredProfileOptions(options, dependencies.resolve)
	if err != nil {
		return nil, errProfileRejected
	}

	roots, err := discoverProfileReadRoots(options, resolved, dependencies)
	if err != nil {
		return nil, errProfileRejected
	}
	roots = collapseProfileReadRoots(roots, []string{
		resolved.workspace,
		resolved.directories.Home,
		resolved.directories.Temp,
		resolved.directories.Cache,
	})
	if !profileRootsWithinLimits(roots) {
		return nil, errProfileRejected
	}

	writable := []string{
		resolved.workspace,
		resolved.directories.Home,
		resolved.directories.Temp,
		resolved.directories.Cache,
	}
	sortProfilePaths(writable)
	metadata := profileMetadataAncestors(writable, roots, resolved.shell)

	readRules, err := renderProfileReadRules(metadata, writable, roots)
	if err != nil {
		return nil, errProfileRejected
	}
	writeRules, err := renderProfileWriteRules(writable)
	if err != nil {
		return nil, errProfileRejected
	}
	networkRules, err := renderProfileNetworkRules(resolved.network)
	if err != nil {
		return nil, errProfileRejected
	}
	shellRule, err := renderProfileShellRule(resolved.shell)
	if err != nil {
		return nil, errProfileRejected
	}

	for _, marker := range []string{profileReadMarker, profileWriteMarker, profileNetworkMarker, profileShellMarker} {
		if strings.Count(profileTemplate, marker) != 1 {
			return nil, errProfileRejected
		}
	}
	profile := strings.NewReplacer(
		profileReadMarker, readRules,
		profileWriteMarker, writeRules,
		profileNetworkMarker, networkRules,
		profileShellMarker, shellRule,
	).Replace(profileTemplate)
	return []byte(profile), nil
}

func resolveRequiredProfileOptions(options profileOptions, resolve func(string) (resolvedProfilePath, error)) (resolvedProfileOptions, error) {
	workspace, err := resolveRequiredProfileDirectory(options.Workspace, resolve)
	if err != nil {
		return resolvedProfileOptions{}, errProfileRejected
	}
	root, err := resolveRequiredProfileDirectory(options.Directories.Root, resolve)
	if err != nil {
		return resolvedProfileOptions{}, errProfileRejected
	}
	homeDirectory, err := resolveRequiredProfileDirectory(options.Directories.Home, resolve)
	if err != nil {
		return resolvedProfileOptions{}, errProfileRejected
	}
	tempDirectory, err := resolveRequiredProfileDirectory(options.Directories.Temp, resolve)
	if err != nil {
		return resolvedProfileOptions{}, errProfileRejected
	}
	cacheDirectory, err := resolveRequiredProfileDirectory(options.Directories.Cache, resolve)
	if err != nil {
		return resolvedProfileOptions{}, errProfileRejected
	}
	resolvedHome, err := resolveRequiredProfileDirectory(options.Home, resolve)
	if err != nil {
		return resolvedProfileOptions{}, errProfileRejected
	}
	shell, err := resolve(options.Shell)
	if err != nil || !validResolvedProfilePath(shell) ||
		shell.kind != profilePathRegular || !shell.executable {
		return resolvedProfileOptions{}, errProfileRejected
	}

	directories := sandbox.PrivateDirectories{
		Root:  root,
		Home:  homeDirectory,
		Temp:  tempDirectory,
		Cache: cacheDirectory,
	}
	if directories.Home != filepath.Join(root, "home") ||
		directories.Temp != filepath.Join(root, "tmp") ||
		directories.Cache != filepath.Join(root, "cache") ||
		pathsOverlap(workspace, root) || pathsOverlap(shell.path, root) {
		return resolvedProfileOptions{}, errProfileRejected
	}
	if options.Network != sandbox.NetworkAllow && options.Network != sandbox.NetworkDeny {
		return resolvedProfileOptions{}, errProfileRejected
	}
	return resolvedProfileOptions{
		workspace:   workspace,
		directories: directories,
		shell:       shell.path,
		home:        resolvedHome,
		network:     options.Network,
	}, nil
}

func resolveRequiredProfileDirectory(path string, resolve func(string) (resolvedProfilePath, error)) (string, error) {
	resolved, err := resolve(path)
	if err != nil || !validResolvedProfilePath(resolved) || resolved.kind != profilePathDirectory {
		return "", errProfileRejected
	}
	return resolved.path, nil
}

func discoverProfileReadRoots(options profileOptions, resolved resolvedProfileOptions, dependencies profileDependencies) ([]resolvedProfilePath, error) {
	roots := make([]resolvedProfilePath, 0, len(dependencies.fixedPaths)+len(options.ReadPaths)+8)
	for _, automatic := range dependencies.fixedPaths {
		candidate, present, err := resolveAutomaticProfilePath(automatic.path, automatic.kind, dependencies.resolve)
		if err != nil {
			return nil, errProfileRejected
		}
		if !present {
			continue
		}
		if exactBroadAutomaticAnchor(candidate.path, resolved.home) {
			continue
		}
		if pathsOverlap(candidate.path, resolved.directories.Root) {
			return nil, errProfileRejected
		}
		if !safeFixedAutomaticRoot(automatic, candidate.path, resolved.home) {
			continue
		}
		roots = append(roots, candidate)
	}

	developerRoot, present, err := dependencies.developerRoot()
	if err != nil {
		return nil, errProfileRejected
	}
	if present {
		candidate, resolveErr := dependencies.resolve(developerRoot)
		if resolveErr != nil || !validResolvedProfilePath(candidate) || candidate.kind != profilePathDirectory {
			return nil, errProfileRejected
		}
		if !exactBroadAutomaticAnchor(candidate.path, resolved.home) {
			if pathsOverlap(candidate.path, resolved.directories.Root) {
				return nil, errProfileRejected
			}
			if safeAutomaticDirectoryRoot(candidate.path, resolved.home, false) {
				roots = append(roots, candidate)
			}
		}
	}

	pathEntries, err := profilePATHEntries(options.HostEntries)
	if err != nil {
		return nil, errProfileRejected
	}
	for _, path := range pathEntries {
		if path == "" || !filepath.IsAbs(path) {
			continue
		}
		candidate, present, resolveErr := resolveAutomaticProfilePath(path, profilePathDirectory, dependencies.resolve)
		if resolveErr != nil {
			return nil, errProfileRejected
		}
		if !present {
			continue
		}
		if exactBroadAutomaticAnchor(candidate.path, resolved.home) {
			continue
		}
		if pathsOverlap(candidate.path, resolved.directories.Root) {
			return nil, errProfileRejected
		}
		if !safeAutomaticDirectoryRoot(candidate.path, resolved.home, true) {
			continue
		}
		roots = append(roots, candidate)
	}

	for _, configuredPath := range options.ReadPaths {
		expanded, expandErr := expandProfileReadPath(configuredPath, resolved.home)
		if expandErr != nil {
			return nil, errProfileRejected
		}
		candidate, resolveErr := dependencies.resolve(expanded)
		if resolveErr != nil || !validResolvedProfilePath(candidate) ||
			(candidate.kind != profilePathDirectory && candidate.kind != profilePathRegular) ||
			pathsOverlap(candidate.path, resolved.directories.Root) {
			return nil, errProfileRejected
		}
		roots = append(roots, candidate)
	}
	return roots, nil
}

func resolveAutomaticProfilePath(path string, expectedKind profilePathKind, resolve func(string) (resolvedProfilePath, error)) (resolvedProfilePath, bool, error) {
	if !validProfilePathText(path) || !filepath.IsAbs(path) ||
		(expectedKind != profilePathDirectory && expectedKind != profilePathRegular) {
		return resolvedProfilePath{}, false, errProfileRejected
	}
	resolved, err := resolve(path)
	if errors.Is(err, fs.ErrNotExist) {
		return resolvedProfilePath{}, false, nil
	}
	if err != nil || !validResolvedProfilePath(resolved) || resolved.kind != expectedKind {
		return resolvedProfilePath{}, false, errProfileRejected
	}
	return resolved, true, nil
}

func profilePATHEntries(hostEntries []string) ([]string, error) {
	pathValue := ""
	found := false
	for _, entry := range hostEntries {
		if !strings.HasPrefix(entry, "PATH=") {
			continue
		}
		if found {
			return nil, errProfileRejected
		}
		found = true
		pathValue = strings.TrimPrefix(entry, "PATH=")
	}
	if !found || pathValue == "" {
		return nil, nil
	}
	if !utf8.ValidString(pathValue) || strings.IndexByte(pathValue, 0) >= 0 {
		return nil, errProfileRejected
	}
	return filepath.SplitList(pathValue), nil
}

func expandProfileReadPath(path, home string) (string, error) {
	if !validProfilePathText(path) {
		return "", errProfileRejected
	}
	if strings.HasPrefix(path, "~/") {
		return filepath.Join(home, strings.TrimPrefix(path, "~/")), nil
	}
	if !filepath.IsAbs(path) {
		return "", errProfileRejected
	}
	return path, nil
}

func exactBroadAutomaticAnchor(path, home string) bool {
	if path == home {
		return true
	}
	switch path {
	case "/", "/Users", "/Applications", "/Library", "/Network", "/Volumes",
		"/dev", "/private", "/private/etc", "/private/tmp", "/private/var",
		"/usr", "/opt", "/opt/homebrew", "/usr/local":
		return true
	default:
		return false
	}
}

func safeAutomaticDirectoryRoot(path, home string, allowHomeDescendant bool) bool {
	if exactBroadAutomaticAnchor(path, home) {
		return false
	}
	if pathWithin(home, path) {
		return allowHomeDescendant && path != home
	}
	if pathWithin("/Users", path) || pathWithin("/Network", path) || pathWithin("/Volumes", path) ||
		pathWithin("/dev", path) || pathWithin("/private/etc", path) || pathWithin("/private/tmp", path) ||
		pathWithin("/private/var", path) || pathWithin("/opt/homebrew/etc", path) ||
		pathWithin("/opt/homebrew/var", path) || pathWithin("/usr/local/etc", path) ||
		pathWithin("/usr/local/var", path) {
		return false
	}
	return true
}

func safeFixedAutomaticRoot(automatic profileAutomaticPath, canonical, home string) bool {
	if automatic.kind == profilePathDirectory {
		return safeAutomaticDirectoryRoot(canonical, home, false)
	}
	if automatic.kind != profilePathRegular || exactBroadAutomaticAnchor(canonical, home) ||
		pathWithin(home, canonical) || pathWithin("/Users", canonical) || pathWithin("/Network", canonical) ||
		pathWithin("/Volumes", canonical) || pathWithin("/dev", canonical) || pathWithin("/private/tmp", canonical) ||
		pathWithin("/opt/homebrew/etc", canonical) || pathWithin("/opt/homebrew/var", canonical) ||
		pathWithin("/usr/local/etc", canonical) || pathWithin("/usr/local/var", canonical) {
		return false
	}
	if pathWithin("/private/etc", canonical) {
		return strings.HasPrefix(automatic.path, "/private/etc/")
	}
	if pathWithin("/private/var", canonical) {
		return automatic.path == "/private/etc/resolv.conf" && canonical == "/private/var/run/resolv.conf"
	}
	return true
}

func collapseProfileReadRoots(roots []resolvedProfilePath, writable []string) []resolvedProfilePath {
	byPath := make(map[string]resolvedProfilePath, len(roots))
	for _, root := range roots {
		if existing, ok := byPath[root.path]; !ok || existing.kind == root.kind {
			byPath[root.path] = root
		}
	}
	unique := make([]resolvedProfilePath, 0, len(byPath))
	for _, root := range byPath {
		alreadyWritable := false
		for _, writableRoot := range writable {
			if pathWithin(writableRoot, root.path) {
				alreadyWritable = true
				break
			}
		}
		if !alreadyWritable {
			unique = append(unique, root)
		}
	}
	sort.Slice(unique, func(i, j int) bool {
		leftDepth := profilePathDepth(unique[i].path)
		rightDepth := profilePathDepth(unique[j].path)
		if leftDepth != rightDepth {
			return leftDepth < rightDepth
		}
		return unique[i].path < unique[j].path
	})

	collapsed := make([]resolvedProfilePath, 0, len(unique))
	for _, candidate := range unique {
		contained := false
		for _, parent := range collapsed {
			if parent.kind == profilePathDirectory && pathWithin(parent.path, candidate.path) {
				contained = true
				break
			}
		}
		if !contained {
			collapsed = append(collapsed, candidate)
		}
	}
	return collapsed
}

func profileRootsWithinLimits(roots []resolvedProfilePath) bool {
	if len(roots) > maxProfileDynamicRoots {
		return false
	}
	bytes := 0
	for _, root := range roots {
		if len(root.path) > maxProfileDynamicPathBytes-bytes {
			return false
		}
		bytes += len(root.path)
	}
	return true
}

func profileMetadataAncestors(writable []string, roots []resolvedProfilePath, shell string) []string {
	metadata := make(map[string]struct{})
	addAncestors := func(path string) {
		for ancestor := filepath.Dir(path); ; ancestor = filepath.Dir(ancestor) {
			metadata[ancestor] = struct{}{}
			next := filepath.Dir(ancestor)
			if next == ancestor {
				break
			}
		}
	}
	for _, path := range writable {
		addAncestors(path)
	}
	for _, root := range roots {
		addAncestors(root.path)
	}
	addAncestors(shell)

	paths := make([]string, 0, len(metadata))
	for path := range metadata {
		paths = append(paths, path)
	}
	sortProfilePaths(paths)
	return paths
}

func renderProfileReadRules(metadata, writable []string, roots []resolvedProfilePath) (string, error) {
	var builder strings.Builder
	builder.WriteString("; OTTO-DYNAMIC-READ-BEGIN\n")
	builder.WriteString("; OTTO-DYNAMIC-METADATA-BEGIN\n")
	if err := writeProfileRule(&builder, "file-read-metadata", profileLiteralFilters(metadata)); err != nil {
		return "", err
	}
	builder.WriteString("; OTTO-DYNAMIC-METADATA-END\n")
	builder.WriteString("; OTTO-DYNAMIC-READ-DATA-BEGIN\n")
	filters := make([]profileFilter, 0, len(writable)+len(roots))
	for _, path := range writable {
		filters = append(filters, profileFilter{kind: "subpath", path: path})
	}
	for _, root := range roots {
		kind := "literal"
		if root.kind == profilePathDirectory {
			kind = "subpath"
		}
		filters = append(filters, profileFilter{kind: kind, path: root.path})
	}
	if err := writeProfileRule(&builder, "file-read*", filters); err != nil {
		return "", err
	}
	builder.WriteString("; OTTO-DYNAMIC-READ-DATA-END\n")
	builder.WriteString("; OTTO-DYNAMIC-READ-END")
	return builder.String(), nil
}

func renderProfileWriteRules(writable []string) (string, error) {
	var builder strings.Builder
	builder.WriteString("; OTTO-DYNAMIC-WRITE-BEGIN\n")
	filters := make([]profileFilter, 0, len(writable))
	for _, path := range writable {
		filters = append(filters, profileFilter{kind: "subpath", path: path})
	}
	if err := writeProfileRule(&builder, "file-write*", filters); err != nil {
		return "", err
	}
	builder.WriteString("; OTTO-DYNAMIC-WRITE-END")
	return builder.String(), nil
}

func renderProfileNetworkRules(network sandbox.NetworkMode) (string, error) {
	switch network {
	case sandbox.NetworkDeny:
		return "; OTTO-DYNAMIC-NETWORK-BEGIN\n; OTTO-DYNAMIC-NETWORK-END", nil
	case sandbox.NetworkAllow:
		return strings.Join([]string{
			"; OTTO-DYNAMIC-NETWORK-BEGIN",
			"(allow mach-lookup",
			"  (global-name \"com.apple.mDNSResponder\"))",
			// getaddrinfo on current macOS connects this exact resolver socket
			// directly (observed via sandbox-exec bisection on macOS 26), in
			// addition to the mDNSResponder mach broker above; without this
			// literal exception, network = "allow" cannot resolve hostnames.
			"(allow network-outbound",
			"  (remote ip)",
			"  (remote unix-socket (path \"/private/var/run/mDNSResponder\")))",
			// Go's crypto/x509 verifier evaluates server certificates through
			// the Security framework, which brokers to trustd. Without these
			// exact services, every Go TLS client inside the sandbox fails with
			// "x509: OSStatus -26276" even though the CA bundle is readable.
			"(allow mach-lookup",
			"  (global-name \"com.apple.trustd\")",
			"  (global-name \"com.apple.trustd.agent\"))",
			"(allow network-bind",
			"  (local ip))",
			"(allow network-inbound",
			"  (local ip))",
			"; OTTO-DYNAMIC-NETWORK-END",
		}, "\n"), nil
	default:
		return "", errProfileRejected
	}
}

func renderProfileShellRule(shell string) (string, error) {
	var builder strings.Builder
	builder.WriteString("; OTTO-DYNAMIC-SHELL-BEGIN\n")
	if err := writeProfileRule(&builder, "file-read*", []profileFilter{{kind: "literal", path: shell}}); err != nil {
		return "", err
	}
	builder.WriteString("; OTTO-DYNAMIC-SHELL-END")
	return builder.String(), nil
}

type profileFilter struct {
	kind string
	path string
}

func profileLiteralFilters(paths []string) []profileFilter {
	filters := make([]profileFilter, 0, len(paths))
	for _, path := range paths {
		filters = append(filters, profileFilter{kind: "literal", path: path})
	}
	return filters
}

func writeProfileRule(builder *strings.Builder, operation string, filters []profileFilter) error {
	if len(filters) == 0 {
		return errProfileRejected
	}
	builder.WriteString("(allow ")
	builder.WriteString(operation)
	builder.WriteByte('\n')
	for _, filter := range filters {
		literal, err := profileStringLiteral(filter.path)
		if err != nil || filter.kind != "literal" && filter.kind != "subpath" {
			return errProfileRejected
		}
		builder.WriteString("  (")
		builder.WriteString(filter.kind)
		builder.WriteByte(' ')
		builder.WriteString(literal)
		builder.WriteString(")\n")
	}
	builder.WriteString(")\n")
	return nil
}

func profileStringLiteral(value string) (string, error) {
	if !validProfilePathText(value) {
		return "", errProfileRejected
	}
	var builder strings.Builder
	builder.Grow(len(value) + 2)
	builder.WriteByte('"')
	for _, character := range value {
		switch character {
		case '\\', '"':
			builder.WriteByte('\\')
			builder.WriteRune(character)
		default:
			builder.WriteRune(character)
		}
	}
	builder.WriteByte('"')
	return builder.String(), nil
}

func resolveProfilePath(path string) (resolvedProfilePath, error) {
	if !validProfilePathText(path) || !filepath.IsAbs(path) {
		return resolvedProfilePath{}, errProfileRejected
	}
	canonical, err := canonicalFilesystemPath(path)
	if err != nil {
		return resolvedProfilePath{}, err
	}
	if !validProfilePathText(canonical) || !filepath.IsAbs(canonical) || filepath.Clean(canonical) != canonical {
		return resolvedProfilePath{}, errProfileRejected
	}
	info, err := os.Lstat(canonical)
	if err != nil {
		return resolvedProfilePath{}, err
	}
	if info.Mode()&os.ModeSymlink != 0 {
		return resolvedProfilePath{}, errProfileRejected
	}
	resolved := resolvedProfilePath{path: canonical, kind: profilePathSpecial}
	switch {
	case info.IsDir():
		resolved.kind = profilePathDirectory
	case info.Mode().IsRegular():
		resolved.kind = profilePathRegular
		resolved.executable = info.Mode().Perm()&0o111 != 0
	}
	return resolved, nil
}

func canonicalFilesystemPath(path string) (string, error) {
	resolved, err := filepath.EvalSymlinks(path)
	if err != nil {
		return "", err
	}
	if !filepath.IsAbs(resolved) || filepath.Clean(resolved) != resolved {
		return "", errProfileRejected
	}

	volume := filepath.VolumeName(resolved)
	root := volume + string(filepath.Separator)
	relative := strings.TrimPrefix(resolved, root)
	if relative == "" {
		return root, nil
	}
	current := root
	for _, component := range strings.Split(relative, string(filepath.Separator)) {
		requested := filepath.Join(current, component)
		requestedInfo, err := os.Lstat(requested)
		if err != nil {
			return "", err
		}
		entries, err := os.ReadDir(current)
		if err != nil {
			return "", err
		}
		actual := ""
		for _, entry := range entries {
			if entry.Name() == component {
				actual = entry.Name()
				break
			}
		}
		if actual == "" {
			for _, entry := range entries {
				entryInfo, infoErr := entry.Info()
				if infoErr != nil {
					return "", infoErr
				}
				if os.SameFile(requestedInfo, entryInfo) && (actual == "" || entry.Name() < actual) {
					actual = entry.Name()
				}
			}
		}
		if actual == "" {
			return "", errProfileRejected
		}
		current = filepath.Join(current, actual)
	}
	return current, nil
}

func validResolvedProfilePath(path resolvedProfilePath) bool {
	return validProfilePathText(path.path) && filepath.IsAbs(path.path) && filepath.Clean(path.path) == path.path &&
		(path.kind == profilePathDirectory || path.kind == profilePathRegular || path.kind == profilePathSpecial)
}

func validProfilePathText(path string) bool {
	if path == "" || !utf8.ValidString(path) || strings.IndexByte(path, 0) >= 0 {
		return false
	}
	for _, character := range path {
		if unicode.IsControl(character) || unicode.Is(unicode.Zl, character) || unicode.Is(unicode.Zp, character) {
			return false
		}
	}
	return true
}

//lint:ignore U1000 Reachable through generateProfile when the Task 5 Driver is added.
func discoverDeveloperRoot() (string, bool, error) {
	root, err := filepath.EvalSymlinks("/var/db/xcode_select_link")
	if errors.Is(err, fs.ErrNotExist) {
		return "", false, nil
	}
	if err != nil {
		return "", false, errProfileRejected
	}
	return root, true, nil
}

func pathWithin(parent, child string) bool {
	relative, err := filepath.Rel(parent, child)
	return err == nil && relative != ".." &&
		!strings.HasPrefix(relative, ".."+string(filepath.Separator))
}

func pathsOverlap(left, right string) bool {
	return pathWithin(left, right) || pathWithin(right, left)
}

func profilePathDepth(path string) int {
	if path == string(filepath.Separator) {
		return 0
	}
	return strings.Count(filepath.Clean(path), string(filepath.Separator))
}

func sortProfilePaths(paths []string) {
	sort.Slice(paths, func(i, j int) bool {
		leftDepth := profilePathDepth(paths[i])
		rightDepth := profilePathDepth(paths[j])
		if leftDepth != rightDepth {
			return leftDepth < rightDepth
		}
		return paths[i] < paths[j]
	})
}
