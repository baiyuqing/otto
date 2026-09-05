<p align="center">
  <img src="docs/logo.png" alt="Otto logo" width="320">
</p>

# Otto — AI Coding Agent for macOS

[简体中文](README.zh-CN.md) · [User manual](docs/user-manual.md)

**Otto is a local-first AI coding assistant for your terminal, built in Go.**
Read an unfamiliar codebase, make focused edits, and run checks in one session.
Connect through an OpenAI-compatible API endpoint or sign in with `otto login`
for the ChatGPT provider. Model requests go to the selected provider; local-first
refers to the runtime, session history, and memory storage.

- **Work in your terminal.** An inline TUI for interactive work, a REPL for pipes,
  and headless mode for scripts.
- **Keep access bounded.** Workspace-confined file tools and macOS Seatbelt
  sandboxing for shell commands by default.
- **Continue across tasks.** Persistent sessions, context compaction, local
  memory, reusable skills, and bounded sub-agents.

## Install from source

Requires **macOS and Go 1.26 or newer**, plus access to one of the two providers.

```bash
git clone https://github.com/baiyuqing/otto.git
cd otto
go build -trimpath -o ./otto ./cmd/otto
./otto --help
```

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
- [Local server](docs/user-manual.md#agent-server): `otto serve` over a Unix socket.
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
  session naming, deletion, or search.
- No automatic memory extraction or memory backup/restore/verify commands.
- No user-facing `/skills` or `/skill` commands or per-skill `allowed-tools`
  enforcement.
- No nested sub-agent delegation; child transcripts are not persisted.
- The local server has no TCP listener and relies on socket file permissions
  for access control.

Configure shell permissions interactively with `otto sandbox setup`; see the
[setup guide](docs/user-manual.md#interactive-sandbox-setup).

Read [tools and safety](docs/user-manual.md#tools-and-safety) before granting
access to a workspace.

## Contributing

See [AGENTS.md](AGENTS.md) for repository conventions and development checks.
Design documents live in [docs/specs](docs/specs/).

```bash
make build
make check
```

## License

[MIT](LICENSE).
