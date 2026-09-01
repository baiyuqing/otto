package sandbox

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"
	"unicode/utf8"
)

func TestParseEnvironmentIsDeterministicAndLastDuplicateWins(t *testing.T) {
	entries := []string{
		"ZETA=first",
		"ALPHA=alpha",
		"ZETA=last",
		"EMPTY=",
	}
	first, err := ParseEnvironment(entries)
	if err != nil {
		t.Fatal(err)
	}
	second, err := ParseEnvironment(entries)
	if err != nil {
		t.Fatal(err)
	}
	want := map[string]string{"ALPHA": "alpha", "EMPTY": "", "ZETA": "last"}
	if !reflect.DeepEqual(first, want) || !reflect.DeepEqual(second, want) {
		t.Fatal("ParseEnvironment() was nondeterministic or did not retain the last duplicate")
	}

	entries[2] = "ZETA=mutated"
	if first["ZETA"] != "last" {
		t.Fatal("ParseEnvironment() retained caller slice storage")
	}
}

func TestParseEnvironmentRejectsMalformedEntries(t *testing.T) {
	invalidValueUTF8 := string([]byte{'V', 'A', 'L', 'U', 'E', '=', 0xff})
	invalidNameUTF8 := string([]byte{'N', 0xff, '=', 'x'})
	tests := []struct {
		name    string
		entries []string
	}{
		{name: "missing equals", entries: []string{"MALFORMED"}},
		{name: "empty name", entries: []string{"=value"}},
		{name: "leading digit", entries: []string{"1INVALID=value"}},
		{name: "punctuation", entries: []string{"INVALID-NAME=value"}},
		{name: "NUL in name", entries: []string{"INVALID\x00NAME=value"}},
		{name: "NUL in value", entries: []string{"VALID=unsafe\x00value"}},
		{name: "invalid UTF-8 name", entries: []string{invalidNameUTF8}},
		{name: "invalid UTF-8 value", entries: []string{invalidValueUTF8}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			parsed, err := ParseEnvironment(test.entries)
			if parsed != nil || !errors.Is(err, ErrEnvironmentUnsafe) {
				t.Fatalf("ParseEnvironment() returned an unsafe result: map-nil=%t error=%v", parsed == nil, err)
			}
			if err.Error() != ErrEnvironmentUnsafe.Error() {
				t.Fatalf("ParseEnvironment() returned an unbounded error: %v", err)
			}
		})
	}
}

func TestResolveEnvironmentFailureRetainsBoundedRedactionsFromEveryValidEntry(t *testing.T) {
	directories := newPrivateDirectories(t)
	snapshot, err := ResolveEnvironment(EnvironmentOptions{
		HostEntries: []string{
			"AWS_SECRET_ACCESS_KEY=first-aws-secret",
			"HTTPS_PROXY=http://raw%20user:raw%2Fpass@[::1]:8443/path",
			"BROKEN",
			"AWS_SECRET_ACCESS_KEY=second-aws-secret",
		},
		PrivateDirectories: &directories,
	})
	if !errors.Is(err, ErrEnvironmentUnsafe) || err.Error() != ErrEnvironmentUnsafe.Error() {
		t.Fatalf("ResolveEnvironment() error = %v, want fixed ErrEnvironmentUnsafe", err)
	}
	if snapshot.Entries() != nil {
		t.Fatalf("failed resolution returned command entries: %#v", snapshot.Entries())
	}
	assertRedactionsContain(t, snapshot.RedactionValues(), []string{
		"first-aws-secret",
		"second-aws-secret",
		"raw%20user:raw%2Fpass",
		"raw%20user",
		"raw%2Fpass",
		"raw user:raw/pass",
		"raw user",
		"raw/pass",
		directories.Root,
		directories.Home,
		directories.Temp,
		directories.Cache,
	})
	assertRedactionsSorted(t, snapshot.RedactionValues())
}

func TestResolveEnvironmentRemovesProviderCredentials(t *testing.T) {
	host := []string{
		"OTTO_API_KEY=otto-sensitive-value",
		"otto_api_key=lower-otto-sensitive-value",
		"selected_key=selected-sensitive-value",
		"CONFIGURED_KEY=configured-sensitive-value",
		"ORDINARY=preserved",
	}
	options := EnvironmentOptions{
		HostEntries:   host,
		ProviderNames: []string{"SELECTED_KEY", "CONFIGURED_KEY"},
		AllowNames:    []string{"OTTO_API_KEY", "otto_api_key", "selected_key", "CONFIGURED_KEY"},
	}
	snapshot, err := ResolveEnvironment(options)
	if err != nil {
		t.Fatal(err)
	}
	assertEnvironmentNames(t, snapshot, []string{"ORDINARY"})
	assertRedactionsContain(t, snapshot.RedactionValues(), []string{
		"otto-sensitive-value",
		"lower-otto-sensitive-value",
		"selected-sensitive-value",
		"configured-sensitive-value",
	})
}

func TestResolveEnvironmentNeverRestoresLoaderOrShellInjection(t *testing.T) {
	names := []string{
		"DYLD_INSERT_LIBRARIES",
		"dyld_custom",
		"LD_PRELOAD",
		"ld_library_path",
		"BASH_ENV",
		"env",
		"ZDOTDIR",
		"PROMPT_COMMAND",
		"CDPATH",
		"SHELLOPTS",
		"BASHOPTS",
	}
	host := make([]string, 0, len(names)+1)
	for index, name := range names {
		host = append(host, fmt.Sprintf("%s=classified-value-%02d", name, index))
	}
	host = append(host, "AUTHORS=preserved")

	snapshot, err := ResolveEnvironment(EnvironmentOptions{HostEntries: host, AllowNames: slices.Clone(names)})
	if err != nil {
		t.Fatal(err)
	}
	assertEnvironmentNames(t, snapshot, []string{"AUTHORS"})
	if got := len(snapshot.RedactionValues()); got != len(names) {
		t.Fatalf("redaction count = %d, want %d", got, len(names))
	}
}

func TestResolveEnvironmentNeverRestoresInternalOrControlVariables(t *testing.T) {
	names := []string{
		"OTTO_SANDBOX",
		"otto_sandbox_profile_path",
		"SSH_AUTH_SOCK",
		"docker_host",
		"CONTAINER_HOST",
	}
	host := make([]string, 0, len(names)+1)
	originalValues := make([]string, 0, len(names))
	for index, name := range names {
		value := fmt.Sprintf("non-restorable-value-%02d", index)
		host = append(host, name+"="+value)
		originalValues = append(originalValues, value)
	}
	host = append(host, "SANDBOX_PROFILE=preserved")

	snapshot, err := ResolveEnvironment(EnvironmentOptions{
		HostEntries: host,
		AllowNames:  slices.Clone(names),
	})
	if err != nil {
		t.Fatal(err)
	}
	assertEnvironmentNames(t, snapshot, []string{"SANDBOX_PROFILE"})
	assertRedactionsContain(t, snapshot.RedactionValues(), originalValues)
}

func TestResolveEnvironmentClassifiesSensitiveSuffixesCaseInsensitively(t *testing.T) {
	names := []string{
		"BUILD_TOKEN",
		"build_secret",
		"DATABASE_PASSWORD",
		"database_passwd",
		"SERVICE_API_KEY",
		"service_access_key",
		"signing_private_key",
		"APP_CREDENTIAL",
		"app_credentials",
	}
	host := make([]string, 0, len(names)+2)
	for index, name := range names {
		host = append(host, fmt.Sprintf("%s=suffix-sensitive-%02d", name, index))
	}
	host = append(host, "AUTHORS=preserved", "PASSWORD_POLICY=preserved")

	snapshot, err := ResolveEnvironment(EnvironmentOptions{HostEntries: host})
	if err != nil {
		t.Fatal(err)
	}
	assertEnvironmentNames(t, snapshot, []string{"AUTHORS", "PASSWORD_POLICY"})
	if got := len(snapshot.RedactionValues()); got != len(names) {
		t.Fatalf("redaction count = %d, want %d", got, len(names))
	}
}

func TestResolveEnvironmentClassifiesFixedCredentialNames(t *testing.T) {
	names := []string{
		"AWS_ACCESS_KEY_ID",
		"azure_client_id",
		"GCP_SERVICE_ACCOUNT_KEY",
		"github_pat",
		"GITLAB_CI_JOB_JWT",
		"NPM_CONFIG__AUTH",
		"REGISTRY_AUTH_FILE",
		"DOCKER_AUTH_CONFIG",
		"CI_JOB_JWT",
		"AUTHORIZATION",
		"http_cookie",
		"SSH_AUTH_SOCK",
		"DOCKER_HOST",
		"CONTAINER_HOST",
	}
	host := make([]string, 0, len(names)+1)
	for index, name := range names {
		host = append(host, fmt.Sprintf("%s=fixed-sensitive-%02d", name, index))
	}
	host = append(host, "AUTHORS=preserved")

	snapshot, err := ResolveEnvironment(EnvironmentOptions{HostEntries: host})
	if err != nil {
		t.Fatal(err)
	}
	assertEnvironmentNames(t, snapshot, []string{"AUTHORS"})
	if got := len(snapshot.RedactionValues()); got != len(names) {
		t.Fatalf("redaction count = %d, want %d", got, len(names))
	}
}

func TestResolveEnvironmentExactAllowRestoresOnlyAutomaticNonProviderNames(t *testing.T) {
	host := []string{
		"PROJECT_TOKEN=restored-sensitive-value",
		"project_token=case-sensitive-not-restored",
		"NPM_CONFIG__AUTH=restored-fixed-value",
		"PROVIDER_TOKEN=provider-sensitive-value",
		"BASH_ENV=shell-sensitive-value",
		"ORDINARY=preserved",
	}
	snapshot, err := ResolveEnvironment(EnvironmentOptions{
		HostEntries:   host,
		ProviderNames: []string{"PROVIDER_TOKEN"},
		AllowNames:    []string{"PROJECT_TOKEN", "NPM_CONFIG__AUTH", "PROVIDER_TOKEN", "BASH_ENV"},
	})
	if err != nil {
		t.Fatal(err)
	}
	assertEnvironmentNames(t, snapshot, []string{"NPM_CONFIG__AUTH", "ORDINARY", "PROJECT_TOKEN"})
	assertRedactionsContain(t, snapshot.RedactionValues(), []string{
		"restored-sensitive-value",
		"case-sensitive-not-restored",
		"restored-fixed-value",
		"provider-sensitive-value",
		"shell-sensitive-value",
	})
}

func TestResolveEnvironmentCollectsProxyUserinfoWithoutRewritingProxy(t *testing.T) {
	proxy := "http://raw%20user:raw%2Fpass@[2001:db8::1]:8443/path"
	secondProxy := "socks5://solo%20user@[::1]:9000"
	snapshot, err := ResolveEnvironment(EnvironmentOptions{HostEntries: []string{
		"HTTPS_PROXY=" + proxy,
		"all_proxy=" + secondProxy,
	}})
	if err != nil {
		t.Fatal(err)
	}
	environment := snapshotEnvironment(t, snapshot)
	if environment["HTTPS_PROXY"] != proxy || environment["all_proxy"] != secondProxy {
		t.Fatal("ResolveEnvironment() rewrote a proxy URI")
	}
	if !strings.Contains(environment["HTTPS_PROXY"], "@[2001:db8::1]") ||
		!strings.Contains(environment["all_proxy"], "@[::1]") {
		t.Fatal("ResolveEnvironment() did not preserve balanced IPv6 brackets")
	}
	assertRedactionsContain(t, snapshot.RedactionValues(), []string{
		"raw%20user:raw%2Fpass",
		"raw user:raw/pass",
		"raw user",
		"raw/pass",
		"solo%20user",
		"solo user",
	})
	assertRedactionsSorted(t, snapshot.RedactionValues())
}

func TestResolveEnvironmentConservativelyExtractsMalformedProxyAuthorities(t *testing.T) {
	tests := []struct {
		name  string
		proxy string
		want  []string
	}{
		{
			name:  "ambiguous extra slash",
			proxy: "http:///proxy-user:proxy-pass@example.test",
			want:  []string{"proxy-user:proxy-pass", "proxy-user", "proxy-pass"},
		},
		{
			name:  "backslash authority",
			proxy: `http:\\proxy%20user:proxy%2Fpass@example.test\path`,
			want:  []string{"proxy%20user:proxy%2Fpass", "proxy%20user", "proxy%2Fpass", "proxy user:proxy/pass", "proxy user", "proxy/pass"},
		},
		{
			name:  "missing scheme",
			proxy: "proxy%20user:proxy%2Fpass@example.test/path",
			want:  []string{"proxy%20user:proxy%2Fpass", "proxy%20user", "proxy%2Fpass", "proxy user:proxy/pass", "proxy user", "proxy/pass"},
		},
		{
			name:  "odd scheme",
			proxy: "ht!tp://odd%20user:odd%2Fpass@example.test/path",
			want:  []string{"odd%20user:odd%2Fpass", "odd%20user", "odd%2Fpass", "odd user:odd/pass", "odd user", "odd/pass"},
		},
		{
			name:  "opaque odd scheme retains both interpretations",
			proxy: "http:odd%20user:odd%2Fpass@example.test/path",
			want:  []string{"http:odd%20user:odd%2Fpass", "odd%20user:odd%2Fpass", "odd%20user", "odd%2Fpass", "odd user", "odd/pass"},
		},
		{
			name:  "host parse failure scans later authority-like at",
			proxy: "http://[::1/path/proxy%20user:proxy%2Fpass@example.test",
			want:  []string{"proxy%20user:proxy%2Fpass", "proxy%20user", "proxy%2Fpass", "proxy user", "proxy/pass"},
		},
		{
			name:  "malformed username independently decodes password",
			proxy: "http://bad%zz:pass%2Fword@[::1]:8080/path?next=@ignored#@ignored",
			want:  []string{"bad%zz:pass%2Fword", "bad%zz", "pass%2Fword", "pass/word"},
		},
		{
			name:  "malformed password independently decodes username",
			proxy: "http://user%20name:bad%zz@[2001:db8::1]:8080/path",
			want:  []string{"user%20name:bad%zz", "user%20name", "bad%zz", "user name"},
		},
		{
			name:  "malformed normal authority survives later path at",
			proxy: "http://bad%zz:pass%2Fword@[::1]:8080/path/user:other@example.test",
			want:  []string{"bad%zz:pass%2Fword", "bad%zz", "pass%2Fword", "pass/word"},
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			snapshot, err := ResolveEnvironment(EnvironmentOptions{HostEntries: []string{"HTTPS_PROXY=" + test.proxy}})
			if !errors.Is(err, ErrEnvironmentUnsafe) || err.Error() != ErrEnvironmentUnsafe.Error() {
				t.Fatalf("ResolveEnvironment() error = %v, want fixed ErrEnvironmentUnsafe", err)
			}
			if snapshot.Entries() != nil {
				t.Fatalf("malformed proxy reached command environment: %#v", snapshot.Entries())
			}
			assertRedactionsContain(t, snapshot.RedactionValues(), test.want)
			for _, value := range snapshot.RedactionValues() {
				if !utf8.ValidString(value) {
					t.Fatalf("redaction retained invalid UTF-8: %q", value)
				}
			}
		})
	}
}

func TestResolveEnvironmentCanonicalizesPercentDecodedInvalidUTF8ProxyUserinfo(t *testing.T) {
	proxy := "http://user%FF:pass%C0%AF@[::1]:8080/path"
	snapshot, err := ResolveEnvironment(EnvironmentOptions{HostEntries: []string{"HTTPS_PROXY=" + proxy}})
	if err != nil {
		t.Fatalf("ResolveEnvironment() rejected syntactically valid encoded proxy: %v", err)
	}
	if got := snapshotEnvironment(t, snapshot)["HTTPS_PROXY"]; got != proxy {
		t.Fatalf("proxy = %q, want unchanged raw form %q", got, proxy)
	}
	assertRedactionsContain(t, snapshot.RedactionValues(), []string{
		"user%FF:pass%C0%AF", "user%FF", "pass%C0%AF", "user�:pass��", "user�", "pass��",
	})
	for _, value := range snapshot.RedactionValues() {
		if !utf8.ValidString(value) {
			t.Fatalf("redaction retained invalid UTF-8: %q", value)
		}
	}
}

func TestResolveEnvironmentDoesNotTreatValidProxyPathQueryOrFragmentAtAsUserinfo(t *testing.T) {
	proxy := "http://[2001:db8::1]:8080/path/user:pass@example.test?next=user:pass@example.test#user:pass@example.test"
	snapshot, err := ResolveEnvironment(EnvironmentOptions{HostEntries: []string{"HTTPS_PROXY=" + proxy}})
	if err != nil {
		t.Fatalf("ResolveEnvironment() rejected valid balanced-IPv6 proxy: %v", err)
	}
	if got := snapshotEnvironment(t, snapshot)["HTTPS_PROXY"]; got != proxy {
		t.Fatalf("proxy = %q, want unchanged %q", got, proxy)
	}
	if len(snapshot.RedactionValues()) != 0 {
		t.Fatalf("path/query/fragment @ was misclassified as userinfo: %#v", snapshot.RedactionValues())
	}
}

func TestResolveEnvironmentRejectsMalformedProxyWithoutInventingPathUserinfo(t *testing.T) {
	for _, proxy := range []string{
		"http://[2001:db8::1]:8080/path/user:pass@example.test?broken=%zz",
		"http://example.test/path%zz",
		"http:///example.test/path",
		`http:\\example.test\path`,
		"example.test:8080",
	} {
		t.Run(proxy, func(t *testing.T) {
			snapshot, err := ResolveEnvironment(EnvironmentOptions{HostEntries: []string{"HTTPS_PROXY=" + proxy}})
			if !errors.Is(err, ErrEnvironmentUnsafe) || err.Error() != ErrEnvironmentUnsafe.Error() {
				t.Fatalf("ResolveEnvironment() error = %v, want fixed ErrEnvironmentUnsafe", err)
			}
			if snapshot.Entries() != nil || !snapshot.RedactionsComplete() {
				t.Fatalf("rejected snapshot = entries %#v complete %t", snapshot.Entries(), snapshot.RedactionsComplete())
			}
			if len(snapshot.RedactionValues()) != 0 {
				t.Fatalf("malformed proxy invented path userinfo: %#v", snapshot.RedactionValues())
			}
		})
	}
}

func TestResolveEnvironmentRewritesPrivatePathsAndCreatesCaches(t *testing.T) {
	directories := newPrivateDirectories(t)
	host := []string{
		"HOME=/host/home",
		"TMPDIR=/host/tmpdir",
		"TMP=/host/tmp",
		"TEMP=/host/temp",
		"XDG_CACHE_HOME=/host/xdg",
		"GOCACHE=/host/go-build",
		"GOMODCACHE=/host/go-mod",
		"NPM_CONFIG_CACHE=/host/npm",
		"PIP_CACHE_DIR=/host/pip",
		"UV_CACHE_DIR=/host/uv",
		"home=preserved-lowercase",
		"HOME_SUFFIX=preserved-suffix",
	}
	snapshot, err := ResolveEnvironment(EnvironmentOptions{
		HostEntries:        host,
		PrivateDirectories: &directories,
	})
	if err != nil {
		t.Fatal(err)
	}
	environment := snapshotEnvironment(t, snapshot)
	want := map[string]string{
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
		"home":             "preserved-lowercase",
		"HOME_SUFFIX":      "preserved-suffix",
	}
	if !reflect.DeepEqual(environment, want) {
		t.Fatal("ResolveEnvironment() did not apply only the exact private variable mapping")
	}

	for _, name := range []string{"xdg", "go-build", "go-mod", "npm", "pip", "uv"} {
		info, statErr := os.Lstat(filepath.Join(directories.Cache, name))
		if statErr != nil || !info.IsDir() || info.Mode()&os.ModeSymlink != 0 || info.Mode().Perm() != 0o700 {
			t.Fatalf("derived cache %s is not an owner-only non-symlink directory", name)
		}
	}
	assertRedactionsContain(t, snapshot.RedactionValues(), []string{
		directories.Root,
		directories.Home,
		directories.Temp,
		directories.Cache,
	})
	assertRedactionsSorted(t, snapshot.RedactionValues())

	directories.Root = "/mutated/root"
	directories.Home = "/mutated/home"
	if reflect.DeepEqual(snapshot.Entries(), []string{}) ||
		slices.Contains(snapshot.RedactionValues(), directories.Root) ||
		slices.Contains(snapshot.RedactionValues(), directories.Home) {
		t.Fatal("EnvironmentSnapshot retained the PrivateDirectories pointer")
	}
}

func TestResolveEnvironmentDirectModePreservesHostPaths(t *testing.T) {
	host := []string{
		"HOME=/host/home",
		"TMPDIR=/host/tmpdir",
		"TMP=/host/tmp",
		"TEMP=/host/temp",
		"XDG_CACHE_HOME=/host/xdg",
		"GOCACHE=/host/go-build",
		"GOMODCACHE=/host/go-mod",
		"NPM_CONFIG_CACHE=/host/npm",
		"PIP_CACHE_DIR=/host/pip",
		"UV_CACHE_DIR=/host/uv",
	}
	snapshot, err := ResolveEnvironment(EnvironmentOptions{HostEntries: host})
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(snapshotEnvironment(t, snapshot), environmentEntriesMap(t, host)) {
		t.Fatal("direct environment did not preserve host private/cache paths")
	}
	if len(snapshot.RedactionValues()) != 0 {
		t.Fatal("direct environment unexpectedly redacted ordinary host paths")
	}
}

func TestResolveEnvironmentRejectsUnsafePrivateDirectories(t *testing.T) {
	t.Run("cache symlink", func(t *testing.T) {
		directories := newPrivateDirectories(t)
		if err := os.Remove(directories.Cache); err != nil {
			t.Fatal(err)
		}
		if err := os.Symlink(t.TempDir(), directories.Cache); err != nil {
			t.Fatal(err)
		}
		assertUnsafeEnvironmentResolution(t, EnvironmentOptions{PrivateDirectories: &directories})
	})

	t.Run("derived symlink", func(t *testing.T) {
		directories := newPrivateDirectories(t)
		if err := os.Symlink(t.TempDir(), filepath.Join(directories.Cache, "xdg")); err != nil {
			t.Fatal(err)
		}
		assertUnsafeEnvironmentResolution(t, EnvironmentOptions{PrivateDirectories: &directories})
	})

	t.Run("unsafe base permissions", func(t *testing.T) {
		directories := newPrivateDirectories(t)
		if err := os.Chmod(directories.Cache, 0o750); err != nil {
			t.Fatal(err)
		}
		assertUnsafeEnvironmentResolution(t, EnvironmentOptions{PrivateDirectories: &directories})
	})

	t.Run("unsafe derived permissions", func(t *testing.T) {
		directories := newPrivateDirectories(t)
		path := filepath.Join(directories.Cache, "xdg")
		if err := os.Mkdir(path, 0o755); err != nil {
			t.Fatal(err)
		}
		assertUnsafeEnvironmentResolution(t, EnvironmentOptions{PrivateDirectories: &directories})
	})
}

func TestResolveEnvironmentEnforcesSensitiveValueCountBound(t *testing.T) {
	exactlyAtLimit := sensitiveEnvironmentEntries(512)
	snapshot, err := ResolveEnvironment(EnvironmentOptions{HostEntries: exactlyAtLimit})
	if err != nil {
		t.Fatalf("ResolveEnvironment() rejected exactly 512 sensitive values: %v", err)
	}
	if got := len(snapshot.RedactionValues()); got != 512 {
		t.Fatalf("redaction count = %d, want 512", got)
	}

	_, err = ResolveEnvironment(EnvironmentOptions{HostEntries: sensitiveEnvironmentEntries(513)})
	if !errors.Is(err, ErrEnvironmentUnsafe) || err.Error() != ErrEnvironmentUnsafe.Error() {
		t.Fatalf("ResolveEnvironment() error = %v, want fixed ErrEnvironmentUnsafe", err)
	}
}

func TestResolveEnvironmentMarksIncompleteRedactionSnapshots(t *testing.T) {
	complete, err := ResolveEnvironment(EnvironmentOptions{HostEntries: sensitiveEnvironmentEntries(512)})
	if err != nil || !complete.RedactionsComplete() {
		t.Fatalf("exact-limit snapshot = complete %t error %v, want complete success", complete.RedactionsComplete(), err)
	}

	overflow, err := ResolveEnvironment(EnvironmentOptions{HostEntries: sensitiveEnvironmentEntries(513)})
	if !errors.Is(err, ErrEnvironmentUnsafe) || overflow.RedactionsComplete() {
		t.Fatalf("513-value snapshot = complete %t error %v, want incomplete failure", overflow.RedactionsComplete(), err)
	}
	if slices.Contains(overflow.RedactionValues(), "unique-sensitive-value-512") {
		t.Fatal("overflow snapshot unexpectedly retained the omitted 513th value")
	}

	overBudget, err := ResolveEnvironment(EnvironmentOptions{HostEntries: []string{"LARGE_TOKEN=" + strings.Repeat("z", (1<<20)+1)}})
	if !errors.Is(err, ErrEnvironmentUnsafe) || overBudget.RedactionsComplete() {
		t.Fatalf("over-budget snapshot = complete %t error %v, want incomplete failure", overBudget.RedactionsComplete(), err)
	}

	malformedEntry, err := ResolveEnvironment(EnvironmentOptions{HostEntries: []string{"BROKEN"}})
	if !errors.Is(err, ErrEnvironmentUnsafe) || malformedEntry.RedactionsComplete() {
		t.Fatalf("malformed-entry snapshot = complete %t error %v, want incomplete failure", malformedEntry.RedactionsComplete(), err)
	}

	malformedProxy, err := ResolveEnvironment(EnvironmentOptions{HostEntries: []string{"HTTPS_PROXY=http:///user:pass@example.test"}})
	if !errors.Is(err, ErrEnvironmentUnsafe) || !malformedProxy.RedactionsComplete() {
		t.Fatalf("fully extracted malformed proxy = complete %t error %v, want complete redactions with rejected environment", malformedProxy.RedactionsComplete(), err)
	}
}

func TestResolveEnvironmentEnforcesSensitiveByteBoundBeforePrivatePaths(t *testing.T) {
	exactlyAtLimit := strings.Repeat("x", 1<<20)
	snapshot, err := ResolveEnvironment(EnvironmentOptions{HostEntries: []string{"LARGE_TOKEN=" + exactlyAtLimit}})
	if err != nil {
		t.Fatalf("ResolveEnvironment() rejected exactly 1 MiB of sensitive bytes: %v", err)
	}
	if got := snapshot.RedactionValues(); len(got) != 1 || len(got[0]) != 1<<20 {
		t.Fatal("ResolveEnvironment() did not retain the boundary-size redaction")
	}

	directories := newPrivateDirectories(t)
	_, err = ResolveEnvironment(EnvironmentOptions{
		HostEntries:        []string{"LARGE_TOKEN=" + strings.Repeat("y", 1<<20+1)},
		PrivateDirectories: &directories,
	})
	if !errors.Is(err, ErrEnvironmentUnsafe) || err.Error() != ErrEnvironmentUnsafe.Error() {
		t.Fatalf("ResolveEnvironment() error = %v, want fixed ErrEnvironmentUnsafe", err)
	}
	for _, name := range []string{"xdg", "go-build", "go-mod", "npm", "pip", "uv"} {
		if _, statErr := os.Lstat(filepath.Join(directories.Cache, name)); !os.IsNotExist(statErr) {
			t.Fatalf("derived cache %s was created before the sensitive bound passed", name)
		}
	}
}

func TestResolveEnvironmentKeepsPrivatePathRedactionsInsideSensitiveByteBound(t *testing.T) {
	directories := PrivateDirectories{
		Root:  strings.Repeat("r", 1<<20),
		Home:  "/known/private/home",
		Temp:  "/known/private/temp",
		Cache: "/known/private/cache",
	}
	snapshot, err := ResolveEnvironment(EnvironmentOptions{PrivateDirectories: &directories})
	if !errors.Is(err, ErrEnvironmentUnsafe) || snapshot.Entries() != nil {
		t.Fatalf("ResolveEnvironment() = (%#v, %v), want failed bounded snapshot", snapshot.Entries(), err)
	}
	total := 0
	for _, value := range snapshot.RedactionValues() {
		total += len(value)
	}
	if total > 1<<20 {
		t.Fatalf("private redaction bytes = %d, want <= 1 MiB", total)
	}
}

func TestResolveEnvironmentReturnsSortedImmutableClones(t *testing.T) {
	host := []string{
		"ZED=ordinary",
		"ALPHA_TOKEN=longest-sensitive-value",
		"BETA_SECRET=aa",
		"GAMMA_PASSWORD=zz",
	}
	allow := []string{"ALPHA_TOKEN"}
	snapshot, err := ResolveEnvironment(EnvironmentOptions{HostEntries: host, AllowNames: allow})
	if err != nil {
		t.Fatal(err)
	}

	host[0] = "MUTATED=caller"
	allow[0] = "MUTATED_TOKEN"
	wantEntries := []string{"ALPHA_TOKEN=longest-sensitive-value", "ZED=ordinary"}
	wantRedactions := []string{"longest-sensitive-value", "aa", "zz"}
	if !reflect.DeepEqual(snapshot.Entries(), wantEntries) || !reflect.DeepEqual(snapshot.RedactionValues(), wantRedactions) {
		t.Fatal("EnvironmentSnapshot retained inputs or returned nondeterministic ordering")
	}

	entries := snapshot.Entries()
	redactions := snapshot.RedactionValues()
	entries[0] = "MUTATED=result"
	redactions[0] = "mutated-result"
	if !reflect.DeepEqual(snapshot.Entries(), wantEntries) || !reflect.DeepEqual(snapshot.RedactionValues(), wantRedactions) {
		t.Fatal("EnvironmentSnapshot accessors exposed internal slices")
	}

	empty, err := ResolveEnvironment(EnvironmentOptions{HostEntries: []string{}})
	if err != nil {
		t.Fatal(err)
	}
	if empty.Entries() == nil || empty.RedactionValues() == nil {
		t.Fatal("EnvironmentSnapshot returned nil rather than explicit empty slices")
	}
}

func TestResolveEnvironmentRejectsInvalidOptionNames(t *testing.T) {
	tests := []EnvironmentOptions{
		{HostEntries: []string{}, ProviderNames: []string{"INVALID-NAME"}},
		{HostEntries: []string{}, AllowNames: []string{"*_TOKEN"}},
		{HostEntries: []string{}, AllowNames: []string{"DUPLICATE", "DUPLICATE"}},
	}
	for _, options := range tests {
		_, err := ResolveEnvironment(options)
		if !errors.Is(err, ErrEnvironmentUnsafe) || err.Error() != ErrEnvironmentUnsafe.Error() {
			t.Fatalf("ResolveEnvironment() error = %v, want fixed ErrEnvironmentUnsafe", err)
		}
	}
}

func snapshotEnvironment(t *testing.T, snapshot EnvironmentSnapshot) map[string]string {
	t.Helper()
	return environmentEntriesMap(t, snapshot.Entries())
}

func environmentEntriesMap(t *testing.T, entries []string) map[string]string {
	t.Helper()
	environment := make(map[string]string, len(entries))
	for _, entry := range entries {
		name, value, found := strings.Cut(entry, "=")
		if !found || name == "" {
			t.Fatal("environment output contained a malformed entry")
		}
		if _, duplicate := environment[name]; duplicate {
			t.Fatal("environment output contained a duplicate name")
		}
		environment[name] = value
	}
	return environment
}

func assertEnvironmentNames(t *testing.T, snapshot EnvironmentSnapshot, want []string) {
	t.Helper()
	environment := snapshotEnvironment(t, snapshot)
	got := make([]string, 0, len(environment))
	for name := range environment {
		got = append(got, name)
	}
	slices.Sort(got)
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("environment names = %q, want %q", got, want)
	}
}

func assertRedactionsContain(t *testing.T, got, want []string) {
	t.Helper()
	for index, value := range want {
		if !slices.Contains(got, value) {
			t.Fatalf("redactions did not contain required value at index %d", index)
		}
	}
}

func assertRedactionsSorted(t *testing.T, values []string) {
	t.Helper()
	if !slices.IsSortedFunc(values, func(left, right string) int {
		if len(left) != len(right) {
			return len(right) - len(left)
		}
		return strings.Compare(left, right)
	}) {
		t.Fatal("redactions are not sorted longest-first then lexically")
	}
}

func newPrivateDirectories(t *testing.T) PrivateDirectories {
	t.Helper()
	root := filepath.Join(t.TempDir(), "private")
	if err := os.Mkdir(root, 0o700); err != nil {
		t.Fatal(err)
	}
	directories := PrivateDirectories{
		Root:  root,
		Home:  filepath.Join(root, "home"),
		Temp:  filepath.Join(root, "tmp"),
		Cache: filepath.Join(root, "cache"),
	}
	for _, path := range []string{directories.Home, directories.Temp, directories.Cache} {
		if err := os.Mkdir(path, 0o700); err != nil {
			t.Fatal(err)
		}
	}
	return directories
}

func assertUnsafeEnvironmentResolution(t *testing.T, options EnvironmentOptions) {
	t.Helper()
	_, err := ResolveEnvironment(options)
	if !errors.Is(err, ErrEnvironmentUnsafe) || err.Error() != ErrEnvironmentUnsafe.Error() {
		t.Fatalf("ResolveEnvironment() error = %v, want fixed ErrEnvironmentUnsafe", err)
	}
}

func sensitiveEnvironmentEntries(count int) []string {
	entries := make([]string, 0, count)
	for index := range count {
		entries = append(entries, fmt.Sprintf("VALUE_%03d_TOKEN=unique-sensitive-value-%03d", index, index))
	}
	return entries
}
