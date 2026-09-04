<p align="center">
  <img src="docs/logo.png" alt="Otto logo" width="320">
</p>

# Otto

**Otto is a local-first coding agent for macOS.** It gives developers an
interactive AI teammate that can inspect a workspace, edit files, run commands in
a sandbox, preserve session history, and delegate bounded work to sub-agents —
all from a small Go CLI.

Otto is designed for day-to-day development work: understand an unfamiliar
repository, make focused changes, run the relevant checks, keep context across
sessions, and stay explicit about what the agent can access.

For the complete command and configuration reference, see the
[Otto user manual](docs/user-manual.md).

## Highlights

- **Native macOS CLI** with automatic TUI/REPL selection.
- **OpenAI-compatible provider support** using streaming Chat Completions.
- **Workspace-aware tools** for reading, searching, listing, writing, and exact
  file edits.
- **Sandboxed shell execution by default** through macOS Seatbelt, with an
  explicit unsafe `off` mode when you choose direct execution.
- **Durable sessions** stored as append-only JSONL, with continue, resume, and
  non-destructive archive flows.
- **Context compaction** to keep long coding sessions usable as conversations
  grow.
- **Local memory** backed by SQLite/FTS5 for explicit recall, remember, and
  forget workflows.
- **Skills** for reusable instruction sets loaded from configured user and
  workspace directories.
- **Sub-agents** for parallel investigation or review tasks in the same
  workspace and sandbox policy.
- **Headless mode and local server mode** for scripts, automation, and editor or
  client integrations.

## Current release scope

Otto's current GA scope is the Stage 1 macOS product surface.

### Included

- `otto` CLI for macOS.
- OpenAI-compatible provider support only.
- Adaptive frontend selection: full-screen TUI on terminal stdin/stdout, REPL
  otherwise.
- Streaming TUI and REPL with session, compaction, archive, and basic lifecycle
  commands.
- Built-in tools: `read`, `grep`, `find`, `ls`, `write`, `edit`, `bash`, and
  model-facing `skill` when skills are configured.
- Persistent append-only Pi v3 JSONL sessions with `--continue`, `--resume`,
  `--no-session`, and archiving.
- Manual and automatic context compaction.
- Global TOML configuration at `~/.config/otto/config.toml`.
- SQLite-backed memory with explicit search, remember, review, and forget flows.
- TOML-configured skills and sub-agent definitions.
- `otto serve` over a Unix domain socket.
- `--approve` for non-interactive single-prompt runs.

### Not included in Stage 1

- Codex or ChatGPT subscription login as a working Stage 1 provider.
- Claude subscription login.
- Anthropic-native provider support.
- Plugins or automatic project-local config discovery.
- Windows or Linux support commitments.
- Session trees/forks, session naming, deletion, or search.
- Automatic memory extraction, memory backup/restore/verify commands, skill user
  commands, per-skill `allowed-tools` enforcement, nested sub-agent delegation,
  or persisted child transcripts.

Planned providers are listed in [Roadmap](#roadmap).

## Requirements

- macOS.
- Go 1.26 or newer.
- An OpenAI-compatible HTTP(S) endpoint that supports SSE streaming Chat
  Completions.
- An API key exported through an environment variable. Otto does not accept an
  `--api-key` flag and API keys should not be stored in TOML.

## Install from source

```bash
git clone <repo-url> otto
cd otto
go build -trimpath -o ./otto ./cmd/otto
```

Run the binary from the repository root:

```bash
./otto --help
```

Or place it on your `PATH` and use `otto` directly.

The Makefile wraps the common development gates:

```bash
make build
make check
```

## Quick start

Create `~/.config/otto/config.toml`:

```toml
default_profile = "default"

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

[memory]
enabled = true

[skills]
enabled = true
paths = ["~/.otto/skills", ".otto/skills"]

[agents]
enabled = true
paths = ["~/.otto/agents", ".otto/agents"]
max_parallel = 4

[server]
socket = "~/.otto/otto.sock"

[profiles.default]
provider = "openai-compatible"
base_url = "https://example.invalid/v1"
model = "example-model"
api_key_env = "EXAMPLE_API_KEY"
```

Export the key named by `api_key_env` and start Otto:

```bash
export EXAMPLE_API_KEY=your-key
./otto --config ~/.config/otto/config.toml --profile default
```

For an ad hoc run without a config file:

```bash
EXAMPLE_API_KEY=your-key ./otto \
  --provider openai-compatible \
  --base-url https://example.invalid/v1 \
  --model example-model \
  --no-session
```

## Common workflows

### Start an interactive coding session

```bash
otto --cwd /path/to/project
```

With `--ui auto` (the default), Otto starts the full-screen TUI when stdin and
stdout are terminals. It falls back to the line-oriented REPL for pipes,
redirected output, and other non-TTY environments.

Useful interactive commands:

```text
/help                 show command help
/session              show current session details
/new                  start a fresh session
/compact [focus]      create a context checkpoint
/archive              archive a session without deleting it
/exit                 exit when idle
```

The TUI also includes a session resume/archive picker, task views, Markdown
rendering, folded tool output, and keyboard shortcuts documented in the user
manual.

### Run one prompt and exit

`--approve` runs a single prompt without entering the interactive loop:

```bash
otto --approve "summarize TODOs in this repo"
otto --approve @prompt.txt --no-session
otto --approve "explain cmd/otto/main.go" --thinking high --continue
```

Exit codes are `0` on success, `1` on error, and `130` on interrupt.

### Continue, resume, and archive sessions

```bash
otto --cwd /path/to/project --continue
otto --cwd /path/to/project --resume /absolute/path/to/session.jsonl
otto --cwd /path/to/project --archive /absolute/path/to/active-session.jsonl
```

Sessions are append-only JSONL files under:

```text
~/.otto/sessions/<workspace-key>/<session-id>.jsonl
```

Archiving moves a session into a sibling `archive/` directory. It preserves the
file byte-for-byte and removes it from normal continue/resume pickers, but it can
still be resumed later by explicit path.

### Use memory explicitly

Memory is enabled by default and stores records locally in SQLite/FTS5. Otto can
recall matching records before a turn, and both the model and the human can use
explicit memory workflows.

Interactive commands include:

```text
/memory search <query>
/remember [--scope user|workspace] [--kind K] [--key K] <text>
/memory review <id> accept|reject
/memory forget <id>
```

Standalone commands:

```bash
otto memory status --cwd /path/to/project
otto memory forget <id> --cwd /path/to/project
```

Model-originated writes are pending candidates until a human reviews them.
Automatic memory extraction is not wired in Stage 1.

### Add reusable skills

A skill is a directory with a `SKILL.md` file containing YAML frontmatter and a
Markdown body. By default Otto discovers skills from:

```text
~/.otto/skills
<workspace>/.otto/skills
```

Example:

```text
~/.otto/skills/release-notes/SKILL.md
```

```markdown
---
name: release-notes
description: Draft release notes from a git diff and changelog entries.
---
Follow the project's release-note style. Group changes by user-visible impact.
```

Discovered skills appear in the model's system prompt. The model can request a
skill's full instructions with the `skill` tool. User-facing `/skills` and
`/skill` commands are not part of Stage 1.

### Delegate work to sub-agents

Sub-agents let the model start bounded child tasks for parallel exploration,
review, or research. They run in the same workspace, sandbox, and provider as the
parent, with a fresh context by default.

Optional named definitions live under:

```text
~/.otto/agents
<workspace>/.otto/agents
```

Example:

```text
~/.otto/agents/reviewer/AGENT.md
```

```markdown
---
name: reviewer
description: Review a diff for correctness and missing tests.
tools: read, grep, find, ls, bash
context: fresh
---
Report findings as file:line bullets ordered by severity.
```

Interactive task commands:

```text
/tasks                 list tasks
/task <id|name>        show one task
/task cancel <id|name> cancel a queued or running task
```

Child agents cannot start nested agents and their transcripts are not persisted
in Stage 1.

### Run Otto as a local server

`otto serve` exposes the same agent runtime over HTTP, JSON, and SSE on a Unix
domain socket:

```bash
otto serve --socket ~/.otto/otto.sock
```

Example calls:

```bash
curl -s --unix-socket ~/.otto/otto.sock \
  -X POST http://otto/v1/sessions \
  -d '{}'

curl -N --unix-socket ~/.otto/otto.sock \
  -X POST http://otto/v1/sessions/<id>/turns \
  -d '{"text":"list files"}'
```

The server provides session routes, turn streaming, task routes, health checks,
and Prometheus metrics. It listens on Unix sockets only in Stage 1.

## Configuration model

Otto auto-discovers only the global config file:

```text
~/.config/otto/config.toml
```

You can select another file explicitly with `--config PATH`. Otto does not
automatically load repository-local config.

Important precedence rules:

- `--profile` selects a configured profile; otherwise Otto uses
  `default_profile` or a compatible resumed session default.
- `--provider` and `--model` override `OTTO_PROVIDER` and `OTTO_MODEL`; those
  environment variables override the selected profile.
- `--base-url` overrides the selected profile's `base_url`.
- API keys are read only from the profile's `api_key_env` variable, with `OPENAI_API_KEY`
  as a fallback.
- `--sandbox auto|seatbelt|off` overrides `[sandbox].driver`.
- `[memory]`, `[skills]`, and `[agents]` are TOML-only.
- `--thinking low|medium|high|xhigh|max` is passed through as provider reasoning
  effort when set.

Run `otto --help` for the supported CLI flags.

## Tools and safety

Otto is intentionally conservative about local access.

- File tools are always confined to the initial canonical workspace, even when
  `--sandbox off` is selected.
- Paths are canonicalized, symlinks are resolved, and workspace escapes are
  rejected.
- `bash` uses macOS Seatbelt by default with writable workspace access and
  private home/tmp/cache directories.
- `network = "allow"` is the default; `network = "deny"` removes ordinary IP
  networking and local binds for sandboxed commands.
- `allow_env` restores only explicit environment variables into sandboxed
  commands. Provider API-key variables and common credential/socket variables are
  blocked.
- `--sandbox off` is explicit and unsafe: `bash` then runs directly as your
  current macOS user.

Seatbelt improves isolation, but it is not a VM boundary. It does not protect
against every same-user or same-kernel attack, resource exhaustion, or deliberate
destruction of files inside the writable workspace.

Session files can contain prompts, assistant responses, tool arguments/results,
file metadata, and compaction summaries. Treat them like source data. Otto does
not write provider API keys, OAuth tokens, authorization headers, or cookie
values to sessions.

## CLI overview

```text
otto [options] [prompt]
otto serve [options] [--socket PATH]
otto memory status|forget <id> [--config PATH] [--cwd PATH]
```

Common options:

```text
--config PATH          configuration file
--cwd PATH             workspace directory
--profile NAME         configuration profile
--provider NAME        provider override
--base-url URL         provider base URL override
--model NAME           model override
--thinking LEVEL       low, medium, high, xhigh, or max
--approve PROMPT       run PROMPT or @FILE without interaction and exit
--ui MODE              auto, tui, or repl
--sandbox MODE         auto, seatbelt, or off
--shell-timeout D      shell command timeout
--max-output-bytes N   maximum tool output bytes
--no-session           use an in-memory session
--continue             continue newest workspace session
--resume PATH          resume a session file
--archive PATH         archive an active session file
--socket PATH          Unix socket path for the serve subcommand
```

## Build and test

```bash
test -z "$(gofmt -l .)"
go test ./...
go test -race ./...
go vet ./...
go run honnef.co/go/tools/cmd/staticcheck@latest ./...
go build -trimpath -o ./otto ./cmd/otto
git diff --check
```

Or run the Makefile wrapper:

```bash
make check
```

## Troubleshooting

### `otto: missing api key`

Export the environment variable named by the selected profile's `api_key_env`, or
set `OPENAI_API_KEY` as a fallback.

### `otto: missing base_url`, `otto: invalid base_url`, or request failures

Check the selected profile, `--base-url`, and endpoint path. Stage 1 expects an
HTTP(S) OpenAI-compatible base URL and posts to `<base-url>/chat/completions`.

### Streaming errors

Errors such as `read chat completion stream: ...` or `chat completion stream
ended without [DONE]` usually mean the provider or proxy is not delivering valid
SSE Chat Completions output. Confirm streaming is enabled and not buffered or
rewritten.

### Bash sandbox warnings

On macOS, `--sandbox auto` means Seatbelt. Otto does not fall back to Docker or
direct execution unless you explicitly select `--sandbox off` or
`driver = "off"`. If sandbox setup fails, Otto disables `bash` and leaves the
workspace-bound file tools available.

### Context-length failures

Use `/compact [focus]`, start a new session with `/new`, reduce pasted input or
large tool output, or configure `context_window` and `compaction_window` for
private model IDs so proactive compaction can size requests safely.

## Contributing

See [`AGENTS.md`](AGENTS.md) for repository conventions, Go workflow, testing
expectations, and safety rules.

## Roadmap

- **Stage 1:** macOS GA with OpenAI-compatible provider support.
- **Stage 2:** ChatGPT/Codex subscription support.
- **Stage 3:** Claude subscription support.

Codex/ChatGPT subscription support and Claude support are roadmap items, not
working Stage 1 providers.

The staged design lives in
[`docs/superpowers/specs/2026-08-26-otto-coding-agent-design.md`](docs/superpowers/specs/2026-08-26-otto-coding-agent-design.md).
