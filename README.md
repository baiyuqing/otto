<p align="center">
  <img src="docs/logo.svg?v=2" alt="Otto logo" width="320">
</p>

# Otto — a local-first personal AI agent

[简体中文](README.zh-CN.md) · [Documentation](docs/README.md) · [Changelog](CHANGELOG.md)

Otto helps with research, writing, planning, coding and practical tasks. Use it
from a terminal, [otto-plus](https://github.com/baiyuqing/otto-plus) browser client,
or Telegram and Feishu through `otto-connect`. Connecting to `otto serve` lets
clients share server-side sessions.

- **Keep context:** persistent sessions, memory, reusable skills and reflection.
- **Delegate and automate:** sub-agents, scheduled tasks and recoverable workflows.
- **Control access:** workspace-confined file tools, approvals and macOS shell sandboxing.

Otto supports **OpenAI-compatible** endpoints and **ChatGPT** sign-in. Model
requests go to the selected provider; “local-first” describes the runtime,
session history and default memory storage, not a local model.

## Install from source

Install Rust through rustup (the repository pins the toolchain in
[`rust-toolchain.toml`](rust-toolchain.toml)) and the Go version required by
[`chat-connector/go.mod`](chat-connector/go.mod). macOS is the supported platform.
Linux builds and runs, but has no native shell sandbox.

```bash
git clone https://github.com/baiyuqing/otto.git
cd otto
make install
otto --version
otto setup
```

`make install` builds Otto and `otto-connect`, installs both to `~/.local/bin/`,
and installs bundled skills under `~/.otto/skills/`. Add `~/.local/bin` to your
`PATH`. For a checkout-only build, use `make build` and `./otto` instead.

## First use

`otto setup` creates a profile without storing an API key. Choose a model
available to your account or provider.

For ChatGPT:

```bash
otto login
otto
```

For an OpenAI-compatible endpoint, select its URL, model and API-key environment
variable during setup. Set that variable privately in the environment before
starting `otto`; never put its value in a config file or prompt.

Try asking Otto to explain a repository, research a question or implement a
small change. Use `/help` for commands and `/new` to start a fresh session.

To upgrade a source installation, update the checkout, rerun `make install`,
and restart running Otto/connector processes. See the
[getting-started guide](docs/manual/getting-started.md) for configuration,
one-off CLI flags, the launcher script and Docker setup.

## Choose a workflow

| Goal | Guide |
| --- | --- |
| Terminal interaction, sessions and headless scripts | [Interactive use](docs/manual/interactive-use.md) |
| Approvals, sandboxing, tools and sub-agents | [Safety and tools](docs/manual/safety-and-tools.md) |
| Browser/API server, ACP, Telegram or Feishu | [Servers and connectors](docs/manual/servers-and-connectors.md) |
| Durable workflows and workspace identity | [Automation](docs/manual/automation.md) |
| Memory, skills, MCP and extensions | [Memory and extensions](docs/manual/memory-and-extensions.md) |
| Embed Otto in a Rust application | [Embedding API](docs/development/architecture.md#embedding-api) |
| Diagnose failures | [Troubleshooting](docs/manual/troubleshooting.md) |

## Safety and limitations

- File tools stay within the selected workspace. On macOS, Bash uses the native
  Seatbelt sandbox by default. Linux and containers cannot provide that boundary;
  shell execution there requires explicitly disabling the sandbox.
- Approvals and sandbox permissions are separate. Approval text may be displayed
  and persisted: do not include secrets. Keep API keys and chat secrets in
  environment variables; ChatGPT credentials come from `otto login`.
- Providers, remote memory and configured integrations may receive task content.
  Inspect their configuration before using sensitive data.
- Commands can still have real effects within granted permissions. Review
  proposed changes and keep backups for valuable work.

See [safety and tools](docs/manual/safety-and-tools.md) for permission modes and
recovery behavior. Only macOS with standalone Command Line Tools can validate
Seatbelt behavior; Linux checks do not establish sandbox conformance.

## Contributing

Read [`AGENTS.md`](AGENTS.md), the [code organization map](docs/code-organization.md)
and [development guide](docs/development.md). Work in a dedicated branch/worktree,
install the secret-scanning hook with `make setup-git-hooks` (requires Gitleaks),
and run the checks for the code you change. The repository has no CI; PRs must
report local verification.

[Architecture diagram](docs/architecture/otto-architecture.svg) ·
[License](LICENSE)
