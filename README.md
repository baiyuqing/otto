# Otto

Otto is a minimal macOS coding agent written in Go.

Stage 1 ships a line-oriented REPL, append-only JSONL sessions, workspace-bound file tools, an unsandboxed `bash` tool, TOML profiles, and an OpenAI-compatible Chat Completions provider.

## Stage 1 status

### Included

- `otto` CLI for macOS
- OpenAI-compatible provider support only
- Streaming REPL with `/help`, `/exit`, `/new`, and `/session`
- Built-in `read`, `write`, `edit`, and `bash` tools
- Persistent JSONL sessions with `--continue` and `--resume`
- Global TOML configuration at `~/.config/otto/config.toml`

### Excluded in Stage 1

- Codex subscription login
- Claude subscription login
- TUI/full-screen interface
- Plugins, skills, or project-local config
- Windows or Linux support commitments

### Planned providers

- Stage 2: Codex subscription support
- Stage 3: Claude subscription support

See the approved design: [`docs/superpowers/specs/2026-08-26-otto-coding-agent-design.md`](docs/superpowers/specs/2026-08-26-otto-coding-agent-design.md).

## Prerequisites

- macOS
- Go 1.26+
- A reachable OpenAI-compatible endpoint with SSE chat-completions streaming
- An API key exposed through an environment variable

## Build

```bash
go build -trimpath -o ./otto ./cmd/otto
```

You can then run `./otto ...` from the repo root. If you place the binary on your `PATH`, the same commands work as `otto ...`.

## Quick start

Create `~/.config/otto/config.toml`:

```toml
default_profile = "deepseek"

[agent]
max_turns = 20
shell_timeout = "120s"
max_output_bytes = 51200

[profiles.deepseek]
provider = "openai-compatible"
base_url = "https://api.deepseek.com/v1"
model = "deepseek-chat"
api_key_env = "DEEPSEEK_API_KEY"
```

Then export the selected profile's key and start Otto:

```bash
export DEEPSEEK_API_KEY=your-key
./otto --config ~/.config/otto/config.toml --profile deepseek
```

For an ad hoc run without a config file:

```bash
OTTO_API_KEY=your-key ./otto \
  --provider openai-compatible \
  --base-url https://api.deepseek.com/v1 \
  --model deepseek-chat \
  --no-session
```

## Configuration and precedence

Otto auto-discovers only the global config file at `~/.config/otto/config.toml`. It does not auto-discover project-local configuration, but you can explicitly select any path with `--config`.

Default path:

```text
~/.config/otto/config.toml
```

Resolution rules:

1. CLI flags, including an explicit `--profile`
2. Environment variables: `OTTO_PROVIDER`, `OTTO_MODEL`
3. Provider/model stored in a resumed session
4. `default_profile` from TOML
5. Built-in defaults for agent limits

Additional rules:

- `--base-url` overrides the profile base URL.
- The API key comes from the selected profile's `api_key_env`, with `OTTO_API_KEY` as fallback.
- `--continue` and `--resume` reuse the active session's provider/model unless you explicitly select a different profile.
- `--no-session` cannot be combined with `--continue` or `--resume`.
- Raw secrets do not belong in TOML.

## REPL behavior

- Otto accepts one prompt per line.
- `/help` shows commands.
- `/session` prints the current session ID, path, provider, and model.
- `/new` closes the current session and starts a fresh one in the same process.
- `/exit` or EOF exits.
- `Ctrl+C` during an active provider call or tool run cancels only that turn and returns you to the prompt.
- `Ctrl+C` while Otto is idle exits with status 130.

## Sessions

Persistent sessions live under:

```text
~/.otto/sessions/<workspace-key>/<session-id>.jsonl
```

Examples:

```bash
./otto --continue
./otto --resume /absolute/path/to/session.jsonl
./otto --no-session
```

Notes:

- `--continue` reopens the newest session for the current canonical workspace.
- `--resume` reopens a specific session file, but only if its recorded workspace matches the current `--cwd`.
- `/session` shows the exact session path.
- `--no-session` keeps history in memory only.

## Tools and safety

### File tools

- `read`, `write`, and `edit` are restricted to the initial workspace.
- Otto canonicalizes paths, resolves symlinks, and rejects workspace escapes.
- `edit` requires exactly one exact text match.

### `bash` warning

`bash` is intentionally **unsandboxed**.

It starts in the selected workspace, but commands run as your current macOS user and can access anything that user can access. Treat Otto as trusted local automation, not a sandbox.

Default limits:

- 20 model turns per user prompt
- 120 second shell timeout
- 50 KiB tool output cap

## Build and test commands

```bash
test -z "$(gofmt -l .)"
go test ./...
go test -race ./...
go vet ./...
go run honnef.co/go/tools/cmd/staticcheck@latest ./...
go build -trimpath -o ./otto ./cmd/otto
```

`go run ...staticcheck@latest` is the safest Stage 1 gate when a globally installed `staticcheck` binary was built with an older Go release.

## Troubleshooting

### `otto: missing api key`

Export the environment variable named by the selected profile's `api_key_env`, or set `OTTO_API_KEY` as a fallback.

### `otto: missing base_url`, `otto: invalid base_url`, or request failures

Check the selected profile, `--base-url`, and endpoint path. Stage 1 expects an HTTP(S) OpenAI-compatible base URL and posts to `<base-url>/chat/completions`.

### `read chat completion stream: ...` or `chat completion stream ended without [DONE]`

Your provider or proxy is not delivering valid SSE chat-completions output. Confirm streaming is enabled, SSE is not buffered or rewritten, and the upstream really emits `[DONE]`.

### `OpenAI-compatible HTTP 400: ...` with context-length or prompt-size wording

Stage 1 forwards the provider's bounded error body. Start a fresh session with `/new`, shorten the prompt, or reduce large file/tool output in the conversation.

## Contributing

See [`AGENTS.md`](AGENTS.md) for Go-specific contributor instructions.

## Roadmap

- Stage 1: current OpenAI-compatible MVP
- Stage 2: Codex subscription auth and provider adapter
- Stage 3: Claude subscription auth and provider adapter

The full staged design lives in [`docs/superpowers/specs/2026-08-26-otto-coding-agent-design.md`](docs/superpowers/specs/2026-08-26-otto-coding-agent-design.md).
