package main

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/app"
)

// writeAgentFileForTest writes <root>/<name>/AGENT.md with the given
// frontmatter name and description, and returns the agent directory.
func writeAgentFileForTest(t *testing.T, root, dirName, frontmatterName, description, body string) string {
	t.Helper()
	dir := filepath.Join(root, dirName)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	content := "---\nname: " + frontmatterName + "\ndescription: " + description + "\n---\n" + body
	if err := os.WriteFile(filepath.Join(dir, "AGENT.md"), []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	return dir
}

func writeCLIConfigWithAgentsDisabled(t *testing.T, providerName, keyEnv, baseURL string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "otto.toml")
	content := "default_profile = \"test\"\n[agents]\nenabled = false\n" + fmt.Sprintf(`[profiles.test]
provider = %q
base_url = %q
model = "test-model"
api_key_env = %q
`, providerName, baseURL, keyEnv)
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

// TestRunAgentsSandboxReadPaths covers appending existing agent roots to the
// Seatbelt read paths: only roots that exist as a directory are added, and
// [agents] enabled = false adds none.
func TestRunAgentsSandboxReadPaths(t *testing.T) {
	for _, test := range []struct {
		name             string
		makeUserDir      bool
		makeWorkspaceDir bool
		disabled         bool
		wantUser         bool
		wantWorkspace    bool
	}{
		{name: "both roots exist", makeUserDir: true, makeWorkspaceDir: true, wantUser: true, wantWorkspace: true},
		{name: "roots absent", makeUserDir: false, makeWorkspaceDir: false, wantUser: false, wantWorkspace: false},
		{name: "disabled with roots present", makeUserDir: true, makeWorkspaceDir: true, disabled: true, wantUser: false, wantWorkspace: false},
	} {
		t.Run(test.name, func(t *testing.T) {
			home := t.TempDir()
			rawWorkspace := t.TempDir()
			workspace, err := filepath.EvalSymlinks(rawWorkspace)
			if err != nil {
				t.Fatal(err)
			}
			userAgentsDir := filepath.Join(home, ".otto", "agents")
			workspaceAgentsDir := filepath.Join(workspace, ".otto", "agents")
			if test.makeUserDir {
				if err := os.MkdirAll(userAgentsDir, 0o755); err != nil {
					t.Fatal(err)
				}
			}
			if test.makeWorkspaceDir {
				if err := os.MkdirAll(workspaceAgentsDir, 0o755); err != nil {
					t.Fatal(err)
				}
			}

			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				_, _ = io.Copy(io.Discard, r.Body)
				writeSSE(w, `{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`)
			}))
			defer server.Close()

			var configPath string
			if test.disabled {
				configPath = writeCLIConfigWithAgentsDisabled(t, "openai-compatible", "TEST_KEY", server.URL)
			} else {
				configPath = writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)
			}

			var capturedOptions sandboxOpenOptions
			deps := deterministicRunDependencies(t)
			deps.openSandbox = func(_ context.Context, options sandboxOpenOptions) sandboxRuntime {
				capturedOptions = options
				return fakeSandboxRuntime(app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true}, &recordingSandboxExecutor{}, []string{})
			}

			var stdout, stderr bytes.Buffer
			code := runWithDependencies(context.Background(), []string{"--config", configPath, "--cwd", rawWorkspace}, strings.NewReader("hi\n/exit\n"), &stdout, &stderr, testEnviron(map[string]string{
				"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
			}), deps)
			if code != 0 {
				t.Fatalf("code = %d, stderr = %q", code, stderr.String())
			}

			if got := slices.Contains(capturedOptions.Settings.ReadPaths, userAgentsDir); got != test.wantUser {
				t.Fatalf("ReadPaths contains user agents dir = %v, want %v (ReadPaths = %v)", got, test.wantUser, capturedOptions.Settings.ReadPaths)
			}
			if got := slices.Contains(capturedOptions.Settings.ReadPaths, workspaceAgentsDir); got != test.wantWorkspace {
				t.Fatalf("ReadPaths contains workspace agents dir = %v, want %v (ReadPaths = %v)", got, test.wantWorkspace, capturedOptions.Settings.ReadPaths)
			}
		})
	}
}

// TestRunAgentsMaxParallelOutOfRangeFailsStartup covers a startup error when
// [agents].max_parallel is outside 1..16.
func TestRunAgentsMaxParallelOutOfRangeFailsStartup(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	path := filepath.Join(t.TempDir(), "otto.toml")
	content := "default_profile = \"test\"\n[agents]\nmax_parallel = 17\n[profiles.test]\nprovider = \"openai-compatible\"\nbase_url = \"http://example.invalid\"\nmodel = \"test-model\"\napi_key_env = \"TEST_KEY\"\n"
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}

	var stdout, stderr bytes.Buffer
	code := runForTest(t, context.Background(), []string{"--config", path, "--cwd", workspace}, strings.NewReader(""), &stdout, &stderr, testEnviron(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}))
	if code == 0 {
		t.Fatalf("code = 0, want nonzero; stderr = %q", stderr.String())
	}
	if !strings.Contains(stderr.String(), "[agents].max_parallel must be between 1 and 16, got 17") {
		t.Fatalf("stderr = %q, want max_parallel range error", stderr.String())
	}
}

// TestRunAgentsSectionPresentInSystemPrompt covers a workspace-level agent
// definition appearing in the system prompt's "## Agents" section and in
// the tool list.
func TestRunAgentsSectionPresentInSystemPrompt(t *testing.T) {
	home := t.TempDir()
	rawWorkspace := t.TempDir()
	workspace, err := filepath.EvalSymlinks(rawWorkspace)
	if err != nil {
		t.Fatal(err)
	}
	writeAgentFileForTest(t, filepath.Join(workspace, ".otto", "agents"), "reviewer", "reviewer",
		"Reviews code for style and correctness.", "# Reviewer\nFocus on style.\n")

	var systemPrompt string
	var toolNames []string
	server := newStopServer(t, func(payload skillPromptPayload) {
		if len(payload.Messages) > 0 {
			systemPrompt = payload.Messages[0].Content
		}
		toolNames = skillPromptToolNames(payload)
	})

	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)
	var stdout, stderr bytes.Buffer
	code := runForTest(t, context.Background(), []string{"--config", configPath, "--cwd", rawWorkspace}, strings.NewReader("hi\n/exit\n"), &stdout, &stderr, testEnviron(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}))
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if !strings.Contains(systemPrompt, "\n\n## Agents\n") {
		t.Fatalf("system prompt missing Agents section, got %q", systemPrompt)
	}
	if !strings.Contains(systemPrompt, `<agent name="reviewer">`) {
		t.Fatalf("system prompt missing reviewer agent entry, got %q", systemPrompt)
	}
	if !slices.Contains(toolNames, "agent") {
		t.Fatalf("tool names = %v, want agent", toolNames)
	}
}

// TestRunAgentsDisabledOmitsSectionAndTools covers [agents] enabled = false
// with a definition present: neither the "## Agents" section nor the
// agent/agent_wait/agent_status tools appear.
func TestRunAgentsDisabledOmitsSectionAndTools(t *testing.T) {
	home := t.TempDir()
	rawWorkspace := t.TempDir()
	workspace, err := filepath.EvalSymlinks(rawWorkspace)
	if err != nil {
		t.Fatal(err)
	}
	writeAgentFileForTest(t, filepath.Join(workspace, ".otto", "agents"), "reviewer", "reviewer",
		"Reviews code for style and correctness.", "body\n")

	var systemPrompt string
	var toolNames []string
	server := newStopServer(t, func(payload skillPromptPayload) {
		if len(payload.Messages) > 0 {
			systemPrompt = payload.Messages[0].Content
		}
		toolNames = skillPromptToolNames(payload)
	})

	configPath := writeCLIConfigWithAgentsDisabled(t, "openai-compatible", "TEST_KEY", server.URL)
	var stdout, stderr bytes.Buffer
	code := runForTest(t, context.Background(), []string{"--config", configPath, "--cwd", rawWorkspace}, strings.NewReader("hi\n/exit\n"), &stdout, &stderr, testEnviron(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}))
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if strings.Contains(systemPrompt, "## Agents") {
		t.Fatalf("system prompt should not contain an Agents section when disabled, got %q", systemPrompt)
	}
	for _, name := range []string{"agent", "agent_wait", "agent_status"} {
		if slices.Contains(toolNames, name) {
			t.Fatalf("tool names should not contain %q when disabled, got %v", name, toolNames)
		}
	}
}

// TestRuntimeBuilderBoundaryToolDefinitionsExcludesAgentToolsWhenAgentsDisabled
// covers boundaryToolDefinitions dropping the agent/agent_wait/agent_status
// tools when [agents].enabled = false, mirroring buildRunner's own gate.
func TestRuntimeBuilderBoundaryToolDefinitionsExcludesAgentToolsWhenAgentsDisabled(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))

	names := func() []string {
		definitions := builder.boundaryToolDefinitions(nil)
		out := make([]string, len(definitions))
		for i, d := range definitions {
			out[i] = d.Name
		}
		return out
	}

	got := names()
	for _, want := range []string{"agent", "agent_wait", "agent_status"} {
		if !slices.Contains(got, want) {
			t.Fatalf("boundaryToolDefinitions() = %v, want it to include %q", got, want)
		}
	}

	disabled := false
	builder.config.Agents.Enabled = &disabled
	got = names()
	for _, unwanted := range []string{"agent", "agent_wait", "agent_status"} {
		if slices.Contains(got, unwanted) {
			t.Fatalf("boundaryToolDefinitions() = %v, want it to exclude %q when [agents] is disabled", got, unwanted)
		}
	}
}
