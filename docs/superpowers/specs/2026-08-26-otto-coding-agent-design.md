# Otto Coding Agent MVP Design

**Date:** 2026-08-26  
**Status:** Approved

## 1. Purpose

Otto is a minimal terminal coding agent written in Go for macOS. It follows Pi's design principle of keeping the agent core small while isolating model-provider details behind clear interfaces.

The first usable milestone provides a simple streaming REPL, coding tools, linear JSONL sessions, and an OpenAI-compatible Chat Completions provider. Later milestones add Codex and Claude subscription authentication without changing the agent loop.

## 2. Scope

### Included

- A macOS command-line executable named `otto`
- A line-oriented REPL using standard input and output
- Provider-neutral messages, tool calls, usage, and streaming events
- OpenAI-compatible Chat Completions with configurable endpoint, model, and API-key environment variable
- Codex subscription support through OAuth and the Codex Responses protocol
- Claude Pro/Max support through OAuth and the Anthropic Messages protocol
- `read`, `write`, `edit`, and `bash` tools
- Workspace-bound filesystem tools
- An unsandboxed shell that starts in the workspace
- Append-only JSONL session persistence and resume
- Global TOML configuration with named profiles
- Unit, contract, integration, race, and static-analysis checks

### Excluded

- A full-screen TUI
- Windows and Linux support commitments
- Plugins or extensions
- Skills and prompt-template discovery
- Context compaction
- Session trees or branching
- Sub-agents
- Permission popups
- Project-local configuration
- Simultaneous sessions in one process

## 3. Delivery Stages

### Stage 1: Core and OpenAI-compatible provider

- Neutral model and provider contracts
- Streaming agent/tool loop
- OpenAI-compatible Chat Completions adapter
- API-key authentication through an environment variable
- REPL, built-in tools, TOML configuration, and JSONL sessions

### Stage 2: Codex subscription

- ChatGPT Plus/Pro OAuth
- Browser login and headless device-code login
- Automatic token refresh
- Codex Responses adapter
- A static, tested model catalog

### Stage 3: Claude subscription

- Claude Pro/Max OAuth
- Browser login with manual redirect-URL fallback
- Automatic token refresh
- Anthropic Messages adapter
- A static, tested model catalog
- A clear warning that third-party harness usage may use Anthropic extra usage rather than normal plan limits

Each stage must pass its complete verification suite before work starts on the next stage.

## 4. Architecture

```text
cmd/otto
   |
   +-- config
   +-- repl --------------------+
   |                            | typed events
   +-- agent loop --------------+
          |
          +-- provider registry
          |      +-- openai-compatible adapter
          |      +-- codex adapter
          |      +-- claude adapter
          |
          +-- credential manager
          |      +-- API-key resolver
          |      +-- Codex OAuth
          |      +-- Claude OAuth
          |
          +-- tool registry
          |      +-- read
          |      +-- write
          |      +-- edit
          |      +-- bash
          |
          +-- session store
                 +-- JSONL
```

### Package boundaries

- `cmd/otto`: CLI entry point, subcommands, dependency construction, and process lifecycle.
- `internal/model`: Provider-neutral messages, content blocks, tool definitions, finish reasons, and usage.
- `internal/provider`: Provider interface, registry, request types, and streaming event types.
- `internal/provider/openaicompat`: OpenAI-compatible HTTP and SSE translation.
- `internal/provider/codex`: Codex Responses translation.
- `internal/provider/anthropic`: Anthropic Messages translation.
- `internal/auth`: Credential contracts, secure storage, refresh coordination, and OAuth implementations.
- `internal/agent`: Model-to-tool orchestration loop.
- `internal/tool`: Tool interface, registry, validation, and built-in tools.
- `internal/session`: Append-only persistence, loading, validation, and interrupted-call repair.
- `internal/repl`: Prompt input and event rendering.
- `internal/config`: TOML, environment, flag, profile, and default resolution.

Dependencies point toward neutral model types. Provider-specific request and response structures cannot enter the agent, tool, session, or REPL packages.

## 5. Provider and Agent Contracts

The provider-neutral interface is conceptually:

```go
type Provider interface {
    Complete(
        ctx context.Context,
        req CompletionRequest,
        emit func(StreamEvent),
    ) (AssistantMessage, error)
}
```

`CompletionRequest` contains the model identifier, system prompt, conversation messages, available tool schemas, and optional generation settings.

`AssistantMessage` contains neutral text and tool-call content blocks, usage metadata, and a finish reason.

Provider adapters own all protocol-specific work, including:

- Authentication headers
- HTTP request and response structures
- SSE parsing
- Text delta emission
- Assembly of fragmented tool-call arguments
- Translation of finish reasons and usage

### Agent loop

For each user prompt, the agent:

1. Appends and persists the user message.
2. Sends the current history and tool schemas to the selected provider.
3. Streams provider events to the REPL.
4. Appends and persists the completed assistant message.
5. If the assistant requested tools, validates and executes each call sequentially.
6. Appends and persists each tool result.
7. Repeats the provider call until the provider stops requesting tools.

### Application events

The core emits lightweight typed events:

- `AgentStarted`
- `AgentFinished`
- `TextDelta`
- `ToolCallStarted`
- `ToolCallFinished`
- `ProviderUsage`
- `AgentError`

The REPL renders these events but the core has no terminal dependency. This permits a future TUI or RPC adapter without introducing a general event framework now.

## 6. Authentication

Transport and authentication are separate concerns. An `Authenticator` performs login and refresh. A `CredentialManager` resolves credentials for adapters.

Credentials are stored at:

```text
~/.otto/auth.json
```

Requirements:

- Create the file with mode `0600`.
- Write updates atomically through a temporary file and rename.
- Never store credentials in configuration or session files.
- Never print credentials or authorization headers.
- Refresh expired OAuth tokens automatically.
- Deduplicate concurrent refreshes for the same provider.
- Preserve a valid existing credential if refresh persistence fails.

CLI commands are:

```text
otto auth login codex
otto auth login claude
otto auth status
otto auth logout codex
otto auth logout claude
```

OAuth flows use PKCE, verify callback state, open the system browser on macOS, support cancellation, and permit manual redirect-URL entry when a loopback callback is unavailable. Codex also supports device-code login for headless use.

OAuth endpoints, public-client identifiers, scopes, model catalogs, and upstream protocol details remain confined to their provider/auth packages because they can change independently.

## 7. Tools and Safety

The tool interface is conceptually:

```go
type Tool interface {
    Definition() model.ToolDefinition
    Execute(ctx context.Context, args json.RawMessage) ToolResult
}
```

The registry rejects unknown tools. Each tool owns argument decoding and validation.

### `read`

- Reads UTF-8 text files.
- Supports line offset and limit.
- Rejects binary content.
- Truncates output above the configured limit and reports truncation.

### `write`

- Creates or replaces a file.
- Creates missing parent directories.
- Writes through a temporary file followed by an atomic rename.

### `edit`

- Performs exact text replacement.
- Requires the target text to occur exactly once.
- Rejects missing or ambiguous matches.
- Returns a concise change summary.

### `bash`

- Executes `<configured-shell> -lc <command>`.
- Starts in the selected workspace.
- Captures stdout, stderr, and exit status.
- Applies a configurable timeout.
- Terminates the process group on cancellation or timeout.

### Workspace boundary

Filesystem tools canonicalize paths, account for existing and not-yet-existing path components, resolve symlinks, and reject escapes from the initial workspace root. String-prefix checks alone are insufficient.

The shell is intentionally not sandboxed. Commands can access anything available to the current macOS user even though they start in the workspace. Otto displays this fact in startup help.

Default limits:

- Tool output: 50 KiB
- Shell timeout: 120 seconds

## 8. Configuration

The default configuration path is:

```text
~/.config/otto/config.toml
```

A caller may select another global configuration file with `--config <path>`. Otto never discovers project-local configuration automatically.

Example:

```toml
default_profile = "deepseek"

[agent]
shell_timeout = "120s"
max_output_bytes = 51200

[profiles.deepseek]
provider = "openai-compatible"
base_url = "https://api.deepseek.com/v1"
model = "deepseek-chat"
api_key_env = "DEEPSEEK_API_KEY"

[profiles.codex]
provider = "codex"
model = "gpt-5.3-codex"

[profiles.claude]
provider = "claude"
model = "claude-sonnet-4-6"
```

Raw secrets are invalid in TOML. OpenAI-compatible profiles name the environment variable containing the API key. Subscription credentials come only from the credential store.

Unknown fields, missing profiles, invalid durations, unavailable credentials, and unsupported provider/model combinations fail before the REPL starts.

### Resolution precedence

Highest to lowest:

1. Command-line flags, including a profile explicitly selected with `--profile`
2. Environment variables
3. Provider and model recorded by a resumed session
4. The default configuration profile
5. Built-in defaults

An explicit `--profile` selects that profile's provider and model over resumed-session metadata. Merely changing `default_profile` does not silently change an existing session.

Principal flags include:

- `--config <path>`
- `--profile <name>`
- `--provider <id>`
- `--model <id>`
- `--base-url <url>`
- `--cwd <path>`
- `--continue`
- `--resume <file>`
- `--no-session`
- `--shell-timeout <duration>`

`OTTO_PROVIDER` and `OTTO_MODEL` provide environment-level overrides. `OTTO_API_KEY` is the fallback API-key variable when an OpenAI-compatible profile does not specify `api_key_env`.

## 9. REPL

- Start a new session by default.
- Accept one user prompt per line.
- Stream assistant text directly to standard output.
- Display compact tool start and result summaries.
- Use `Ctrl+C` to cancel an active provider request or tool.
- Use EOF or `/exit` to terminate.
- Support `/help`, `/exit`, `/new`, and `/session`.
- Keep the selected provider and model fixed for the active session.

The simple REPL must not own agent state or provider logic.

## 10. Session Persistence

Sessions are stored under:

```text
~/.otto/sessions/<workspace-key>/<session-id>.jsonl
```

The format is append-only and contains:

- A versioned session header
- Workspace and session identity
- Provider, profile, and model metadata
- User messages
- Completed assistant messages and tool calls
- Tool results
- Usage, timestamps, and stable record IDs

Partial streamed responses are not persisted. If loading finds a completed assistant tool call without a matching result, the loader appends a synthetic result stating that execution was interrupted. A malformed trailing record caused by an interrupted append is reported and recoverable only when it is the final record; corruption earlier in the file fails safely.

The MVP history is linear. Stable IDs permit a later schema version to add parent IDs and branching.

## 11. Error Handling

- Configuration and authentication errors fail before entering the REPL.
- Provider errors display a concise redacted message and leave the session usable.
- Tool errors become structured tool results so the model can recover.
- Persistence failures stop prompt acceptance when durable history is enabled.
- Cancellation propagates through provider requests and tools.

Provider calls retry at most twice for rate limits, connection failures, and retryable 5xx responses. Otto respects `Retry-After`. It does not retry a stream after visible text or tool-call data arrives, avoiding duplicate output or tool execution.

Error formatting and diagnostic logging redact authorization headers, API keys, access tokens, refresh tokens, and sensitive OAuth query parameters.

## 12. Testing and Verification

### Unit and contract tests

- Scripted fake providers and tools verify agent sequencing, cancellation, and recovery.
- Local HTTP/SSE servers verify fragmented stream handling, tool-call assembly, malformed payloads, status codes, and retries.
- Temporary workspaces verify path traversal rejection, symlink escape prevention, exact edits, atomic writes, and output limits.
- Process tests verify shell timeout, cancellation, and process-group termination on macOS.
- Session tests verify append/load, trailing-write recovery, dangling tool-call repair, and version rejection.
- Auth tests verify PKCE, state validation, refresh deduplication, atomic storage, file permissions, cancellation, and redaction.
- CLI tests verify TOML parsing, precedence, profiles, startup validation, and exit codes.

### Live tests

Live provider and OAuth tests are opt-in, require explicit environment flags, and never run as part of the default test suite.

### Completion gates

Each delivery stage must pass:

```text
gofmt check
go test ./...
go test -race ./...
go vet ./...
staticcheck ./...
```

A stage is not complete based only on compilation or mocked happy-path tests.
