package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"reflect"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/auth"
	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/memory"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/safetext"
	"github.com/baiyuqing/otto/internal/sandbox"
	"github.com/baiyuqing/otto/internal/sandbox/direct"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/skill"
	"github.com/baiyuqing/otto/internal/tool"
)

// bindSpyService wraps a real memory.Service and records the last Binding
// returned by Bind, wrapped in a spyBinding, so tests can assert it was
// closed on an error path that must not leak it.
type bindSpyService struct {
	memory.Service
	lastBinding *spyBinding
}

func (s *bindSpyService) Bind(ctx context.Context, options memory.BindOptions) (memory.Binding, error) {
	bound, err := s.Service.Bind(ctx, options)
	if err != nil {
		return nil, err
	}
	spy := &spyBinding{Binding: bound}
	s.lastBinding = spy
	return spy, nil
}

type spyBinding struct {
	memory.Binding
	closed bool
}

func (b *spyBinding) Close() error {
	b.closed = true
	return b.Binding.Close()
}

type closableRunner struct{ closeCalls atomic.Int32 }

func (*closableRunner) Run(context.Context, string, func(agent.Event)) error {
	return nil
}

func (*closableRunner) Compact(context.Context, string, func(agent.Event)) (agent.CompactionResult, error) {
	return agent.CompactionResult{Noop: true}, nil
}

func (r *closableRunner) Close() error {
	r.closeCalls.Add(1)
	return nil
}

func TestRuntimeBuilderUsesStoredProfileProviderAndModel(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default", "resumed"))

	runtime, err := builder.resolveSession(session.RuntimeMetadata{Profile: "resumed", Provider: "openai-compatible", Model: "stored-model"})
	if err != nil {
		t.Fatal(err)
	}
	if runtime.Profile != "resumed" || runtime.Provider != "openai-compatible" || runtime.Model != "stored-model" || runtime.APIKey != "resumed-secret" || runtime.BaseURL != "https://resumed.example/v1" {
		t.Fatalf("runtime = %#v", redactedRuntime(runtime))
	}
}

func TestRuntimeBuilderExternalPiUsesDefaultEndpointWithSessionModel(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))

	runtime, err := builder.resolveSession(session.RuntimeMetadata{Provider: "openai-compatible", Model: "pi-model"})
	if err != nil {
		t.Fatal(err)
	}
	if runtime.Profile != "default" || runtime.Provider != "openai-compatible" || runtime.Model != "pi-model" || runtime.APIKey != "default-secret" || runtime.BaseURL != "https://default.example/v1" {
		t.Fatalf("runtime = %#v", redactedRuntime(runtime))
	}
}

func TestRuntimeBuilderResolveSessionIgnoresProcessModelOverrides(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	builder.environment["OTTO_PROVIDER"] = "codex"
	builder.environment["OTTO_MODEL"] = "env-model"

	runtime, err := builder.resolveSession(session.RuntimeMetadata{Profile: "default", Provider: "openai-compatible", Model: "stored-model"})
	if err != nil {
		t.Fatal(err)
	}
	if runtime.Provider != "openai-compatible" || runtime.Model != "stored-model" {
		t.Fatalf("runtime = %#v", redactedRuntime(runtime))
	}
}

func TestRuntimeBuilderResolveSessionIgnoresProcessProfileOverrides(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default", "stored", "env"))
	builder.environment["OTTO_PROFILE"] = "env"

	runtime, err := builder.resolveSession(session.RuntimeMetadata{Profile: "stored", Provider: "openai-compatible", Model: "stored-model"})
	if err != nil {
		t.Fatal(err)
	}
	if runtime.Profile != "stored" || runtime.APIKey != "stored-secret" || runtime.BaseURL != "https://stored.example/v1" {
		t.Fatalf("runtime = %#v", redactedRuntime(runtime))
	}
}

func TestRuntimeBuilderBuildNewReplacementPreservesExplicitBaseURL(t *testing.T) {
	const overrideBaseURL = "https://cli-override.example/v1"
	tests := []struct {
		name        string
		file        config.File
		environment map[string]string
		current     app.RuntimeInfo
	}{
		{
			name:        "profile-less runtime",
			file:        config.File{},
			environment: map[string]string{"OTTO_API_KEY": "fallback-secret"},
			current:     app.RuntimeInfo{Provider: "openai-compatible", Model: "gpt-4.1"},
		},
		{
			name:        "profile endpoint overridden",
			file:        configWithProfiles("default"),
			environment: map[string]string{"DEFAULT_KEY": "default-secret"},
			current:     app.RuntimeInfo{Provider: "openai-compatible", Profile: "default", Model: "gpt-4.1"},
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			workspacePath := mustCanonicalDirectory(t, t.TempDir())
			workspace, err := tool.NewWorkspace(workspacePath)
			if err != nil {
				t.Fatal(err)
			}
			var createdRuntime config.Runtime
			deps := runDependencies{newSession: func(_ bool, _ string, workspace string, runtime config.Runtime) (session.Session, error) {
				createdRuntime = runtime
				return session.NewMemory(session.Header{
					Version: session.CurrentVersion, ID: "fresh", Workspace: workspace,
					Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model, CreatedAt: time.Now().UTC(),
				}), nil
			}}
			builder := newRuntimeBuilder(filepath.Join(t.TempDir(), "config.toml"), tt.file, tt.environment, workspace, workspacePath, filepath.Join(t.TempDir(), "sessions"), "/bin/sh", cliOptions{
				baseURL: overrideBaseURL,
			}, nil, deps)
			builder.buildRunnerOverride = func(session.Session, config.Runtime) (app.Runner, error) {
				return commandRunnerFunc(func(context.Context, string, func(agent.Event)) error { return nil }), nil
			}

			replacement, err := builder.buildNewReplacement(context.Background(), tt.current)
			if err != nil {
				t.Fatal(err)
			}
			defer replacement.Session.Close()
			if createdRuntime.BaseURL != overrideBaseURL {
				t.Fatalf("new replacement base URL = %q, want explicit override %q", createdRuntime.BaseURL, overrideBaseURL)
			}
		})
	}
}

func TestRuntimeBuilderRejectsCLIBaseURLSecretsWithBoundedRedactedError(t *testing.T) {
	const (
		username = "cli-userinfo-name"
		password = "cli-userinfo-password"
		query    = "cli-query-value"
		fragment = "cli-fragment-value"
	)
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	builder.runtimeOverrides.BaseURL = "https://" + username + ":" + password + "@example.test/v1?tenant=" + query + "#" + fragment

	_, err := builder.resolveSession(session.RuntimeMetadata{Profile: "default", Provider: "openai-compatible", Model: "stored-model"})
	if err == nil || !strings.Contains(err.Error(), "invalid base_url") {
		t.Fatalf("resolveSession() error = %v, want invalid base_url", err)
	}
	for _, secret := range []string{username, password, query, fragment} {
		if strings.Contains(err.Error(), secret) {
			t.Fatalf("resolveSession() leaked CLI endpoint value %q: %v", secret, err)
		}
	}
	if len(err.Error()) > 512 {
		t.Fatalf("resolveSession() error length = %d, want <= 512", len(err.Error()))
	}
	boundary := builder.redactError(fmt.Errorf("boundary exposed %s %s %s %s", username, password, query, fragment), nil)
	for _, secret := range []string{username, password, query, fragment} {
		if strings.Contains(boundary.Error(), secret) {
			t.Fatalf("redactError() leaked CLI endpoint value %q: %v", secret, boundary)
		}
	}
}

func TestRuntimeBuilderRejectsBaseURLUserinfoWithBoundedRedactedError(t *testing.T) {
	username := "userinfo-name"
	password := "userinfo-" + strings.Repeat("secret", 8<<10)
	file := config.File{
		DefaultProfile: "default",
		Profiles: map[string]config.Profile{
			"default": {
				Provider: "openai-compatible", Model: "test-model", APIKeyEnv: "DEFAULT_KEY",
				BaseURL: "https://" + username + ":" + password + "@example.test/v1",
			},
		},
	}
	builder := newRuntimeBuilderForTest(t, file)

	_, err := builder.resolveSession(session.RuntimeMetadata{Profile: "default", Provider: "openai-compatible", Model: "stored-model"})
	if err == nil || !strings.Contains(err.Error(), "invalid base_url") {
		t.Fatalf("resolveSession() error = %v, want invalid base_url", err)
	}
	if strings.Contains(err.Error(), username) || strings.Contains(err.Error(), password) {
		t.Fatalf("resolveSession() leaked URL userinfo: %.200s", err)
	}
	if len(err.Error()) > 512 {
		t.Fatalf("resolveSession() error length = %d, want <= 512", len(err.Error()))
	}
}

func TestRuntimeBuilderCollectsEncodedAndMalformedEndpointSecretForms(t *testing.T) {
	const (
		encodedEndpoint   = "https://raw%2Buser:raw%2Fpass@example.test/v1?token=raw+query&encoded=percent%2Fvalue#frag%2Bvalue"
		malformedEndpoint = "https://bad%zz:pass%2Fword@example.test/v1?token=plus+value&broken=bad%zz#raw%2Gfragment"
	)
	file := configWithProfiles("active")
	file.Profiles["encoded-inactive"] = config.Profile{
		Provider: "openai-compatible", BaseURL: encodedEndpoint, Model: "inactive", APIKeyEnv: "ENCODED_KEY",
	}
	file.Profiles["malformed-inactive"] = config.Profile{
		Provider: "openai-compatible", BaseURL: malformedEndpoint, Model: "inactive", APIKeyEnv: "MALFORMED_KEY",
	}
	builder := newRuntimeBuilderForTest(t, file)
	values := builder.secretValues(nil)
	for _, want := range []string{
		encodedEndpoint,
		"raw%2Buser:raw%2Fpass",
		"raw%2Buser",
		"raw%2Fpass",
		"raw+user:raw/pass",
		"raw+user",
		"raw/pass",
		"raw+query",
		"raw query",
		"percent%2Fvalue",
		"percent/value",
		"frag%2Bvalue",
		"frag+value",
		malformedEndpoint,
		"bad%zz:pass%2Fword",
		"bad%zz",
		"pass%2Fword",
		"pass/word",
		"plus+value",
		"plus value",
		"bad%zz",
		"raw%2Gfragment",
	} {
		if !containsString(values, want) {
			t.Fatalf("secretValues() omitted endpoint form %q", want)
		}
	}
}

func TestRuntimeBuilderExpandsJSONStringEscapeFormsForProviderAndEndpointSecrets(t *testing.T) {
	const (
		rawKey          = `key\"value`
		decodedKey      = `key"value`
		rawEndpoint     = `https://example.test/v1?token=endpoint\u003cvalue`
		decodedEndpoint = `https://example.test/v1?token=endpoint<value`
	)
	file := configWithProfiles("active")
	file.Profiles["escaped-inactive"] = config.Profile{
		Provider: "openai-compatible", BaseURL: rawEndpoint, Model: "inactive", APIKeyEnv: "ESCAPED_KEY",
	}
	builder := newRuntimeBuilderForTest(t, file)
	builder.environment["ESCAPED_KEY"] = rawKey
	values := builder.secretValues(nil)
	for _, want := range []string{rawKey, decodedKey, rawEndpoint, decodedEndpoint, `endpoint\u003cvalue`, "endpoint<value"} {
		if !containsString(values, want) {
			t.Fatalf("secretValues() omitted JSON string escape form %q: %#v", want, values)
		}
	}
}

func TestRuntimeBuilderRedactsJSONStringEscapeFormsFromRawProviderBody(t *testing.T) {
	tests := []struct {
		raw     string
		decoded string
	}{
		{raw: `less\u003cthan`, decoded: "less<than"},
		{raw: `\\u003c`, decoded: `\u003c`},
		{raw: `greater\u003Ethan`, decoded: "greater>than"},
		{raw: `amp\u0026ersand`, decoded: "amp&ersand"},
		{raw: `quote\"value`, decoded: `quote"value`},
		{raw: `back\\slash`, decoded: `back\slash`},
		{raw: `slash\/value`, decoded: "slash/value"},
		{raw: `line\nbreak`, decoded: "line\nbreak"},
		{raw: `separator\u2028value`, decoded: "separator\u2028value"},
		{raw: `paragraph\u2029value`, decoded: "paragraph\u2029value"},
	}
	var requestBody string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(r.Body)
		if err != nil {
			t.Errorf("read request body: %v", err)
		}
		requestBody = string(body)
		writeSSE(w, `{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`)
	}))
	defer server.Close()

	file := configWithProfiles("active")
	active := file.Profiles["active"]
	active.BaseURL = server.URL
	active.APIKeyEnv = "ESCAPED_KEY_0"
	file.Profiles["active"] = active
	builder := newRuntimeBuilderForTest(t, file)
	decoded := make([]string, len(tests))
	for index, test := range tests {
		name := fmt.Sprintf("ESCAPED_KEY_%d", index)
		builder.environment[name] = test.raw
		decoded[index] = test.decoded
		if index > 0 {
			fileProfile := config.Profile{Provider: "openai-compatible", BaseURL: "https://inactive.example/v1", Model: "inactive", APIKeyEnv: name}
			builder.config.Profiles[fmt.Sprintf("inactive-%d", index)] = fileProfile
		}
	}
	memory := session.NewMemory(session.Header{
		Version: session.CurrentVersion, ID: "raw-provider-body", Workspace: builder.workspacePath,
		Provider: "openai-compatible", Profile: "active", Model: "active-model", CreatedAt: time.Now().UTC(),
	})
	runner, err := builder.buildRunner(context.Background(), memory, config.Runtime{
		Profile: "active", Provider: "openai-compatible", BaseURL: server.URL, Model: "active-model",
		APIKeyEnv: "ESCAPED_KEY_0", APIKey: tests[0].raw, ShellTimeout: time.Second, MaxOutputBytes: 4096,
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := runner.Run(context.Background(), strings.Join(decoded, " | "), nil); err != nil {
		t.Fatal(err)
	}
	for _, test := range tests {
		if strings.Contains(requestBody, test.raw) || strings.Contains(requestBody, test.decoded) {
			t.Fatalf("raw provider body retained JSON escape secret (%q, %q): %s", test.raw, test.decoded, requestBody)
		}
	}
}

func TestRuntimeBuilderCanonicalizesInvalidUTF8DecodedEndpointForms(t *testing.T) {
	const endpoint = "https://user%FF:pass%C0%AF@example.test/v1?ff=decoded%FF&over=overlong%C0%AF#fragment%FF"
	file := configWithProfiles("active")
	file.Profiles["inactive-invalid-utf8"] = config.Profile{
		Provider: "openai-compatible", BaseURL: endpoint, Model: "inactive", APIKeyEnv: "INVALID_UTF8_KEY",
	}
	builder := newRuntimeBuilderForTest(t, file)
	values := builder.secretValues(nil)

	for _, value := range values {
		if !utf8.ValidString(value) {
			t.Fatalf("secretValues() retained invalid UTF-8: %q", value)
		}
	}
	for _, want := range []string{
		endpoint,
		"user%FF", "pass%C0%AF", "decoded%FF", "overlong%C0%AF", "fragment%FF",
		"user�", "pass��", "decoded�", "overlong��", "fragment�",
	} {
		if !containsString(values, want) {
			t.Fatalf("secretValues() omitted canonical/raw endpoint form %q", want)
		}
	}
}

func TestRuntimeBuilderConservativelyExtractsMalformedEndpointAuthorities(t *testing.T) {
	tests := []struct {
		name     string
		endpoint string
		want     []string
	}{
		{
			name:     "ambiguous extra slash",
			endpoint: "https:///raw%20user:raw%2Fpass@example.test/v1",
			want:     []string{"raw%20user:raw%2Fpass", "raw%20user", "raw%2Fpass", "raw user:raw/pass", "raw user", "raw/pass"},
		},
		{
			name:     "backslash authority",
			endpoint: `https:\\raw%20user:raw%2Fpass@example.test\v1`,
			want:     []string{"raw%20user:raw%2Fpass", "raw%20user", "raw%2Fpass", "raw user:raw/pass", "raw user", "raw/pass"},
		},
		{
			name:     "missing scheme",
			endpoint: "raw%20user:raw%2Fpass@example.test/v1",
			want:     []string{"raw%20user:raw%2Fpass", "raw user:raw/pass", "raw user", "raw/pass"},
		},
		{
			name:     "one malformed component",
			endpoint: "https://bad%zz:pass%2Fword@[::1]:8443/v1?next=@ignored#@ignored",
			want:     []string{"bad%zz:pass%2Fword", "bad%zz", "pass%2Fword", "pass/word"},
		},
		{
			name:     "reviewer exact multi at",
			endpoint: "http:///real%20user:real%2Fpass@proxy/path%20user:path%2Fpass@example",
			want:     []string{"real%20user:real%2Fpass", "real user:real/pass", "path%20user:path%2Fpass", "path user:path/pass"},
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			var values []string
			collectURLSecretValues(test.endpoint, func(value string) bool { values = append(values, value); return true })
			if !containsString(values, test.endpoint) {
				t.Fatalf("complete raw endpoint was not retained: %#v", values)
			}
			for _, want := range test.want {
				if !containsString(values, want) {
					t.Fatalf("endpoint forms omitted %q: %#v", want, values)
				}
			}
		})
	}
}

func TestRuntimeBuilderMalformedConfiguredEndpointMakesBoundaryIncomplete(t *testing.T) {
	file := configWithProfiles("active")
	file.Profiles["malformed-inactive"] = config.Profile{
		Provider:  "openai-compatible",
		BaseURL:   "http:///real%20user:real%2Fpass@proxy/path%20user:path%2Fpass@example",
		Model:     "inactive",
		APIKeyEnv: "MALFORMED_KEY",
	}
	builder := newRuntimeBuilderForTest(t, file)

	if got := builder.boundaryRedactor(nil).RedactString("dynamic-provider-content"); got != "" {
		t.Fatalf("malformed configured endpoint left boundary complete: %q", got)
	}
	info := builder.runtimeInfo(config.Runtime{
		Provider: "openai-compatible", Profile: "active", Model: "model",
	})
	if info.Provider != "" || info.Profile != "" || info.Model != "" {
		t.Fatalf("malformed configured endpoint exposed runtime metadata: %#v", info)
	}
}

func TestRuntimeBuilderBoundsMalformedRepeatedEndpointPrefixes(t *testing.T) {
	var endpoint strings.Builder
	endpoint.WriteString("http:///")
	for index := 0; index < 600; index++ {
		fmt.Fprintf(&endpoint, "u%03d:p%03d@", index, index)
	}
	endpoint.WriteString("example.test")
	file := configWithProfiles("active")
	file.Profiles["malformed-inactive"] = config.Profile{
		Provider:  "openai-compatible",
		BaseURL:   endpoint.String(),
		Model:     "inactive",
		APIKeyEnv: "MALFORMED_KEY",
	}
	builder := newRuntimeBuilderForTest(t, file)
	values, complete := builder.boundarySecretValues(nil)
	if complete {
		t.Fatal("boundarySecretValues() complete = true, want bounded incomplete prefix")
	}
	if len(values) > 512 {
		t.Fatalf("boundarySecretValues() count = %d, want <= 512", len(values))
	}
	if containsString(values, "u000:p000@u001:p001") {
		t.Fatalf("boundarySecretValues() retained an intermediate cumulative prefix: %#v", values[:min(len(values), 8)])
	}
}

func TestRuntimeBuilderDoesNotTreatValidEndpointPathAtAsUserinfo(t *testing.T) {
	endpoint := "https://[2001:db8::1]:8443/path/user:pass@example.test"
	var values []string
	collectURLSecretValues(endpoint, func(value string) bool { values = append(values, value); return true })
	for _, unexpected := range []string{"user:pass", "user", "pass"} {
		if containsString(values, unexpected) {
			t.Fatalf("valid endpoint path @ was misclassified as userinfo %q: %#v", unexpected, values)
		}
	}
}

func TestRuntimeBuilderMalformedEndpointPathAtDoesNotInventUserinfo(t *testing.T) {
	endpoint := "https://[2001:db8::1]:8443/path/user:pass@example.test?broken=%zz"
	var values []string
	collectURLSecretValues(endpoint, func(value string) bool { values = append(values, value); return true })
	if !containsString(values, endpoint) {
		t.Fatalf("complete malformed endpoint was not retained: %#v", values)
	}
	for _, unexpected := range []string{"user:pass", "user", "pass"} {
		if containsString(values, unexpected) {
			t.Fatalf("malformed endpoint path @ was misclassified as userinfo %q: %#v", unexpected, values)
		}
	}
}

func TestRuntimeBuilderResolveSessionRejectsMissingModel(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))

	_, err := builder.resolveSession(session.RuntimeMetadata{Provider: "openai-compatible"})
	if err == nil || !strings.Contains(err.Error(), "missing model") {
		t.Fatalf("resolveSession() error = %v, want missing model", err)
	}
}

func TestRuntimeBuilderOpenReplacementRejectsInvalidRuntimeBeforeOpeningCandidate(t *testing.T) {
	workspace := mustCanonicalDirectory(t, t.TempDir())
	root := filepath.Join(t.TempDir(), "sessions")

	for _, tt := range []struct {
		name        string
		file        config.File
		path        string
		environment map[string]string
		wantErr     string
		forbidden   []string
	}{
		{
			name:    "missing stored profile",
			file:    configWithProfiles("default"),
			path:    createStoredSession(t, root, workspace, session.Header{Version: session.CurrentVersion, ID: "missing-profile", Workspace: workspace, Provider: "openai-compatible", Profile: "missing", Model: "stored-model", CreatedAt: time.Now().UTC()}),
			wantErr: `profile "missing" not found`,
		},
		{
			name:    "unsupported provider",
			file:    configWithProfiles("default"),
			path:    createStoredSession(t, root, workspace, session.Header{Version: session.CurrentVersion, ID: "unsupported-provider", Workspace: workspace, Provider: "codex", Profile: "default", Model: "stored-model", CreatedAt: time.Now().UTC()}),
			wantErr: `unsupported provider`,
		},
		{
			name: "missing api key",
			file: config.File{
				DefaultProfile: "default",
				Profiles: map[string]config.Profile{
					"default": {Provider: "openai-compatible", BaseURL: "https://default.example/v1", Model: "default-model", APIKeyEnv: "MISSING_KEY"},
				},
			},
			environment: map[string]string{},
			path:        createStoredSession(t, root, workspace, session.Header{Version: session.CurrentVersion, ID: "missing-key", Workspace: workspace, Provider: "openai-compatible", Profile: "default", Model: "stored-model", CreatedAt: time.Now().UTC()}),
			wantErr:     `missing api key`,
		},
		{
			name: "invalid endpoint redacts secrets",
			file: config.File{
				DefaultProfile: "default",
				Profiles: map[string]config.Profile{
					"default": {Provider: "openai-compatible", BaseURL: "https://example.test/v1?tenant=query-secret", Model: "default-model", APIKeyEnv: "DEFAULT_KEY"},
				},
			},
			path:      createStoredSession(t, root, workspace, session.Header{Version: session.CurrentVersion, ID: "invalid-endpoint", Workspace: workspace, Provider: "openai-compatible", Profile: "default", Model: "stored-model", CreatedAt: time.Now().UTC()}),
			wantErr:   `invalid base_url`,
			forbidden: []string{"query-secret", "default-secret"},
		},
	} {
		t.Run(tt.name, func(t *testing.T) {
			builder := newRuntimeBuilderForTest(t, tt.file)
			builder.workspacePath = workspace
			if tt.environment != nil {
				builder.environment = tt.environment
			}
			builder.prepareSession = func(ctx context.Context, path, workspace string) (preparedSession, error) {
				prepared, err := prepareSession(ctx, path, workspace)
				if err != nil {
					return nil, err
				}
				return &fakePreparedSession{
					info: prepared.Info(),
					activate: func(context.Context) (session.Session, []session.Warning, error) {
						return nil, nil, errors.New("activation must not run")
					},
					close: prepared.Close,
				}, nil
			}

			_, err := builder.openReplacement(context.Background(), tt.path)
			if err == nil || !strings.Contains(err.Error(), tt.wantErr) {
				t.Fatalf("openReplacement() error = %v, want %q", err, tt.wantErr)
			}
			for _, forbidden := range tt.forbidden {
				if strings.Contains(err.Error(), forbidden) {
					t.Fatalf("openReplacement() error leaked %q: %v", forbidden, err)
				}
			}
		})
	}
}

func TestRuntimeBuilderInvalidRuntimeAbandonsPreparedFileWithoutMutation(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	path := createStoredSession(t, builder.sessionRoot, builder.workspacePath, session.Header{
		Version: session.CurrentVersion, ID: "abandoned-invalid-runtime", Workspace: builder.workspacePath,
		Provider: "openai-compatible", Profile: "missing", Model: "stored-model", CreatedAt: time.Now().UTC(),
	})
	before := bytes.TrimSuffix(mustReadFile(t, path), []byte{'\n'})
	if err := os.WriteFile(path, before, 0o600); err != nil {
		t.Fatal(err)
	}

	_, err := builder.openReplacement(context.Background(), path)
	if err == nil || !strings.Contains(err.Error(), `profile "missing" not found`) {
		t.Fatalf("openReplacement() error = %v, want missing profile", err)
	}
	if after := mustReadFile(t, path); !bytes.Equal(after, before) {
		t.Fatal("invalid runtime activation path mutated repairable session")
	}
}

func TestRuntimeBuilderOpenReplacementReturnsWarningsAndRuntimeInfo(t *testing.T) {
	const maliciousToolCallID = "malicious-warning-tool-call-id"
	file := configWithProfiles("default", "resumed")
	contextWindow := 131_072
	profile := file.Profiles["resumed"]
	profile.ContextWindow = &contextWindow
	file.Profiles["resumed"] = profile
	builder := newRuntimeBuilderForTest(t, file)
	builder.sandboxSecrets = append(builder.sandboxSecrets, maliciousToolCallID)
	path := createStoredSession(t, builder.sessionRoot, builder.workspacePath, session.Header{Version: session.CurrentVersion, ID: "resumed-session", Workspace: builder.workspacePath, Provider: "openai-compatible", Profile: "resumed", Model: "stored-model", CreatedAt: time.Now().UTC()})
	warnings := []session.Warning{{Message: "repaired dangling tool call " + maliciousToolCallID}}
	var captured config.Runtime

	builder.prepareSession = func(ctx context.Context, path, workspace string) (preparedSession, error) {
		prepared, err := prepareSession(ctx, path, workspace)
		if err != nil {
			return nil, err
		}
		return &fakePreparedSession{
			info: prepared.Info(),
			activate: func(ctx context.Context) (session.Session, []session.Warning, error) {
				store, _, err := prepared.Activate(ctx)
				return store, warnings, err
			},
			close: prepared.Close,
		}, nil
	}
	builder.buildRunnerOverride = func(current session.Session, runtime config.Runtime) (app.Runner, error) {
		captured = runtime
		return commandRunnerFunc(func(context.Context, string, func(agent.Event)) error { return nil }), nil
	}

	replacement, err := builder.openReplacement(context.Background(), path)
	if err != nil {
		t.Fatal(err)
	}
	defer replacement.Session.Close()
	if replacement.RuntimeInfo.Provider != "openai-compatible" || replacement.RuntimeInfo.Profile != "resumed" || replacement.RuntimeInfo.Model != "stored-model" {
		t.Fatalf("runtime info = %#v", replacement.RuntimeInfo)
	}
	if replacement.RuntimeInfo.ContextWindow != 131_072 {
		t.Fatalf("runtime info context window = %d, want 131072", replacement.RuntimeInfo.ContextWindow)
	}
	if len(replacement.Warnings) != 1 || strings.Contains(replacement.Warnings[0].Message, maliciousToolCallID) || !strings.Contains(replacement.Warnings[0].Message, "repaired dangling tool call") {
		t.Fatalf("warnings = %#v", replacement.Warnings)
	}
	replacement.Warnings[0].Message = "mutated"
	if warnings[0].Message != "repaired dangling tool call "+maliciousToolCallID {
		t.Fatalf("warnings mutated = %#v", warnings)
	}
	if captured.Profile != "resumed" || captured.Model != "stored-model" || captured.APIKey != "resumed-secret" {
		t.Fatalf("captured runtime = %#v", redactedRuntime(captured))
	}
}

func TestRuntimeBuilderActivationErrorClosesReturnedCandidateOnce(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	path := createStoredSession(t, builder.sessionRoot, builder.workspacePath, session.Header{
		Version: session.CurrentVersion, ID: "activate-error", Workspace: builder.workspacePath,
		Provider: "openai-compatible", Profile: "default", Model: "stored-model", CreatedAt: time.Now().UTC(),
	})
	candidate := &trackedReplacementSession{
		Session: session.NewMemory(session.Header{
			Version: session.CurrentVersion, ID: "candidate", Workspace: builder.workspacePath,
			Provider: "openai-compatible", Profile: "default", Model: "stored-model", CreatedAt: time.Now().UTC(),
		}),
		closed: make(chan struct{}),
	}
	activateErr := errors.New("activate failed after returning candidate")
	builder.prepareSession = func(context.Context, string, string) (preparedSession, error) {
		return &fakePreparedSession{
			info: session.SessionInfo{Path: path, ID: "activate-error", CWD: builder.workspacePath, Profile: "default", Provider: "openai-compatible", Model: "stored-model"},
			activate: func(context.Context) (session.Session, []session.Warning, error) {
				return candidate, nil, activateErr
			},
		}, nil
	}

	if _, err := builder.openReplacement(context.Background(), path); !errors.Is(err, activateErr) {
		t.Fatalf("openReplacement() error = %v, want activation error", err)
	}
	if candidate.closeCalls.Load() != 1 {
		t.Fatalf("candidate close calls = %d, want 1", candidate.closeCalls.Load())
	}
}

func TestRuntimeBuilderRejectsActivatedSessionMetadataMismatchAndClosesCandidate(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	path := filepath.Join(builder.sessionRoot, "requested.jsonl")
	candidate := &trackedReplacementSession{
		Session: session.NewMemory(session.Header{
			Version: session.CurrentVersion, ID: "different", Workspace: builder.workspacePath,
			Provider: "openai-compatible", Profile: "default", Model: "different-model", CreatedAt: time.Now().UTC(),
		}),
		closed: make(chan struct{}),
	}
	builder.prepareSession = func(context.Context, string, string) (preparedSession, error) {
		return &fakePreparedSession{
			info: session.SessionInfo{Path: path, ID: "expected", CWD: builder.workspacePath, Profile: "default", Provider: "openai-compatible", Model: "stored-model"},
			activate: func(context.Context) (session.Session, []session.Warning, error) {
				return candidate, nil, nil
			},
		}, nil
	}
	builder.buildRunnerOverride = func(session.Session, config.Runtime) (app.Runner, error) {
		return nil, errors.New("runner must not build for mismatched metadata")
	}

	_, err := builder.openReplacement(context.Background(), path)
	if err == nil || !strings.Contains(err.Error(), "metadata changed") {
		t.Fatalf("openReplacement() error = %v, want metadata mismatch", err)
	}
	if candidate.closeCalls.Load() != 1 {
		t.Fatalf("candidate close calls = %d, want 1", candidate.closeCalls.Load())
	}
}

func TestRuntimeBuilderListedResumeRejectsWorkspaceDirectorySymlinkSwapAndKeepsCurrent(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	listedPath := createStoredSession(t, builder.sessionRoot, builder.workspacePath, session.Header{
		Version: session.CurrentVersion, ID: "listed", Workspace: builder.workspacePath,
		Provider: "openai-compatible", Profile: "default", Model: "test-model", CreatedAt: time.Now().UTC(),
	})
	listed, err := session.List(context.Background(), builder.sessionRoot, builder.workspacePath, "", 20)
	if err != nil || len(listed.Sessions) != 1 || listed.Sessions[0].Path != listedPath {
		t.Fatalf("List() = %#v, %v", listed, err)
	}

	outsideRoot := t.TempDir()
	outsidePath := createStoredSession(t, outsideRoot, builder.workspacePath, session.Header{
		Version: session.CurrentVersion, ID: "listed", Workspace: builder.workspacePath,
		Provider: "openai-compatible", Profile: "default", Model: "test-model", CreatedAt: time.Now().UTC(),
	})
	outsideBefore := mustReadFile(t, outsidePath)
	listedDirectory := filepath.Dir(listedPath)
	if err := os.Rename(listedDirectory, listedDirectory+".moved"); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(filepath.Dir(outsidePath), listedDirectory); err != nil {
		t.Fatal(err)
	}

	builder.buildRunnerOverride = func(session.Session, config.Runtime) (app.Runner, error) {
		return commandRunnerFunc(func(context.Context, string, func(agent.Event)) error { return nil }), nil
	}
	current := &trackedReplacementSession{
		Session: session.NewMemory(session.Header{
			Version: session.CurrentVersion, ID: "current", Workspace: builder.workspacePath,
			Provider: "openai-compatible", Profile: "default", Model: "test-model", CreatedAt: time.Now().UTC(),
		}),
		closed: make(chan struct{}),
	}
	var oldRunnerCalls atomic.Int32
	oldRunner := commandRunnerFunc(func(context.Context, string, func(agent.Event)) error {
		oldRunnerCalls.Add(1)
		return nil
	})
	controller, err := app.New(current, func() (session.Session, error) { return nil, errors.New("unused") }, func(session.Session) app.Runner {
		return oldRunner
	}, app.WithSessionBrowser(func(context.Context, int) (session.ListResult, error) {
		return listed, nil
	}, builder.openReplacement))
	if err != nil {
		t.Fatal(err)
	}
	defer controller.Close()

	if _, err := controller.ResumeSession(context.Background(), listedPath); !errors.Is(err, session.ErrInvalidSession) {
		t.Fatalf("ResumeSession() error = %v, want ErrInvalidSession", err)
	}
	if got := controller.Info(); got.SessionID != "current" {
		t.Fatalf("current session changed after rejected listed candidate: %#v", got)
	}
	if current.closeCalls.Load() != 0 {
		t.Fatalf("current close calls = %d, want 0", current.closeCalls.Load())
	}
	if err := controller.Prompt(context.Background(), "still usable", nil); err != nil {
		t.Fatal(err)
	}
	if oldRunnerCalls.Load() != 1 {
		t.Fatalf("old runner calls = %d, want 1", oldRunnerCalls.Load())
	}
	if after := mustReadFile(t, outsidePath); !bytes.Equal(after, outsideBefore) {
		t.Fatal("rejected resume mutated the outside session")
	}
}

func TestRuntimeBuilderRunnerFailureDoesNotPersistCandidateRuntimeOverride(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	builder.runtimeOverrides.Model = "effective-model"
	candidate := &runtimeUpdateTrackingSession{Session: session.NewMemory(session.Header{
		Version: session.CurrentVersion, ID: "candidate", Workspace: builder.workspacePath,
		Provider: "openai-compatible", Profile: "default", Model: "stored-model", CreatedAt: time.Now().UTC(),
	})}
	builder.prepareSession = func(context.Context, string, string) (preparedSession, error) {
		return &fakePreparedSession{
			info: session.SessionInfo{
				Path: "/sessions/candidate.jsonl", ID: "candidate", CWD: builder.workspacePath,
				Profile: "default", Provider: "openai-compatible", Model: "stored-model",
			},
			activate: func(context.Context) (session.Session, []session.Warning, error) { return candidate, nil, nil },
		}, nil
	}
	builder.buildRunnerOverride = func(session.Session, config.Runtime) (app.Runner, error) {
		return nil, errors.New("runner failed")
	}

	if _, err := builder.openReplacement(context.Background(), "/sessions/candidate.jsonl"); err == nil {
		t.Fatal("openReplacement() succeeded")
	}
	if candidate.updateCalls.Load() != 0 {
		t.Fatalf("runtime update calls = %d, want 0 before successful runner construction", candidate.updateCalls.Load())
	}
	if got := candidate.Header(); got.Model != "stored-model" {
		t.Fatalf("failed candidate header mutated: %#v", got)
	}
	if candidate.closeCalls.Load() != 1 {
		t.Fatalf("candidate close calls = %d, want 1", candidate.closeCalls.Load())
	}
}

func TestRuntimeBuilderFailureClosesCandidateStore(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	candidate := &trackedReplacementSession{Session: createOpenPiStore(t, builder.sessionRoot, builder.workspacePath, "candidate"), closed: make(chan struct{})}
	header := candidate.Header()
	builder.prepareSession = func(context.Context, string, string) (preparedSession, error) {
		return &fakePreparedSession{
			info: session.SessionInfo{
				Path: candidate.Path(), ID: header.ID, CWD: header.Workspace,
				Profile: header.Profile, Provider: header.Provider, Model: header.Model,
			},
			activate: func(context.Context) (session.Session, []session.Warning, error) {
				return candidate, nil, nil
			},
		}, nil
	}
	builder.buildRunnerOverride = func(session.Session, config.Runtime) (app.Runner, error) {
		return nil, errors.New("runner failed")
	}

	if _, err := builder.openReplacement(context.Background(), candidate.Path()); err == nil {
		t.Fatal("expected replacement failure")
	}
	select {
	case <-candidate.closed:
	case <-time.After(time.Second):
		t.Fatal("candidate was not closed")
	}
	if candidate.closeCalls.Load() != 1 {
		t.Fatalf("candidate close calls = %d, want 1", candidate.closeCalls.Load())
	}
}

func TestRuntimeBuilderBuildRunnerMapsResolvedCompactionAndKeepsClientRequestSizer(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	current := session.NewMemory(session.Header{
		Version: session.CurrentVersion, ID: "compaction-options", Workspace: builder.workspacePath,
		Provider: "openai-compatible", Profile: "default", Model: "gpt-5.3-codex-spark", CreatedAt: time.Now().UTC(),
	})
	runtime := config.Runtime{
		Profile: "default", Provider: "openai-compatible", BaseURL: "https://default.example/v1",
		Model: "gpt-5.3-codex-spark", APIKey: "secret", APIKeyEnv: "DEFAULT_KEY",
		ShellTimeout: time.Second, MaxOutputBytes: 64 << 10,
		Compaction: config.CompactionRuntime{
			Auto: true, ContextWindow: 128_000, HardInputWindow: 111_000, WorkingWindow: 99_000,
			MaxOutputTokens: 32_000, ReserveTokens: 12_345, KeepRecentTokens: 23_456,
		},
	}

	runner, err := builder.buildRunner(context.Background(), current, runtime)
	if err != nil {
		t.Fatal(err)
	}
	value := reflect.ValueOf(runner)
	if value.Kind() != reflect.Pointer || value.Elem().Type().PkgPath() != "github.com/baiyuqing/otto/internal/agent" {
		t.Fatalf("runner type = %T, want *agent.Agent", runner)
	}
	options := value.Elem().FieldByName("options")
	if !options.IsValid() {
		t.Fatal("agent options field not found")
	}
	compaction := options.FieldByName("Compaction")
	got := agent.CompactionSettings{
		Auto:             compaction.FieldByName("Auto").Bool(),
		HardInputWindow:  int(compaction.FieldByName("HardInputWindow").Int()),
		WorkingWindow:    int(compaction.FieldByName("WorkingWindow").Int()),
		ReserveTokens:    int(compaction.FieldByName("ReserveTokens").Int()),
		KeepRecentTokens: int(compaction.FieldByName("KeepRecentTokens").Int()),
	}
	want := agent.CompactionSettings{
		Auto: true, HardInputWindow: 111_000, WorkingWindow: 99_000,
		ReserveTokens: 12_345, KeepRecentTokens: 23_456,
	}
	if got != want {
		t.Fatalf("agent compaction settings = %#v, want %#v", got, want)
	}
	if requestSizer := options.FieldByName("RequestSizer"); !requestSizer.IsValid() || requestSizer.IsNil() {
		t.Fatal("OpenAI-compatible client was not retained as automatic RequestSizer")
	}
}

func TestRuntimeBuilderBuildRunnerMapsConfiguredMemoryRecallLimits(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	builder.memoryRecallLimit = 7
	builder.memoryRecallTokenBudget = 999
	current := session.NewMemory(session.Header{
		Version: session.CurrentVersion, ID: "recall-limits", Workspace: builder.workspacePath,
		Provider: "openai-compatible", Profile: "default", Model: "m", CreatedAt: time.Now().UTC(),
	})
	runtime := config.Runtime{
		Profile: "default", Provider: "openai-compatible", BaseURL: "https://default.example/v1",
		Model: "m", ShellTimeout: time.Second, MaxOutputBytes: 64 << 10,
	}

	runner, err := builder.buildRunner(context.Background(), current, runtime)
	if err != nil {
		t.Fatal(err)
	}
	options := reflect.ValueOf(runner).Elem().FieldByName("options")
	if got := options.FieldByName("MemoryRecallLimit").Int(); got != 7 {
		t.Fatalf("MemoryRecallLimit = %d, want 7 (from configured memory.max_results)", got)
	}
	if got := options.FieldByName("MemoryRecallTokenBudget").Int(); got != 999 {
		t.Fatalf("MemoryRecallTokenBudget = %d, want 999 (from configured memory.recall_tokens)", got)
	}
}

func TestRuntimeBuilderBuildRunnerSkipsMemoryToolsWhenNotUsable(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var payload struct {
			Messages []json.RawMessage `json:"messages"`
		}
		_ = json.NewDecoder(r.Body).Decode(&payload)
		if len(payload.Messages) > 2 {
			writeSSE(w, `{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`)
			return
		}
		writeSSE(w, `{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"memory_search","arguments":"{\"query\":\"test\"}"}}]},"finish_reason":"tool_calls"}]}`)
	}))
	defer server.Close()

	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	current := session.NewMemory(session.Header{
		Version: session.CurrentVersion, ID: "no-memory", Workspace: builder.workspacePath,
		Provider: "openai-compatible", Profile: "default", Model: "m", CreatedAt: time.Now().UTC(),
	})
	runtime := config.Runtime{
		Profile: "default", Provider: "openai-compatible", BaseURL: server.URL, Model: "m",
		ShellTimeout: time.Second, MaxOutputBytes: 64 << 10,
	}

	runner, err := builder.buildRunner(context.Background(), current, runtime)
	if err != nil {
		t.Fatal(err)
	}
	var toolContent string
	_ = runner.Run(context.Background(), "search memory", func(event agent.Event) {
		toolContent += event.ToolResult.Content
	})
	if !strings.Contains(toolContent, "unknown tool: memory_search") {
		t.Fatalf("tool result = %q, want unknown-tool error (memory tools must not be registered when memory is unusable)", toolContent)
	}
}

func TestRuntimeBuilderBuildRunnerRegistersAndBindsMemoryWhenUsable(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var payload struct {
			Messages []json.RawMessage `json:"messages"`
		}
		_ = json.NewDecoder(r.Body).Decode(&payload)
		if len(payload.Messages) > 2 {
			writeSSE(w, `{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`)
			return
		}
		writeSSE(w, `{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"memory_search","arguments":"{\"query\":\"test\"}"}}]},"finish_reason":"tool_calls"}]}`)
	}))
	defer server.Close()

	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	dbPath := filepath.Join(t.TempDir(), "memory", "memory.db")
	memoryCfg := config.MemoryRuntime{Enabled: true, Backend: "sqlite", SQLitePath: dbPath}
	service, userScope, usable, err := openMemoryService(context.Background(), memoryCfg, nil, &bytes.Buffer{})
	if err != nil {
		t.Fatalf("openMemoryService() error = %v", err)
	}
	if !usable {
		t.Fatal("openMemoryService() usable = false, want true")
	}
	t.Cleanup(func() { _ = service.Close() })
	workspaceScope, err := workspaceMemoryScope(memoryCfg, builder.workspacePath)
	if err != nil {
		t.Fatalf("workspaceMemoryScope() error = %v", err)
	}
	builder.memoryService = service
	builder.memoryUsable = true
	builder.memoryUserScope = userScope
	builder.memoryWorkspaceScope = workspaceScope

	current := session.NewMemory(session.Header{
		Version: session.CurrentVersion, ID: "with-memory", Workspace: builder.workspacePath,
		Provider: "openai-compatible", Profile: "default", Model: "m", CreatedAt: time.Now().UTC(),
	})
	runtime := config.Runtime{
		Profile: "default", Provider: "openai-compatible", BaseURL: server.URL, Model: "m",
		ShellTimeout: time.Second, MaxOutputBytes: 64 << 10,
	}

	runner, err := builder.buildRunner(context.Background(), current, runtime)
	if err != nil {
		t.Fatal(err)
	}
	value := reflect.ValueOf(runner)
	memoryField := value.Elem().FieldByName("options").FieldByName("Memory")
	if !memoryField.IsValid() || memoryField.IsNil() {
		t.Fatal("agent.Options.Memory is nil, want a bound memory.Binding")
	}

	var toolContent string
	runErr := runner.Run(context.Background(), "search memory", func(event agent.Event) {
		toolContent += event.ToolResult.Content
	})
	if runErr != nil {
		t.Fatalf("Run() error = %v", runErr)
	}
	if !strings.Contains(toolContent, "no matching records") {
		t.Fatalf("tool result = %q, want a real (empty) memory search result", toolContent)
	}
}

func TestRuntimeBuilderBuildRunnerClosesMemoryBindingWhenToolRegistryFails(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	dbPath := filepath.Join(t.TempDir(), "memory", "memory.db")
	memoryCfg := config.MemoryRuntime{Enabled: true, Backend: "sqlite", SQLitePath: dbPath}
	realService, userScope, usable, err := openMemoryService(context.Background(), memoryCfg, nil, &bytes.Buffer{})
	if err != nil {
		t.Fatalf("openMemoryService() error = %v", err)
	}
	if !usable {
		t.Fatal("openMemoryService() usable = false, want true")
	}
	t.Cleanup(func() { _ = realService.Close() })
	workspaceScope, err := workspaceMemoryScope(memoryCfg, builder.workspacePath)
	if err != nil {
		t.Fatalf("workspaceMemoryScope() error = %v", err)
	}
	spy := &bindSpyService{Service: realService}
	builder.memoryService = spy
	builder.memoryUsable = true
	builder.memoryUserScope = userScope
	builder.memoryWorkspaceScope = workspaceScope
	// A duplicate "read" tool forces tool.NewRegistry to fail after the
	// memory binding has already succeeded, exercising the cleanup path.
	builder.extraTools = []tool.Tool{tool.NewReadTool(builder.workspace, 64<<10)}

	current := session.NewMemory(session.Header{
		Version: session.CurrentVersion, ID: "leak-check", Workspace: builder.workspacePath,
		Provider: "openai-compatible", Profile: "default", Model: "m", CreatedAt: time.Now().UTC(),
	})
	runtime := config.Runtime{
		Profile: "default", Provider: "openai-compatible", BaseURL: "https://default.example/v1",
		Model: "m", ShellTimeout: time.Second, MaxOutputBytes: 64 << 10,
	}

	_, err = builder.buildRunner(context.Background(), current, runtime)
	if err == nil {
		t.Fatal("buildRunner() error = nil, want a tool-registry error from the duplicate \"read\" tool")
	}
	if spy.lastBinding == nil {
		t.Fatal("memory.Service.Bind was never called")
	}
	if !spy.lastBinding.closed {
		t.Fatal("bound memory.Binding leaked: not closed after tool registry construction failed")
	}
}

func TestRuntimeBuilderBuildNewReplacementResolvesCurrentSessionRuntimeTransactionally(t *testing.T) {
	contextWindow := 65_536
	compactionWindow := 49_152
	reserve := 9_000
	keep := 12_000
	file := configWithProfiles("startup", "resumed")
	profile := file.Profiles["resumed"]
	profile.ContextWindow = &contextWindow
	profile.CompactionWindow = &compactionWindow
	file.Profiles["resumed"] = profile
	file.Agent.Compaction.ReserveTokens = &reserve
	file.Agent.Compaction.KeepRecentTokens = &keep
	builder := newRuntimeBuilderForTest(t, file)
	builder.noSession = false

	var createdRuntime config.Runtime
	var runnerRuntime config.Runtime
	builder.deps.newSession = func(_ bool, _ string, workspace string, runtime config.Runtime) (session.Session, error) {
		createdRuntime = runtime
		return session.NewMemory(session.Header{
			Version: session.CurrentVersion, ID: "fresh", Workspace: workspace,
			Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model, CreatedAt: time.Now().UTC(),
		}), nil
	}
	builder.buildRunnerOverride = func(_ session.Session, runtime config.Runtime) (app.Runner, error) {
		runnerRuntime = runtime
		return commandRunnerFunc(func(context.Context, string, func(agent.Event)) error { return nil }), nil
	}

	current := app.RuntimeInfo{Provider: "openai-compatible", Profile: "resumed", Model: "gpt-5.3-codex-spark", ContextWindow: 1}
	replacement, err := builder.buildNewReplacement(context.Background(), current)
	if err != nil {
		t.Fatal(err)
	}
	defer replacement.Session.Close()
	if replacement.RuntimeInfo.Provider != current.Provider || replacement.RuntimeInfo.Profile != current.Profile || replacement.RuntimeInfo.Model != current.Model {
		t.Fatalf("runtime info = %#v, want current identity %#v", replacement.RuntimeInfo, current)
	}
	if replacement.RuntimeInfo.ContextWindow != contextWindow {
		t.Fatalf("runtime info context window = %d, want %d", replacement.RuntimeInfo.ContextWindow, contextWindow)
	}
	if !reflect.DeepEqual(createdRuntime, runnerRuntime) {
		t.Fatalf("create runtime = %#v, runner runtime = %#v", redactedRuntime(createdRuntime), redactedRuntime(runnerRuntime))
	}
	if createdRuntime.Profile != "resumed" || createdRuntime.Model != "gpt-5.3-codex-spark" || createdRuntime.APIKey != "resumed-secret" {
		t.Fatalf("resolved runtime = %#v", redactedRuntime(createdRuntime))
	}
	wantCompaction := config.CompactionRuntime{
		Auto: true, ContextWindow: contextWindow, HardInputWindow: contextWindow, WorkingWindow: compactionWindow,
		MaxOutputTokens: 32_000, ReserveTokens: reserve, KeepRecentTokens: keep,
	}
	if createdRuntime.Compaction != wantCompaction {
		t.Fatalf("compaction runtime = %#v, want %#v", createdRuntime.Compaction, wantCompaction)
	}
	if header := replacement.Session.Header(); header.Profile != "resumed" || header.Model != "gpt-5.3-codex-spark" {
		t.Fatalf("new session header = %#v", header)
	}
}

func TestRuntimeBuilderBuildProfileReplacementSwitchesToNamedProfile(t *testing.T) {
	file := configWithProfiles("startup")
	file.Profiles["chatgpt"] = config.Profile{Provider: "chatgpt", Model: "gpt-5"}
	builder := newRuntimeBuilderForTest(t, file)
	builder.noSession = false

	var createdRuntime config.Runtime
	builder.deps.newSession = func(_ bool, _ string, workspace string, runtime config.Runtime) (session.Session, error) {
		createdRuntime = runtime
		return session.NewMemory(session.Header{
			Version: session.CurrentVersion, ID: "fresh", Workspace: workspace,
			Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model, CreatedAt: time.Now().UTC(),
		}), nil
	}
	builder.buildRunnerOverride = func(session.Session, config.Runtime) (app.Runner, error) {
		return commandRunnerFunc(func(context.Context, string, func(agent.Event)) error { return nil }), nil
	}

	replacement, err := builder.buildProfileReplacement(context.Background(), "chatgpt")
	if err != nil {
		t.Fatal(err)
	}
	defer replacement.Session.Close()
	if replacement.RuntimeInfo.Provider != "chatgpt" || replacement.RuntimeInfo.Profile != "chatgpt" || replacement.RuntimeInfo.Model != "gpt-5" {
		t.Fatalf("runtime info = %#v", replacement.RuntimeInfo)
	}
	if createdRuntime.Provider != "chatgpt" || createdRuntime.Model != "gpt-5" {
		t.Fatalf("resolved runtime = %#v", redactedRuntime(createdRuntime))
	}
	if header := replacement.Session.Header(); header.Provider != "chatgpt" || header.Profile != "chatgpt" || header.Model != "gpt-5" {
		t.Fatalf("session header = %#v", header)
	}
}

func TestRuntimeBuilderBuildProfileReplacementDoesNotCarryStartupEndpointOverride(t *testing.T) {
	file := configWithProfiles("startup", "named")
	builder := newRuntimeBuilderForTest(t, file)
	builder.runtimeOverrides = config.Overrides{
		Provider:       "openai-compatible",
		BaseURL:        "https://cli-override.example/v1",
		Model:          "cli-model",
		Thinking:       "high",
		ShellTimeout:   45 * time.Second,
		MaxOutputBytes: 1234,
	}
	var createdRuntime config.Runtime
	builder.deps.newSession = func(_ bool, _ string, workspace string, runtime config.Runtime) (session.Session, error) {
		createdRuntime = runtime
		return session.NewMemory(session.Header{
			Version: session.CurrentVersion, ID: "fresh", Workspace: workspace,
			Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model, CreatedAt: time.Now().UTC(),
		}), nil
	}
	builder.buildRunnerOverride = func(session.Session, config.Runtime) (app.Runner, error) {
		return commandRunnerFunc(func(context.Context, string, func(agent.Event)) error { return nil }), nil
	}

	replacement, err := builder.buildProfileReplacement(context.Background(), "named")
	if err != nil {
		t.Fatal(err)
	}
	defer replacement.Session.Close()
	if createdRuntime.Profile != "named" || createdRuntime.BaseURL != "https://named.example/v1" || createdRuntime.Model != "named-profile-model" {
		t.Fatalf("resolved runtime = %#v", redactedRuntime(createdRuntime))
	}
	if createdRuntime.Thinking != "high" || createdRuntime.ShellTimeout != 45*time.Second || createdRuntime.MaxOutputBytes != 1234 {
		t.Fatalf("neutral overrides were not preserved: %#v", redactedRuntime(createdRuntime))
	}
	if replacement.RuntimeInfo.Profile != "named" || replacement.RuntimeInfo.Model != "named-profile-model" {
		t.Fatalf("runtime info = %#v", replacement.RuntimeInfo)
	}
}

func TestRuntimeBuilderBuildProfileReplacementUnknownProfileErrors(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("startup"))
	builder.buildRunnerOverride = func(session.Session, config.Runtime) (app.Runner, error) {
		t.Fatal("runner must not build for an unknown profile")
		return nil, nil
	}
	_, err := builder.buildProfileReplacement(context.Background(), "missing")
	if err == nil || !strings.Contains(err.Error(), `profile "missing" not found`) {
		t.Fatalf("error = %v, want profile not found", err)
	}
}

func TestRuntimeBuilderProfileNamesSorted(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("beta", "alpha", "chatgpt"))
	names := builder.profileNames()
	want := []string{"alpha", "beta", "chatgpt"}
	if !reflect.DeepEqual(names, want) {
		t.Fatalf("profileNames() = %#v, want %#v", names, want)
	}
}

func TestRuntimeBuilderCollectSecretValuesIncludesCapturedOAuthCredentials(t *testing.T) {
	file := configWithProfiles("default")
	values := collectSecretValuesWithAuth(file, environmentForProfiles(file), nil, &auth.Credentials{
		AccessToken:  "oauth-access",
		RefreshToken: "oauth-refresh",
		IDToken:      "oauth-id",
		AccountID:    "oauth-account",
	})
	for _, want := range []string{"oauth-access", "oauth-refresh", "oauth-id", "oauth-account"} {
		found := false
		for _, got := range values {
			if got == want {
				found = true
				break
			}
		}
		if !found {
			t.Fatalf("collectSecretValuesWithAuth() missing %q in %#v", want, values)
		}
	}
}

func TestRuntimeBuilderBuildProviderUsesCapturedCredentialSnapshot(t *testing.T) {
	home := t.TempDir()
	path := auth.PathForHome(home)
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(`{"access_token":"mutated-token"`), 0o600); err != nil {
		t.Fatal(err)
	}
	builder := newRuntimeBuilderForTest(t, config.File{})
	builder.authPath = path
	builder.authCredentials = auth.Credentials{
		AccessToken: "captured-token", RefreshToken: "captured-refresh", AccountID: "captured-account", Expiry: time.Now().Add(time.Hour),
	}
	builder.authCredentialsLoaded = true
	providerClient, err := builder.buildProvider(context.Background(), config.Runtime{Provider: "chatgpt"})
	if err != nil {
		t.Fatal(err)
	}
	if providerClient == nil {
		t.Fatal("buildProvider() returned nil provider")
	}
}

func TestRuntimeBuilderBuildProviderTracesChatGPTHTTPClient(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, config.File{})
	builder.authPath = filepath.Join(t.TempDir(), "auth.json")
	builder.authCredentials = auth.Credentials{
		AccessToken: "captured-token", RefreshToken: "captured-refresh", AccountID: "captured-account", Expiry: time.Now().Add(time.Hour),
	}
	builder.authCredentialsLoaded = true
	builder.traceWriter = &bytes.Buffer{}

	providerClient, err := builder.buildProvider(context.Background(), config.Runtime{Provider: "chatgpt"})
	if err != nil {
		t.Fatal(err)
	}

	clientValue := reflect.ValueOf(providerClient).Elem().FieldByName("httpClient")
	if clientValue.IsNil() {
		t.Fatal("chatgpt provider httpClient is nil")
	}
	transportValue := clientValue.Elem().FieldByName("Transport")
	if transportValue.IsNil() {
		t.Fatal("chatgpt provider transport is nil")
	}
	if got := transportValue.Elem().Type().String(); got != "*trace.RoundTripper" {
		t.Fatalf("chatgpt provider transport type = %s, want *trace.RoundTripper", got)
	}
}

func TestRuntimeBuilderBuildProviderRequiresInjectedAuthPath(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, config.File{})
	builder.environment = map[string]string{"HOME": t.TempDir()}
	builder.authCredentials = auth.Credentials{
		AccessToken: "captured-token", RefreshToken: "captured-refresh", AccountID: "captured-account", Expiry: time.Now().Add(time.Hour),
	}
	builder.authCredentialsLoaded = true
	_, err := builder.buildProvider(context.Background(), config.Runtime{Provider: "chatgpt"})
	if !errors.Is(err, auth.ErrCredentialsUnavailable) {
		t.Fatalf("buildProvider() error = %v, want ErrCredentialsUnavailable", err)
	}
}

func TestRuntimeBuilderOpenReplacementProvenanceFailureClosesCandidateAndRunner(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	base := &trackedReplacementSession{
		Session: session.NewMemory(session.Header{
			Version: session.CurrentVersion, ID: "candidate", Workspace: builder.workspacePath,
			Provider: "openai-compatible", Profile: "default", Model: "stale-model", CreatedAt: time.Now().UTC(),
		}),
		closed: make(chan struct{}),
	}
	candidate := &flippingHeaderSession{
		trackedReplacementSession: base,
		first:                     session.Header{Version: session.CurrentVersion, ID: "candidate", Workspace: builder.workspacePath, Provider: "openai-compatible", Profile: "default", Model: "default-profile-model", CreatedAt: time.Now().UTC()},
		second:                    session.Header{Version: session.CurrentVersion, ID: "candidate", Workspace: builder.workspacePath, Provider: "openai-compatible", Profile: "default", Model: "stale-model", CreatedAt: time.Now().UTC()},
	}
	builder.prepareSession = func(context.Context, string, string) (preparedSession, error) {
		return &fakePreparedSession{
			info: session.SessionInfo{
				Path: "/sessions/candidate.jsonl", ID: "candidate", CWD: builder.workspacePath,
				Profile: "default", Provider: "openai-compatible", Model: "default-profile-model",
			},
			activate: func(context.Context) (session.Session, []session.Warning, error) {
				return candidate, nil, nil
			},
		}, nil
	}
	runner := &closableRunner{}
	builder.buildRunnerOverride = func(session.Session, config.Runtime) (app.Runner, error) {
		return runner, nil
	}

	_, err := builder.openReplacement(context.Background(), "/sessions/candidate.jsonl")
	if err == nil || !strings.Contains(err.Error(), "runtime provenance updates") {
		t.Fatalf("openReplacement() error = %v, want provenance update error", err)
	}
	if base.closeCalls.Load() != 1 || runner.closeCalls.Load() != 1 {
		t.Fatalf("cleanup close calls = session %d runner %d, want 1 each", base.closeCalls.Load(), runner.closeCalls.Load())
	}
}

func TestRuntimeBuilderBuildNewReplacementLeavesUnknownPrivateModelWindowUnset(t *testing.T) {
	file := configWithProfiles("private")
	file.Profiles["private"] = config.Profile{
		Provider:  "openai-compatible",
		BaseURL:   "https://private.example/v1",
		Model:     "private-model",
		APIKeyEnv: "PRIVATE_KEY",
	}
	builder := newRuntimeBuilderForTest(t, file)
	builder.buildRunnerOverride = func(session.Session, config.Runtime) (app.Runner, error) {
		return commandRunnerFunc(func(context.Context, string, func(agent.Event)) error { return nil }), nil
	}

	replacement, err := builder.buildNewReplacement(context.Background(), app.RuntimeInfo{Provider: "openai-compatible", Profile: "private", Model: "private-model", ContextWindow: 99})
	if err != nil {
		t.Fatal(err)
	}
	defer replacement.Session.Close()
	if replacement.RuntimeInfo.ContextWindow != 0 {
		t.Fatalf("runtime info context window = %d, want 0 for unknown private model", replacement.RuntimeInfo.ContextWindow)
	}
}

func TestRuntimeBuilderBuildNewReplacementCreationErrorClosesReturnedCandidate(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	candidate := &trackedReplacementSession{
		Session: session.NewMemory(session.Header{
			Version: session.CurrentVersion, ID: "candidate", Workspace: builder.workspacePath,
			Provider: "openai-compatible", Profile: "default", Model: "gpt-4.1", CreatedAt: time.Now().UTC(),
		}),
		closed: make(chan struct{}),
	}
	createErr := errors.New("create failed after returning candidate")
	builder.deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) {
		return candidate, createErr
	}

	_, err := builder.buildNewReplacement(context.Background(), app.RuntimeInfo{
		Provider: "openai-compatible", Profile: "default", Model: "gpt-4.1",
	})
	if !errors.Is(err, createErr) {
		t.Fatalf("buildNewReplacement() error = %v, want creation error", err)
	}
	if candidate.closeCalls.Load() != 1 {
		t.Fatalf("candidate close calls = %d, want 1", candidate.closeCalls.Load())
	}
}

func TestRuntimeBuilderBuildNewReplacementCleansCanceledCandidate(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	candidate := &trackedReplacementSession{
		Session: session.NewMemory(session.Header{
			Version: session.CurrentVersion, ID: "candidate", Workspace: builder.workspacePath,
			Provider: "openai-compatible", Profile: "default", Model: "gpt-4.1", CreatedAt: time.Now().UTC(),
		}),
		closed: make(chan struct{}),
	}
	ctx, cancel := context.WithCancel(context.Background())
	builder.deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) {
		cancel()
		return candidate, nil
	}
	var runnerCalls atomic.Int32
	builder.buildRunnerOverride = func(session.Session, config.Runtime) (app.Runner, error) {
		runnerCalls.Add(1)
		return commandRunnerFunc(func(context.Context, string, func(agent.Event)) error { return nil }), nil
	}

	_, err := builder.buildNewReplacement(ctx, app.RuntimeInfo{
		Provider: "openai-compatible", Profile: "default", Model: "gpt-4.1",
	})
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("buildNewReplacement() error = %v, want context.Canceled", err)
	}
	if runnerCalls.Load() != 0 {
		t.Fatalf("runner build calls = %d, want 0", runnerCalls.Load())
	}
	if candidate.closeCalls.Load() != 1 {
		t.Fatalf("candidate close calls = %d, want 1", candidate.closeCalls.Load())
	}
}

func TestRuntimeBuilderBuildNewReplacementRejectsNilSessionAndRunner(t *testing.T) {
	t.Run("nil session", func(t *testing.T) {
		builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
		builder.deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) {
			return nil, nil
		}
		_, err := builder.buildNewReplacement(context.Background(), app.RuntimeInfo{
			Provider: "openai-compatible", Profile: "default", Model: "gpt-4.1",
		})
		if err == nil || err.Error() != "session factory returned nil session" {
			t.Fatalf("buildNewReplacement() error = %v, want nil session error", err)
		}
	})

	t.Run("nil runner", func(t *testing.T) {
		builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
		candidate := &trackedReplacementSession{
			Session: session.NewMemory(session.Header{
				Version: session.CurrentVersion, ID: "candidate", Workspace: builder.workspacePath,
				Provider: "openai-compatible", Profile: "default", Model: "gpt-4.1", CreatedAt: time.Now().UTC(),
			}),
			closed: make(chan struct{}),
		}
		builder.deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) {
			return candidate, nil
		}
		builder.buildRunnerOverride = func(session.Session, config.Runtime) (app.Runner, error) { return nil, nil }

		_, err := builder.buildNewReplacement(context.Background(), app.RuntimeInfo{
			Provider: "openai-compatible", Profile: "default", Model: "gpt-4.1",
		})
		if err == nil || err.Error() != "runner factory returned nil runner" {
			t.Fatalf("buildNewReplacement() error = %v, want nil runner error", err)
		}
		if candidate.closeCalls.Load() != 1 {
			t.Fatalf("candidate close calls = %d, want 1", candidate.closeCalls.Load())
		}
	})
}

func TestRuntimeBuilderBuildNewReplacementProvenanceFailureClosesCandidate(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	candidate := &trackedReplacementSession{
		Session: session.NewMemory(session.Header{
			Version: session.CurrentVersion, ID: "candidate", Workspace: builder.workspacePath,
			Provider: "openai-compatible", Profile: "default", Model: "stale-model", CreatedAt: time.Now().UTC(),
		}),
		closed: make(chan struct{}),
	}
	runner := &closableRunner{}
	builder.deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) { return candidate, nil }
	builder.buildRunnerOverride = func(session.Session, config.Runtime) (app.Runner, error) {
		return runner, nil
	}

	_, err := builder.buildNewReplacement(context.Background(), app.RuntimeInfo{
		Provider: "openai-compatible", Profile: "default", Model: "gpt-4.1",
	})
	if err == nil || !strings.Contains(err.Error(), "runtime provenance updates") {
		t.Fatalf("buildNewReplacement() error = %v, want provenance update error", err)
	}
	if candidate.closeCalls.Load() != 1 || runner.closeCalls.Load() != 1 {
		t.Fatalf("cleanup close calls = session %d runner %d, want 1 each", candidate.closeCalls.Load(), runner.closeCalls.Load())
	}
}

func TestRuntimeBuilderBoundaryRedactionCannotSynthesizeConfiguredSecret(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	builder.sandboxSecrets = []string{"TOKEN", "[REDACTED]"}

	err := builder.redactError(errors.New("boundary TOKEN"), nil)
	for _, forbidden := range builder.sandboxSecrets {
		if strings.Contains(err.Error(), forbidden) {
			t.Fatalf("redactError() synthesized or exposed %q: %v", forbidden, err)
		}
	}
}

func TestRuntimeBuilderCleanupCandidateRedactsJoinedCancellationErrors(t *testing.T) {
	const (
		apiKey      = "joined-cancel-api-key-secret"
		urlSecret   = "joined-cancel-url-secret"
		urlPassword = "joined-cancel-url-password"
	)
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	runtime := config.Runtime{
		APIKey:  apiKey,
		BaseURL: "https://user:" + urlPassword + "@example.test/v1?tenant=" + urlSecret,
	}

	for _, cause := range []error{context.Canceled, context.DeadlineExceeded} {
		t.Run(cause.Error(), func(t *testing.T) {
			closeErr := fmt.Errorf("candidate close exposed %s %s %s", apiKey, urlSecret, urlPassword)
			candidate := &trackedReplacementSession{
				Session:  session.NewMemory(session.Header{Version: session.CurrentVersion, ID: "candidate", CreatedAt: time.Now().UTC()}),
				closed:   make(chan struct{}),
				closeErr: closeErr,
			}

			err := builder.cleanupCandidate(candidate, cause, &runtime)
			if !errors.Is(err, cause) || !errors.Is(err, closeErr) {
				t.Fatalf("cleanupCandidate() error identities = %v, want %v and close error", err, cause)
			}
			for _, secret := range []string{apiKey, urlSecret, urlPassword} {
				if strings.Contains(err.Error(), secret) {
					t.Fatalf("cleanupCandidate() error leaked %q: %v", secret, err)
				}
			}
			if _, ok := err.(interface{ Unwrap() error }); ok {
				t.Fatalf("redacted cancellation error exposes its cause through Unwrap: %T", err)
			}
			if _, ok := err.(interface{ Unwrap() []error }); ok {
				t.Fatalf("redacted cancellation error exposes joined causes through Unwrap: %T", err)
			}
			if candidate.closeCalls.Load() != 1 {
				t.Fatalf("candidate close calls = %d, want 1", candidate.closeCalls.Load())
			}
		})
	}
	if got := builder.redactError(context.Canceled, &runtime); got != context.Canceled {
		t.Fatalf("plain cancellation = %#v, want original sentinel", got)
	}
}

func TestRuntimeBuilderBuildNewReplacementFailureClosesCandidateAndRedacts(t *testing.T) {
	const secret = "new-replacement-secret"
	file := configWithProfiles("default")
	profile := file.Profiles["default"]
	profile.APIKeyEnv = "NEW_REPLACEMENT_KEY"
	file.Profiles["default"] = profile
	builder := newRuntimeBuilderForTest(t, file)
	builder.environment[profile.APIKeyEnv] = secret
	candidate := &trackedReplacementSession{
		Session: session.NewMemory(session.Header{
			Version: session.CurrentVersion, ID: "candidate", Workspace: builder.workspacePath,
			Provider: "openai-compatible", Profile: "default", Model: "gpt-4.1", CreatedAt: time.Now().UTC(),
		}),
		closed: make(chan struct{}),
	}
	builder.deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) { return candidate, nil }
	builder.buildRunnerOverride = func(session.Session, config.Runtime) (app.Runner, error) {
		return nil, fmt.Errorf("runner failed with %s", secret)
	}

	_, err := builder.buildNewReplacement(context.Background(), app.RuntimeInfo{
		Provider: "openai-compatible", Profile: "default", Model: "gpt-4.1",
	})
	if err == nil || !strings.Contains(err.Error(), "runner failed") || strings.Contains(err.Error(), secret) {
		t.Fatalf("buildNewReplacement() error = %v", err)
	}
	if candidate.closeCalls.Load() != 1 {
		t.Fatalf("candidate close calls = %d, want 1", candidate.closeCalls.Load())
	}
}

func TestRuntimeBuilderBuildRunnerRemovesAndRedactsEveryProfileCredential(t *testing.T) {
	const (
		activeEnv   = "OTTO_RUNTIME_BUILDER_ACTIVE_KEY"
		inactiveEnv = "OTTO_RUNTIME_BUILDER_INACTIVE_KEY"
		activeKey   = "credential-alpha-329847"
		inactiveKey = "credential-bravo-761205"
		fallbackKey = "credential-charlie-458913"
	)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if got := r.Header.Get("Authorization"); got != "Bearer "+activeKey {
			t.Errorf("Authorization = %q, want active profile credential", got)
		}
		var payload struct {
			Messages []struct {
				Role    string `json:"role"`
				Content string `json:"content"`
			} `json:"messages"`
		}
		_ = json.NewDecoder(r.Body).Decode(&payload)
		if len(payload.Messages) > 2 {
			writeSSE(w, `{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`)
			return
		}
		command := fmt.Sprintf("printf 'active=%%s inactive=%%s fallback=%%s' \"$%s\" \"$%s\" \"$OTTO_API_KEY\"; printf ' inactive-stderr=%%s' \"$%s\" >&2; exit 7", activeEnv, inactiveEnv, inactiveEnv)
		writeSSE(w, fmt.Sprintf(`{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"bash","arguments":%q}}]},"finish_reason":"tool_calls"}]}`,
			fmt.Sprintf(`{"command":%q}`, command)))
	}))
	defer server.Close()

	file := config.File{
		DefaultProfile: "active",
		Profiles: map[string]config.Profile{
			"active":   {Provider: "openai-compatible", BaseURL: server.URL, Model: "active-model", APIKeyEnv: activeEnv},
			"inactive": {Provider: "openai-compatible", BaseURL: "https://inactive.example/v1", Model: "inactive-model", APIKeyEnv: inactiveEnv},
		},
	}
	builder := newRuntimeBuilderForTest(t, file)
	builder.environment = map[string]string{activeEnv: activeKey, inactiveEnv: inactiveKey, "OTTO_API_KEY": fallbackKey}
	store, err := session.Create(builder.sessionRoot, session.Header{
		Version: session.CurrentVersion, ID: "all-profile-credentials", Workspace: builder.workspacePath,
		Provider: "openai-compatible", Profile: "active", Model: "active-model", CreatedAt: time.Now().UTC(),
	})
	if err != nil {
		t.Fatal(err)
	}
	path := store.Path()

	runner, err := builder.buildRunner(context.Background(), store, config.Runtime{
		Profile: "active", Provider: "openai-compatible", BaseURL: server.URL, Model: "active-model",
		APIKey: activeKey, APIKeyEnv: activeEnv, ShellTimeout: time.Second, MaxOutputBytes: 64 << 10,
	})
	if err != nil {
		_ = store.Close()
		t.Fatal(err)
	}
	var eventText strings.Builder
	runErr := runner.Run(context.Background(), "check every profile credential", func(event agent.Event) {
		if event.Err != nil {
			eventText.WriteString(event.Err.Error())
		}
		eventText.WriteString(event.ToolResult.Content)
	})
	if runErr != nil {
		_ = store.Close()
		t.Fatalf("Run() error = %v, want tool-turn success", runErr)
	}
	messages := store.Messages()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	persisted, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}

	locations := map[string]string{
		"events":   eventText.String(),
		"messages": fmt.Sprintf("%#v", messages),
		"session":  string(persisted),
	}
	for location, content := range locations {
		for _, secret := range []string{activeKey, inactiveKey, fallbackKey} {
			if strings.Contains(content, secret) {
				t.Fatalf("%s leaked %q: %s", location, secret, content)
			}
		}
	}
	var toolResult string
	for _, message := range messages {
		for _, block := range message.Blocks {
			if block.Type == model.BlockToolResult {
				toolResult = block.Text
			}
		}
	}
	if toolResult == "" {
		t.Fatalf("no tool result in messages: %#v", messages)
	}
	if strings.Contains(toolResult, activeEnv+"=") || strings.Contains(toolResult, inactiveEnv+"=") || strings.Contains(toolResult, "OTTO_API_KEY=") {
		t.Fatalf("credential environment reached bash: %q", toolResult)
	}
	if !strings.Contains(toolResult, "inactive-stderr=") || !strings.Contains(toolResult, "exit_code: 7") {
		t.Fatalf("tool result = %q", toolResult)
	}
}

func TestRuntimeBuilderFailsClosedForInactiveMalformedEndpointAcrossBoundaries(t *testing.T) {
	const (
		encodedEndpoint   = "https://raw%2Buser:raw%2Fpass@example.test/v1?token=raw+query&encoded=percent%2Fvalue#frag%2Bvalue"
		malformedEndpoint = "https://bad%zz:pass%2Fword@example.test/v1?token=plus+value&broken=bad%zz#raw%2Gfragment"
	)
	secretForms := []string{
		encodedEndpoint,
		"raw%2Buser:raw%2Fpass",
		"raw%2Buser",
		"raw%2Fpass",
		"raw+user:raw/pass",
		"raw+user",
		"raw/pass",
		"raw+query",
		"raw query",
		"percent%2Fvalue",
		"percent/value",
		"frag%2Bvalue",
		"frag+value",
		malformedEndpoint,
		"bad%zz:pass%2Fword",
		"bad%zz",
		"pass%2Fword",
		"pass/word",
		"plus+value",
		"plus value",
		"raw%2Gfragment",
	}

	var requestMu sync.Mutex
	var requestBodies []string
	var requests atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(r.Body)
		if err != nil {
			t.Errorf("read request: %v", err)
		}
		requestMu.Lock()
		requestBodies = append(requestBodies, string(body))
		requestMu.Unlock()
		if requests.Add(1) == 1 {
			payload := fmt.Sprintf(`{"choices":[{"delta":{"content":%q,"tool_calls":[{"index":0,"id":"call-bash","type":"function","function":{"name":"bash","arguments":%q}}]},"finish_reason":"tool_calls"}]}`,
				"provider event "+strings.Join(secretForms, " | "), `{"command":"ignored"}`)
			writeSSE(w, payload)
			return
		}
		writeSSE(w, `{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`)
	}))
	defer server.Close()

	file := config.File{
		DefaultProfile: "active",
		Profiles: map[string]config.Profile{
			"active": {
				Provider: "openai-compatible", BaseURL: server.URL, Model: "active-model", APIKeyEnv: "ACTIVE_KEY",
			},
			"encoded-inactive": {
				Provider: "openai-compatible", BaseURL: encodedEndpoint, Model: "inactive-model", APIKeyEnv: "ENCODED_KEY",
			},
			"malformed-inactive": {
				Provider: "openai-compatible", BaseURL: malformedEndpoint, Model: "inactive-model", APIKeyEnv: "MALFORMED_KEY",
			},
		},
	}
	builder := newRuntimeBuilderForTest(t, file)
	builder.commandExecutor = &recordingSandboxExecutor{execute: func(_ context.Context, _ sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
		_, err := io.WriteString(streams.Stdout, strings.Join(secretForms, "\n"))
		return sandbox.ExitStatus{Code: 0}, err
	}}
	builder.sandboxEnvironment = []string{}
	store, err := session.Create(builder.sessionRoot, session.Header{
		Version: session.CurrentVersion, ID: "endpoint-redaction", Workspace: builder.workspacePath,
		Provider: "openai-compatible", Profile: "active", Model: "active-model", CreatedAt: time.Now().UTC(),
	})
	if err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	runner, err := builder.buildRunner(context.Background(), store, config.Runtime{
		Profile: "active", Provider: "openai-compatible", BaseURL: server.URL, Model: "active-model",
		APIKey: "active-key", APIKeyEnv: "ACTIVE_KEY", ShellTimeout: time.Second, MaxOutputBytes: 64 << 10,
	})
	if err != nil {
		_ = store.Close()
		t.Fatal(err)
	}
	var events []agent.Event
	if err := runner.Run(context.Background(), "inspect endpoints", func(event agent.Event) { events = append(events, event) }); err != nil {
		_ = store.Close()
		t.Fatal(err)
	}
	messages := store.Messages()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	persisted, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	requestMu.Lock()
	bodies := append([]string(nil), requestBodies...)
	requestMu.Unlock()
	if len(bodies) != 0 {
		t.Fatalf("incomplete endpoint boundary made %d provider requests", len(bodies))
	}
	if len(messages) != 0 {
		t.Fatalf("incomplete endpoint boundary mutated session: %#v", messages)
	}
	eventJSON, err := json.Marshal(events)
	if err != nil {
		t.Fatal(err)
	}
	boundaryErr := builder.redactError(fmt.Errorf("boundary: %s", strings.Join(secretForms, " | ")), nil)
	boundaries := map[string]string{
		"events":         string(eventJSON),
		"Pi JSONL":       string(persisted),
		"boundary error": boundaryErr.Error(),
	}
	for boundary, content := range boundaries {
		for _, secret := range secretForms {
			if strings.Contains(content, secret) {
				t.Fatalf("%s leaked inactive endpoint form %q", boundary, secret)
			}
		}
	}
}

func TestRuntimeBuilderMarkerExhaustionDisablesDynamicRuntimeAndBash(t *testing.T) {
	secret := markerExhaustingProviderSecret(t)
	var requests atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		requests.Add(1)
		writeSSE(w, `{"choices":[{"delta":{"content":"dynamic"},"finish_reason":"stop"}]}`)
	}))
	defer server.Close()

	file := config.File{
		DefaultProfile: "active",
		Profiles: map[string]config.Profile{
			"active": {
				Provider: "openai-compatible", BaseURL: server.URL, Model: "active", APIKeyEnv: "ACTIVE_KEY",
			},
			"inactive": {
				Provider: "openai-compatible", BaseURL: "https://inactive.example/v1", Model: "inactive", APIKeyEnv: "INACTIVE_KEY",
			},
		},
	}
	builder := newRuntimeBuilderForTest(t, file)
	builder.environment["ACTIVE_KEY"] = "active-key"
	builder.environment["INACTIVE_KEY"] = secret
	executor := &recordingSandboxExecutor{}
	builder.commandExecutor = executor
	builder.sandboxEnvironment = []string{}
	runtime := config.Runtime{
		Profile: "active", Provider: "openai-compatible", BaseURL: server.URL, Model: "active",
		APIKey: "active-key", APIKeyEnv: "ACTIVE_KEY", ShellTimeout: time.Second, MaxOutputBytes: 4096,
		Compaction: config.CompactionRuntime{ContextWindow: 128_000},
	}
	if builder.bashConfigured() {
		t.Fatal("marker exhaustion left Bash configured")
	}
	if got := builder.effectiveSandboxInfo(); got.Mode != app.SandboxUnavailable || got.BashAvailable {
		t.Fatalf("sandbox info = %#v, want unavailable Bash", got)
	}
	if got := builder.runtimeInfo(runtime); got.Provider != "" || got.Profile != "" || got.Model != "" || got.ContextWindow != 0 {
		t.Fatalf("runtime info exposed dynamic metadata: %#v", got)
	}

	store, err := session.Create(builder.sessionRoot, session.Header{
		Version: session.CurrentVersion, ID: "marker-exhaustion", Workspace: builder.workspacePath,
		Provider: "openai-compatible", Profile: "active", Model: "active", CreatedAt: time.Now().UTC(),
	})
	if err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	runner, err := builder.buildRunner(context.Background(), store, runtime)
	if err != nil {
		_ = store.Close()
		t.Fatalf("buildRunner() rejected marker adversary: %v", err)
	}
	var events []agent.Event
	if err := runner.Run(context.Background(), "run safely", func(event agent.Event) { events = append(events, event) }); err != nil {
		_ = store.Close()
		t.Fatal(err)
	}
	messages := store.Messages()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	persisted, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if requests.Load() != 0 || executor.calls.Load() != 0 || len(messages) != 0 {
		t.Fatalf("suppressed runtime crossed dynamic boundary: provider=%d Bash=%d messages=%#v", requests.Load(), executor.calls.Load(), messages)
	}
	if len(events) != 2 || events[0].Type != agent.EventAgentStarted || events[1].Type != agent.EventAgentFinished {
		t.Fatalf("events = %#v, want fixed lifecycle", events)
	}
	if strings.Contains(string(persisted), secret) {
		t.Fatal("Pi JSONL leaked marker-exhausting provider secret")
	}
}

func TestRuntimeBuilderBuildRunnerEnforcesShellTimeoutOutputLimitAndRedaction(t *testing.T) {
	const (
		apiKey          = "runtime-secret"
		fallbackAPIKey  = "fallback-secret"
		apiKeyEnv       = "RUNTIME_KEY"
		unrelatedEnvKey = "OTTO_RUNTIME_BUILDER_UNRELATED"
		shellTimeout    = time.Millisecond
	)
	var requests atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		if requests.Add(1) != 1 {
			writeSSE(w, `{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`)
			return
		}
		writeSSE(w, `{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"ignored\"}"}}]},"finish_reason":"tool_calls"}]}`)
	}))
	defer server.Close()

	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	executionStarted := make(chan struct{})
	builder.commandExecutor = &recordingSandboxExecutor{execute: func(ctx context.Context, request sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
		if !reflect.DeepEqual(request.Env, []string{"PATH=/usr/bin:/bin", unrelatedEnvKey + "=keep-me"}) {
			t.Errorf("Executor environment = %#v", request.Env)
		}
		output := "runtime=missing fallback=missing unrelated=keep-me literal=" + apiKey + " " + strings.Repeat("0", 200)
		if _, err := io.WriteString(streams.Stdout, output); err != nil {
			return sandbox.ExitStatus{}, err
		}
		close(executionStarted)
		<-ctx.Done()
		return sandbox.ExitStatus{Code: -1, Signaled: true, Signal: "killed"}, context.Canceled
	}}
	builder.sandboxEnvironment = []string{"PATH=/usr/bin:/bin", unrelatedEnvKey + "=keep-me"}
	memory := session.NewMemory(session.Header{Version: session.CurrentVersion, ID: "runtime-builder", Workspace: builder.workspacePath, Provider: "openai-compatible", Model: "runtime-model", CreatedAt: time.Now().UTC()})
	runner, err := builder.buildRunner(context.Background(), memory, config.Runtime{
		Profile:        "default",
		Provider:       "openai-compatible",
		BaseURL:        server.URL,
		Model:          "runtime-model",
		APIKey:         apiKey,
		APIKeyEnv:      apiKeyEnv,
		ShellTimeout:   shellTimeout,
		MaxOutputBytes: 96,
	})
	if err != nil {
		t.Fatal(err)
	}

	runDone := make(chan error, 1)
	go func() { runDone <- runner.Run(context.Background(), "run the tool", nil) }()
	select {
	case <-executionStarted:
	case <-time.After(2 * time.Second):
		t.Fatal("fake Executor did not reach the timeout barrier")
	}
	select {
	case err := <-runDone:
		if err != nil {
			t.Fatalf("Run() error = %v, want tool-turn success", err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("timed out waiting for deterministic fake Executor cancellation")
	}
	if requests.Load() != 2 {
		t.Fatalf("provider requests = %d, want 2 (tool turn then final stop)", requests.Load())
	}
	messages := memory.Messages()
	if len(messages) != 4 {
		t.Fatalf("messages = %#v, want user+assistant+tool+assistant", messages)
	}
	toolResult := messages[2].Blocks[0].Text
	if !strings.Contains(toolResult, "runtime=missing") || !strings.Contains(toolResult, "fallback=missing") || !strings.Contains(toolResult, "unrelated=keep-me") || !strings.Contains(toolResult, "literal=") || strings.Contains(toolResult, "literal="+apiKey) || !strings.Contains(toolResult, "[truncated:") || !strings.Contains(toolResult, "status: timed out after 1ms") {
		t.Fatalf("tool result = %q", toolResult)
	}
	for _, forbidden := range []string{apiKey, fallbackAPIKey} {
		if strings.Contains(toolResult, forbidden) {
			t.Fatalf("tool result leaked %q: %q", forbidden, toolResult)
		}
	}
}

func TestRuntimeBuilderUnavailableSandboxRegistersExactlySixFileTools(t *testing.T) {
	var toolNames []string
	var systemPrompt string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var payload struct {
			Messages []struct {
				Role    string `json:"role"`
				Content string `json:"content"`
			} `json:"messages"`
			Tools []struct {
				Function struct {
					Name string `json:"name"`
				} `json:"function"`
			} `json:"tools"`
		}
		if err := json.NewDecoder(r.Body).Decode(&payload); err != nil {
			t.Errorf("decode request: %v", err)
		}
		if len(payload.Messages) > 0 {
			systemPrompt = payload.Messages[0].Content
		}
		for _, item := range payload.Tools {
			toolNames = append(toolNames, item.Function.Name)
		}
		writeSSE(w, `{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`)
	}))
	defer server.Close()

	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	builder.commandExecutor = nil
	builder.sandboxEnvironment = nil
	builder.sandboxInfo = app.SandboxInfo{Mode: app.SandboxUnavailable, BashAvailable: false, Reason: app.SandboxReasonSelfTestFailed}
	memory := session.NewMemory(session.Header{Version: session.CurrentVersion, ID: "no-bash", Workspace: builder.workspacePath, Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Now().UTC()})
	runner, err := builder.buildRunner(context.Background(), memory, config.Runtime{
		Provider: "openai-compatible", BaseURL: server.URL, Model: "test-model", APIKey: "provider-value",
		ShellTimeout: time.Second, MaxOutputBytes: 4096,
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := runner.Run(context.Background(), "inspect", nil); err != nil {
		t.Fatal(err)
	}
	want := []string{"read", "grep", "find", "ls", "write", "edit"}
	if !reflect.DeepEqual(toolNames, want) {
		t.Fatalf("tool names = %#v, want %#v", toolNames, want)
	}
	if strings.Contains(systemPrompt, "bash,") || strings.Contains(systemPrompt, ", bash") || !strings.Contains(systemPrompt, "Bash is unavailable") {
		t.Fatalf("system prompt = %q, want unavailable Bash and six file tools", systemPrompt)
	}
}

func TestRuntimeBuilderTypedNilExecutorKeepsFileToolsAndTruthfulStatus(t *testing.T) {
	var toolNames []string
	var systemPrompt string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var payload struct {
			Messages []struct {
				Role    string `json:"role"`
				Content string `json:"content"`
			} `json:"messages"`
			Tools []struct {
				Function struct {
					Name string `json:"name"`
				} `json:"function"`
			} `json:"tools"`
		}
		if err := json.NewDecoder(r.Body).Decode(&payload); err != nil {
			t.Errorf("decode request: %v", err)
		}
		if len(payload.Messages) > 0 {
			systemPrompt = payload.Messages[0].Content
		}
		for _, item := range payload.Tools {
			toolNames = append(toolNames, item.Function.Name)
		}
		writeSSE(w, `{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`)
	}))
	defer server.Close()

	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	var typedNil *recordingSandboxExecutor
	builder.commandExecutor = typedNil
	builder.sandboxEnvironment = []string{}
	builder.sandboxInfo = app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true}
	memory := session.NewMemory(session.Header{Version: session.CurrentVersion, ID: "typed-nil", Workspace: builder.workspacePath, Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Now().UTC()})
	runner, err := builder.buildRunner(context.Background(), memory, config.Runtime{
		Provider: "openai-compatible", BaseURL: server.URL, Model: "test-model", APIKey: "provider-value",
		ShellTimeout: time.Second, MaxOutputBytes: 4096,
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := runner.Run(context.Background(), "inspect", nil); err != nil {
		t.Fatal(err)
	}
	if want := []string{"read", "grep", "find", "ls", "write", "edit"}; !reflect.DeepEqual(toolNames, want) {
		t.Fatalf("tool names = %#v, want %#v", toolNames, want)
	}
	if !strings.Contains(systemPrompt, "Bash is unavailable") || strings.Contains(systemPrompt, "Seatbelt confines Bash") {
		t.Fatalf("typed-nil system prompt was not truthful: %q", systemPrompt)
	}
}

func TestRuntimeBuilderReusesOneSandboxExecutorForInitialNewAndResume(t *testing.T) {
	var providerCalls atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		call := providerCalls.Add(1)
		if call%2 == 1 {
			writeSSE(w, `{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-bash","type":"function","function":{"name":"bash","arguments":"{\"command\":\"printf reused\"}"}}]},"finish_reason":"tool_calls"}]}`)
			return
		}
		writeSSE(w, `{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`)
	}))
	defer server.Close()

	file := configWithProfiles("default")
	profile := file.Profiles["default"]
	profile.BaseURL = server.URL
	file.Profiles["default"] = profile
	builder := newRuntimeBuilderForTest(t, file)
	executor := &recordingSandboxExecutor{execute: func(_ context.Context, _ sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
		_, _ = io.WriteString(streams.Stdout, "reused")
		return sandbox.ExitStatus{Code: 0}, nil
	}}
	builder.commandExecutor = executor
	builder.sandboxEnvironment = []string{"PATH=/usr/bin:/bin", "UNRELATED=preserved"}
	builder.sandboxInfo = app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true}

	runtime, err := config.Resolve(file, builder.environment, config.SessionDefaults{}, config.Overrides{})
	if err != nil {
		t.Fatal(err)
	}
	initial := session.NewMemory(session.Header{Version: session.CurrentVersion, ID: "initial", Workspace: builder.workspacePath, Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model, CreatedAt: time.Now().UTC()})
	initialRunner, err := builder.buildRunner(context.Background(), initial, runtime)
	if err != nil {
		t.Fatal(err)
	}
	if err := initialRunner.Run(context.Background(), "initial", nil); err != nil {
		t.Fatal(err)
	}

	newReplacement, err := builder.buildNewReplacement(context.Background(), app.RuntimeInfo{Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model})
	if err != nil {
		t.Fatal(err)
	}
	defer newReplacement.Session.Close()
	if err := newReplacement.Runner.Run(context.Background(), "new", nil); err != nil {
		t.Fatal(err)
	}

	resumePath := createStoredSession(t, builder.sessionRoot, builder.workspacePath, session.Header{
		Version: session.CurrentVersion, ID: "resume-executor", Workspace: builder.workspacePath,
		Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model, CreatedAt: time.Now().UTC(),
	})
	resumeReplacement, err := builder.openReplacement(context.Background(), resumePath)
	if err != nil {
		t.Fatal(err)
	}
	defer resumeReplacement.Session.Close()
	if err := resumeReplacement.Runner.Run(context.Background(), "resume", nil); err != nil {
		t.Fatal(err)
	}

	if executor.calls.Load() != 3 || providerCalls.Load() != 6 {
		t.Fatalf("executor/provider calls = %d/%d, want 3/6", executor.calls.Load(), providerCalls.Load())
	}
	executor.mu.Lock()
	requests := append([]sandbox.Request(nil), executor.requests...)
	executor.mu.Unlock()
	for index, request := range requests {
		if !reflect.DeepEqual(request.Env, []string{"PATH=/usr/bin:/bin", "UNRELATED=preserved"}) {
			t.Fatalf("request %d environment = %#v", index, request.Env)
		}
	}
}

func TestRuntimeBuilderCapsCombinedRuntimeSecretsAndSuppressesDynamicRuntime(t *testing.T) {
	file := config.File{DefaultProfile: "p000", Profiles: map[string]config.Profile{}}
	for index := 0; index < 513; index++ {
		name := fmt.Sprintf("p%03d", index)
		file.Profiles[name] = config.Profile{
			Provider:  "openai-compatible",
			BaseURL:   "https://shared.example/v1",
			Model:     "shared-model",
			APIKeyEnv: fmt.Sprintf("KEY_%03d", index),
		}
	}
	workspacePath := mustCanonicalDirectory(t, t.TempDir())
	workspace, err := tool.NewWorkspace(workspacePath)
	if err != nil {
		t.Fatal(err)
	}
	builder := newRuntimeBuilder("", file, environmentForProfiles(file), workspace, workspacePath, filepath.Join(t.TempDir(), "sessions"), "/bin/sh", cliOptions{}, nil, runDependencies{})
	builder.sandboxInfo = app.SandboxInfo{Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true}
	values, complete := builder.boundarySecretValues(nil)
	if complete {
		t.Fatal("boundarySecretValues() complete = true, want capped incomplete prefix")
	}
	if len(values) != 512 {
		t.Fatalf("boundarySecretValues() count = %d, want 512 retained values", len(values))
	}

	var providerCalls atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		providerCalls.Add(1)
		writeSSE(w, `{"choices":[{"delta":{"content":"must not run"},"finish_reason":"stop"}]}`)
	}))
	defer server.Close()
	builder.commandExecutor = &recordingSandboxExecutor{}
	builder.sandboxEnvironment = []string{}
	runtime := config.Runtime{
		Profile: "p000", Provider: "openai-compatible", BaseURL: server.URL, Model: "shared-model",
		APIKey: "p000-secret", APIKeyEnv: "KEY_000", ShellTimeout: time.Second, MaxOutputBytes: 4096,
	}
	memory := session.NewMemory(session.Header{Version: session.CurrentVersion, ID: "capped-secrets", Workspace: builder.workspacePath, Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model, CreatedAt: time.Now().UTC()})
	runner, err := builder.buildRunner(context.Background(), memory, runtime)
	if err != nil {
		t.Fatal(err)
	}
	var events []agent.Event
	if err := runner.Run(context.Background(), "inspect", func(event agent.Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}
	if providerCalls.Load() != 0 || len(memory.Messages()) != 0 || !reflect.DeepEqual(runtimeBuilderEventTypes(events), []agent.EventType{agent.EventAgentStarted, agent.EventAgentFinished}) {
		t.Fatalf("suppressed runtime crossed dynamic boundary: provider=%d messages=%#v events=%#v", providerCalls.Load(), memory.Messages(), runtimeBuilderEventTypes(events))
	}
}

func TestRuntimeBuilderMandatoryFieldCollisionsSuppressBeforeProviderAndSessionMutation(t *testing.T) {
	contextWindow := 131_072
	file := configWithProfiles("active", "inactive")
	active := file.Profiles["active"]
	active.ContextWindow = &contextWindow
	active.Model = "active-model"
	file.Profiles["active"] = active

	for _, test := range []struct {
		name   string
		secret func(runtime config.Runtime, builder runtimeBuilder) string
	}{
		{name: "provider", secret: func(runtime config.Runtime, _ runtimeBuilder) string { return runtime.Provider }},
		{name: "profile", secret: func(runtime config.Runtime, _ runtimeBuilder) string { return runtime.Profile }},
		{name: "model", secret: func(runtime config.Runtime, _ runtimeBuilder) string { return runtime.Model }},
		{name: "thinking", secret: func(runtime config.Runtime, _ runtimeBuilder) string { return runtime.Thinking }},
		{name: "workspace", secret: func(_ config.Runtime, builder runtimeBuilder) string { return builder.workspacePath }},
		{name: "context-window", secret: func(runtime config.Runtime, _ runtimeBuilder) string {
			return fmt.Sprint(runtime.Compaction.ContextWindow)
		}},
		{name: "system-prompt", secret: func(runtime config.Runtime, builder runtimeBuilder) string {
			bashDefinition, err := tool.NewBashTool(builder.workspace, &recordingSandboxExecutor{}, "/bin/sh", []string{}, time.Second, runtime.MaxOutputBytes, nil)
			if err != nil {
				t.Fatalf("NewBashTool() error = %v", err)
			}
			definitions := []model.ToolDefinition{
				tool.NewReadTool(builder.workspace, runtime.MaxOutputBytes).Definition(),
				tool.NewGrepTool(builder.workspace, runtime.MaxOutputBytes).Definition(),
				tool.NewFindTool(builder.workspace, runtime.MaxOutputBytes).Definition(),
				tool.NewLSTool(builder.workspace, runtime.MaxOutputBytes).Definition(),
				tool.NewWriteTool(builder.workspace).Definition(),
				tool.NewEditTool(builder.workspace).Definition(),
				bashDefinition.Definition(),
				tool.NewSkillTool(skill.Catalog{}, runtime.MaxOutputBytes).Definition(),
			}
			return systemPromptFor(definitions, builder.sandboxInfo)
		}},
		{name: "tool-name", secret: func(config.Runtime, runtimeBuilder) string { return "read" }},
	} {
		t.Run(test.name, func(t *testing.T) {
			var providerCalls atomic.Int32
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
				providerCalls.Add(1)
				writeSSE(w, `{"choices":[{"delta":{"content":"must not run"},"finish_reason":"stop"}]}`)
			}))
			defer server.Close()

			builder := newRuntimeBuilderForTest(t, file)
			builder.commandExecutor = &recordingSandboxExecutor{}
			builder.sandboxEnvironment = []string{}
			builder.runtimeOverrides.Thinking = "high"
			builder.config.Profiles["active"] = config.Profile{Provider: "openai-compatible", BaseURL: server.URL, Model: "active-model", APIKeyEnv: "ACTIVE_KEY", ContextWindow: &contextWindow}
			runtime, err := config.Resolve(builder.config, builder.environment, config.SessionDefaults{}, builder.runtimeOverrides)
			if err != nil {
				t.Fatal(err)
			}
			builder.environment["INACTIVE_KEY"] = test.secret(runtime, builder)
			if got := builder.runtimeInfo(runtime); got.Provider != "" || got.Profile != "" || got.Model != "" || got.ContextWindow != 0 {
				t.Fatalf("runtime info exposed colliding metadata: %#v", got)
			}
			var createCalls atomic.Int32
			builder.deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) {
				createCalls.Add(1)
				return session.NewMemory(session.Header{}), nil
			}
			activated := new(atomic.Int32)
			builder.prepareSession = func(context.Context, string, string) (preparedSession, error) {
				return &fakePreparedSession{info: session.SessionInfo{Path: "/sessions/collision.jsonl", CWD: builder.workspacePath, Profile: runtime.Profile, Provider: runtime.Provider, Model: runtime.Model}, activate: func(context.Context) (session.Session, []session.Warning, error) {
					activated.Add(1)
					return session.NewMemory(session.Header{Version: session.CurrentVersion, ID: "collision", Workspace: builder.workspacePath, Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model, CreatedAt: time.Now().UTC()}), nil, nil
				}}, nil
			}
			if _, err := builder.buildNewReplacement(context.Background(), app.RuntimeInfo{Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model}); err == nil {
				t.Fatal("buildNewReplacement() succeeded")
			}
			if createCalls.Load() != 0 {
				t.Fatalf("buildNewReplacement() created %d sessions", createCalls.Load())
			}
			if _, err := builder.openReplacement(context.Background(), "/sessions/collision.jsonl"); err == nil {
				t.Fatal("openReplacement() succeeded")
			}
			if activated.Load() != 0 {
				t.Fatalf("openReplacement() activated %d sessions", activated.Load())
			}
			memory := session.NewMemory(session.Header{Version: session.CurrentVersion, ID: "collision", Workspace: builder.workspacePath, Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model, CreatedAt: time.Now().UTC()})
			runner, err := builder.buildRunner(context.Background(), memory, runtime)
			if err != nil {
				t.Fatal(err)
			}
			var events []agent.Event
			if err := runner.Run(context.Background(), "inspect", func(event agent.Event) { events = append(events, event) }); err != nil {
				t.Fatal(err)
			}
			if providerCalls.Load() != 0 || len(memory.Messages()) != 0 || !reflect.DeepEqual(runtimeBuilderEventTypes(events), []agent.EventType{agent.EventAgentStarted, agent.EventAgentFinished}) {
				t.Fatalf("collision crossed dynamic boundary: provider=%d messages=%#v events=%#v", providerCalls.Load(), memory.Messages(), runtimeBuilderEventTypes(events))
			}
		})
	}
}

func TestRuntimeBuilderIncompleteRedactionRunsInitialLifecycleButRejectsNewAndResume(t *testing.T) {
	const omitted = "omitted-runtime-builder-environment-value"
	var requestMu sync.Mutex
	var requestBodies []string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(r.Body)
		if err != nil {
			t.Errorf("read request: %v", err)
		}
		requestMu.Lock()
		requestBodies = append(requestBodies, string(body))
		requestMu.Unlock()
		writeSSE(w, fmt.Sprintf(`{"choices":[{"delta":{"content":%q},"finish_reason":"stop"}]}`, "provider "+omitted))
	}))
	defer server.Close()

	file := configWithProfiles("default")
	profile := file.Profiles["default"]
	profile.BaseURL = server.URL
	file.Profiles["default"] = profile
	builder := newRuntimeBuilderForTest(t, file)
	builder.commandExecutor = nil
	builder.sandboxEnvironment = nil
	builder.sandboxSecretsComplete = false
	builder.sandboxInfo = app.SandboxInfo{Mode: app.SandboxUnavailable, BashAvailable: false, Reason: app.SandboxReasonEnvironmentRejected}
	runtime, err := config.Resolve(file, builder.environment, config.SessionDefaults{}, config.Overrides{})
	if err != nil {
		t.Fatal(err)
	}

	initial, err := session.Create(builder.sessionRoot, session.Header{
		Version: session.CurrentVersion, ID: "incomplete-initial", Workspace: builder.workspacePath,
		Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model, CreatedAt: time.Now().UTC(),
	})
	if err != nil {
		t.Fatal(err)
	}
	initialPath := initial.Path()
	initialBefore := mustReadFile(t, initialPath)
	initialRunner, err := builder.buildRunner(context.Background(), initial, runtime)
	if err != nil {
		t.Fatal(err)
	}
	var events []agent.Event
	if err := initialRunner.Run(context.Background(), "initial "+omitted, func(event agent.Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}
	if len(initial.Messages()) != 0 || len(events) != 2 || events[0].Type != agent.EventAgentStarted || events[1].Type != agent.EventAgentFinished {
		t.Fatalf("initial suppressed run mutated state or events: messages=%#v events=%#v", initial.Messages(), events)
	}
	if err := initial.Close(); err != nil {
		t.Fatal(err)
	}
	if after := mustReadFile(t, initialPath); !bytes.Equal(after, initialBefore) {
		t.Fatal("initial suppressed run mutated Pi JSONL")
	}

	if _, err := builder.buildNewReplacement(context.Background(), app.RuntimeInfo{
		Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model,
	}); err == nil {
		t.Fatal("incomplete buildNewReplacement() succeeded")
	}

	resumePath := createStoredSession(t, builder.sessionRoot, builder.workspacePath, session.Header{
		Version: session.CurrentVersion, ID: "incomplete-resume", Workspace: builder.workspacePath,
		Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model, CreatedAt: time.Now().UTC(),
	})
	resumeBefore := mustReadFile(t, resumePath)
	if _, err := builder.openReplacement(context.Background(), resumePath); err == nil {
		t.Fatal("incomplete openReplacement() succeeded")
	}
	if after := mustReadFile(t, resumePath); !bytes.Equal(after, resumeBefore) {
		t.Fatal("incomplete openReplacement() mutated candidate bytes")
	}

	requestMu.Lock()
	bodies := append([]string(nil), requestBodies...)
	requestMu.Unlock()
	if len(bodies) != 0 {
		t.Fatalf("incomplete runtime made %d provider requests", len(bodies))
	}
}

func TestRuntimeBuilderIncompleteRedactionRejectsNewAndResumeBeforeMutation(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	builder.sandboxSecretsComplete = false
	var createCalls, prepareCalls atomic.Int32
	builder.deps.newSession = func(bool, string, string, config.Runtime) (session.Session, error) {
		createCalls.Add(1)
		return session.NewMemory(session.Header{}), nil
	}
	builder.prepareSession = func(context.Context, string, string) (preparedSession, error) {
		prepareCalls.Add(1)
		return nil, errors.New("must not prepare")
	}

	if _, err := builder.buildNewReplacement(context.Background(), app.RuntimeInfo{
		Provider: "openai-compatible", Profile: "default", Model: "stored-model", ContextWindow: 424_242,
	}); err == nil {
		t.Fatal("incomplete buildNewReplacement() succeeded")
	}
	if _, err := builder.openReplacement(context.Background(), "/sessions/incomplete.jsonl"); err == nil {
		t.Fatal("incomplete openReplacement() succeeded")
	}
	if createCalls.Load() != 0 || prepareCalls.Load() != 0 {
		t.Fatalf("incomplete replacement callbacks = create %d prepare %d", createCalls.Load(), prepareCalls.Load())
	}

	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := builder.buildNewReplacement(ctx, app.RuntimeInfo{}); !errors.Is(err, context.Canceled) {
		t.Fatalf("pre-canceled buildNewReplacement() error = %v", err)
	}
	if _, err := builder.openReplacement(ctx, "/sessions/incomplete.jsonl"); !errors.Is(err, context.Canceled) {
		t.Fatalf("pre-canceled openReplacement() error = %v", err)
	}
	if createCalls.Load() != 0 || prepareCalls.Load() != 0 {
		t.Fatalf("pre-canceled replacement callbacks = create %d prepare %d", createCalls.Load(), prepareCalls.Load())
	}
}

func TestRuntimeBuilderIncompleteResumePreservesRepairableSessionBytes(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	builder.sandboxSecretsComplete = false
	fixtures := map[string][]byte{
		"missing delimiter": []byte(`{"type":"session","version":3,"id":"missing-delimiter","timestamp":"2026-01-01T00:00:00Z","cwd":"/workspace"}`),
		"truncated tail":    []byte("{\"type\":\"session\"}\n{\"type\":"),
		"dangling tool call": []byte("{\"type\":\"session\"}\n" +
			"{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"toolCall\",\"id\":\"call\",\"name\":\"read\",\"arguments\":{}}]}}\n"),
	}
	for name, before := range fixtures {
		t.Run(name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "candidate.jsonl")
			if err := os.WriteFile(path, before, 0o600); err != nil {
				t.Fatal(err)
			}
			if _, err := builder.openReplacement(context.Background(), path); err == nil {
				t.Fatal("openReplacement() succeeded")
			}
			after, err := os.ReadFile(path)
			if err != nil {
				t.Fatal(err)
			}
			if !bytes.Equal(after, before) {
				t.Fatalf("candidate bytes changed: before %q after %q", before, after)
			}
		})
	}
}

func TestRuntimeBuilderSandboxInfoIsFixedAcrossNewAndResumeReplacements(t *testing.T) {
	processSandbox := app.SandboxInfo{
		Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkDenied, BashAvailable: true, Reason: app.SandboxReasonNone,
	}
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	builder.sandboxInfo = processSandbox
	builder.buildRunnerOverride = func(session.Session, config.Runtime) (app.Runner, error) {
		return commandRunnerFunc(func(context.Context, string, func(agent.Event)) error { return nil }), nil
	}

	newReplacement, err := builder.buildNewReplacement(context.Background(), app.RuntimeInfo{
		Provider: "openai-compatible", Profile: "default", Model: "gpt-4.1",
		Sandbox: app.SandboxInfo{Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true},
	})
	if err != nil {
		t.Fatal(err)
	}
	defer newReplacement.Session.Close()
	if got := newReplacement.RuntimeInfo.Sandbox; got != processSandbox {
		t.Fatalf("new replacement Sandbox = %#v, want process value %#v", got, processSandbox)
	}

	path := createStoredSession(t, builder.sessionRoot, builder.workspacePath, session.Header{
		Version: session.CurrentVersion, ID: "sandbox-resume", Workspace: builder.workspacePath,
		Provider: "openai-compatible", Profile: "default", Model: "stored-model", CreatedAt: time.Now().UTC(),
	})
	resumeReplacement, err := builder.openReplacement(context.Background(), path)
	if err != nil {
		t.Fatal(err)
	}
	defer resumeReplacement.Session.Close()
	if got := resumeReplacement.RuntimeInfo.Sandbox; got != processSandbox {
		t.Fatalf("resume replacement Sandbox = %#v, want process value %#v", got, processSandbox)
	}
}

type fakePreparedSession struct {
	info       session.SessionInfo
	activate   func(context.Context) (session.Session, []session.Warning, error)
	close      func() error
	closeErr   error
	closeCalls atomic.Int32
}

func (p *fakePreparedSession) Info() session.SessionInfo { return p.info }

func (p *fakePreparedSession) Activate(ctx context.Context) (session.Session, []session.Warning, error) {
	return p.activate(ctx)
}

func (p *fakePreparedSession) Close() error {
	p.closeCalls.Add(1)
	if p.close != nil {
		return p.close()
	}
	return p.closeErr
}

type runtimeUpdateTrackingSession struct {
	session.Session
	updateCalls atomic.Int32
	closeCalls  atomic.Int32
}

func (s *runtimeUpdateTrackingSession) UpdateRuntime(ctx context.Context, metadata session.RuntimeMetadata) error {
	s.updateCalls.Add(1)
	return s.Session.(session.RuntimeUpdater).UpdateRuntime(ctx, metadata)
}

func (s *runtimeUpdateTrackingSession) Close() error {
	s.closeCalls.Add(1)
	return s.Session.Close()
}

type trackedReplacementSession struct {
	session.Session
	closed     chan struct{}
	closeCalls atomic.Int32
	closeErr   error
}

type flippingHeaderSession struct {
	*trackedReplacementSession
	first       session.Header
	second      session.Header
	headerCalls atomic.Int32
}

func (s *flippingHeaderSession) Header() session.Header {
	if s.headerCalls.Add(1) == 1 {
		return s.first
	}
	return s.second
}

func TestRuntimeBuilderIncompleteRedactErrorDoesNotInvokeAttackerErrorMethods(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	builder.sandboxSecretsComplete = false
	panicErr := &panicRuntimeBuilderError{}
	got := builder.redactError(panicErr, nil)
	if got == nil || got.Error() != "" {
		t.Fatalf("redactError() = %#v, want fixed empty error", got)
	}
	if panicErr.calls() != 0 {
		t.Fatalf("attacker error methods were called %d times", panicErr.calls())
	}
	if got := builder.redactError(context.Canceled, nil); got != context.Canceled {
		t.Fatalf("canceled redactError() = %#v, want direct context.Canceled", got)
	}

	builder.sandboxSecretsComplete = true
	builder.sandboxSecrets = []string{strings.Repeat("x", safetext.MaxDynamicValueBytes+1)}
	panicErr = &panicRuntimeBuilderError{}
	got = builder.redactError(panicErr, nil)
	if got == nil || got.Error() != "" {
		t.Fatalf("complex-boundary redactError() = %#v, want fixed empty error", got)
	}
	if panicErr.calls() != 0 {
		t.Fatalf("complex-boundary attacker error methods were called %d times", panicErr.calls())
	}
}

func (s *trackedReplacementSession) Close() error {
	s.closeCalls.Add(1)
	err := s.Session.Close()
	select {
	case <-s.closed:
	default:
		close(s.closed)
	}
	return errors.Join(err, s.closeErr)
}

type panicRuntimeBuilderError struct {
	errorCalls  int
	isCalls     int
	unwrapCalls int
}

func (e *panicRuntimeBuilderError) Error() string {
	e.errorCalls++
	panic("unexpected Error call")
}

func (e *panicRuntimeBuilderError) Is(error) bool {
	e.isCalls++
	panic("unexpected Is call")
}

func (e *panicRuntimeBuilderError) Unwrap() error {
	e.unwrapCalls++
	panic("unexpected Unwrap call")
}

func (e *panicRuntimeBuilderError) calls() int {
	return e.errorCalls + e.isCalls + e.unwrapCalls
}

func runtimeBuilderEventTypes(events []agent.Event) []agent.EventType {
	types := make([]agent.EventType, len(events))
	for index, event := range events {
		types[index] = event.Type
	}
	return types
}

func markerExhaustingProviderSecret(t *testing.T) string {
	t.Helper()
	var value strings.Builder
	for candidate := rune(1); candidate <= utf8.MaxRune; candidate++ {
		if !utf8.ValidRune(candidate) || unicode.IsControl(candidate) {
			continue
		}
		value.WriteRune(candidate)
	}
	result := value.String()
	if !utf8.ValidString(result) {
		t.Fatal("marker adversary is not valid UTF-8")
	}
	return result
}

func newRuntimeBuilderForTest(t *testing.T, file config.File) runtimeBuilder {
	t.Helper()
	workspacePath := mustCanonicalDirectory(t, t.TempDir())
	workspace, err := tool.NewWorkspace(workspacePath)
	if err != nil {
		t.Fatal(err)
	}
	environment := environmentForProfiles(file)
	hostEntries := []string{"HOME=", "PATH=/usr/bin:/bin", "OTTO_RUNTIME_BUILDER_UNRELATED=keep-me"}
	names := make([]string, 0, len(environment))
	for name := range environment {
		names = append(names, name)
	}
	sort.Strings(names)
	for _, name := range names {
		hostEntries = append(hostEntries, name+"="+environment[name])
	}
	providerNames := sandboxProviderEnvironmentNames(file, "")
	snapshot, err := sandbox.ResolveEnvironment(sandbox.EnvironmentOptions{HostEntries: hostEntries, ProviderNames: providerNames})
	if err != nil {
		t.Fatal(err)
	}
	driver := direct.New()
	executor, err := sandbox.NewExecutor(driver, sandbox.Policy{Filesystem: sandbox.FilesystemUnconfined, Network: sandbox.NetworkAllow}, workspacePath)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if err := executor.Close(); err != nil {
			t.Errorf("Executor.Close() error = %v", err)
		}
	})
	return runtimeBuilder{
		config:                 file,
		environment:            environment,
		workspace:              workspace,
		workspacePath:          workspacePath,
		sessionRoot:            filepath.Join(t.TempDir(), "sessions"),
		shell:                  canonicalSandboxRuntimeShell(t),
		commandExecutor:        executor,
		sandboxEnvironment:     snapshot.Entries(),
		sandboxSecrets:         snapshot.RedactionValues(),
		sandboxSecretsComplete: snapshot.RedactionsComplete(),
		sandboxInfo: app.SandboxInfo{
			Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true, Reason: app.SandboxReasonNone,
		},
	}
}

func configWithProfiles(names ...string) config.File {
	file := config.File{Profiles: map[string]config.Profile{}}
	if len(names) > 0 {
		file.DefaultProfile = names[0]
	}
	for _, name := range names {
		file.Profiles[name] = config.Profile{
			Provider:  "openai-compatible",
			BaseURL:   fmt.Sprintf("https://%s.example/v1", name),
			Model:     name + "-profile-model",
			APIKeyEnv: strings.ToUpper(name) + "_KEY",
		}
	}
	return file
}

func environmentForProfiles(file config.File) map[string]string {
	environment := map[string]string{"OTTO_API_KEY": "fallback-secret"}
	for name, profile := range file.Profiles {
		if profile.APIKeyEnv != "" {
			environment[profile.APIKeyEnv] = name + "-secret"
		}
	}
	return environment
}

func redactedRuntime(runtime config.Runtime) config.Runtime {
	runtime.APIKey = "[REDACTED]"
	return runtime
}

func mustReadFile(t *testing.T, path string) []byte {
	t.Helper()
	contents, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return contents
}

func mustCanonicalDirectory(t *testing.T, path string) string {
	t.Helper()
	canonical, err := canonicalDirectory(path)
	if err != nil {
		t.Fatal(err)
	}
	return canonical
}

func createStoredSession(t *testing.T, root, workspace string, header session.Header) string {
	t.Helper()
	store, err := session.Create(root, header)
	if err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	return path
}

func createOpenPiStore(t *testing.T, root, workspace, id string) session.Session {
	t.Helper()
	store, err := session.Create(root, session.Header{Version: session.CurrentVersion, ID: id, Workspace: workspace, Provider: "openai-compatible", Profile: "default", Model: "test-model", CreatedAt: time.Now().UTC()})
	if err != nil {
		t.Fatal(err)
	}
	return store
}
