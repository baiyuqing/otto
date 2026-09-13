<p align="center">
  <img src="docs/logo.png" alt="Otto logo" width="320">
</p>

# Otto — AI Coding Agent for macOS

[简体中文](README.zh-CN.md) · [User manual](docs/user-manual.md)

**Otto is a local-first AI coding assistant for your terminal, built in Rust.**
Read an unfamiliar codebase, make focused edits, and run checks in one session.
Connect through an OpenAI-compatible API endpoint or sign in with `otto login`
for the ChatGPT provider. Model requests go to the selected provider; local-first
refers to the runtime, session history, and memory storage.

- **Work in your terminal.** A full-screen TUI for interactive work, a REPL for pipes,
  and headless mode for scripts.
- **Keep access bounded.** Workspace-confined file tools and macOS Seatbelt
  sandboxing for shell commands by default.
- **Continue across tasks.** Persistent sessions, context compaction, local
  memory, reusable skills, and bounded sub-agents.

## Install from source

Requires **macOS**, the pinned **Rust 1.98** toolchain (`rustup toolchain
install` picks it up from `rust-toolchain.toml`), and access to one of the two
providers.

```bash
git clone https://github.com/baiyuqing/otto.git
cd otto
make build
./otto --help
```

`make build` embeds whatever is already in `ui/dist`. Run `make ui` first
(needs Node 24+ and `wasm-pack` 0.15) to embed the real web UI; otherwise
`otto serve` serves a one-line placeholder page at `/` instead of the UI.

The examples below run `./otto` from this directory. Put the binary on your
`PATH` to use `otto` from other directories.

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

## Try a coding task

Start Otto in the project you want to work on:

```bash
./otto --provider chatgpt --model YOUR_MODEL_ID --cwd /path/to/project
```

Example prompts to enter in the interactive session:

```text
Explain this repository's entry points and how to run its tests.
Add a failing test for the bug we just identified, then make the smallest fix.
Run the relevant tests and summarize the diff.
```

After configuring a default profile, you can also run one prompt and exit:

```bash
./otto --approve "summarize TODOs in this repo"
./otto --continue
```

Use `/help` for interactive commands. Sessions support continuing, resuming, and
archiving. See [sessions](docs/user-manual.md#sessions) and
[headless mode](docs/user-manual.md#headless-mode).

## More workflows

- [Local memory](docs/user-manual.md#memory): search, remember, review, and forget.
- [Skills](docs/user-manual.md#skills): reusable instructions in `SKILL.md` files.
- [Local server](docs/user-manual.md#agent-server): `otto serve` over a Unix
  socket or a loopback TCP port, with an embedded browser UI.
- [Configuration](docs/user-manual.md#configuration),
  [CLI reference](docs/user-manual.md#command-line-reference), and
  [troubleshooting](docs/user-manual.md#troubleshooting).

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

Child agents cannot start nested agents, and their transcripts are not
persisted.

## Safety and limitations

- **macOS only.** Supported providers are `openai-compatible` and `chatgpt`.
- File tools stay within the selected workspace. Shell commands use Seatbelt by
  default; `--sandbox off` explicitly disables shell sandboxing. Seatbelt is not
  a VM and does not prevent destructive changes inside the writable workspace.
- Session files may contain source code, prompts, and tool results. Treat them
  as sensitive project data.
- No plugins, automatic project-local config discovery, session trees/forks,
  deletion, or search.
- No automatic memory extraction or memory backup/restore/verify commands.
- No user-facing `/skills` or `/skill` commands or per-skill `allowed-tools`
  enforcement.
- No nested sub-agent delegation; child transcripts are not persisted.
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
  skills, sub-agents, and the `otto serve` server.
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
