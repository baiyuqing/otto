package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/app"
)

// writeSkillFileForTest writes <root>/<name>/SKILL.md with the given
// frontmatter name and description, and returns the skill directory.
func writeSkillFileForTest(t *testing.T, root, dirName, frontmatterName, description, body string) string {
	t.Helper()
	dir := filepath.Join(root, dirName)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	content := "---\nname: " + frontmatterName + "\ndescription: " + description + "\n---\n" + body
	if err := os.WriteFile(filepath.Join(dir, "SKILL.md"), []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	return dir
}

type skillPromptPayload struct {
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

func decodeSkillPromptPayload(t *testing.T, r *http.Request) skillPromptPayload {
	t.Helper()
	var payload skillPromptPayload
	if err := json.NewDecoder(r.Body).Decode(&payload); err != nil {
		t.Errorf("decode request: %v", err)
	}
	return payload
}

func skillPromptToolNames(payload skillPromptPayload) []string {
	names := make([]string, 0, len(payload.Tools))
	for _, item := range payload.Tools {
		names = append(names, item.Function.Name)
	}
	return names
}

// newStopServer replies "done" and stop to every request, for tests that
// only need to inspect the first outgoing request.
func newStopServer(t *testing.T, onRequest func(payload skillPromptPayload)) *httptest.Server {
	t.Helper()
	var requestCount int
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		requestCount++
		payload := decodeSkillPromptPayload(t, r)
		if requestCount == 1 && onRequest != nil {
			onRequest(payload)
		}
		writeSSE(w, `{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`)
	}))
	t.Cleanup(server.Close)
	return server
}

func writeCLIConfigWithSkillsDisabled(t *testing.T, providerName, keyEnv, baseURL string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "otto.toml")
	content := "default_profile = \"test\"\n[skills]\nenabled = false\n" + fmt.Sprintf(`[profiles.test]
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

// TestRunSkillListingAndToolRoundTrip covers a user-level skill appearing in
// the system prompt and the tools list, and a full skill tool call/result
// round trip through the provider.
func TestRunSkillListingAndToolRoundTrip(t *testing.T) {
	home := t.TempDir()
	rawWorkspace := t.TempDir()
	workspace, err := filepath.EvalSymlinks(rawWorkspace)
	if err != nil {
		t.Fatal(err)
	}
	skillDir := writeSkillFileForTest(t, filepath.Join(home, ".otto", "skills"), "pdf", "pdf",
		"Extract text and tables from PDF files.", "# PDF handling\nExtract pdfs from files.\n")

	var requestCount int
	var firstSystemPrompt string
	var firstToolNames []string
	var toolResultContent string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		requestCount++
		payload := decodeSkillPromptPayload(t, r)
		if requestCount == 1 {
			if len(payload.Messages) > 0 {
				firstSystemPrompt = payload.Messages[0].Content
			}
			firstToolNames = skillPromptToolNames(payload)
			writeSSE(w, `{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-skill","type":"function","function":{"name":"skill","arguments":"{\"name\":\"pdf\"}"}}]},"finish_reason":"tool_calls"}]}`)
			return
		}
		for _, message := range payload.Messages {
			if message.Role == "tool" {
				toolResultContent = message.Content
			}
		}
		writeSSE(w, `{"choices":[{"delta":{"content":"complete"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":1}}`)
	}))
	defer server.Close()

	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)
	var stdout, stderr bytes.Buffer
	code := runForTest(t, context.Background(), []string{"--config", configPath, "--cwd", rawWorkspace}, strings.NewReader("use the pdf skill\n/exit\n"), &stdout, &stderr, testEnviron(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}))
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if requestCount != 2 {
		t.Fatalf("requests = %d, want 2", requestCount)
	}

	if !strings.Contains(firstSystemPrompt, "\n\n## Skills\n") {
		t.Fatalf("system prompt missing Skills section, got %q", firstSystemPrompt)
	}
	if !strings.Contains(firstSystemPrompt, `<skill name="pdf" location="`+skillDir) {
		t.Fatalf("system prompt missing pdf skill entry for %q, got %q", skillDir, firstSystemPrompt)
	}
	if envIndex, skillsIndex := strings.Index(firstSystemPrompt, "## Environment"), strings.Index(firstSystemPrompt, "## Skills"); envIndex < 0 || skillsIndex < envIndex {
		t.Fatalf("Skills section must follow Environment section, envIndex=%d skillsIndex=%d", envIndex, skillsIndex)
	}
	wantTools := []string{"read", "grep", "find", "ls", "write", "edit", "bash", "memory_search", "remember", "forget", "skill", "agent", "agent_wait", "agent_status"}
	if !reflect.DeepEqual(firstToolNames, wantTools) {
		t.Fatalf("tool names = %v, want %v", firstToolNames, wantTools)
	}
	if !strings.Contains(toolResultContent, "skill: pdf") || !strings.Contains(toolResultContent, "# PDF handling") {
		t.Fatalf("tool result content = %q", toolResultContent)
	}
	_ = workspace
}

// TestRunWorkspaceSkillOverridesUserLevelSkill covers a name conflict: the
// workspace-level root is scanned after the user-level root, so it wins.
func TestRunWorkspaceSkillOverridesUserLevelSkill(t *testing.T) {
	home := t.TempDir()
	rawWorkspace := t.TempDir()
	workspace, err := filepath.EvalSymlinks(rawWorkspace)
	if err != nil {
		t.Fatal(err)
	}
	userSkillDir := writeSkillFileForTest(t, filepath.Join(home, ".otto", "skills"), "repo-notes", "repo-notes",
		"User-level notes skill.", "user body\n")
	workspaceSkillDir := writeSkillFileForTest(t, filepath.Join(workspace, ".otto", "skills"), "repo-notes", "repo-notes",
		"Workspace-level notes skill.", "workspace body\n")

	var systemPrompt string
	server := newStopServer(t, func(payload skillPromptPayload) {
		if len(payload.Messages) > 0 {
			systemPrompt = payload.Messages[0].Content
		}
	})

	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)
	var stdout, stderr bytes.Buffer
	code := runForTest(t, context.Background(), []string{"--config", configPath, "--cwd", rawWorkspace}, strings.NewReader("hi\n/exit\n"), &stdout, &stderr, testEnviron(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}))
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if !strings.Contains(systemPrompt, `<skill name="repo-notes" location="`+workspaceSkillDir) {
		t.Fatalf("system prompt missing workspace skill entry for %q, got %q", workspaceSkillDir, systemPrompt)
	}
	if strings.Contains(systemPrompt, userSkillDir) {
		t.Fatalf("system prompt should not reference overridden user skill dir %q, got %q", userSkillDir, systemPrompt)
	}
}

// TestRunNoSkillsOmitsSectionAndTool covers the empty-catalog case: no
// "## Skills" section and no "skill" tool.
func TestRunNoSkillsOmitsSectionAndTool(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()

	var systemPrompt string
	server := newStopServer(t, func(payload skillPromptPayload) {
		if len(payload.Messages) > 0 {
			systemPrompt = payload.Messages[0].Content
		}
	})

	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)
	var stdout, stderr bytes.Buffer
	code := runForTest(t, context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader("hi\n/exit\n"), &stdout, &stderr, testEnviron(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}))
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if strings.Contains(systemPrompt, "## Skills") {
		t.Fatalf("system prompt should not contain a Skills section, got %q", systemPrompt)
	}
}

// TestRunSkillsDisabledOmitsSectionAndTool covers [skills] enabled = false
// with a skill present: neither the section nor the tool appear.
func TestRunSkillsDisabledOmitsSectionAndTool(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	writeSkillFileForTest(t, filepath.Join(home, ".otto", "skills"), "pdf", "pdf", "Extract pdfs.", "body\n")

	var systemPrompt string
	var toolNames []string
	server := newStopServer(t, func(payload skillPromptPayload) {
		if len(payload.Messages) > 0 {
			systemPrompt = payload.Messages[0].Content
		}
		toolNames = skillPromptToolNames(payload)
	})

	configPath := writeCLIConfigWithSkillsDisabled(t, "openai-compatible", "TEST_KEY", server.URL)
	var stdout, stderr bytes.Buffer
	code := runForTest(t, context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader("hi\n/exit\n"), &stdout, &stderr, testEnviron(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}))
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if strings.Contains(systemPrompt, "## Skills") {
		t.Fatalf("system prompt should not contain a Skills section when disabled, got %q", systemPrompt)
	}
	if slices.Contains(toolNames, "skill") {
		t.Fatalf("tool names should not contain skill when disabled, got %v", toolNames)
	}
}

// TestRunInvalidSkillWarnsAndSkipsButValidSkillListed covers an invalid
// skill directory (frontmatter name mismatch) alongside a valid one: it is
// skipped with a stderr warning, and the valid skill still appears.
func TestRunInvalidSkillWarnsAndSkipsButValidSkillListed(t *testing.T) {
	home := t.TempDir()
	workspace := t.TempDir()
	skillsRoot := filepath.Join(home, ".otto", "skills")
	pdfDir := writeSkillFileForTest(t, skillsRoot, "pdf", "pdf", "Extract pdfs.", "body\n")
	writeSkillFileForTest(t, skillsRoot, "bad", "other", "Mismatched name.", "body\n")

	var systemPrompt string
	server := newStopServer(t, func(payload skillPromptPayload) {
		if len(payload.Messages) > 0 {
			systemPrompt = payload.Messages[0].Content
		}
	})

	configPath := writeCLIConfig(t, "openai-compatible", "TEST_KEY", server.URL)
	var stdout, stderr bytes.Buffer
	code := runForTest(t, context.Background(), []string{"--config", configPath, "--cwd", workspace}, strings.NewReader("hi\n/exit\n"), &stdout, &stderr, testEnviron(map[string]string{
		"HOME": home, "SHELL": "/bin/sh", "TEST_KEY": "secret",
	}))
	if code != 0 {
		t.Fatalf("code = %d, stderr = %q", code, stderr.String())
	}
	if !strings.Contains(stderr.String(), "warning: skill ") || !strings.Contains(stderr.String(), `does not match directory "bad"`) {
		t.Fatalf("stderr = %q, want a name-mismatch warning for %q", stderr.String(), "bad")
	}
	if !strings.Contains(systemPrompt, `<skill name="pdf" location="`+pdfDir) {
		t.Fatalf("system prompt missing valid pdf skill entry, got %q", systemPrompt)
	}
}

// TestRunSkillsSandboxReadPaths covers appending existing skill roots to the
// Seatbelt read paths: only roots that exist as a directory are added, and
// enabled = false adds none.
func TestRunSkillsSandboxReadPaths(t *testing.T) {
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
			userSkillsDir := filepath.Join(home, ".otto", "skills")
			workspaceSkillsDir := filepath.Join(workspace, ".otto", "skills")
			if test.makeUserDir {
				if err := os.MkdirAll(userSkillsDir, 0o755); err != nil {
					t.Fatal(err)
				}
			}
			if test.makeWorkspaceDir {
				if err := os.MkdirAll(workspaceSkillsDir, 0o755); err != nil {
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
				configPath = writeCLIConfigWithSkillsDisabled(t, "openai-compatible", "TEST_KEY", server.URL)
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

			if got := slices.Contains(capturedOptions.Settings.ReadPaths, userSkillsDir); got != test.wantUser {
				t.Fatalf("ReadPaths contains user skills dir = %v, want %v (ReadPaths = %v)", got, test.wantUser, capturedOptions.Settings.ReadPaths)
			}
			if got := slices.Contains(capturedOptions.Settings.ReadPaths, workspaceSkillsDir); got != test.wantWorkspace {
				t.Fatalf("ReadPaths contains workspace skills dir = %v, want %v (ReadPaths = %v)", got, test.wantWorkspace, capturedOptions.Settings.ReadPaths)
			}
		})
	}
}

// TestRuntimeBuilderBoundaryToolDefinitionsIncludesSkillWhenEnabled covers
// boundaryToolDefinitions, which the redaction boundary check uses to
// detect a system-prompt change: the skill definition joins the list by
// default and drops out when skills are disabled.
func TestRuntimeBuilderBoundaryToolDefinitionsIncludesSkillWhenEnabled(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))

	names := func() []string {
		definitions := builder.boundaryToolDefinitions(nil)
		out := make([]string, len(definitions))
		for i, d := range definitions {
			out[i] = d.Name
		}
		return out
	}

	if got := names(); !slices.Contains(got, "skill") {
		t.Fatalf("boundaryToolDefinitions() = %v, want it to include skill", got)
	}

	disabled := false
	builder.config.Skills.Enabled = &disabled
	if got := names(); slices.Contains(got, "skill") {
		t.Fatalf("boundaryToolDefinitions() = %v, want it to exclude skill when disabled", got)
	}
}

func TestRuntimeBuilderBoundaryToolDefinitionsIncludesEnabledMemoryTools(t *testing.T) {
	builder := newRuntimeBuilderForTest(t, configWithProfiles("default"))
	builder.memoryUsable = true
	definitions := builder.boundaryToolDefinitions(nil)
	names := make(map[string]bool, len(definitions))
	for _, definition := range definitions {
		names[definition.Name] = true
	}
	for _, want := range []string{"memory_search", "remember", "forget"} {
		if !names[want] {
			t.Fatalf("boundaryToolDefinitions() missing enabled memory tool %q: %v", want, names)
		}
	}
}

// TestRuntimeBuilderBoundaryToolDefinitionsIncludesAgentToolsWhenDynamicAllowed
// covers T4: boundaryToolDefinitions predicts the same agent/agent_wait/
// agent_status tools that buildRunner registers when a provider client will
// be built, and drops them when the redaction boundary does not allow
// dynamic content (mirroring buildRunner's own gating condition).
func TestRuntimeBuilderBoundaryToolDefinitionsIncludesAgentToolsWhenDynamicAllowed(t *testing.T) {
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

	builder.environment["OTTO_API_KEY"] = markerExhaustingProviderSecret(t)
	if got := names(); slices.Contains(got, "agent") {
		t.Fatalf("boundaryToolDefinitions() = %v, want it to exclude agent tools when dynamic content is not allowed", got)
	}
}
