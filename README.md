# otto — a personal AI agent

[简体中文](README.zh-CN.md) · [Documentation](docs/README.md) · [Changelog](CHANGELOG.md)

Otto helps with research, writing, planning, coding and practical tasks. Use it
from a terminal, [otto-plus](https://github.com/baiyuqing/otto-plus) browser client,
or Telegram through `otto-connect`. Connecting to `otto serve` lets
clients share server-side sessions.

- **Keep context:** persistent sessions, memory and reusable skills.
- **Delegate and automate:** sub-agents, scheduled tasks and recoverable workflows.
- **Control access:** workspace-confined file tools, approvals and macOS shell sandboxing.

Otto supports the first-party **Claude API**, **OpenAI-compatible** endpoints,
and **ChatGPT** sign-in. Model requests go to the selected provider;
“local-first” describes the runtime, session history and default memory storage,
not a local model.

## Architecture

[![otto architecture: parallel frontend crates, five runtime layers and independent support packages](docs/architecture/otto-architecture.png?v=0a730279-axonometric-1)](docs/architecture/otto-architecture.svg)

[Code organization](docs/code-organization.md) · [SVG](docs/architecture/otto-architecture.svg) · [Editable HTML](docs/architecture/otto-architecture.html)

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

`otto setup` writes a default model and its provider table with one model
entry, without storing an API key. Choose a model available to your
account or provider. Models are selected as `<provider>/<model_id>`; see
[provider and model configuration](docs/manual/getting-started.md#providers-and-models).

For ChatGPT:

```bash
otto login
otto
```

For Claude or an OpenAI-compatible endpoint, select its model and API-key
environment variable during setup (and the endpoint URL for an OpenAI-compatible
provider). Set that variable privately in the environment before starting
`otto`; never put its value in a config file or prompt.

Try asking Otto to explain a repository, research a question or implement a
small change. Use `/help` for commands and `/new` to start a fresh session.

To upgrade a source installation, update the checkout, stop every running Otto
process (including `otto serve` and connectors), rerun `make install`, and start
them again; an old and a new binary must not share `~/.otto/` at the same time.
Check the [changelog](CHANGELOG.md) for breaking changes first. See the
[getting-started guide](docs/manual/getting-started.md) for configuration,
one-off CLI flags, the launcher script and Docker setup.

## Choose a workflow

| Goal | Guide |
| --- | --- |
| Terminal interaction, sessions and headless scripts | [Interactive use](docs/manual/interactive-use.md) |
| Approvals, sandboxing, tools and sub-agents | [Safety and tools](docs/manual/safety-and-tools.md) |
| Browser/API server, ACP or Telegram | [Servers and connectors](docs/manual/servers-and-connectors.md) |
| Durable workflows and workspace identity | [Automation](docs/manual/automation.md) |
| Memory, skills, MCP and extensions | [Memory and extensions](docs/manual/memory-and-extensions.md) |
| Diagnose failures | [Troubleshooting](docs/manual/troubleshooting.md) |

## Safety and limitations

- File tools stay within the selected workspace. On macOS, Bash uses the native
  Seatbelt sandbox by default. Linux and containers cannot provide that boundary;
  shell execution there requires explicitly disabling the sandbox.
- Approvals and sandbox permissions are separate. Approval text may be displayed
  and persisted: do not include secrets. Keep API keys and chat secrets in
  environment variables; ChatGPT credentials come from `otto login`.
- Providers, memory plugins and configured integrations may receive task content.
  Inspect their configuration before using sensitive data. Before each provider
  request Otto locally replaces known credentials, high-confidence secret formats,
  email addresses, `+` phone numbers, payment cards and Chinese resident
  identity-card numbers; other personal data is sent unchanged. See the
  [provider egress guard](docs/manual/safety-and-tools.md#provider-egress-secret-and-pii-guard).
- `otto serve` listens on a Unix socket by default. A non-loopback TCP listener
  requires TLS (`[server].tls_cert`, `[server].tls_key`) and `OTTO_SERVER_TOKEN`;
  see [Listener](docs/manual/servers-and-connectors.md#listener). Release builds
  abort the whole process on a panic, so run a long-lived server under a
  supervisor that restarts it.
- Commands can still have real effects within granted permissions. Review
  proposed changes and keep backups for valuable work.

See [safety and tools](docs/manual/safety-and-tools.md) for permission modes and
recovery behavior. Only macOS with standalone Command Line Tools can validate
Seatbelt behavior; Linux checks do not establish sandbox conformance.

## Contributing

Read [`AGENTS.md`](AGENTS.md), the [code organization map](docs/code-organization.md)
and [development guide](docs/development.md). Work in a dedicated branch/worktree,
install the pre-commit hook with `make setup-git-hooks` (requires Gitleaks and Node),
and run the checks for the code you change. The repository has no CI; PRs must
report local verification.

[Architecture and crate boundaries](docs/code-organization.md#rust-layers) ·
[Use `otto-core` as a Rust library](crates/otto-core/README.md) ·
[License](LICENSE)
