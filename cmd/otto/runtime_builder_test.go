package main

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
)

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
			builder.openSession = func(string, string) (session.Session, []session.Warning, error) {
				return nil, nil, errors.New("openSession must not run")
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

func TestRuntimeBuilderOpenReplacementReturnsWarningsAndRuntimeInfo(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default", "resumed"))
	path := createStoredSession(t, builder.sessionRoot, builder.workspacePath, session.Header{Version: session.CurrentVersion, ID: "resumed-session", Workspace: builder.workspacePath, Provider: "openai-compatible", Profile: "resumed", Model: "stored-model", CreatedAt: time.Now().UTC()})
	warnings := []session.Warning{{Message: "repaired dangling tool call"}}
	var captured config.Runtime

	builder.openSession = func(path, workspace string) (session.Session, []session.Warning, error) {
		store, _, err := openSession(path, workspace)
		if err != nil {
			return nil, nil, err
		}
		return store, warnings, nil
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
	if replacement.RuntimeInfo != (app.RuntimeInfo{Provider: "openai-compatible", Profile: "resumed", Model: "stored-model"}) {
		t.Fatalf("runtime info = %#v", replacement.RuntimeInfo)
	}
	if len(replacement.Warnings) != 1 || replacement.Warnings[0].Message != warnings[0].Message {
		t.Fatalf("warnings = %#v", replacement.Warnings)
	}
	replacement.Warnings[0].Message = "mutated"
	if warnings[0].Message != "repaired dangling tool call" {
		t.Fatalf("warnings mutated = %#v", warnings)
	}
	if captured.Profile != "resumed" || captured.Model != "stored-model" || captured.APIKey != "resumed-secret" {
		t.Fatalf("captured runtime = %#v", redactedRuntime(captured))
	}
}

func TestRuntimeBuilderFailureClosesCandidateStore(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	candidate := &trackedReplacementSession{Session: createOpenPiStore(t, builder.sessionRoot, builder.workspacePath, "candidate"), closed: make(chan struct{})}
	builder.openSession = func(string, string) (session.Session, []session.Warning, error) {
		return candidate, nil, nil
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

func TestRuntimeBuilderBuildRunnerUsesRuntimeLimitsAndRedaction(t *testing.T) {
	const (
		apiKey          = "runtime-secret"
		fallbackAPIKey  = "fallback-secret"
		apiKeyEnv       = "RUNTIME_KEY"
		unrelatedEnvKey = "OTTO_RUNTIME_BUILDER_UNRELATED"
	)
	t.Setenv(apiKeyEnv, apiKey)
	t.Setenv("OTTO_API_KEY", fallbackAPIKey)
	t.Setenv(unrelatedEnvKey, "keep-me")

	var requests atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		requests.Add(1)
		command := fmt.Sprintf("printf \"runtime=%%s fallback=%%s unrelated=%%s literal=%%s\" \"${%s:-missing}\" \"${OTTO_API_KEY:-missing}\" \"${%s:-missing}\" %q", apiKeyEnv, unrelatedEnvKey, apiKey)
		writeSSE(w, fmt.Sprintf(`{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"bash","arguments":%q}}]},"finish_reason":"tool_calls"}]}`,
			fmt.Sprintf(`{"command":%q}`, command)))
	}))
	defer server.Close()

	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	memory := session.NewMemory(session.Header{Version: session.CurrentVersion, ID: "runtime-builder", Workspace: builder.workspacePath, Provider: "openai-compatible", Model: "runtime-model", CreatedAt: time.Now().UTC()})
	runner, err := builder.buildRunner(memory, config.Runtime{
		Profile:        "default",
		Provider:       "openai-compatible",
		BaseURL:        server.URL,
		Model:          "runtime-model",
		APIKey:         apiKey,
		APIKeyEnv:      apiKeyEnv,
		MaxTurns:       1,
		ShellTimeout:   500 * time.Millisecond,
		MaxOutputBytes: 4096,
	})
	if err != nil {
		t.Fatal(err)
	}

	err = runner.Run(context.Background(), "run the tool", nil)
	if !errors.Is(err, agent.ErrMaxTurns) {
		t.Fatalf("Run() error = %v, want agent.ErrMaxTurns", err)
	}
	if requests.Load() != 1 {
		t.Fatalf("provider requests = %d, want 1", requests.Load())
	}
	messages := memory.Messages()
	if len(messages) != 3 {
		t.Fatalf("messages = %#v, want user+assistant+tool", messages)
	}
	toolResult := messages[2].Blocks[0].Text
	if !strings.Contains(toolResult, "runtime=missing") || !strings.Contains(toolResult, "fallback=missing") || !strings.Contains(toolResult, "unrelated=keep-me") || !strings.Contains(toolResult, "literal=[REDACTED]") || !strings.Contains(toolResult, "exit_code: 0") {
		t.Fatalf("tool result = %q", toolResult)
	}
	for _, forbidden := range []string{apiKey, fallbackAPIKey} {
		if strings.Contains(toolResult, forbidden) {
			t.Fatalf("tool result leaked %q: %q", forbidden, toolResult)
		}
	}
}

type trackedReplacementSession struct {
	session.Session
	closed     chan struct{}
	closeCalls atomic.Int32
}

func (s *trackedReplacementSession) Close() error {
	s.closeCalls.Add(1)
	err := s.Session.Close()
	select {
	case <-s.closed:
	default:
		close(s.closed)
	}
	return err
}

func newRuntimeBuilderForTest(t *testing.T, file config.File) runtimeBuilder {
	t.Helper()
	workspacePath := mustCanonicalDirectory(t, t.TempDir())
	workspace, err := tool.NewWorkspace(workspacePath)
	if err != nil {
		t.Fatal(err)
	}
	return runtimeBuilder{
		config:        file,
		environment:   environmentForProfiles(file),
		workspace:     workspace,
		workspacePath: workspacePath,
		sessionRoot:   filepath.Join(t.TempDir(), "sessions"),
		shell:         "/bin/sh",
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
