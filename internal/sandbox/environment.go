package sandbox

import (
	"os"
	"path/filepath"
	"slices"
	"strings"
	"syscall"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/safetext"
	"github.com/baiyuqing/otto/internal/urlprivacy"
)

type EnvironmentOptions struct {
	HostEntries        []string
	ProviderNames      []string
	AllowNames         []string
	PrivateDirectories *PrivateDirectories
}

type EnvironmentSnapshot struct {
	entries            []string
	redactions         []string
	redactionsComplete bool
}

func ParseEnvironment(entries []string) (map[string]string, error) {
	environment := make(map[string]string, len(entries))
	for _, entry := range entries {
		if !utf8.ValidString(entry) || strings.IndexByte(entry, 0) >= 0 {
			return nil, ErrEnvironmentUnsafe
		}
		name, value, found := strings.Cut(entry, "=")
		if !found || !validEnvironmentName(name) {
			return nil, ErrEnvironmentUnsafe
		}
		environment[strings.Clone(name)] = strings.Clone(value)
	}
	return environment, nil
}

func ResolveEnvironment(options EnvironmentOptions) (EnvironmentSnapshot, error) {
	collector := safetext.NewSecretCollector()
	unsafe := false
	redactionsComplete := true

	providerNames := make(environmentNameSet, len(options.ProviderNames))
	for _, name := range options.ProviderNames {
		if name == "" {
			continue
		}
		if !utf8.ValidString(name) || !validEnvironmentName(name) {
			unsafe = true
			redactionsComplete = false
			continue
		}
		providerNames[strings.ToUpper(name)] = struct{}{}
	}
	allowNames := make(environmentNameSet, len(options.AllowNames))
	for _, name := range options.AllowNames {
		if !utf8.ValidString(name) || !validEnvironmentName(name) || allowNames.contains(name) {
			unsafe = true
			continue
		}
		allowNames[strings.Clone(name)] = struct{}{}
	}

	host := make(map[string]string, len(options.HostEntries))
	for _, entry := range options.HostEntries {
		if !utf8.ValidString(entry) || strings.IndexByte(entry, 0) >= 0 {
			unsafe = true
			redactionsComplete = false
			continue
		}
		name, value, found := strings.Cut(entry, "=")
		if !found || !validEnvironmentName(name) {
			unsafe = true
			redactionsComplete = false
			continue
		}

		proxyValues, proxyErr := proxyUserinfoRedactions(name, value)
		if proxyErr != nil {
			unsafe = true
			redactionsComplete = false
		}
		for _, proxyValue := range proxyValues {
			if !collector.Add(proxyValue) {
				unsafe = true
				redactionsComplete = false
				break
			}
		}
		if classifyEnvironmentName(name, providerNames) != environmentOrdinary && !collector.Add(value) {
			unsafe = true
			redactionsComplete = false
		}
		host[strings.Clone(name)] = strings.Clone(value)
	}

	var directories *PrivateDirectories
	if options.PrivateDirectories != nil {
		copy := *options.PrivateDirectories
		directories = &copy
		for _, privatePath := range []string{copy.Root, copy.Home, copy.Temp, copy.Cache} {
			if !addPrivateEnvironmentRedaction(collector, privatePath) {
				unsafe = true
				redactionsComplete = false
			}
		}
	}
	if unsafe {
		return failedEnvironmentSnapshot(collector.Values(), redactionsComplete), ErrEnvironmentUnsafe
	}

	names := make([]string, 0, len(host))
	for name := range host {
		names = append(names, name)
	}
	slices.Sort(names)
	resolved := make(map[string]string, len(host))
	for _, name := range names {
		value := host[name]
		classification := classifyEnvironmentName(name, providerNames)
		if classification == environmentOrdinary ||
			(classification == environmentAutomatic && allowNames.contains(name)) {
			resolved[strings.Clone(name)] = strings.Clone(value)
		}
	}

	if directories != nil {
		if err := preparePrivateEnvironment(*directories); err != nil {
			return failedEnvironmentSnapshot(collector.Values(), redactionsComplete), ErrEnvironmentUnsafe
		}
		privateValues := map[string]string{
			"HOME":             directories.Home,
			"TMPDIR":           directories.Temp,
			"TMP":              directories.Temp,
			"TEMP":             directories.Temp,
			"XDG_CACHE_HOME":   filepath.Join(directories.Cache, "xdg"),
			"GOCACHE":          filepath.Join(directories.Cache, "go-build"),
			"GOMODCACHE":       filepath.Join(directories.Cache, "go-mod"),
			"NPM_CONFIG_CACHE": filepath.Join(directories.Cache, "npm"),
			"PIP_CACHE_DIR":    filepath.Join(directories.Cache, "pip"),
			"UV_CACHE_DIR":     filepath.Join(directories.Cache, "uv"),
		}
		for name, value := range privateValues {
			resolved[name] = strings.Clone(value)
		}
	}

	return newEnvironmentSnapshot(resolved, collector.Values()), nil
}

func (s EnvironmentSnapshot) Entries() []string {
	if s.entries == nil {
		return nil
	}
	return append([]string{}, s.entries...)
}

func (s EnvironmentSnapshot) RedactionValues() []string {
	return append([]string{}, s.redactions...)
}

func (s EnvironmentSnapshot) RedactionsComplete() bool {
	return s.redactionsComplete
}

type environmentNameSet map[string]struct{}

func (s environmentNameSet) contains(name string) bool {
	_, ok := s[name]
	return ok
}

type environmentClassification uint8

const (
	environmentOrdinary environmentClassification = iota
	environmentAutomatic
	environmentNonRestorable
)

func classifyEnvironmentName(name string, providerNames environmentNameSet) environmentClassification {
	upperName := strings.ToUpper(name)
	if upperName == "OTTO_API_KEY" || providerNames.contains(upperName) ||
		strings.HasPrefix(upperName, "DYLD_") || strings.HasPrefix(upperName, "LD_") ||
		isNonRestorableEnvironmentName(upperName) {
		return environmentNonRestorable
	}
	if hasSensitiveEnvironmentSuffix(upperName) || isFixedSensitiveEnvironmentName(upperName) {
		return environmentAutomatic
	}
	return environmentOrdinary
}

func isNonRestorableEnvironmentName(upperName string) bool {
	if upperName == "OTTO_SANDBOX" || strings.HasPrefix(upperName, "OTTO_SANDBOX_") {
		return true
	}
	switch upperName {
	case "BASH_ENV", "ENV", "ZDOTDIR", "PROMPT_COMMAND", "CDPATH", "SHELLOPTS", "BASHOPTS",
		"SSH_AUTH_SOCK", "DOCKER_HOST", "CONTAINER_HOST":
		return true
	default:
		return false
	}
}

func hasSensitiveEnvironmentSuffix(upperName string) bool {
	for _, suffix := range [...]string{
		"_TOKEN",
		"_SECRET",
		"_PASSWORD",
		"_PASSWD",
		"_API_KEY",
		"_ACCESS_KEY",
		"_PRIVATE_KEY",
		"_CREDENTIAL",
		"_CREDENTIALS",
	} {
		if strings.HasSuffix(upperName, suffix) {
			return true
		}
	}
	return false
}

func isFixedSensitiveEnvironmentName(upperName string) bool {
	switch upperName {
	case "TOKEN", "SECRET", "PASSWORD", "PASSWD", "API_KEY", "ACCESS_KEY", "PRIVATE_KEY", "CREDENTIAL", "CREDENTIALS",
		"AWS_ACCESS_KEY_ID", "AWS_SECRET_KEY", "AWS_SHARED_CREDENTIALS_FILE", "AWS_WEB_IDENTITY_TOKEN_FILE",
		"AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "AWS_CONTAINER_CREDENTIALS_FULL_URI", "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
		"AZURE_CLIENT_ID", "AZURE_TENANT_ID", "AZURE_CLIENT_CERTIFICATE_PATH", "AZURE_FEDERATED_TOKEN_FILE",
		"ARM_CLIENT_ID", "ARM_TENANT_ID", "ARM_CLIENT_CERTIFICATE_PATH", "ARM_OIDC_REQUEST_TOKEN", "ARM_OIDC_REQUEST_URL",
		"MSI_ENDPOINT", "IDENTITY_ENDPOINT", "IDENTITY_HEADER", "IMDS_ENDPOINT",
		"GOOGLE_APPLICATION_CREDENTIALS", "GOOGLE_APPLICATION_CREDENTIALS_JSON", "GOOGLE_CREDENTIALS",
		"GCP_CREDENTIALS", "GCP_SERVICE_ACCOUNT", "GCP_SERVICE_ACCOUNT_KEY", "GCLOUD_SERVICE_KEY", "CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE",
		"GITHUB_PAT", "GITLAB_CI_JOB_JWT", "GITLAB_CI_JOB_JWT_V2",
		"NPM_CONFIG__AUTH", "NPM_CONFIG__AUTHTOKEN", "NPM_CONFIG_AUTH", "NPM_CONFIG_AUTHTOKEN",
		"REGISTRY_AUTH", "REGISTRY_AUTH_FILE", "CONTAINER_AUTH_FILE",
		"DOCKER_AUTH_CONFIG", "DOCKER_CONFIG", "DOCKER_CERT_PATH", "DOCKER_HOST", "CONTAINER_HOST",
		"CI_JOB_JWT", "CI_JOB_JWT_V2", "CI_DEPLOY_USER", "SYSTEM_ACCESSTOKEN",
		"ACTIONS_ID_TOKEN_REQUEST_URL", "BUILDKITE_AGENT_META_DATA_AWS_ROLE_ARN",
		"AUTH", "AUTHORIZATION", "HTTP_AUTHORIZATION", "PROXY_AUTHORIZATION",
		"COOKIE", "COOKIES", "HTTP_COOKIE", "SET_COOKIE", "SSH_AUTH_SOCK":
		return true
	default:
		return false
	}
}

func addPrivateEnvironmentRedaction(collector *safetext.SecretCollector, value string) bool {
	if value == "" {
		return true
	}
	if !utf8.ValidString(value) || strings.IndexByte(value, 0) >= 0 {
		return false
	}
	return collector.Add(value)
}

func proxyUserinfoRedactions(name, value string) ([]string, error) {
	if value == "" || !isProxyEnvironmentName(name) || strings.EqualFold(name, "NO_PROXY") {
		return nil, nil
	}
	values, ambiguous := urlprivacy.UserinfoForms(value)
	if ambiguous {
		return values, ErrEnvironmentUnsafe
	}
	return values, nil
}

func isProxyEnvironmentName(name string) bool {
	upperName := strings.ToUpper(name)
	return upperName == "PROXY" || strings.HasSuffix(upperName, "_PROXY")
}

func preparePrivateEnvironment(directories PrivateDirectories) error {
	basePaths := []string{directories.Root, directories.Home, directories.Temp, directories.Cache}
	for _, path := range basePaths {
		if !validPrivateDirectoryPath(path) || verifyPrivateDirectory(path) != nil {
			return ErrEnvironmentUnsafe
		}
	}
	for _, name := range []string{"go-build", "go-mod", "npm", "pip", "uv", "xdg"} {
		if err := ensurePrivateDirectory(filepath.Join(directories.Cache, name)); err != nil {
			return ErrEnvironmentUnsafe
		}
	}
	return nil
}

func validPrivateDirectoryPath(path string) bool {
	return path != "" && utf8.ValidString(path) && strings.IndexByte(path, 0) < 0 &&
		filepath.IsAbs(path) && filepath.Clean(path) == path
}

func ensurePrivateDirectory(path string) error {
	err := os.Mkdir(path, 0o700)
	if err != nil {
		if !os.IsExist(err) {
			return ErrEnvironmentUnsafe
		}
		return verifyPrivateDirectory(path)
	}

	entryInfo, err := os.Lstat(path)
	if err != nil || entryInfo.Mode()&os.ModeSymlink != 0 || !entryInfo.IsDir() {
		return ErrEnvironmentUnsafe
	}
	file, err := os.Open(path)
	if err != nil {
		return ErrEnvironmentUnsafe
	}
	defer file.Close()
	openedInfo, err := file.Stat()
	if err != nil || !os.SameFile(entryInfo, openedInfo) {
		return ErrEnvironmentUnsafe
	}
	if err := file.Chmod(0o700); err != nil {
		return ErrEnvironmentUnsafe
	}
	openedInfo, err = file.Stat()
	if err != nil || !securePrivateDirectoryInfo(openedInfo) {
		return ErrEnvironmentUnsafe
	}
	finalInfo, err := os.Lstat(path)
	if err != nil || !os.SameFile(openedInfo, finalInfo) || finalInfo.Mode()&os.ModeSymlink != 0 {
		return ErrEnvironmentUnsafe
	}
	return nil
}

func verifyPrivateDirectory(path string) error {
	entryInfo, err := os.Lstat(path)
	if err != nil || entryInfo.Mode()&os.ModeSymlink != 0 || !securePrivateDirectoryInfo(entryInfo) {
		return ErrEnvironmentUnsafe
	}
	file, err := os.Open(path)
	if err != nil {
		return ErrEnvironmentUnsafe
	}
	defer file.Close()
	openedInfo, err := file.Stat()
	if err != nil || !os.SameFile(entryInfo, openedInfo) || !securePrivateDirectoryInfo(openedInfo) {
		return ErrEnvironmentUnsafe
	}
	return nil
}

func securePrivateDirectoryInfo(info os.FileInfo) bool {
	if !info.IsDir() || info.Mode().Perm() != 0o700 ||
		info.Mode()&(os.ModeSetuid|os.ModeSetgid|os.ModeSticky) != 0 {
		return false
	}
	stat, ok := info.Sys().(*syscall.Stat_t)
	return ok && int(stat.Uid) == os.Geteuid()
}

func newEnvironmentSnapshot(environment map[string]string, redactions []string) EnvironmentSnapshot {
	names := make([]string, 0, len(environment))
	for name := range environment {
		names = append(names, name)
	}
	slices.Sort(names)
	entries := make([]string, 0, len(names))
	for _, name := range names {
		entries = append(entries, name+"="+environment[name])
	}

	return EnvironmentSnapshot{entries: entries, redactions: append([]string(nil), redactions...), redactionsComplete: true}
}

func failedEnvironmentSnapshot(redactions []string, complete bool) EnvironmentSnapshot {
	return EnvironmentSnapshot{redactions: append([]string(nil), redactions...), redactionsComplete: complete}
}
