package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/model"
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
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default", "resumed"))
	path := createStoredSession(t, builder.sessionRoot, builder.workspacePath, session.Header{Version: session.CurrentVersion, ID: "resumed-session", Workspace: builder.workspacePath, Provider: "openai-compatible", Profile: "resumed", Model: "stored-model", CreatedAt: time.Now().UTC()})
	warnings := []session.Warning{{Message: "repaired dangling tool call"}}
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

func TestRuntimeBuilderBuildRunnerRemovesAndRedactsEveryProfileCredential(t *testing.T) {
	const (
		activeEnv   = "OTTO_RUNTIME_BUILDER_ACTIVE_KEY"
		inactiveEnv = "OTTO_RUNTIME_BUILDER_INACTIVE_KEY"
		activeKey   = "credential-alpha-329847"
		inactiveKey = "credential-bravo-761205"
		fallbackKey = "credential-charlie-458913"
	)
	t.Setenv(activeEnv, activeKey)
	t.Setenv(inactiveEnv, inactiveKey)
	t.Setenv("OTTO_API_KEY", fallbackKey)

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

	runner, err := builder.buildRunner(store, config.Runtime{
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

func TestRuntimeBuilderBuildRunnerEnforcesShellTimeoutOutputLimitAndRedaction(t *testing.T) {
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
		if requests.Add(1) != 1 {
			writeSSE(w, `{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`)
			return
		}
		command := fmt.Sprintf("printf \"runtime=%%s fallback=%%s unrelated=%%s literal=%%s \" \"${%s:-missing}\" \"${OTTO_API_KEY:-missing}\" \"${%s:-missing}\" %q; printf '%%0200d' 0; sleep 1", apiKeyEnv, unrelatedEnvKey, apiKey)
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
		ShellTimeout:   50 * time.Millisecond,
		MaxOutputBytes: 96,
	})
	if err != nil {
		t.Fatal(err)
	}

	err = runner.Run(context.Background(), "run the tool", nil)
	if err != nil {
		t.Fatalf("Run() error = %v, want tool-turn success", err)
	}
	if requests.Load() != 2 {
		t.Fatalf("provider requests = %d, want 2 (tool turn then final stop)", requests.Load())
	}
	messages := memory.Messages()
	if len(messages) != 4 {
		t.Fatalf("messages = %#v, want user+assistant+tool+assistant", messages)
	}
	toolResult := messages[2].Blocks[0].Text
	redactedLiteral := strings.Contains(toolResult, "literal=[REDACTED]") || strings.Contains(toolResult, "literal=█")
	if !strings.Contains(toolResult, "runtime=missing") || !strings.Contains(toolResult, "fallback=missing") || !strings.Contains(toolResult, "unrelated=keep-me") || !redactedLiteral || !strings.Contains(toolResult, "[truncated:") || !strings.Contains(toolResult, "status: timed out after 50ms") {
		t.Fatalf("tool result = %q", toolResult)
	}
	for _, forbidden := range []string{apiKey, fallbackAPIKey} {
		if strings.Contains(toolResult, forbidden) {
			t.Fatalf("tool result leaked %q: %q", forbidden, toolResult)
		}
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
