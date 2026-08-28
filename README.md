# Otto

Otto is a minimal macOS coding agent written in Go.

Stage 1 ships adaptive frontends (a full-screen Charmbracelet TUI or a line-oriented REPL), append-only JSONL sessions, workspace-bound file tools, an unsandboxed `bash` tool, TOML profiles, and an OpenAI-compatible Chat Completions provider.

## Stage 1 status

### Included

- `otto` CLI for macOS
- OpenAI-compatible provider support only
- Adaptive UI selection: full-screen TUI on terminal stdin/stdout, REPL otherwise
- Streaming TUI and REPL with `/help`, `/exit`, `/new`, and `/session`; the TUI also provides `/resume`
- Markdown assistant rendering and collapsible tool output in the TUI
- Built-in `read`, `grep`, `find`, `ls`, `write`, `edit`, and `bash` tools
- Persistent JSONL sessions with `--continue` and `--resume`
- Global TOML configuration at `~/.config/otto/config.toml`
- `--thinking` pass-through for model reasoning effort (`low`, `medium`, `high`, `xhigh`, `max`)
- `--approve` headless mode for non-interactive single-prompt runs

### Excluded in Stage 1

- Codex subscription login
- Claude subscription login
- Plugins, skills, or project-local config
- Windows or Linux support commitments
- Session trees/forks, session naming, deletion, or search

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

[ui]
mode = "auto"

[agent]
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

Set model thinking effort with `--thinking`:

```bash
./otto --thinking high
```

Run a single prompt headless with `--approve` and exit:

```bash
./otto --approve "summarize TODOs in this repo"
./otto --approve @prompt.txt --no-session
./otto --approve "explain main.go" --thinking max --continue
```

## Configuration and precedence

Otto auto-discovers only the global config file at `~/.config/otto/config.toml`. It does not auto-discover project-local configuration, but you can explicitly select any path with `--config`.

Default path:

```text
~/.config/otto/config.toml
```

Startup resolution is field-specific:

- **Profile:** explicit `--profile` selects a profile. Otherwise a startup `--continue` or `--resume` uses the session's stored profile when present; a new session (or an external Pi session without an Otto profile) uses `default_profile`.
- **Provider and model:** explicit `--provider` / `--model` override `OTTO_PROVIDER` / `OTTO_MODEL`. Those environment variables override the selected profile and any provider/model stored in a startup-resumed session. Without direct or environment overrides, startup resume uses the stored provider/model; however, explicit `--profile` makes that profile's provider/model the baseline instead. An explicit profile does **not** outrank `OTTO_PROVIDER` or `OTTO_MODEL`.
- **Endpoint:** `--base-url` overrides the selected profile's `base_url`. There is no base-URL environment override, and session files do not supply an endpoint.
- **API key:** the selected profile determines `api_key_env`. A nonempty value from that environment variable wins; `OTTO_API_KEY` is its fallback. API keys have no CLI flag and must not be stored in TOML.
- **Agent limits:** direct `--shell-timeout` and `--max-output-bytes` values override `[agent]` values, which override built-in defaults. Profiles and resumed sessions do not contain these limits.
- **Thinking effort:** `--thinking` (`low`, `medium`, `high`, `xhigh`, or `max`) is sent as `reasoning_effort` on OpenAI-compatible requests. It has no environment variable or TOML key and is omitted from requests when unset. Like agent limits, it stays in effect across in-process `/resume` and `/new`.

Startup `--continue` / `--resume` therefore restores session provider/model only as defaults: direct flags and `OTTO_PROVIDER` / `OTTO_MODEL` can override them as described above. In contrast, an in-process TUI `/resume` restores the selected session's stored provider/model and ignores the process's provider/model/profile/base-URL overrides and `OTTO_PROVIDER` / `OTTO_MODEL`; its stored profile selects the endpoint and key environment. Agent-limit overrides remain in effect. `/new` returns to the runtime resolved at process startup.

`--no-session` cannot be combined with `--continue` or `--resume`.

UI mode precedence:

1. `--ui`
2. `OTTO_UI`
3. `[ui].mode` in TOML
4. built-in `auto`

## Frontends

Examples:

```bash
otto --ui auto
otto --ui tui
otto --ui repl
OTTO_UI=repl otto
```

Selection rules:

- `auto` starts the full-screen TUI only when **both** stdin and stdout are terminals.
- `auto` falls back to the line-oriented REPL for piped input, redirected output, and other non-TTY runs.
- `tui` forces the full-screen UI and fails fast if stdin or stdout is not a terminal.
- `repl` forces the line-oriented REPL even from an interactive terminal.
- In `auto`, non-TTY runs stay in the REPL and do not emit alternate-screen control sequences.

### Headless mode

`--approve` runs a single prompt without interaction and exits: `0` on success, `1` on error, `130` on interrupt. The value is the prompt text, or `@PATH` to read the prompt from a file. Output uses the REPL's line-oriented rendering without the banner or input prompts. Otto's tools never require interactive approval; `--approve` only supplies the prompt and removes the interactive loop. It cannot be combined with `--ui tui`, and it composes with `--continue`, `--resume`, and `--no-session`.

```bash
otto --approve "summarize TODOs in this repo"
otto --approve @prompt.txt --no-session
```

### TUI behavior

- The TUI uses the terminal alternate screen buffer.
- Assistant responses render as Markdown in the transcript.
- If Markdown rendering fails, Otto falls back to escaped plain text instead of raw control sequences.
- Tool calls are collapsed to bounded one-line summaries by default; `Ctrl+O` toggles complete arguments and tool output.
- Mouse-wheel transcript scrolling remains enabled. Hold `Shift` while dragging to select visible terminal text, then use the terminal's normal copy command.
- The footer adapts to the available width and shows workspace/profile/model, token totals, and session ID when space allows.
- If the terminal is smaller than `40x8`, Otto shows a resize message until the window is large enough.

### TUI keys

| Key | Action |
| --- | --- |
| `Enter` | Submit the current prompt or an exact slash command |
| `Tab` | Complete the selected slash-command suggestion |
| `↑` / `↓` | Select a slash-command suggestion |
| `Shift+Enter` / `Alt+Enter` | Insert a newline in the composer |
| `?` | Open the help overlay when the composer is empty |
| `Ctrl+O` | Toggle complete tool arguments and output |
| `Shift`+drag | Select visible terminal text while mouse reporting is active |
| Mouse wheel or `PgUp` / `PgDn` | Scroll the transcript |
| `Home` / `End` | Jump to the top or bottom of the transcript |
| `Esc` | Cancel the active turn or close the current overlay |
| `Ctrl+C` | Cancel, clear, then quit on a second press within one second |

### Slash commands

In the TUI, typing `/` opens a filtered command suggestion panel. Use `↑`/`↓` to select a command and `Tab` to complete it; `Enter` executes only an exact command.

Shared commands:

- `/help` shows command help (TUI overlay or REPL text).
- `/session` shows session details. In the TUI it opens an overlay with the session ID, path, provider, profile, and model.
- `/new` closes the current session and starts a fresh one in the same process.
- `/exit` exits when idle. In the REPL, EOF also exits.

TUI-only command:

- `/resume` opens a modal containing up to the 20 most recently modified valid sessions for the current canonical workspace. Use `↑`/`↓` or `PgUp`/`PgDn` to navigate, `Enter` to resume, and `Esc` to close it. It does not search other workspaces or session contents.

## REPL behavior

- Otto accepts one prompt per line.
- `/help` shows commands.
- `/session` prints the current session ID, path, provider, and model.
- `/new` closes the current session and starts a fresh one in the same process.
- `/exit` or EOF exits.
- `Ctrl+C` during an active provider call or tool run cancels only that turn and returns you to the prompt.
- `Ctrl+C` while Otto is idle exits with status 130.

## Sessions

Otto writes append-only JSONL in the **Pi session format version 3**, compatible with the public session format and `SessionManager` API in Pi 0.84.3. Otto keeps its own storage root, separate from Pi:

```text
~/.otto/sessions/<workspace-key>/<session-id>.jsonl
```

It does not write under Pi's `~/.pi/agent/sessions` root.

A new session file is created lazily: starting Otto reserves a session, but the JSONL file is only written on the first user prompt. Starting and quitting without a prompt leaves no session file behind.

Examples:

```bash
./otto --cwd /path/to/project --continue
./otto --cwd /path/to/project --resume /absolute/path/to/session.jsonl
./otto --cwd /path/to/project --no-session
```

Notes:

- `--continue` reopens the newest valid Pi v3 session for the current canonical workspace. Invalid files and old Otto v1 files are skipped.
- `--resume PATH` reopens a specific valid Pi v3 session file only when its recorded workspace matches the current `--cwd`.
- Old Otto v1 files are left untouched, but they are unsupported, are not listed by `/resume`, and cannot be resumed.
- `/resume` is TUI-only and shows at most the recent 20 sessions in the current workspace; controls are documented above.
- `/session` shows the exact session path.
- `--no-session` keeps history in memory only.
- Session files are created lazily on the first user prompt; `/new` closes the current session (writing nothing if it had no prompts) and the replacement also stays lazy until its first prompt.
- Session files contain sensitive prompt text, assistant responses, tool calls, tool arguments, and tool results. Protect them like source data. Session records do not contain API-key, OAuth-token, or authorization-header fields.
- Stage 1 has no session tree/fork UI, naming, deletion, or search.

### Optional Pi interoperability probe

If Pi 0.84.3 (or a compatible package exposing the public Pi v3 `SessionManager` API) is installed, this opt-in probe opens one session, builds its context, and prints bounded JSON metadata only—never message or tool content, credentials, or authorization data:

```bash
OTTO_PI_INTEROP=1 node ./scripts/pi-session-interop.mjs /tmp/otto-session.jsonl
```

The environment variable is an explicit opt-in marker for operators; the script accepts exactly one session path. It exits 77 with a `SKIP` message when Pi is unavailable and exits nonzero for an invalid session. Default Go tests and builds never invoke Node or Pi.

## Tools and safety

### File tools

All file tools are enabled by default and restricted to the initial workspace:

- `read` reads UTF-8 text with optional line offsets and limits.
- `grep` searches file contents with Go RE2 regular expressions. It supports case-insensitive matching, optional `**` glob filtering, and up to 100 matches by default (1000 maximum).
- `find` returns sorted regular-file paths matching `**` globs, up to 1000 results by default (10000 maximum).
- `ls` lists one directory level in sorted order; directories end in `/` and symbolic links in `@`.
- `write` writes a complete file atomically.
- `edit` requires exactly one exact text match.

Recursive `grep` and `find` skip `.git` and discovered symbolic links but include other dotfiles. Binary files, invalid UTF-8 files, and files containing lines larger than 1 MiB are skipped by `grep`. Search and listing output respects the configured tool-output cap.

Otto canonicalizes input paths, resolves symlinks, and rejects workspace escapes.

### `bash` warning

`bash` is intentionally **unsandboxed**.

It starts in the selected workspace, but commands run as your current macOS user and can access anything that user can access. Treat Otto as trusted local automation, not a sandbox.

Default limits:

- 120 second shell timeout
- 50 KiB tool output cap

## Build and test commands

A `Makefile` wraps the same gates. `make build` compiles the binary and `make check` runs every CI gate (fmt, vet, lint, test, race, diff check). See `make help` for all targets.

```bash
make check
make build
```

The underlying commands are:

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
