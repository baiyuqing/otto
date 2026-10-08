<p align="center">
  <img src="docs/logo.svg?v=2" alt="Otto logo" width="320">
</p>

# Otto — a local-first personal AI agent

[简体中文](README.zh-CN.md) · [User manual](docs/user-manual.md) · [Changelog](CHANGELOG.md)

**Otto helps with research, writing, planning, coding, and practical tasks.**
Work with it from your terminal, a browser, Telegram, or Feishu. It uses the
tools and permissions available in each session, keeps context across tasks,
and can turn useful experience into reviewed memory proposals and reusable skills.

The runtime is built in Rust with a WebAssembly-safe core. Connect through an
OpenAI-compatible API endpoint or sign in with `otto login` for the ChatGPT
provider. Model requests go to the selected provider; local-first refers to
the runtime, session history, and default memory storage.

- **Choose where you work.** Terminal TUI, REPL, headless scripts, browser UI,
  and Telegram/Feishu through `otto-connect`. Connect to `otto serve` to share
  sessions across the terminal, browser, and chats.
- **Keep useful context.** Persistent sessions, memory with shared approval
  review, reusable skills and plugin packages, and selective reflection after completed work.
- **Carry work forward.** Workspace tools, web fetch and search, and MCP integrations, bounded
  sub-agents, timers, durable workflows with approvals and restart recovery, and
  [delivery tasks](docs/user-manual.md#cross-workspace-deliveries) that hand work to another admitted repository.
- **Control access.** Workspace-confined file tools, macOS Seatbelt shell
  sandboxing by default, and explicit permission requests for additional access.

## Code architecture

[![Otto code architecture: access, frontends, application runtime, native services, and portable core](docs/architecture/otto-architecture.png?v=a37f68b)](docs/architecture/otto-architecture.svg)

[Code organization](docs/code-organization.md) · [SVG](docs/architecture/otto-architecture.svg) · [Editable HTML](docs/architecture/otto-architecture.html)

## Install from source

Requires the pinned **Rust 1.98** toolchain (`rustup toolchain install` picks
it up from `rust-toolchain.toml`), **Go** (version pinned in `chat-connector/go.mod`), and access
to one of the two providers. **macOS** is the supported platform: it is the
only one with a sandbox, and the only one the acceptance gate runs on. Otto
also builds and runs on **Linux**, with the limits under
[Safety and limitations](#safety-and-limitations).

```bash
git clone https://github.com/baiyuqing/otto.git
cd otto
make install
otto --help
otto --version   # otto 0.1.0+a1b2c3d4e5f6; `.dirty` is appended for uncommitted changes
```

`make install` builds Otto and otto-connect, installs both to `~/.local/bin/`, and installs bundled skills under `~/.otto/skills/`. Make sure that directory is on your `PATH`. Browser UI development and hosting live in [otto-plus](https://github.com/baiyuqing/otto-plus); building Otto needs no npm or WASM frontend bundle.

If you only want a local copy in the checkout, run `make build` and then
`./otto --help`.

To keep the startup options in one place instead of retyping them, copy the
launcher template and start Otto through the script:

```bash
cp otto.env.example otto.env   # provider, model, workspace, sandbox
make run                       # or: scripts/otto.sh
```

`otto.env` is ignored by Git and read like a shell profile; see the
[launcher script](docs/user-manual.md#launcher-script).

## Docker Desktop on Mac

With Docker Desktop running, build and start the Otto API from this checkout:

```bash
docker compose build
docker compose run --rm otto setup
docker compose up -d
docker compose logs otto
```

Use the printed host/port and token with an API client or the separate
[otto-plus](https://github.com/baiyuqing/otto-plus) frontend.
The image builds natively on Apple Silicon and Intel Macs and serves the API. Configuration and sessions persist in a named volume;
`OTTO_WORKSPACE` selects the directory mounted into the container.
For API-key profiles, select `OTTO_API_KEY` during setup and export it before
starting Compose. See [Docker on Mac](docs/user-manual.md#docker-on-mac) for
CLI usage, architecture selection and lifecycle commands. Containers run
Linux, so Bash requires explicitly selecting `--sandbox off`.

## Quick start

Run `otto setup` after installation to create a default profile. It stores no
API key and leaves shell commands sandboxed by default. Choose a model ID
available to your own account or provider; Otto does not select one for you.

```bash
./otto setup
```

For ChatGPT, sign in and then start Otto:

```bash
./otto login
./otto
```

For an OpenAI-compatible provider, export the environment variable selected by
setup, then start Otto:

```bash
export OPENAI_API_KEY=your-key
./otto
```

You can still run with one-off provider flags instead of creating a profile.

### ChatGPT sign-in

Sign in, then choose a model available to your account (replace `YOUR_MODEL_ID`):

```bash
./otto login
./otto --provider chatgpt --model YOUR_MODEL_ID
```

See [sign-in and profiles](docs/user-manual.md#chatgpt-subscription) for details.

### OpenAI-compatible API

Export your API key as `OTTO_API_KEY` in your shell. Replace the endpoint and
model below with values from your provider; the endpoint must support streaming
Chat Completions.

```bash
./otto --provider openai-compatible \
  --base-url https://example.invalid/v1 \
  --model YOUR_MODEL_ID
```

Otto reads API keys from the profile's `api_key_env` variable or falls back to
`OTTO_API_KEY`. Keys have no CLI flag and must not be stored in TOML. For a
persistent setup, see [configuration](docs/user-manual.md#configuration).
Otto merges user, shared and local project configuration beneath managed
policy. `otto config explain --json` shows sources and ignored fields;
`otto config set KEY TOML_VALUE --scope user|shared|local` edits one source.
Project MCP, skills and agents require `otto trust DIR --executables`.

## Try a task

Start Otto in the workspace you want it to use:

```bash
./otto --provider chatgpt --model YOUR_MODEL_ID --cwd /path/to/workspace
```

Example prompts to enter in the interactive session:

```text
Help me plan my day and draft a short invitation.
Read this workspace and explain how the project is organized.
Remind me in five minutes to follow up.
Remember that I prefer concise progress updates.
```

After configuring a default profile, you can also run one prompt and exit:

```bash
./otto --prompt "summarize what this workspace is for"
./otto --continue
```

Use `/help` for interactive commands. Sessions support continuing, resuming, and
archiving. See [sessions](docs/user-manual.md#sessions) and
[headless mode](docs/user-manual.md#headless-mode).

Workspace identities persist across restarts and directory moves. See
[workspace identity](docs/user-manual.md#workspace-identity) for copying and forking.

In the TUI, `/image <path>` attaches one PNG, JPEG, or WebP image to the next
prompt. In the Web UI, choose **Image** or paste a screenshot before sending.
The selected model and OpenAI-compatible endpoint must support image input.

## More workflows

- [Local memory](docs/user-manual.md#memory): search, remember, review, and forget, plus [reflection](docs/user-manual.md#reflection), which writes a retrospective of a session to `~/.otto/reflection/runs/`, proposes memories from it for your review, and can write vetted skills you can undo with `/skill revert`. It runs on `/reflect`, when you ask the agent for a retrospective, and, by default, after a completed turn with at least 10 successful tool operations or a compaction; `auto = "off"` turns that off.
- [Skills](docs/user-manual.md#skills): reusable instructions in `SKILL.md` files. Reflection can also extract reusable Python scripts, syntax-check them without execution, and publish the complete skill after approval; `/skill revert` restores both instructions and scripts.
- [MCP servers](docs/user-manual.md#mcp-servers): connect stdio or HTTP Model
  Context Protocol servers and use their tools from the same turn loop.
- [Timers](docs/user-manual.md#remind): the model schedules a later wake with
  `remind`; `/timers` lists this session's outstanding timers and
  `/timers cancel <id>` stops one.
- [Scheduled deliveries](docs/user-manual.md#schedule): `schedule` sends a
  message or the reply of a prompt run at a later time, once or daily, to the
  chat, terminal or browser UI that asked for it. A prompt run needs authorization
  through the session's approval policy when it is created.
- [Local server](docs/user-manual.md#agent-server): `otto serve` exposes an
  authenticated API over a Unix socket or a loopback TCP port. The browser UI
  lives in [otto-plus](https://github.com/baiyuqing/otto-plus).
  [`otto --attach`](docs/user-manual.md#the-tui-attached-to-otto-serve) runs
  the TUI on the same sessions.
- [ACP agent](docs/user-manual.md#acp-agent-server): `otto acp` speaks the
  Agent Client Protocol on stdio for ACP clients; `otto acp --attach`
  forwards it to a running `otto serve`.
- [Chat connector](docs/user-manual.md#chat-connector): `otto-connect`, a
  separate Go program in `chat-connector/`, runs `otto acp` and connects it to
  Telegram and Feishu chats, one session per chat, with Approve / Deny buttons and `/allow` / `/deny` for
  elevated `bash` commands and `/sessions` and `/use` to pick a session.
  With `otto acp --attach`, chats share sessions with the `otto serve` web UI
  and `otto --attach` terminals; `scripts/otto-tcp.sh` supervises the direct
  ACP TCP pair instead, where `otto serve --acp-listen` is the agent and
  connect starts no process of its own.
- [Usage history](docs/user-manual.md#observability): a Web UI analysis page
  for local provider token trends and cache hit rate, without prompt or tool
  content.
- [Durable workflows](docs/user-manual.md#durable-workflows): versioned TOML
  DAGs with sequential/concurrent agent and handoff steps, persisted approvals,
  restart recovery, CLI control, and a Web UI inspector.
- [Configuration](docs/user-manual.md#configuration),
  [CLI reference](docs/user-manual.md#command-line-reference), and
  [troubleshooting](docs/user-manual.md#troubleshooting).

### Delegate work to sub-agents

Sub-agents let the model start bounded child tasks that run in parallel. They
use the same workspace, sandbox, and provider as the parent, with a fresh
context by default.

Optional named definitions live under:

```text
~/.otto/agents
<workspace>/.otto/agents
```

Example:

```text
~/.otto/agents/researcher/AGENT.md
```

```markdown
---
name: researcher
description: Gather facts from the workspace and return a short brief.
tools: read, grep, find, ls
context: fresh
---
Return a brief with sources. Do not edit files.
```

Interactive task commands:

```text
/tasks                 list tasks
/task <id|name>        show one task
/task cancel <id|name> cancel a queued or running task
```

The parent can also stop a task from the model side with the `agent_cancel`
tool, which is the same operation as `/task cancel`. Cancellation is
cooperative: a queued task never starts, and a running one stops at its next
checkpoint. Tasks do not need to be stopped by hand to be bounded: a turn
whose model keeps repeating the same call, or keeps getting tool errors, ends
by itself with a `no progress: ...` error, and each sub-agent inherits the
parent's limits (see
[`[agent].no_progress_repeats`](docs/user-manual.md#stopping-a-turn-that-makes-no-progress)).

Child agents cannot start nested agents. Their child-only `agent_report` tool
sends one message to the parent without ending the task; children use it only
to answer a parent question or to report a blocker that needs a parent
decision, never for progress updates, plans, or interim findings, which go in
the final report instead. Each task allows 10 `agent_report` calls on the
child's own initiative, plus one more for each `agent_send` message the
parent sends; a call past that limit returns an error instead of reaching the
parent. The parent can send a follow-up task update to a queued or running
child with the `agent_send` tool; the child reads it at the next safe
checkpoint, not by interrupting an in-flight provider or tool call. Child
transcripts are not persisted.

### Run a durable workflow

Put named `AGENT.md` definitions under `.otto/agents`, then declare a DAG in
`.otto/workflows/review.toml`:

```toml
version = 1

[[steps]]
id = "research"
agent = "researcher"
prompt = "Collect evidence."

[[steps]]
id = "review"
agent = "reviewer"
prompt = "Review the evidence."
needs = ["research"]
```

```text
otto workflow run review --input "review the current change"
otto workflow status <run-id>
otto workflow fork <run-id> --after-step <step-id>
otto workflow resume <run-id> --retry <interrupted-step>
```

Workflow state lives in `~/.otto/workflows.db`; each attempt has a separate
append-only transcript under `~/.otto/workflow-sessions`. Interrupted steps are
never retried automatically.

## Safety and limitations

- **Supported providers** are `openai-compatible` and `chatgpt`.
- **macOS** is the supported platform. On **Linux** Otto builds and runs, but
  it has no sandbox: `bash` is not registered at all unless you pass
  `--sandbox off`, which runs commands as your user with nothing confining
  them. File tools, sessions, memory, skills, sub-agents, MCP, workflows, the
  server, and the TUI work the same on both. `make check` is a macOS gate;
  Linux is covered by `make check-linux`.
- File tools stay within the selected workspace. Shell commands use Seatbelt by
  default; `--sandbox off` explicitly disables shell sandboxing. Seatbelt is not
  a VM and does not prevent destructive changes inside the writable workspace.
  The workspace's `.git` entry, `.git/config` and `.git/hooks` are read-only
  under Seatbelt; see
  [the user manual](docs/user-manual.md#git-metadata-is-read-only-in-the-seatbelt-sandbox).
- In an interactive parent session, Otto can request one-time unsandboxed Bash
  execution. Review the exact command and run `/approve <id>` to grant it once;
  the request remains pending until it is replaced, consumed, or Otto exits, and never changes `config.toml`.
- Session files may contain workspace files, prompts, images, and tool results. Treat
  them as sensitive.
- Plugin packages support Skills, MCP and [Otto hooks/custom tools](docs/user-manual.md#hooks-and-custom-tools); legacy host Hooks, Commands, Agents, LSP,
  OpenAI Apps and Pi runtime extensions are not loaded. See
  [plugin packages](docs/user-manual.md#plugin-packages).
- No automatic project-local config discovery, session trees/forks,
  deletion, or search.
- No memory backup/restore/verify commands.
- No per-skill `allowed-tools` enforcement.
- No nested sub-agent delegation; `agent_report` sends the parent an answer to
  its question or a blocker needing its decision, limited to 10 calls on the
  child's own initiative per task plus one more per `agent_send` message,
  with a call past the limit returning an error; parent-to-child `agent_send`
  delivery waits for the child's next safe checkpoint and does not interrupt
  an in-flight provider or tool call; child transcripts are not persisted.
- No automatic parent-side supervisor that cancels stale sub-agent tasks: the
  parent stops one itself with `agent_cancel`, and every turn has the
  `no progress` guard. A turn stuck inside a single long tool call is bounded
  only by the `[agent]` deadlines, which are unlimited unless you set them.
- Durable workflows are acyclic: no loops, conditions, group chat, nested
  workflows, or automatic retry. Static `handoff` steps can pass control to a
  named next agent; `fork` creates a new run from a committed step boundary
  without rewinding external side effects.
- MCP stdio servers run unsandboxed, with an explicit environment (`PATH`,
  `HOME`, `TMPDIR`, `LANG`, `TERM`, plus the configured `env` table only).
  An MCP server that exits stays disconnected until Otto restarts; Otto does
  not respawn it.
- The local server binds loopback addresses only; there is no TLS, CORS, or
  token persistence. The Unix socket relies on file permissions for access
  control.

Sessions default to **AUTO** smart approval: confined work runs directly; requests
for additional authority receive an isolated review against your task and the
exact operation. User questions, explicit checkpoints and security permission
changes still wait for you. Reviews and decisions are recorded; a review shows
no approval card or modal until it needs your decision. `/auto off` selects
manual approval; `/auto on` restores AUTO for this session. `/permissions`
shows the policy and reviewer; `/permissions policy never` rejects additional
authority without prompting. These settings survive resume. Terminal and Web status
bars show the mode; Telegram and Feishu support the same commands, and Telegram
also offers mode buttons. See [unified approvals](docs/user-manual.md#unified-approvals).

Configure shell permissions interactively with `otto sandbox setup`; see the
[setup guide](docs/user-manual.md#interactive-sandbox-setup). In a running
session, `/sandbox allow <path>` and `/sandbox network allow|deny` write the
same `[sandbox]` settings and apply them to the current process. Models can
also request a persistent read-path grant; in MANUAL mode this uses a centered
TUI approval modal or a Telegram/Feishu approval card; the prompt shows the path
and that it will be saved. The TUI shows one request at a time, has no feedback
field, and selects **Approve** by default; a successful decision closes the
modal immediately. `Esc` dismisses an undecided modal and `/approve` reopens
pending requests. Chat cards and passive memory/reflection notices are sent
only to the exact originating chat rather than every chat bound to the same
session or workspace. While an approval waits, you can ask Otto to explain or
withdraw it; normal tasks remain queued.
Otto can also consult its installed-version user manual and current tool list;
see [self-help and approval controls](docs/user-manual.md#ottos-own-help-and-approval-controls).
`[sandbox].excluded_commands` lists simple commands, such as `lark-cli *`, that
run outside the sandbox with the real `HOME`; see the
[user manual](docs/user-manual.md#configuration).

Read [tools and safety](docs/user-manual.md#tools-and-safety) before granting
access to a workspace.

## Use Otto as a library

Another Rust program can build the same agent `otto acp` runs, in process,
with `otto::embed`. Add a git dependency pinned to a commit (the crates are
not on crates.io):

```toml
[dependencies]
otto = { git = "https://github.com/baiyuqing/otto", rev = "<commit>" }
```

```rust
use otto::embed::{ApprovalDecision, Component, Input, Otto};
use tokio_util::sync::CancellationToken;

let otto = Otto::builder()
    .workspace("/path/to/repo")
    .disable(Component::Mcp)
    .build()
    .await?;
let session = otto.new_session(None).await?;
session.set_auto_mode(false)?; // MANUAL: approval requests reach the callback
let cancel = CancellationToken::new();
let stop = session
    .prompt(
        Input::Text("fix the failing test"),
        &mut |event| println!("{event:?}"),
        &cancel,
        |_request| async { ApprovalDecision::Deny },
    )
    .await?;
otto.close().await?;
```

The agent reads the same config, environment and `~/.otto` state as the CLI,
and its sessions are listed and resumed by the CLI. Every component is on
unless the program disables it; a switch can only turn a component off. One
`Otto` exists per process. The `otto::embed` rustdoc
(`cargo doc -p otto --open`) is the reference; the
[design](docs/specs/2026-10-07-otto-embedding-api.md) records the behavior and
the limits.

## Contributing

The code is a Cargo workspace of two crates:

- `crates/otto-core` holds the provider contract, wire codecs, session codec,
  agent loop, and config. It builds for `wasm32-unknown-unknown`.
- `crates/otto` is the native binary: CLI, REPL, TUI, tools, sandbox, memory,
  skills, sub-agents, the `otto serve` server, and `otto acp`.
- [otto-plus](https://github.com/baiyuqing/otto-plus) owns the browser UI and
  WASM bindings, consuming this repository's shared core at a pinned revision.

See the [code organization map](docs/code-organization.md) for a quick
repository and ownership overview, [AGENTS.md](AGENTS.md) for the task map,
and the [development guide](docs/development.md) for detailed contracts and
validation. Design documents live in [docs/specs](docs/specs/); the
[Rust rewrite plan](docs/specs/2026-09-13-rust-rewrite-plan.md) records why the
Go implementation (tagged `go-final`) was replaced.

```bash
make build
make check-fast  # manual use only: checks for the paths changed since origin/main
make check       # full macOS acceptance: all tests, wasm, PTY, and web UI
                 # (needs wasm-pack 0.15 and Node 24+)
```

## License

[MIT](LICENSE).
