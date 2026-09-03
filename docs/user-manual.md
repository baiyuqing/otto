# Otto User Manual

Otto is a minimal macOS coding agent written in Go. It turns a natural-language
prompt into a loop of model completions, optional tool calls, and — when needed —
context compaction, all in a full-screen TUI or a line-oriented REPL.

This manual describes the behavior implemented by the current build: the Stage 1
OpenAI-compatible provider and the Stage 2 ChatGPT-subscription provider. It
covers only what the CLI actually does today, not planned roadmap features.

## Contents

1. [Prerequisites](#prerequisites)
2. [Quick start](#quick-start)
3. [ChatGPT subscription](#chatgpt-subscription)
4. [Command-line reference](#command-line-reference)
5. [Environment variables](#environment-variables)
6. [Configuration](#configuration)
7. [Frontends](#frontends)
8. [Slash commands](#slash-commands)
9. [Sessions](#sessions)
10. [Context compaction](#context-compaction)
11. [Tools and safety](#tools-and-safety)
12. [Headless mode](#headless-mode)
13. [Memory core (internal, unwired)](#memory-core)
14. [Skills](#skills)
15. [Troubleshooting](#troubleshooting)

---

## Prerequisites

- macOS.
- Go 1.26+ to build from source.
- One of:
  - a reachable OpenAI-compatible endpoint with SSE chat-completions streaming, plus an API key exposed through an environment variable, or
  - a ChatGPT Plus/Pro/Team/Enterprise subscription (see [ChatGPT subscription](#chatgpt-subscription)).

## Quick start

Build the binary:

```bash
go build -trimpath -o ./otto ./cmd/otto
```

Create `~/.config/otto/config.toml`:

```toml
default_profile = "deepseek"

[ui]
mode = "auto"

[agent]
shell_timeout = "120s"
max_output_bytes = 51200

[agent.compaction]
auto = true
reserve_tokens = 16384
keep_recent_tokens = 20000

[sandbox]
driver = "auto"
network = "allow"
read_paths = []
allow_env = []

[profiles.deepseek]
provider = "openai-compatible"
base_url = "https://api.deepseek.com/v1"
model = "deepseek-chat"
api_key_env = "DEEPSEEK_API_KEY"
```

Export the selected profile's key and start Otto:

```bash
export DEEPSEEK_API_KEY=your-key
./otto --config ~/.config/otto/config.toml --profile deepseek
```

An ad hoc run without a config file:

```bash
OTTO_API_KEY=your-key ./otto \
  --provider openai-compatible \
  --base-url https://api.deepseek.com/v1 \
  --model deepseek-chat \
  --no-session
```

## ChatGPT subscription

Otto can authorize requests with a ChatGPT Plus/Pro/Team/Enterprise subscription
instead of a pay-per-token API key, using OpenAI's "Sign in with ChatGPT" OAuth
flow (the same mechanism the Codex CLI uses).

### Signing in

```bash
./otto login
```

`otto login` starts a local callback server, opens your browser to the OpenAI
authorization page, and also prints the URL so you can open it manually if the
browser does not launch. After you approve, it exchanges the authorization code
and writes credentials to `~/.otto/auth/chatgpt.json` with file mode `0600`.

```bash
./otto login --status   # report the ChatGPT sign-in state and access-token expiry; exits nonzero if not signed in
./otto logout           # remove the stored credentials
```

### Using the subscription

Select the `chatgpt` provider. It requires a `model` but no `base_url` and no
API key:

```toml
default_profile = "chatgpt"

[profiles.chatgpt]
provider = "chatgpt"
model = "gpt-5-codex"
```

```bash
./otto --profile chatgpt
```

Or ad hoc, without a profile:

```bash
./otto --provider chatgpt --model gpt-5-codex
```

### How it works

- Subscription traffic goes to OpenAI's Responses backend
  (`https://chatgpt.com/backend-api/codex/responses`), authorized by the OAuth
  access token plus the `chatgpt-account-id` header. This is a different wire
  format from the OpenAI-compatible Chat Completions provider, but the CLI,
  tools, sessions, and compaction behave identically.
- The access token is refreshed automatically from the stored refresh token
  when it nears expiry; rotated tokens are written back to the credential file.
- Tokens are never written to TOML, session files, or logs, and are stripped
  from provider error messages.
- Exchanging the login for an API key (which would bill as API credits rather
  than subscription quota) is intentionally not supported.

## Command-line reference

Otto also has two subcommands that run before the flags below are parsed:

| Command | Description |
| --- | --- |
| `otto login [--status]` | Sign in with a ChatGPT subscription, or (`--status`) report sign-in state. See [ChatGPT subscription](#chatgpt-subscription). |
| `otto logout` | Remove stored ChatGPT credentials. |
| `otto memory status\|forget <id>` | Inspect or delete memory records. See [Memory core](#memory-core). |

| Flag | Description |
| --- | --- |
| `--help` | Show help and exit. |
| `--config PATH` | Configuration file. Defaults to `~/.config/otto/config.toml`. |
| `--cwd PATH` | Workspace directory. Defaults to `.`. |
| `--profile NAME` | Configuration profile. |
| `--provider NAME` | Provider override: `openai-compatible` or `chatgpt`. |
| `--base-url URL` | Provider base URL override. |
| `--model NAME` | Model override. |
| `--thinking LEVEL` | Model reasoning effort: `low`, `medium`, `high`, `xhigh`, or `max`. |
| `--approve PROMPT` | Run one prompt without interaction, then exit. `@FILE` reads the prompt from a file (bounded to 1 MiB). |
| `--ui MODE` | Frontend mode: `auto`, `tui`, or `repl`. |
| `--sandbox MODE` | Sandbox driver: `auto`, `seatbelt`, or `off`. `off` is unsafe. |
| `--shell-timeout D` | Shell command timeout (for the `bash` tool). Must be greater than zero. |
| `--max-output-bytes N` | Maximum tool output bytes. Must be greater than zero. |
| `--no-session` | Keep history in memory only; do not persist a session. Cannot be combined with `--continue`, `--resume`, or `--archive`. |
| `--continue` | Continue the newest valid workspace session. Cannot be combined with `--resume`, `--archive`, or `--no-session`. |
| `--resume PATH` | Resume a specific session file. Cannot be combined with `--continue`, `--archive`, or `--no-session`. |
| `--archive PATH` | Archive one active session file for the current `--cwd`, print the new path, and exit. Cannot be combined with `--continue`, `--resume`, `--no-session`, or `--approve`. |

## Environment variables

| Variable | Meaning |
| --- | --- |
| `OTTO_PROVIDER` | Provider override (overrides the profile; overridden by `--provider`). |
| `OTTO_PROFILE` | Profile override (overrides `default_profile`; overridden by `--profile`). |
| `OTTO_MODEL` | Model override (overridden by `--model`). |
| `OTTO_API_KEY` | Fallback API key, used when the selected profile's `api_key_env` variable is empty. |
| `OTTO_UI` | Frontend mode (`auto`, `tui`, `repl`); overridden by `--ui`. |
| `<api_key_env>` | The variable named by the selected profile's `api_key_env`. Its value wins over `OTTO_API_KEY`. |

API keys are environment variables only. There is no `--api-key` flag, and keys
must never be stored in TOML.

## Configuration

Otto auto-discovers only the global config file at `~/.config/otto/config.toml`.
You can explicitly select any path with `--config`.

Review any explicit config path before you run Otto. Otto never auto-discovers
repository-local config, but an explicit `--config` can request `driver = "off"`,
extra `read_paths`, or `allow_env` grants on the next process start.

```toml
default_profile = "deepseek"

[ui]
mode = "auto"

[agent]
shell_timeout = "120s"
max_output_bytes = 51200

[agent.compaction]
auto = true
reserve_tokens = 16384
keep_recent_tokens = 20000

[sandbox]
driver = "auto"
network = "allow"
read_paths = []
allow_env = []

[skills]
enabled = true
paths = ["~/.otto/skills", ".otto/skills"]

[profiles.example]
provider = "openai-compatible"
base_url = "https://example.invalid/v1"
model = "gpt-5.6"
api_key_env = "EXAMPLE_API_KEY"
context_window = 1050000
compaction_window = 272000
```

Key points:

- `default_profile` names the profile used when no `--profile` is given and no
  session supplies one.
- `[ui].mode` sets the frontend mode (`auto`, `tui`, `repl`).
- `[agent].shell_timeout` and `[agent].max_output_bytes` set default limits.
- `[sandbox]` configures the process-wide shell boundary:

  ```toml
  [sandbox]
  driver = "auto"
  network = "allow"
  read_paths = []
  allow_env = []
  ```

  On macOS, `driver = "auto"` means Seatbelt. Otto never auto-detects or
  auto-falls back to Docker. If Seatbelt cannot be established, Otto fails
  closed by disabling `bash` while keeping `read`, `grep`, `find`, `ls`,
  `write`, and `edit` available.
- `[skills]` discovers reusable instruction sets from configured roots and
  registers the `skill` tool when at least one skill is found. Config keys are
  `enabled` (default true) and `paths` (default `["~/.otto/skills", ".otto/skills"]`);
  TOML-only, no CLI flags or environment variables.
- `[agent.compaction]` configures automatic context compaction (see
  [Context compaction](#context-compaction)).
- Each `[profiles.NAME]` declares `provider`, `base_url`, `model`, and
  `api_key_env`. Optional `context_window` and `compaction_window` size
  proactive compaction for private or unknown model IDs.
- A `provider = "chatgpt"` profile needs only `model`; it ignores `base_url`
  and `api_key_env` and authorizes with the credentials from `otto login`. See
  [ChatGPT subscription](#chatgpt-subscription).
- `[profiles.NAME].max_turns` is accepted by the schema but no longer limits the
  agent loop.

### Precedence

Startup resolution is field-specific:

- **Profile:** explicit `--profile` wins; otherwise a startup `--continue` or
  `--resume` uses the session's stored profile when present, then `OTTO_PROFILE`,
  and a new session uses `OTTO_PROFILE` or `default_profile`.
- **Provider / model:** `--provider` / `--model` override `OTTO_PROVIDER` /
  `OTTO_MODEL`, which override the selected profile and any provider/model
  stored in a resumed session. Explicit `--profile` makes that profile the
  baseline instead, but does not outrank the `OTTO_*` variables.
- **Endpoint:** `--base-url` overrides the profile's `base_url`. There is no
  base-URL environment override, and session files do not supply an endpoint.
- **API key:** the profile's `api_key_env` variable wins if non-empty;
  `OTTO_API_KEY` is the fallback.
- **Agent limits:** `--shell-timeout` and `--max-output-bytes` override
  `[agent]`, which override built-in defaults. These stay in effect across
  in-process `/resume` and `/new`.
- **Sandbox:** `--sandbox auto|seatbelt|off` overrides `[sandbox].driver`;
  otherwise `[sandbox].driver` overrides the built-in `auto`. `network`,
  `read_paths`, and `allow_env` come from `[sandbox]` only. The effective
  sandbox is process-wide and does not change on startup resume or `/new`.
- **Thinking effort:** `--thinking` is sent as `reasoning_effort` on
  OpenAI-compatible requests. It has no environment variable or TOML key and is
  omitted when unset. It stays in effect across `/resume` and `/new`.

Startup `--continue` / `--resume` restore session provider/model only as
defaults; direct flags and `OTTO_*` variables can override them. An in-process
TUI `/resume` restores the selected session's stored provider/model and ignores
the process's provider/model/profile/base-URL overrides; its stored profile
selects the endpoint and key environment.

UI mode precedence:

1. `--ui`
2. `OTTO_UI`
3. `[ui].mode`
4. built-in `auto`

Sandbox-driver precedence:

1. `--sandbox`
2. `[sandbox].driver`
3. built-in `auto`

Future backend authors should also read the
[Sandbox driver authoring guide](sandbox-driver-authoring.md).

## Frontends

Selection:

- `auto` starts the full-screen TUI only when **both** stdin and stdout are
  terminals. It falls back to the REPL for piped input, redirected output, and
  other non-TTY runs.
- `tui` forces the TUI and fails fast if stdin or stdout is not a terminal.
- `repl` forces the line-oriented REPL even from an interactive terminal.

```bash
otto --ui auto
otto --ui tui
otto --ui repl
OTTO_UI=repl otto
```

### TUI behavior

- Uses the terminal alternate screen buffer.
- Assistant responses render as Markdown; if rendering fails, Otto falls back
  to escaped plain text.
- Tool calls and compaction checkpoints are folded by default; `Ctrl+O` toggles
  full tool arguments/output and expanded compaction summaries.
- Mouse-wheel transcript scrolling is enabled. Hold `Shift` while dragging to
  select visible terminal text.
- The footer shows workspace/profile/model, token totals, and session ID when
  space allows.
- If the terminal is smaller than `40x8`, Otto shows a resize message.

### TUI keys

| Key | Action |
| --- | --- |
| `Enter` | Submit the current prompt or an exact slash command |
| `Tab` | Complete the selected slash-command suggestion |
| `↑` / `↓` | Select a slash-command suggestion |
| `Shift+Enter` / `Alt+Enter` | Insert a newline in the composer |
| `?` | Open the help overlay when the composer is empty |
| `Ctrl+O` | Toggle complete tool arguments/output and folded summaries |
| `Shift`+drag | Select visible terminal text while mouse reporting is active |
| Mouse wheel, `PgUp` / `PgDn` | Scroll the transcript |
| `Home` / `End` | Jump to the top or bottom of the transcript |
| `Esc` | Cancel the active turn or close the current overlay |
| `Ctrl+C` | Cancel; a second press within one second clears and quits |

### REPL behavior

- One prompt per line.
- `Ctrl+C` during an active provider call, tool run, or compaction cancels only
  that turn and returns to the prompt; `Ctrl+C` while idle exits with status 130.

## Slash commands

In the TUI, typing `/` opens a filtered suggestion panel; `Enter` executes only
an exact command. In the REPL, type the command and press `Enter`.

Shared commands:

- `/help` shows command help.
- `/session` shows session details (ID, path, provider, profile, model).
- `/new` closes the current session and starts a fresh one in the same process.
- `/compact [focus]` creates a manual context checkpoint, or reports
  `[context] no-op` when nothing can be compacted.
- `/exit` exits when idle (REPL EOF also exits).

TUI-only commands:

- `/resume` opens a modal of the up to 20 most recently modified valid sessions
  for the current canonical workspace. `↑`/`↓` or `PgUp`/`PgDn` to navigate,
  `Enter` to resume, `Esc` to close. It does not search other workspaces.
- `/archive` opens the same modal to archive a session. `Enter` on a non-current
  session moves it into `archive/` and shows `archived session <id>`. `Enter` on
  the current session archives it and starts a fresh session. `Esc` closes
  without archiving. It is accepted only while idle (a turn, `/new`, or
  `/resume` in progress is rejected) and reports `no active sessions found`
  when the workspace has none. In `--no-session` mode it reports that session
  persistence is disabled.

In the REPL, `/archive` archives the current session and starts a fresh one,
printing the archived path and the new session ID.

## Sessions

Otto writes append-only JSONL in the **Pi session format version 3**, compatible
with the public session format and `SessionManager` API in Pi 0.84.3. Otto
keeps its own storage root, separate from Pi:

```text
~/.otto/sessions/<workspace-key>/<session-id>.jsonl
```

A new session file is created lazily: starting Otto reserves a session, but the
JSONL file is only written on the first user prompt. Starting and quitting
without a prompt leaves no session file behind.

```bash
./otto --cwd /path/to/project --continue
./otto --cwd /path/to/project --resume /absolute/path/to/session.jsonl
./otto --cwd /path/to/project --archive /absolute/path/to/active-session.jsonl
./otto --cwd /path/to/project --no-session
```

Notes:

- `--continue` reopens the newest valid Pi v3 session for the current canonical
  workspace. Invalid files and old Otto v1 files are skipped.
- `--resume PATH` reopens a specific valid Pi v3 session only when its recorded
  workspace matches the current `--cwd`.
- Old Otto v1 files are left untouched, are not listed by `/resume`, and cannot
  be resumed.
- `--no-session` keeps history in memory only.
- **Archiving** moves an active session into a sibling `archive/` directory:
  `~/.otto/sessions/<workspace-key>/archive/<session-id>.jsonl`. The move is
  atomic and preserves the file byte-for-byte with its `0600` mode; nothing is
  deleted and no disk space is reclaimed. The `archive/` directory is created
  `0700` on the first archive and is scoped to that workspace, so archiving one
  workspace never affects another. Archived sessions are excluded from
  `/resume`, `--continue`, and the `/archive` picker, but remain resumable by
  explicit path:
  ```bash
  ./otto --cwd /path/to/project --resume ~/.otto/sessions/<key>/archive/<session-id>.jsonl
  ```
  `--archive PATH` archives one active session for the current `--cwd` and
  exits. It cannot be combined with `--continue`, `--resume`, `--no-session`,
  or `--approve`.
- Manual and automatic compaction append Pi v3 `type: "compaction"`
  checkpoints carrying `firstKeptEntryId`, `tokensBefore`, optional usage, and
  bounded file metadata.
- Session files contain sensitive prompt text, responses, summaries, tool
  calls, tool arguments, results, and file metadata. Protect them like source
  data. They do not contain provider API keys, OAuth tokens, authorization
  headers, or cookie values, and Otto does not persist private sandbox profile
  paths as runtime metadata.

### Optional Pi interoperability probe

If Pi 0.84.3 (or a compatible package exposing the public Pi v3 `SessionManager`
API) is installed, an opt-in probe opens one session and prints bounded JSON
metadata only — never message, summary, or tool content:

```bash
OTTO_PI_INTEROP=1 node ./scripts/pi-session-interop.mjs /tmp/otto-session.jsonl
```

It exits 77 with a `SKIP` message when Pi is unavailable or the gate is unset,
and exits nonzero for an invalid session.

## Context compaction

Compaction is configured in `[agent.compaction]` and, when needed, with
per-profile context metadata.

Rules and defaults:

- `auto = true` is the default. With `auto = true`, Otto does two bounded
  automatic checks:
  - **Proactive compaction:** when the model window is known and the next
    request estimate is above `working_window - reserve_tokens`, Otto attempts
    one checkpoint before sending the provider request.
  - **Reactive overflow recovery:** when the provider returns a typed
    context-overflow error, Otto does one automatic compaction and retries once.
- The automatic paths are one-shot only; they never loop.
- `reserve_tokens = 16384` and `keep_recent_tokens = 20000` are the defaults.
- `context_window` and `compaction_window` must be at least `4096` when set.
- `compaction_window` requires `context_window` and must not exceed it.
- Manual `/compact [focus]` works in both frontends even when `auto = false`.

The optional `focus` text is sanitized, bounded to 8 KiB, and appended only to
the hidden summary-system prompt.

### Model window metadata

Otto ships a static limit catalog for common GPT, o-series, and Claude model IDs
so it can size compaction conservatively.

- Listed full-size GPT-5.4/5.5/5.6 aliases use `context_window=1050000`,
  `hard_input_window=922000`, and `compaction_window=272000`.
- `gpt-5.4-mini`, `gpt-5.4-nano`, GPT-5/5.1/5.2 aliases, and `gpt-5.3-codex` use
  the 400K catalog family with a 272K working window.
- `gpt-5.3-codex-spark` is an exact 128000-context / 32000-output override;
  `*-chat-latest` aliases use their catalog chat values.
- GPT-4.1, GPT-4o, o1/o3/o4, and listed Claude aliases use the static catalog.
- Claude metadata is used only when an OpenAI-compatible endpoint exposes a
  Claude-family model ID; Otto does not add an Anthropic provider in Stage 1.

If a model ID is unknown, proactive automation is disabled (no trustworthy local
window), but reactive one-shot recovery can still happen after a typed provider
overflow. For private deployments, set `context_window` and optionally
`compaction_window` on the selected profile.

## Tools and safety

### File tools

The six file tools are always enabled and always restricted to the initial
canonical workspace, even when `--sandbox off` is selected:

- `read` reads UTF-8 text with optional line offsets and limits; files larger
  than 64 MiB are rejected before being read into memory.
- `grep` searches file contents with Go RE2 regular expressions
  (case-insensitive matching, optional `**` glob filtering, up to 100 matches by
  default, 1000 maximum).
- `find` returns sorted regular-file paths matching `**` globs (up to 1000 by
  default, 10000 maximum).
- `ls` lists one directory level in sorted order; directories end in `/` and
  symlinks in `@`.
- `write` writes a complete file atomically.
- `edit` requires exactly one exact text match and shares the 64 MiB size limit
  with `read`.

Recursive `grep` and `find` skip `.git` and discovered symlinks but include
other dotfiles. Binary files, invalid UTF-8 files, and files with lines larger
than 1 MiB are skipped by `grep`. Otto canonicalizes paths, resolves symlinks,
and rejects workspace escapes.

### `bash` sandbox policy

On macOS, the default is `--sandbox auto`, which means Seatbelt. The sandboxed
command gets whole-workspace write access plus Otto-managed private `home`,
`tmp`, and `cache` directories beneath your user cache. Otto keeps generated
profile files in a separate private `profiles` directory that the sandboxed
child cannot read. Otto does not treat the workspace as protected: source,
`.git`, tests, and generated files remain writable.

Host home content is not automatically readable. Git config, shell dotfiles,
and host caches are not implicitly mounted into the command view. Add only the
narrow absolute or `~/...` `read_paths` you need. Broad `read_paths` are high
risk because command code can read them and, with `network = "allow"`, exfiltrate
them. Otto rejects `read_paths` that would include Otto's private sandbox state.
If you need tool-specific config, prefer narrow `read_paths` plus exact config
environment variables over exposing a large home or cache subtree.

`network = "allow"` is the default and permits ordinary IP networking and local
IP binds. `network = "deny"` blocks IP networking and local binds. Phase 1 has
no domain allowlist. Unix sockets stay blocked in both modes, so Docker/Podman
sockets, SSH agents, and similar host control sockets are unavailable.

The command environment is rebuilt from one captured process snapshot. Otto
never restores provider API-key variables, `OTTO_API_KEY`, loader-injection
variables, shell-startup injection variables, `SSH_AUTH_SOCK`, or Otto's own
sandbox variables. `allow_env` restores only exact names after filtering and is
high risk because it grants the restored value to untrusted command code.
Restored values are still added to Otto's exact-value redactor.

If Otto would need to retain more than 512 sensitive values or more than 1 MiB
of sensitive-value bytes for exact redaction, it fails closed by disabling
`bash` for that process. Exact-value redaction is defense in depth only: if a
command transforms or encodes a secret, Otto may not be able to redact it.

If you explicitly select `--sandbox off`, Otto prints a persistent local warning
and `bash` runs unsandboxed as your current macOS user. In that mode,
`network = "deny"`, private-home/cache replacement, and `read_paths` no longer
constrain the shell.

### Seatbelt limitations

Stage 1 depends on Apple's deprecated `/usr/bin/sandbox-exec`. It improves
command isolation on macOS, but it is not a VM boundary. Otto does not claim
protection against same-user or same-kernel attacks, pre-existing hard links,
`setsid` escaping Otto's process-group cleanup, resource exhaustion, or
intentional damage inside the writable workspace. Docker and Apple Container are
planned future drivers only; Stage 1 does not detect or support them.

Default limits remain a 120-second shell timeout and a 50 KiB tool-output cap.
Override them with `--shell-timeout` and `--max-output-bytes` or `[agent]`.

## Headless mode

`--approve` runs a single prompt without interaction and exits: `0` on success,
`1` on error, `130` on interrupt. The value is the prompt text, or `@PATH` to
read the prompt from a file (bounded to 1 MiB).

```bash
./otto --approve "summarize TODOs in this repo"
./otto --approve @prompt.txt --no-session
./otto --approve "explain main.go" --thinking max --continue
```

`--approve` cannot be combined with `--ui tui` or `--archive`, and composes with
`--continue`, `--resume`, and `--no-session`.

## Memory

Otto has a local, per-workspace/per-user memory store backed by SQLite/FTS5
(`internal/memory`, `internal/memory/sqlite`). It is enabled by default.

Config (`[memory]` in TOML; all keys optional):

```toml
[memory]
enabled = true
backend = "sqlite"
required = false
recall_tokens = 2000
max_results = 12
require_encryption = false

[memory.sqlite]
path = "~/.otto/memory/memory.db"
busy_timeout = "5s"

[memory.workspace_ids]
"/canonical/path/to/workspace" = "stable-id"
```

There are no `--memory-*` CLI flags.

What's wired:

- Recall before each turn into a request-local, untrusted context block that is
  never written to Pi session JSONL, compaction summaries, or logs.
- Agent tools: `memory_search`, `remember`, `forget`.
- Human commands in both frontends: `/memory search`, `/memory forget`,
  `/memory review`, and `/remember`.
- Standalone CLI: `otto memory status` and `otto memory forget <id>`.

Model-originated writes always land as pending candidates for human review.
Human `/remember` and `/memory forget` apply immediately.

Not yet implemented:

- No automatic extraction (`Binding.Observe` is not wired).
- No backup/restore/verify commands.
- No `otto memory backup|backups|verify|restore` subcommands.

## Skills

Otto loads reusable instruction sets ("skills") from `~/.otto/skills` (user level)
and workspace `.otto/skills` directories. A skill is a directory containing
`SKILL.md` with YAML frontmatter and Markdown body, following the Agent Skills
format.

Config (`[skills]` in TOML; all keys optional):

```toml
[skills]
enabled = true                              # default true
paths = ["~/.otto/skills", ".otto/skills"]  # default; later entries win on name conflict
```

What's wired:

- Skill listing in the system prompt (capped at 8 KiB).
- The `skill` tool for the model to load instructions by name or read supporting
  files.
- Automatic appending of existing skill roots to Seatbelt read paths at process
  start.
- Validation: `name` equals the directory name (`a-z`, `0-9`, `-`; 1 to 64
  characters) and `description` is 1 to 1024 characters. Invalid skills print
  one stderr warning and are skipped.
- Discovery runs at startup and on `/new`, `/resume`, `/model`; the catalog is
  fixed within a session. A loaded body is a normal tool result stored in the
  session and re-sent on every later request until compaction.

Not yet implemented:

- `/skills` and `/skill <name>` user commands.
- `allowed-tools` enforcement.
- Reading `~/.claude/skills`; hot reload inside a session.

## Troubleshooting

### `otto: missing api key`

Export the environment variable named by the selected profile's `api_key_env`,
or set `OTTO_API_KEY` as a fallback.

### `otto: missing base_url`, `otto: invalid base_url`, or request failures

Check the selected profile, `--base-url`, and endpoint path. Otto posts to
`<base-url>/chat/completions`.

### `read chat completion stream: ...` or stream ended without `[DONE]`

The provider or proxy is not delivering valid SSE chat-completions output.
Confirm streaming is enabled and SSE is not buffered or rewritten.

### `warning: bash is unavailable because the configured sandbox could not be established ...`

On macOS, `auto` means Seatbelt. Otto does not fall back to Docker or direct
execution unless you explicitly choose `--sandbox off` (or `driver = "off"` in
config). Common fixes:

- confirm `/usr/bin/sandbox-exec` is present and usable;
- narrow `read_paths` so they do not include Otto's private sandbox cache root;
- move the selected workspace outside cache-like locations if it would overlap
  Otto's private sandbox state;
- prefer narrow `read_paths` plus exact config variables over broad home or
  cache access;
- use `--sandbox off` only if you accept unsandboxed current-user execution.

### `no chatgpt credentials; run 'otto login'`

The `chatgpt` provider has no stored OAuth credentials. Run `otto login` to sign
in with your ChatGPT subscription, or check state with `otto login --status`.
See [ChatGPT subscription](#chatgpt-subscription).

### Context-length or prompt-size failures

Otto tries one automatic checkpoint before the hard limit (when it knows the
model window) and one typed-overflow recovery checkpoint after a provider
context error. If you still hit a hard input limit:

- run `/compact [focus]` to create a manual checkpoint,
- use `/new` for a completely fresh session,
- reduce large pasted input or large tool output,
- set profile `context_window` / `compaction_window` for private or unknown
  model IDs,
- or choose a model/profile with a larger working window.

### A session disappeared from `/resume` or `--continue`

The session was likely archived. Archive moves the file (not deletion) into
`~/.otto/sessions/<workspace-key>/archive/<session-id>.jsonl`. To reopen it:

```bash
./otto --cwd /path/to/project --resume ~/.otto/sessions/<key>/archive/<session-id>.jsonl
```

The file is still intact; only the active-session surfaces (`/resume`,
`--continue`, and the `/archive` picker) exclude it.
