# Otto Stage 1 MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a working macOS Go coding agent with a streaming REPL, OpenAI-compatible Chat Completions, four coding tools, TOML profiles, and resumable JSONL sessions.

**Architecture:** A line-oriented REPL drives an agent loop expressed only in provider-neutral message types. The OpenAI-compatible adapter translates those types to HTTP/SSE, while tool execution, workspace safety, configuration, and persistence remain independent packages. Codex and Claude subscription adapters are deliberately deferred to separate implementation plans built on these contracts.

**Tech Stack:** Go 1.26, Go standard library, `github.com/pelletier/go-toml/v2`, native `testing`, `httptest`, `go vet`, and Staticcheck.

## Global Constraints

- Target macOS; do not claim Windows or Linux support in Stage 1.
- The executable and Go module are `otto` and `github.com/baiyuqing/otto`.
- Keep provider-specific JSON and HTTP types inside `internal/provider/openaicompat`.
- Implement only `openai-compatible` in Stage 1; Codex and Claude receive later plans.
- File tools are workspace-bound after canonical path and symlink validation.
- `bash` is intentionally unsandboxed and starts in the workspace.
- Default limits are 50 model turns, a 120-second shell timeout, and 50 KiB tool output.
- Configuration defaults to `~/.config/otto/config.toml`; do not load project-local configuration.
- Raw API keys and OAuth credentials must never appear in TOML or JSONL sessions.
- Session writes are append-only; persistence failure stops further durable prompt processing.
- Use exact-replacement editing and reject zero or multiple matches.
- Use TDD for every task and make one focused commit after each task passes.
- Do not force-push `origin/main` until all completion gates pass and the final review is complete.

---

## Planned File Structure

```text
.gitignore                              generated binaries and local state
AGENTS.md                               Go-specific contributor instructions
LICENSE                                 MIT license retained from the old repository
README.md                               setup, safety, configuration, and usage
go.mod
go.sum
cmd/otto/main.go                        process entry point
internal/agent/agent.go                 provider/tool orchestration
internal/agent/agent_test.go
internal/agent/events.go                application event definitions
internal/config/config.go               TOML decoding and default path
internal/config/config_test.go
internal/config/resolve.go              precedence and startup validation
internal/config/resolve_test.go
internal/model/types.go                 neutral messages, blocks, tools, and usage
internal/model/types_test.go
internal/provider/provider.go           neutral provider contract
internal/provider/openaicompat/client.go HTTP request, retry, and status handling
internal/provider/openaicompat/client_test.go
internal/provider/openaicompat/protocol.go OpenAI wire types and translation
internal/provider/openaicompat/stream.go SSE parsing and tool-call assembly
internal/provider/openaicompat/stream_test.go
internal/repl/repl.go                   line input, commands, and event rendering
internal/repl/repl_test.go
internal/session/memory.go              ephemeral session implementation
internal/session/store.go               JSONL create, append, load, and repair
internal/session/store_test.go
internal/session/types.go               session records and common interface
internal/tool/bash.go                   shell execution and process-group cancellation
internal/tool/bash_test.go
internal/tool/edit.go                   exact replacement
internal/tool/file_tools_test.go
internal/tool/read.go                    bounded UTF-8 reads
internal/tool/registry.go               tool interface and lookup
internal/tool/registry_test.go
internal/tool/result.go                 result and capped-output helpers
internal/tool/workspace.go              canonical workspace path enforcement
internal/tool/workspace_test.go
internal/tool/write.go                  atomic file writes
```

---

### Task 1: Bootstrap the Go Module and Neutral Contracts

**Files:**
- Create: `go.mod`
- Create: `.gitignore`
- Create: `LICENSE`
- Create: `internal/model/types.go`
- Create: `internal/model/types_test.go`
- Create: `internal/provider/provider.go`

**Interfaces:**
- Produces: `model.Message`, `model.Block`, `model.ToolDefinition`, `model.Usage`, `provider.Request`, `provider.Response`, `provider.StreamEvent`, and `provider.Provider`.
- Consumes: only the Go standard library.

- [ ] **Step 1: Write neutral-type tests**

Create `internal/model/types_test.go` with tests that marshal and unmarshal a message containing text and tool-call blocks without losing raw JSON arguments:

```go
package model

import (
    "encoding/json"
    "reflect"
    "testing"
    "time"
)

func TestMessageJSONRoundTrip(t *testing.T) {
    original := Message{
        ID: "msg-1",
        Role: RoleAssistant,
        CreatedAt: time.Unix(10, 0).UTC(),
        Blocks: []Block{
            {Type: BlockText, Text: "checking"},
            {Type: BlockToolCall, ToolCallID: "call-1", ToolName: "read", Arguments: json.RawMessage(`{"path":"README.md"}`)},
        },
        FinishReason: FinishToolCalls,
        Usage: &Usage{InputTokens: 11, OutputTokens: 7},
    }
    encoded, err := json.Marshal(original)
    if err != nil {
        t.Fatal(err)
    }
    var decoded Message
    if err := json.Unmarshal(encoded, &decoded); err != nil {
        t.Fatal(err)
    }
    if !reflect.DeepEqual(original, decoded) {
        t.Fatalf("round trip mismatch\nwant: %#v\n got: %#v", original, decoded)
    }
}

func TestMessageTextJoinsOnlyTextBlocks(t *testing.T) {
    message := Message{Blocks: []Block{
        {Type: BlockText, Text: "one"},
        {Type: BlockToolCall, ToolName: "read"},
        {Type: BlockText, Text: "two"},
    }}
    if got := message.Text(); got != "onetwo" {
        t.Fatalf("Text() = %q, want %q", got, "onetwo")
    }
}
```

- [ ] **Step 2: Run the tests and verify the bootstrap failure**

Run:

```bash
go test ./internal/model
```

Expected: FAIL because `go.mod` and the model types do not exist.

- [ ] **Step 3: Create the module and model types**

Create `go.mod`:

```go
module github.com/baiyuqing/otto

go 1.26.0
```

Create `internal/model/types.go` with these exact public types and constants:

```go
package model

import (
    "encoding/json"
    "strings"
    "time"
)

type Role string

const (
    RoleUser      Role = "user"
    RoleAssistant Role = "assistant"
    RoleTool      Role = "tool"
)

type BlockType string

const (
    BlockText       BlockType = "text"
    BlockToolCall   BlockType = "tool_call"
    BlockToolResult BlockType = "tool_result"
)

type Block struct {
    Type       BlockType       `json:"type"`
    Text       string          `json:"text,omitempty"`
    ToolCallID string          `json:"tool_call_id,omitempty"`
    ToolName   string          `json:"tool_name,omitempty"`
    Arguments  json.RawMessage `json:"arguments,omitempty"`
    IsError    bool            `json:"is_error,omitempty"`
}

type Message struct {
    ID           string       `json:"id"`
    Role         Role         `json:"role"`
    Blocks       []Block      `json:"blocks"`
    CreatedAt    time.Time    `json:"created_at"`
    FinishReason FinishReason `json:"finish_reason,omitempty"`
    Usage        *Usage       `json:"usage,omitempty"`
}

func (m Message) Text() string {
    var builder strings.Builder
    for _, block := range m.Blocks {
        if block.Type == BlockText {
            builder.WriteString(block.Text)
        }
    }
    return builder.String()
}

type ToolDefinition struct {
    Name        string         `json:"name"`
    Description string         `json:"description"`
    Parameters  map[string]any `json:"parameters"`
}

type FinishReason string

const (
    FinishStop      FinishReason = "stop"
    FinishToolCalls FinishReason = "tool_calls"
    FinishLength    FinishReason = "length"
    FinishUnknown   FinishReason = "unknown"
)

type Usage struct {
    InputTokens  int `json:"input_tokens"`
    OutputTokens int `json:"output_tokens"`
}
```

Create `internal/provider/provider.go`:

```go
package provider

import (
    "context"

    "github.com/baiyuqing/otto/internal/model"
)

type Request struct {
    Model        string
    SystemPrompt string
    Messages     []model.Message
    Tools        []model.ToolDefinition
}

type Response struct {
    Message      model.Message
    FinishReason model.FinishReason
    Usage        model.Usage
}

type StreamEventType string

const (
    StreamTextDelta     StreamEventType = "text_delta"
    StreamToolCallDelta StreamEventType = "tool_call_delta"
)

type StreamEvent struct {
    Type       StreamEventType
    Text       string
    ToolCallID string
    ToolName   string
    Arguments string
}

type Provider interface {
    Complete(context.Context, Request, func(StreamEvent)) (Response, error)
}
```

Create `.gitignore` containing `/otto`, `/dist/`, `.DS_Store`, `.otto/`, and `*.test`. Retain the MIT license text from `origin/main:LICENSE` in the new `LICENSE` file.

- [ ] **Step 4: Format and run tests**

Run:

```bash
gofmt -w internal/model/types.go internal/model/types_test.go internal/provider/provider.go
go test ./internal/model ./internal/provider
```

Expected: PASS.

- [ ] **Step 5: Commit the contracts**

```bash
git add .gitignore LICENSE go.mod internal/model internal/provider/provider.go
git commit -m "feat: add neutral agent contracts"
```

---

### Task 2: Add Global TOML Configuration and Resolution

**Files:**
- Modify: `go.mod`
- Create: `internal/config/config.go`
- Create: `internal/config/config_test.go`
- Create: `internal/config/resolve.go`
- Create: `internal/config/resolve_test.go`

**Interfaces:**
- Produces: `config.Load(path string)`, `config.Resolve(File, map[string]string, SessionDefaults, Overrides)`, `config.DefaultPath()`, and `config.Runtime`.
- Consumes: no prior application interfaces.

- [ ] **Step 1: Write parsing and precedence tests**

Test these cases explicitly:

```go
func TestLoadRejectsUnknownFields(t *testing.T) {
    path := writeConfig(t, `default_profile = "local"
unknown = true
[profiles.local]
provider = "openai-compatible"
model = "test-model"
base_url = "http://localhost:8080/v1"
api_key_env = "TEST_KEY"
`)
    _, err := Load(path)
    if err == nil || !strings.Contains(err.Error(), "unknown") {
        t.Fatalf("expected unknown-field error, got %v", err)
    }
}

func TestResolvePrecedence(t *testing.T) {
    file := File{
        DefaultProfile: "configured",
        Profiles: map[string]Profile{
            "configured": {Provider: "openai-compatible", Model: "config-model", BaseURL: "https://config.example/v1", APIKeyEnv: "CONFIG_KEY"},
            "explicit": {Provider: "openai-compatible", Model: "profile-model", BaseURL: "https://profile.example/v1", APIKeyEnv: "PROFILE_KEY"},
        },
    }
    runtime, err := Resolve(file, map[string]string{"OTTO_MODEL": "env-model"}, SessionDefaults{Provider: "openai-compatible", Model: "session-model"}, Overrides{Profile: "explicit"})
    if err != nil {
        t.Fatal(err)
    }
    if runtime.Model != "env-model" || runtime.Profile != "explicit" || runtime.BaseURL != "https://profile.example/v1" {
        t.Fatalf("unexpected runtime: %#v", runtime)
    }
}

func TestExplicitProfileOverridesResumedProviderAndModel(t *testing.T) {
    file := File{Profiles: map[string]Profile{
        "explicit": {Provider: "openai-compatible", Model: "profile-model", BaseURL: "https://example.com/v1", APIKeyEnv: "PROFILE_KEY"},
    }}
    runtime, err := Resolve(file, nil, SessionDefaults{Provider: "codex", Model: "old-model"}, Overrides{Profile: "explicit"})
    if err != nil {
        t.Fatal(err)
    }
    if runtime.Provider != "openai-compatible" || runtime.Model != "profile-model" {
        t.Fatalf("explicit profile did not win: %#v", runtime)
    }
}

func TestResolveRejectsRawSecretField(t *testing.T) {
    path := writeConfig(t, `[profiles.bad]
provider = "openai-compatible"
model = "test-model"
base_url = "https://example.com/v1"
api_key = "secret"
`)
    if _, err := Load(path); err == nil {
        t.Fatal("expected raw api_key to be rejected")
    }
}

func writeConfig(t *testing.T, content string) string {
    t.Helper()
    path := filepath.Join(t.TempDir(), "config.toml")
    if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
        t.Fatal(err)
    }
    return path
}
```

Also test missing default files, invalid durations, unknown profiles, missing models, unsupported Stage 1 providers, missing named API-key environment variables, and CLI-over-environment precedence.

- [ ] **Step 2: Run tests and verify they fail**

Run:

```bash
go test ./internal/config
```

Expected: FAIL because the package does not exist.

- [ ] **Step 3: Implement strict TOML loading**

Add the dependency:

```bash
go get github.com/pelletier/go-toml/v2@v2.2.4
```

Define:

```go
type File struct {
    DefaultProfile string             `toml:"default_profile"`
    Agent          Agent              `toml:"agent"`
    Profiles       map[string]Profile `toml:"profiles"`
}

type Agent struct {
    MaxTurns      int    `toml:"max_turns"`
    ShellTimeout  string `toml:"shell_timeout"`
    MaxOutputBytes int   `toml:"max_output_bytes"`
}

type Profile struct {
    Provider  string `toml:"provider"`
    BaseURL   string `toml:"base_url"`
    Model     string `toml:"model"`
    APIKeyEnv string `toml:"api_key_env"`
}

type SessionDefaults struct {
    Provider string
    Model    string
}

type Overrides struct {
    Profile         string
    Provider        string
    BaseURL         string
    Model           string
    MaxTurns        int
    ShellTimeout    time.Duration
    MaxOutputBytes  int
}

type Runtime struct {
    Profile        string
    Provider       string
    BaseURL        string
    Model          string
    APIKey         string
    APIKeyEnv      string
    MaxTurns       int
    ShellTimeout   time.Duration
    MaxOutputBytes int
}
```

`DefaultPath()` must use `os.UserHomeDir()` and return `~/.config/otto/config.toml`. `Load` must use `toml.NewDecoder(reader).DisallowUnknownFields()`. A missing default file returns an empty `File`; an explicitly supplied missing path returns an error.

`Resolve` must implement the approved precedence exactly. Treat a non-empty `Overrides.Profile` as explicit CLI selection that overrides resumed provider/model metadata. Apply defaults of 20, 120 seconds, and 51200 bytes. Stage 1 accepts only `openai-compatible`, requires a model and valid HTTP(S) base URL, and resolves the API key from the profile's `api_key_env`, falling back to `OTTO_API_KEY`.

- [ ] **Step 4: Run configuration tests**

```bash
gofmt -w internal/config
go test ./internal/config
```

Expected: PASS.

- [ ] **Step 5: Commit configuration support**

```bash
git add go.mod go.sum internal/config
git commit -m "feat: add TOML provider profiles"
```

---

### Task 3: Implement Memory and Append-Only JSONL Sessions

**Files:**
- Create: `internal/session/types.go`
- Create: `internal/session/memory.go`
- Create: `internal/session/store.go`
- Create: `internal/session/store_test.go`

**Interfaces:**
- Consumes: `model.Message` and `model.Usage`.
- Produces: `session.Session`, `session.Create`, `session.Open`, `session.Memory`, `session.Header`, and `session.Warning`.

- [ ] **Step 1: Write persistence tests**

Create tests for:

```go
func TestStoreRoundTrip(t *testing.T) {
    root := t.TempDir()
    header := Header{Version: 1, ID: "session-1", Workspace: "/tmp/project", Provider: "openai-compatible", Profile: "local", Model: "test-model", CreatedAt: time.Unix(1, 0).UTC()}
    store, err := Create(root, header)
    if err != nil {
        t.Fatal(err)
    }
    message := model.Message{ID: "msg-1", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "hello"}}, CreatedAt: time.Unix(2, 0).UTC()}
    if err := store.Append(context.Background(), message); err != nil {
        t.Fatal(err)
    }
    path := store.Path()
    if err := store.Close(); err != nil {
        t.Fatal(err)
    }
    reopened, warnings, err := Open(path)
    if err != nil {
        t.Fatal(err)
    }
    defer reopened.Close()
    if len(warnings) != 0 || !reflect.DeepEqual(reopened.Messages(), []model.Message{message}) {
        t.Fatalf("unexpected reopen result: warnings=%v messages=%#v", warnings, reopened.Messages())
    }
}

func TestOpenRepairsDanglingToolCall(t *testing.T) {
    path := createSessionWithDanglingCall(t)
    store, warnings, err := Open(path)
    if err != nil {
        t.Fatal(err)
    }
    defer store.Close()
    messages := store.Messages()
    last := messages[len(messages)-1]
    if len(warnings) != 1 || last.Role != model.RoleTool || !last.Blocks[0].IsError || last.Blocks[0].ToolCallID != "call-1" {
        t.Fatalf("dangling call was not repaired: warnings=%v last=%#v", warnings, last)
    }
}

func createSessionWithDanglingCall(t *testing.T) string {
    t.Helper()
    store, err := Create(t.TempDir(), Header{Version: 1, ID: "dangling", Workspace: t.TempDir(), Provider: "openai-compatible", Model: "test", CreatedAt: time.Unix(1, 0).UTC()})
    if err != nil {
        t.Fatal(err)
    }
    assistant := model.Message{ID: "assistant-1", Role: model.RoleAssistant, CreatedAt: time.Unix(2, 0).UTC(), Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "read", Arguments: json.RawMessage(`{"path":"README.md"}`)}}}
    if err := store.Append(context.Background(), assistant); err != nil {
        t.Fatal(err)
    }
    path := store.Path()
    if err := store.Close(); err != nil {
        t.Fatal(err)
    }
    return path
}
```

Also test: file mode, malformed final line warning and truncation, malformed non-final line failure, unsupported header version, message append after close, context cancellation before append, and independent slices returned by `Messages()`.

- [ ] **Step 2: Run tests and verify failure**

```bash
go test ./internal/session
```

Expected: FAIL because session types are undefined.

- [ ] **Step 3: Define records and the session interface**

Use:

```go
type Header struct {
    Version   int       `json:"version"`
    ID        string    `json:"id"`
    Workspace string    `json:"workspace"`
    Provider  string    `json:"provider"`
    Profile   string    `json:"profile,omitempty"`
    Model     string    `json:"model"`
    CreatedAt time.Time `json:"created_at"`
}

type Record struct {
    Type    string         `json:"type"`
    Header  *Header        `json:"header,omitempty"`
    Message *model.Message `json:"message,omitempty"`
}

type Warning struct {
    Message string
}

type Session interface {
    Header() Header
    Messages() []model.Message
    Append(context.Context, model.Message) error
    Path() string
    Close() error
}
```

`Memory` implements this interface without disk I/O and is constructed with:

```go
func NewMemory(header Header) *Memory
```

`Create(root, header)` must create `<root>/<workspace-key>/<session-id>.jsonl`, where the workspace key is the first 16 lowercase hexadecimal characters of SHA-256 over the canonical workspace path. Create directories with `0700` and files with `0600`. Write and `Sync` the header before returning.

`Append` JSON-encodes exactly one record plus `\n`, writes it under a mutex, calls `Sync`, and only then updates the in-memory slice.

`Open` validates the first record, loads complete records, truncates only a malformed non-empty final line, and repairs unresolved assistant tool calls by appending synthetic error tool messages. Generate repair IDs using `crypto/rand`; do not add a UUID dependency.

- [ ] **Step 4: Run session tests**

```bash
gofmt -w internal/session
go test ./internal/session
```

Expected: PASS.

- [ ] **Step 5: Commit session persistence**

```bash
git add internal/session
git commit -m "feat: add resumable JSONL sessions"
```

---

### Task 4: Add the Tool Registry and Canonical Workspace Boundary

**Files:**
- Create: `internal/tool/registry.go`
- Create: `internal/tool/registry_test.go`
- Create: `internal/tool/result.go`
- Create: `internal/tool/workspace.go`
- Create: `internal/tool/workspace_test.go`

**Interfaces:**
- Consumes: `model.ToolDefinition`.
- Produces: `tool.Tool`, `tool.Result`, `tool.Registry`, `tool.Workspace`, `ResolveExisting`, and `ResolveForWrite`.

- [ ] **Step 1: Write registry and workspace tests**

Use a fake tool to verify duplicate-name and unknown-name errors:

```go
func TestRegistryRejectsDuplicateNames(t *testing.T) {
    fake := fakeTool{name: "read"}
    if _, err := NewRegistry(fake, fake); err == nil {
        t.Fatal("expected duplicate tool error")
    }
}

func TestWorkspaceRejectsParentTraversal(t *testing.T) {
    root := t.TempDir()
    workspace, err := NewWorkspace(root)
    if err != nil {
        t.Fatal(err)
    }
    if _, err := workspace.ResolveForWrite("../escape.txt"); err == nil {
        t.Fatal("expected traversal rejection")
    }
}

func TestWorkspaceRejectsSymlinkEscape(t *testing.T) {
    root := t.TempDir()
    outside := t.TempDir()
    if err := os.Symlink(outside, filepath.Join(root, "link")); err != nil {
        t.Fatal(err)
    }
    workspace, err := NewWorkspace(root)
    if err != nil {
        t.Fatal(err)
    }
    if _, err := workspace.ResolveForWrite("link/new.txt"); err == nil {
        t.Fatal("expected symlink escape rejection")
    }
}

type fakeTool struct {
    name string
}

func (f fakeTool) Definition() model.ToolDefinition {
    return model.ToolDefinition{Name: f.name, Parameters: map[string]any{"type": "object"}}
}

func (f fakeTool) Execute(context.Context, json.RawMessage) Result {
    return Result{Content: "ok"}
}
```

Also verify valid nested missing paths, absolute paths inside the workspace, absolute paths outside it, a symlink that stays inside, root equality, and registry definition ordering.

- [ ] **Step 2: Run tests and verify failure**

```bash
go test ./internal/tool
```

Expected: FAIL because registry and workspace types are undefined.

- [ ] **Step 3: Implement registry and path enforcement**

Define:

```go
type Result struct {
    Content string
    IsError bool
}

type Tool interface {
    Definition() model.ToolDefinition
    Execute(context.Context, json.RawMessage) Result
}

type Registry struct {
    ordered []Tool
    byName  map[string]Tool
}

func NewRegistry(tools ...Tool) (*Registry, error)
func (r *Registry) Definitions() []model.ToolDefinition
func (r *Registry) Execute(ctx context.Context, name string, args json.RawMessage) Result
```

Unknown tools return `Result{Content: "unknown tool: <name>", IsError: true}`.

`NewWorkspace` must call `filepath.Abs`, `filepath.Clean`, and `filepath.EvalSymlinks` on the root. Boundary checks must use `filepath.Rel` and reject `..` or paths beginning with `..` plus the OS separator.

`ResolveExisting` evaluates the complete target symlink path. `ResolveForWrite` walks upward until it finds an existing ancestor, evaluates that ancestor, verifies it remains inside the workspace, then rejoins the missing suffix. Reject any suffix component equal to `..`.

Add a capped byte collector in `result.go` that retains at most `limit` bytes while counting discarded bytes; later file and shell tools use it.

- [ ] **Step 4: Run registry and workspace tests**

```bash
gofmt -w internal/tool
go test ./internal/tool
```

Expected: PASS.

- [ ] **Step 5: Commit the tool foundation**

```bash
git add internal/tool
git commit -m "feat: enforce workspace tool boundaries"
```

---

### Task 5: Implement Read, Write, and Exact Edit Tools

**Files:**
- Create: `internal/tool/read.go`
- Create: `internal/tool/write.go`
- Create: `internal/tool/edit.go`
- Create: `internal/tool/file_tools_test.go`

**Interfaces:**
- Consumes: `tool.Workspace`, `tool.Result`, and `model.ToolDefinition`.
- Produces: `tool.NewReadTool`, `tool.NewWriteTool`, and `tool.NewEditTool`.

- [ ] **Step 1: Write file-tool tests**

Cover successful operations and these exact failure modes:

```go
func TestEditRejectsAmbiguousMatch(t *testing.T) {
    root := t.TempDir()
    path := filepath.Join(root, "sample.txt")
    if err := os.WriteFile(path, []byte("same\nsame\n"), 0o644); err != nil {
        t.Fatal(err)
    }
    workspace, _ := NewWorkspace(root)
    edit := NewEditTool(workspace)
    result := edit.Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"same","new_text":"new"}`))
    if !result.IsError || !strings.Contains(result.Content, "2 occurrences") {
        t.Fatalf("unexpected result: %#v", result)
    }
}

func TestReadRejectsBinaryFile(t *testing.T) {
    root := t.TempDir()
    if err := os.WriteFile(filepath.Join(root, "binary"), []byte{'a', 0, 'b'}, 0o644); err != nil {
        t.Fatal(err)
    }
    workspace, _ := NewWorkspace(root)
    result := NewReadTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"path":"binary"}`))
    if !result.IsError || !strings.Contains(result.Content, "binary") {
        t.Fatalf("unexpected result: %#v", result)
    }
}

func TestWriteIsAtomicAndCreatesParents(t *testing.T) {
    root := t.TempDir()
    workspace, _ := NewWorkspace(root)
    result := NewWriteTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"nested/file.txt","content":"hello"}`))
    if result.IsError {
        t.Fatal(result.Content)
    }
    data, err := os.ReadFile(filepath.Join(root, "nested/file.txt"))
    if err != nil || string(data) != "hello" {
        t.Fatalf("data=%q err=%v", data, err)
    }
}
```

Also test invalid JSON, unknown fields, missing required arguments, read line offset/limit, invalid UTF-8, output truncation notice, absent edit match, successful exact edit, traversal, symlink escape, preservation of an existing file's permissions, and no temporary-file residue after success.

- [ ] **Step 2: Run tests and verify failure**

```bash
go test ./internal/tool -run 'Test(Read|Write|Edit)'
```

Expected: FAIL because constructors are undefined.

- [ ] **Step 3: Implement strict argument decoding and file behavior**

Each tool must use a JSON decoder with `DisallowUnknownFields()` and reject trailing JSON tokens.

Definitions:

```go
type readArgs struct {
    Path   string `json:"path"`
    Offset int    `json:"offset,omitempty"`
    Limit  int    `json:"limit,omitempty"`
}

type writeArgs struct {
    Path    string `json:"path"`
    Content string `json:"content"`
}

type editArgs struct {
    Path    string `json:"path"`
    OldText string `json:"old_text"`
    NewText string `json:"new_text"`
}
```

`read` treats offset as one-based and defaults to line 1. A zero line limit means all remaining lines subject to the byte cap. Reject NUL bytes and invalid UTF-8.

`write` creates parent directories with `0755`, writes a temporary file in the target directory, applies the existing target mode or `0644`, calls `Sync`, closes, and renames. Remove the temporary file on every failure path.

`edit` reads the complete validated UTF-8 target, counts non-overlapping exact matches with `strings.Count`, requires exactly one, replaces it, and calls the same atomic writer as `write`. Its success result reports the path and replaced byte counts without echoing file contents.

- [ ] **Step 4: Run file-tool tests**

```bash
gofmt -w internal/tool/read.go internal/tool/write.go internal/tool/edit.go internal/tool/file_tools_test.go
go test ./internal/tool
```

Expected: PASS.

- [ ] **Step 5: Commit file tools**

```bash
git add internal/tool
git commit -m "feat: add workspace file tools"
```

---

### Task 6: Implement Unsandboxed Shell Execution with Cancellation

**Files:**
- Create: `internal/tool/bash.go`
- Create: `internal/tool/bash_test.go`

**Interfaces:**
- Consumes: `tool.Workspace`, capped output helper, and `model.ToolDefinition`.
- Produces: `tool.NewBashTool(workspace, shell, timeout, maxOutputBytes)`.

- [ ] **Step 1: Write shell behavior tests**

Include:

```go
func TestBashRunsInWorkspaceAndReportsExitCode(t *testing.T) {
    root := t.TempDir()
    workspace, _ := NewWorkspace(root)
    bash := NewBashTool(workspace, "/bin/sh", 5*time.Second, 51200)
    result := bash.Execute(context.Background(), json.RawMessage(`{"command":"pwd; echo problem >&2; exit 7"}`))
    if result.IsError {
        t.Fatalf("tool execution infrastructure failed: %s", result.Content)
    }
    if !strings.Contains(result.Content, root) || !strings.Contains(result.Content, "problem") || !strings.Contains(result.Content, "exit_code: 7") {
        t.Fatalf("unexpected result: %s", result.Content)
    }
}

func TestBashTimeoutKillsProcessGroup(t *testing.T) {
    root := t.TempDir()
    workspace, _ := NewWorkspace(root)
    marker := filepath.Join(root, "child-survived")
    command := fmt.Sprintf("(sleep 1; touch %q) & wait", marker)
    bash := NewBashTool(workspace, "/bin/sh", 50*time.Millisecond, 51200)
    result := bash.Execute(context.Background(), mustJSON(t, map[string]string{"command": command}))
    if !strings.Contains(result.Content, "timed out") {
        t.Fatalf("unexpected result: %s", result.Content)
    }
    time.Sleep(1200 * time.Millisecond)
    if _, err := os.Stat(marker); !errors.Is(err, os.ErrNotExist) {
        t.Fatalf("child process survived: %v", err)
    }
}

func mustJSON(t *testing.T, value any) json.RawMessage {
    t.Helper()
    encoded, err := json.Marshal(value)
    if err != nil {
        t.Fatal(err)
    }
    return encoded
}
```

Also test invalid arguments, empty command, inherited environment, stdout/stderr truncation, caller cancellation, configured shell validation, and a normal zero exit.

- [ ] **Step 2: Run tests and verify failure**

```bash
go test ./internal/tool -run TestBash
```

Expected: FAIL because `NewBashTool` does not exist.

- [ ] **Step 3: Implement shell execution**

Use `exec.Command(shell, "-lc", args.Command)`, set `cmd.Dir` to `workspace.Root()`, inherit `os.Environ()`, and set:

```go
cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
```

After `Start`, wait in a buffered channel. Select among completion, caller cancellation, and a timer. On cancellation or timeout, call `syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)` and wait for `cmd.Wait()` before returning. Treat command non-zero exit as a successful tool invocation whose textual result includes `exit_code`; reserve `Result.IsError` for invalid arguments or failures to start/manage the process.

Combine bounded stdout and stderr in the result with stable labels. Include the discarded-byte count when truncated. Do not include environment values in diagnostics.

- [ ] **Step 4: Run shell and race tests**

```bash
gofmt -w internal/tool/bash.go internal/tool/bash_test.go
go test ./internal/tool
go test -race ./internal/tool
```

Expected: PASS.

- [ ] **Step 5: Commit shell support**

```bash
git add internal/tool/bash.go internal/tool/bash_test.go
git commit -m "feat: add cancellable shell tool"
```

---

### Task 7: Build the OpenAI-Compatible Streaming Adapter

**Files:**
- Create: `internal/provider/openaicompat/protocol.go`
- Create: `internal/provider/openaicompat/stream.go`
- Create: `internal/provider/openaicompat/stream_test.go`
- Create: `internal/provider/openaicompat/client.go`
- Create: `internal/provider/openaicompat/client_test.go`

**Interfaces:**
- Consumes: `provider.Provider`, `provider.Request`, `provider.StreamEvent`, and neutral model types.
- Produces: `openaicompat.New(baseURL, apiKey, httpClient)` implementing `provider.Provider`.

- [ ] **Step 1: Write protocol translation and fragmented-stream tests**

Use an `httptest.Server` that validates the request and emits deliberately fragmented tool-call arguments:

```go
func TestCompleteStreamsTextAndAssemblesToolCall(t *testing.T) {
    server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
        if r.URL.Path != "/v1/chat/completions" || r.Header.Get("Authorization") != "Bearer test-key" {
            t.Errorf("unexpected request: path=%s auth=%s", r.URL.Path, r.Header.Get("Authorization"))
        }
        w.Header().Set("Content-Type", "text/event-stream")
        flusher := w.(http.Flusher)
        chunks := []string{
            `{"choices":[{"delta":{"content":"I will read. "}}]}`,
            `{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"read","arguments":"{\"pa"}}]}}]}`,
            `{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"README.md\"}"}}]},"finish_reason":"tool_calls"}]}`,
            `{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7}}`,
        }
        for _, chunk := range chunks {
            fmt.Fprintf(w, "data: %s\n\n", chunk)
            flusher.Flush()
        }
        fmt.Fprint(w, "data: [DONE]\n\n")
    }))
    defer server.Close()

    client := New(server.URL+"/v1", "test-key", server.Client())
    var deltas strings.Builder
    response, err := client.Complete(context.Background(), provider.Request{Model: "test-model", Messages: []model.Message{{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "inspect"}}}}}, func(event provider.StreamEvent) {
        if event.Type == provider.StreamTextDelta {
            deltas.WriteString(event.Text)
        }
    })
    if err != nil {
        t.Fatal(err)
    }
    if deltas.String() != "I will read. " || response.FinishReason != model.FinishToolCalls {
        t.Fatalf("unexpected response: deltas=%q response=%#v", deltas.String(), response)
    }
    call := response.Message.Blocks[1]
    if call.ToolCallID != "call-1" || call.ToolName != "read" || string(call.Arguments) != `{"path":"README.md"}` {
        t.Fatalf("bad tool call: %#v", call)
    }
}
```

Add tests for system prompts, assistant messages with tool calls, tool result messages, tool schema translation, multiple indexed calls, SSE comments, CRLF framing, an event larger than 64 KiB, malformed JSON, missing `[DONE]`, usage, unknown finish reasons, and cancellation.

- [ ] **Step 2: Write retry and redaction tests**

Verify:

- 429 and 503 retry at most twice before any stream data.
- `Retry-After: 0` is honored without sleeping materially.
- Connection errors retry.
- An error after a text delta does not retry.
- 401 does not retry.
- Error text never contains the API key or complete Authorization header.

Inject retry sleep through a private `sleep func(context.Context, time.Duration) error` field so tests do not wait.

- [ ] **Step 3: Run tests and verify failure**

```bash
go test ./internal/provider/openaicompat
```

Expected: FAIL because the adapter does not exist.

- [ ] **Step 4: Implement OpenAI wire translation and SSE parsing**

`protocol.go` owns all JSON structs. Translate neutral messages as follows:

- System prompt to a leading `system` message.
- User text blocks to a `user` message.
- Assistant text and tool calls to one `assistant` message with `tool_calls`.
- Every neutral tool-result block to an OpenAI `tool` message with `tool_call_id`.
- Neutral tool definitions to an object whose `type` is `function` and whose `function` object contains the exact `name`, `description`, and `parameters` fields.

The request always sets `stream: true` and `stream_options.include_usage: true`.

Parse SSE with `bufio.Reader.ReadString('\n')`, not `bufio.Scanner`, so large events work. Accumulate consecutive `data:` lines until the blank delimiter. Assemble tool calls by `index`, preserving first-seen order. Validate final tool arguments with `json.Valid`; return an error instead of handing malformed arguments to the agent.

- [ ] **Step 5: Implement bounded retry and HTTP behavior**

`New` validates and normalizes the base URL without stripping a path such as `/v1`. `Complete` POSTs to `<base-url>/chat/completions`, sets bearer authentication and JSON headers, and closes every response body.

Retry only connection errors, 429, and 5xx, with at most three total attempts. Respect integer-seconds or HTTP-date `Retry-After`; otherwise use bounded exponential delays of 250 ms then 500 ms. Once any text or tool-call stream event is emitted, return subsequent errors without retrying.

Limit non-success response bodies to 32 KiB and redact the configured API key before formatting an error.

- [ ] **Step 6: Run provider tests and race checks**

```bash
gofmt -w internal/provider/openaicompat
go test ./internal/provider/openaicompat
go test -race ./internal/provider/openaicompat
```

Expected: PASS.

- [ ] **Step 7: Commit the provider adapter**

```bash
git add internal/provider/openaicompat
git commit -m "feat: add OpenAI-compatible streaming"
```

---

### Task 8: Implement the Agent and Tool Loop

**Files:**
- Create: `internal/agent/events.go`
- Create: `internal/agent/agent.go`
- Create: `internal/agent/agent_test.go`

**Interfaces:**
- Consumes: `provider.Provider`, `tool.Registry`, `session.Session`, and neutral model types.
- Produces: `agent.New`, `(*Agent).Run`, and typed `agent.Event` values.

- [ ] **Step 1: Write scripted-provider tests**

Define a fake provider that records requests and returns scripted responses. Test this complete sequence:

```go
func TestRunExecutesToolAndReturnsToProvider(t *testing.T) {
    fakeProvider := &scriptedProvider{responses: []provider.Response{
        {
            Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{
                {Type: model.BlockText, Text: "checking"},
                {Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "echo", Arguments: json.RawMessage(`{"value":"hello"}`)},
            }},
            FinishReason: model.FinishToolCalls,
        },
        {
            Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}},
            FinishReason: model.FinishStop,
        },
    }}
    registry, _ := tool.NewRegistry(echoTool{})
    memory := session.NewMemory(session.Header{Version: 1, ID: "test", Workspace: t.TempDir(), Provider: "test", Model: "test"})
    runner := New(fakeProvider, registry, memory, Options{Model: "test", SystemPrompt: "system", MaxTurns: 20, Now: fixedClock, NewID: fixedIDs()})

    var events []Event
    if err := runner.Run(context.Background(), "inspect", func(event Event) { events = append(events, event) }); err != nil {
        t.Fatal(err)
    }
    messages := memory.Messages()
    if len(messages) != 4 || messages[0].Role != model.RoleUser || messages[1].Role != model.RoleAssistant || messages[2].Role != model.RoleTool || messages[3].Role != model.RoleAssistant {
        t.Fatalf("unexpected message sequence: %#v", messages)
    }
    if len(fakeProvider.requests) != 2 || fakeProvider.requests[1].Messages[2].Blocks[0].Text != "hello" {
        t.Fatalf("unexpected provider requests: %#v", fakeProvider.requests)
    }
}

type scriptedProvider struct {
    responses []provider.Response
    requests  []provider.Request
}

func (p *scriptedProvider) Complete(_ context.Context, request provider.Request, emit func(provider.StreamEvent)) (provider.Response, error) {
    p.requests = append(p.requests, request)
    if len(p.responses) == 0 {
        return provider.Response{}, errors.New("no scripted response")
    }
    response := p.responses[0]
    p.responses = p.responses[1:]
    for _, block := range response.Message.Blocks {
        if block.Type == model.BlockText && block.Text != "" {
            emit(provider.StreamEvent{Type: provider.StreamTextDelta, Text: block.Text})
        }
    }
    return response, nil
}

type echoTool struct{}

func (echoTool) Definition() model.ToolDefinition {
    return model.ToolDefinition{Name: "echo", Parameters: map[string]any{"type": "object", "required": []string{"value"}}}
}

func (echoTool) Execute(_ context.Context, arguments json.RawMessage) tool.Result {
    var args struct {
        Value string `json:"value"`
    }
    if err := json.Unmarshal(arguments, &args); err != nil {
        return tool.Result{Content: err.Error(), IsError: true}
    }
    return tool.Result{Content: args.Value}
}

func fixedClock() time.Time {
    return time.Unix(10, 0).UTC()
}

func fixedIDs() func() string {
    next := 0
    return func() string {
        next++
        return fmt.Sprintf("id-%d", next)
    }
}
```

Also test: text-delta forwarding, tool start/end events, unknown tool results, invalid arguments delegated to the tool, sequential execution of two calls, maximum-turn failure, provider failure, provider cancellation, persistence failure before provider invocation, assistant persistence before tool execution, and tool result persistence before the next provider call.

- [ ] **Step 2: Run tests and verify failure**

```bash
go test ./internal/agent
```

Expected: FAIL because the agent package does not exist.

- [ ] **Step 3: Define agent events and options**

Use:

```go
type EventType string

const (
    EventAgentStarted     EventType = "agent_started"
    EventAgentFinished    EventType = "agent_finished"
    EventTextDelta        EventType = "text_delta"
    EventToolCallStarted  EventType = "tool_call_started"
    EventToolCallFinished EventType = "tool_call_finished"
    EventProviderUsage    EventType = "provider_usage"
    EventAgentError       EventType = "agent_error"
)

type Event struct {
    Type       EventType
    Text       string
    ToolName   string
    ToolCallID string
    ToolResult tool.Result
    Usage      model.Usage
    Err        error
}

type Options struct {
    Model        string
    SystemPrompt string
    MaxTurns     int
    Now          func() time.Time
    NewID        func() string
}
```

Production defaults use `time.Now().UTC()` and 16 random bytes encoded as lowercase hexadecimal IDs.

- [ ] **Step 4: Implement the loop with durable boundaries**

`Run` rejects empty user text, emits `EventAgentStarted`, persists the user message, then performs provider turns. Copy session history and tool definitions into every request. Convert provider stream events to agent events.

After each provider response:

1. Fill missing assistant ID and timestamp.
2. Copy the response finish reason into `Message.FinishReason`; when either token count is non-zero, copy usage into `Message.Usage`.
3. Persist the complete assistant message, including finish reason and usage.
4. Emit usage.
5. Execute assistant tool-call blocks in block order.
6. Emit start and finish events for each call.
7. Persist one `RoleTool` message per result, with the originating call ID and tool name.
8. Continue only if at least one tool call existed.

On any error, emit `EventAgentError` and return the same error. Emit `EventAgentFinished` only after a normal stop. Return a specific `ErrMaxTurns` after the configured number of provider calls.

- [ ] **Step 5: Run agent tests and race checks**

```bash
gofmt -w internal/agent
go test ./internal/agent
go test -race ./internal/agent
```

Expected: PASS.

- [ ] **Step 6: Commit the agent loop**

```bash
git add internal/agent
git commit -m "feat: add durable agent tool loop"
```

---

### Task 9: Wire the CLI, REPL, Sessions, and Cancellation

**Files:**
- Create: `internal/repl/repl.go`
- Create: `internal/repl/repl_test.go`
- Create: `cmd/otto/main.go`
- Create: `cmd/otto/main_test.go`

**Interfaces:**
- Consumes: all Stage 1 packages.
- Produces: the `otto` executable and `/help`, `/exit`, `/new`, and `/session` commands.

- [ ] **Step 1: Write REPL tests**

Inject an `AgentRunner` interface rather than constructing concrete dependencies in tests:

```go
type AgentRunner interface {
    Run(context.Context, string, func(agent.Event)) error
}
```

Test:

```go
func TestREPLRunsPromptAndRendersEvents(t *testing.T) {
    input := strings.NewReader("inspect files\n/exit\n")
    var output bytes.Buffer
    runner := &fakeRunner{run: func(ctx context.Context, prompt string, emit func(agent.Event)) error {
        if prompt != "inspect files" {
            t.Fatalf("prompt = %q", prompt)
        }
        emit(agent.Event{Type: agent.EventTextDelta, Text: "done"})
        emit(agent.Event{Type: agent.EventToolCallStarted, ToolName: "read", ToolCallID: "call-1"})
        emit(agent.Event{Type: agent.EventToolCallFinished, ToolName: "read", ToolCallID: "call-1", ToolResult: tool.Result{Content: "read README.md"}})
        return nil
    }}
    repl := New(input, &output, &output, runner, Info{SessionID: "session-1", SessionPath: "/tmp/session.jsonl", Provider: "openai-compatible", Model: "test"})
    if err := repl.Run(context.Background()); err != nil {
        t.Fatal(err)
    }
    rendered := output.String()
    for _, expected := range []string{"done", "read", "session-1"} {
        if !strings.Contains(rendered, expected) {
            t.Fatalf("output missing %q: %s", expected, rendered)
        }
    }
}

type fakeRunner struct {
    run func(context.Context, string, func(agent.Event)) error
}

func (f *fakeRunner) Run(ctx context.Context, prompt string, emit func(agent.Event)) error {
    return f.run(ctx, prompt, emit)
}
```

Also test EOF, blank lines, `/help`, `/session`, unknown commands, provider errors, scanner input up to 1 MiB, output separation between turns, active-turn `Interrupt` cancellation, and `Interrupt` returning false while idle.

- [ ] **Step 2: Write CLI resolution tests**

Refactor `cmd/otto/main.go` around:

```go
func run(ctx context.Context, args []string, stdin io.Reader, stdout, stderr io.Writer, getenv func(string) string) int
```

Test exit codes and messages for `--help`, missing API key, invalid profile, unsupported provider, missing resume file, `--no-session`, explicit `--config`, `--cwd`, `--max-turns`, `--shell-timeout`, and conflicting `--continue` plus `--resume`.

- [ ] **Step 3: Run tests and verify failure**

```bash
go test ./internal/repl ./cmd/otto
```

Expected: FAIL because the REPL and command do not exist.

- [ ] **Step 4: Implement REPL rendering and commands**

Define the public REPL construction and interruption surface:

```go
type Info struct {
    SessionID   string
    SessionPath string
    Provider    string
    Model       string
}

type REPL struct {
    stdin        io.Reader
    stdout       io.Writer
    stderr       io.Writer
    runner       AgentRunner
    info         Info
    mu           sync.Mutex
    activeCancel context.CancelFunc
}

func New(stdin io.Reader, stdout, stderr io.Writer, runner AgentRunner, info Info) *REPL
func (r *REPL) Run(context.Context) error
func (r *REPL) Interrupt() bool
```

For each non-command prompt, `Run` creates a child context, stores its cancel function while the runner is active, and clears it afterward. `Interrupt` cancels and clears an active turn under the mutex and returns `true`; it returns `false` while idle.

Use `bufio.Scanner` with an explicit 1 MiB maximum token buffer. Terminal detection is unnecessary in Stage 1; always print `> ` before scanning so pipes remain deterministic.

Rendering rules:

- `TextDelta`: write text verbatim.
- Tool start: write `\n[tool] <name> (<call-id>)\n`.
- Tool finish: write `[tool result] <first summary line>\n` and do not dump full file or shell output twice.
- Agent errors: write to stderr.
- End each completed turn with a newline.

`/new` returns a sentinel `ErrNewSession` to the command layer, which closes the current store, creates another session, and constructs a fresh agent without restarting the process. `/session` prints ID, path, provider, and model.

- [ ] **Step 5: Implement CLI construction and signal handling**

Parse flags with a dedicated `flag.FlagSet`. Resolve the workspace to a canonical path before tools or sessions are created. Select:

- `session.NewMemory` for `--no-session`.
- `session.Open` for `--resume`.
- The most recently modified JSONL file under the current workspace key for `--continue`.
- `session.Create` otherwise.

Construct the workspace, four built-in tools, registry, OpenAI-compatible client, agent, and REPL. Resolve the shell from `SHELL`, falling back to `/bin/sh`, and reject a shell path that is missing or not executable. Use `~/.otto/sessions` as the root passed to `session.Create`; session storage is independent of the TOML path.

Use `signal.Notify` for `os.Interrupt`. A signal goroutine calls `currentREPL.Interrupt()`; when it returns `false`, cancel the process context so the command exits with code 130. The first interrupt during a turn therefore cancels only that turn and returns to the REPL. Protect replacement of `currentREPL` during `/new` with the command layer's mutex. Ensure `signal.Stop` and session `Close` execute on all paths.

Use this Stage 1 system prompt as a constant in `cmd/otto/main.go`:

```text
You are Otto, a concise coding agent. Inspect the workspace before changing it. Use read, write, edit, and bash when needed. File tools are restricted to the workspace, but bash is unsandboxed. Prefer exact, minimal changes. Report what changed and what verification ran.
```

- [ ] **Step 6: Run CLI tests and a local mock smoke test**

```bash
gofmt -w internal/repl cmd/otto
go test ./internal/repl ./cmd/otto
go build -o ./otto ./cmd/otto
```

Start a local `httptest`-style fixture from the CLI test that first asks for a `write` tool call and then returns final text. Run `run` against it and assert that the file is created inside a temporary workspace and the JSONL session contains user, assistant, tool, and final assistant messages.

Expected: all tests PASS and `./otto --help` exits successfully.

- [ ] **Step 7: Commit the executable**

```bash
git add internal/repl cmd/otto
git commit -m "feat: add Otto command-line REPL"
```

---

### Task 10: Document Stage 1 and Run Completion Gates

**Files:**
- Create: `README.md`
- Create: `AGENTS.md`
- Modify: `docs/superpowers/specs/2026-08-26-otto-coding-agent-design.md` only if implementation reveals an approved-spec correction.

**Interfaces:**
- Consumes: the complete Stage 1 command behavior.
- Produces: installation, configuration, safety, testing, and contribution documentation.

- [ ] **Step 1: Write a README verification checklist before prose**

Create a temporary checklist in the implementation notes and verify every documented command exists:

```text
go build -o otto ./cmd/otto
OTTO_API_KEY=test otto --provider openai-compatible --base-url http://localhost:8080/v1 --model test-model --no-session
otto --config ~/.config/otto/config.toml --profile deepseek
otto --continue
otto --resume /absolute/path/to/session.jsonl
```

Do not claim Codex or Claude works in Stage 1. List them under “Planned providers.”

- [ ] **Step 2: Write README and contributor guidance**

`README.md` must contain:

- Stage 1 feature list and exclusions
- macOS and Go 1.26 prerequisites
- Build and test commands
- The approved TOML example
- Environment and flag precedence
- REPL commands and `Ctrl+C` active-turn cancellation behavior
- Session locations and resume examples
- The explicit warning that `bash` is unsandboxed
- The guarantee that file tools reject workspace escapes
- Troubleshooting for API-key, endpoint, SSE, and context-length failures
- A roadmap linking Stage 2 Codex and Stage 3 Claude work to the design spec

`AGENTS.md` must replace the old TypeScript guidance with exact Go commands, package boundaries, TDD expectations, secret-handling rules, and the rule that live provider tests are opt-in.

- [ ] **Step 3: Run formatting and unit tests**

```bash
test -z "$(gofmt -l .)"
go test ./...
```

Expected: PASS with no listed unformatted files.

- [ ] **Step 4: Run race, vet, and Staticcheck gates**

```bash
go test -race ./...
go vet ./...
staticcheck ./...
```

Expected: all commands exit 0.

- [ ] **Step 5: Build and perform offline CLI checks**

```bash
go build -trimpath -o ./otto ./cmd/otto
./otto --help
./otto --provider openai-compatible --model test --base-url http://127.0.0.1:1/v1 --no-session </dev/null
rm ./otto
git diff --check
git status --short
```

Expected: build succeeds; help succeeds; EOF exits cleanly without attempting a provider request; no generated binary remains; diff check succeeds.

- [ ] **Step 6: Commit documentation**

```bash
git add README.md AGENTS.md docs go.mod go.sum
git commit -m "docs: document Otto Stage 1"
```

- [ ] **Step 7: Review history without pushing**

```bash
git log --oneline --decorate --reverse origin/main..main
git status --short --branch
git diff --stat origin/main main
```

Expected: the working tree is clean and the fresh Go history contains the approved design plus focused Stage 1 commits. Do not force-push in this task. Use the branch-finishing workflow for final review, then update `origin/main` with `--force-with-lease` only after explicit verification of the expected remote tip.
