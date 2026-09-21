<p align="center">
  <img src="docs/logo.png" alt="Otto logo" width="320">
</p>

# Otto — a local-first agent for macOS

[简体中文](README.zh-CN.md) · [User manual](docs/user-manual.md)

**Otto is a local-first agent for your terminal, built in Rust.**
Give it a task; it runs a loop of model completions, tool calls, and — when
needed — context compaction. Connect through an OpenAI-compatible API endpoint
or sign in with `otto login` for the ChatGPT provider. Model requests go to the
selected provider; local-first refers to the runtime, session history, and
memory storage.

- **Work in your terminal.** A full-screen TUI for interactive work, a REPL for pipes,
  and headless mode for scripts.
- **Keep access bounded.** Workspace-confined file tools and macOS Seatbelt
  sandboxing for shell commands by default.
- **Keep going without you typing.** Persistent sessions, local memory, reusable
  skills, bounded sub-agents, timers, and — with `otto serve` — optional Feishu
  inbound into the session inbox.

## Install from source

Requires **macOS**, the pinned **Rust 1.98** toolchain (`rustup toolchain
install` picks it up from `rust-toolchain.toml`), **Node 24+**, `wasm-pack`
0.15, and access to one of the two providers.

```bash
git clone https://github.com/baiyuqing/otto.git
cd otto
make install
otto --help
```

`make install` refreshes and embeds the Web UI, builds the release binary, and
installs it to `~/.local/bin/otto`. Make sure `~/.local/bin` is on your `PATH`.
A direct `cargo build` without a prior `make ui` embeds a one-line placeholder
instead.

If you only want a local copy in the checkout, run `make build` and then
`./otto --help`.

## Quick start

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

## Try a task

Start Otto in the workspace you want it to use:

```bash
./otto --provider chatgpt --model YOUR_MODEL_ID --cwd /path/to/workspace
```

Example prompts to enter in the interactive session:

```text
What is in this workspace, and what should I do next?
Remind me in five minutes to follow up.
Remember that we decided to ship the inbox path first.
```

After configuring a default profile, you can also run one prompt and exit:

```bash
./otto --approve "summarize what this workspace is for"
./otto --continue
```

Use `/help` for interactive commands. Sessions support continuing, resuming, and
archiving. See [sessions](docs/user-manual.md#sessions) and
[headless mode](docs/user-manual.md#headless-mode).

In the TUI, `/image <path>` attaches one PNG, JPEG, or WebP image to the next
prompt. In the Web UI, choose **Image** or paste a screenshot before sending.
The selected model and OpenAI-compatible endpoint must support image input.

## More workflows

- [Local memory](docs/user-manual.md#memory): search, remember, review, and forget.
- [Skills](docs/user-manual.md#skills): reusable instructions in `SKILL.md` files.
- [MCP servers](docs/user-manual.md#mcp-servers): connect stdio or HTTP Model
  Context Protocol servers and use their tools from the same turn loop.
- [Timers](docs/user-manual.md#remind): the model schedules a later wake with
  `remind`; `/timers` lists this session's outstanding timers and
  `/timers cancel <id>` stops one.
- [Local server](docs/user-manual.md#agent-server): `otto serve` over a Unix
  socket or a loopback TCP port, with an embedded browser UI. Optional
  [Feishu inbound](docs/user-manual.md#feishu-inbound) delivers group and chat
  text into open session inboxes.
- [Usage history](docs/user-manual.md#observability): a Web UI analysis page
  for local provider token trends and cache hit rate, without prompt or tool
  content.
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

Child agents cannot start nested agents, and their transcripts are not
persisted.

## Safety and limitations

- **macOS only.** Supported providers are `openai-compatible` and `chatgpt`.
- File tools stay within the selected workspace. Shell commands use Seatbelt by
  default; `--sandbox off` explicitly disables shell sandboxing. Seatbelt is not
  a VM and does not prevent destructive changes inside the writable workspace.
- In an interactive parent session, Otto can request one-time unsandboxed Bash
  execution. Review the exact command and run `/approve <id>` to grant it once;
  the request expires after five minutes and never changes `config.toml`.
- Session files may contain workspace files, prompts, images, and tool results. Treat
  them as sensitive.
- No plugins, automatic project-local config discovery, session trees/forks,
  deletion, or search.
- No automatic memory extraction or memory backup/restore/verify commands.
- No per-skill `allowed-tools` enforcement.
- No nested sub-agent delegation; child transcripts are not persisted.
- MCP stdio servers run unsandboxed, with an explicit environment (`PATH`,
  `HOME`, `TMPDIR`, `LANG`, `TERM`, plus the configured `env` table only).
  An MCP server that exits stays disconnected until Otto restarts; Otto does
  not respawn it.
- The local server binds loopback addresses only; there is no TLS, CORS, or
  token persistence. The Unix socket relies on file permissions for access
  control.

Configure shell permissions interactively with `otto sandbox setup`; see the
[setup guide](docs/user-manual.md#interactive-sandbox-setup).

Read [tools and safety](docs/user-manual.md#tools-and-safety) before granting
access to a workspace.

## Contributing

The code is a Cargo workspace of three crates:

- `crates/otto-core` holds the provider contract, wire codecs, session codec,
  agent loop, and config. It builds for `wasm32-unknown-unknown`.
- `crates/otto` is the macOS binary: CLI, REPL, TUI, tools, sandbox, memory,
  skills, sub-agents, inbound adapters, and the `otto serve` server.
- `crates/otto-web` compiles `otto-core` to WebAssembly for the browser UI in
  `ui/`, so the web frontend and the binary share one implementation.

See [AGENTS.md](AGENTS.md) for the task map and the
[development guide](docs/development.md) for contracts and validation.
Design documents live in [docs/specs](docs/specs/); the
[Rust rewrite plan](docs/specs/2026-09-13-rust-rewrite-plan.md) records why the
Go implementation (tagged `go-final`) was replaced.

```bash
make build
make check-fast  # rustfmt, clippy, focused otto-core tests
make check       # full macOS acceptance: all tests, wasm, PTY, and web UI
                 # (needs wasm-pack 0.15 and Node 24+)
```

## License

[MIT](LICENSE).
