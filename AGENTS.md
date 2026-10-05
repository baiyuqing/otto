# Repository Guidelines

Read this file first. It is the canonical short rulebook. Use the [code
organization map](docs/code-organization.md) to find ownership and entry
points; the [development guide](docs/development.md) contains the detailed
package contracts and workflow.

## Scope and task map

- Otto supports only the `openai-compatible` and `chatgpt` providers. macOS is the supported platform and the only one with a sandbox (Seatbelt). Otto also builds and runs on Linux, where there is no confined driver: `bash` is unavailable unless `--sandbox off` is passed explicitly, and everything else (file tools, sessions, memory, MCP, server, TUI) works. Keep platform-specific code behind `cfg`, and keep a new platform's claims limited to what its tests actually cover.
- `crates/otto-core` is the wasm-safe library: `approval` (neutral request and lifecycle types), `model`, `provider` (neutral contract), `openaicompat`/`openairesponses` (wire formats), `tool` (definitions), `session` (Pi v3 codec), `agent` (provider/tool turn loop), `config`, `wire`, and `safetext`.
- `crates/otto` is the native binary: `cli` (flags, REPL, process lifecycle, and composing the frontends), `runtime` (assembling one runnable agent: `Builder`, `Runner`, wiring, system prompt, sandbox configuration changes), `metrics` (HTTP and turn metrics), `app` (shared lifecycle and frontend capabilities), `session` (store), `failover` (session lease, commit guard, takeover recovery notification), `config` (path resolution and the backed-up config writer), `tool` (native execution, including the `web_fetch`/`web_search` tools in `tool::web` and their destination policy), `mcp` (stdio/HTTP client, JSON-RPC codec, OAuth sign-in for external tool servers), `sandbox` (Seatbelt/direct drivers), `provider` (HTTP transports), `auth` (ChatGPT OAuth), `approval` (durable requests, decisions, domain application and frontend events), `memory` (Turso/FTS store and service, and advisory review hints in `memory::advice`), `reflection` (the on-demand `/reflect` use case: it queues memory candidates for human review and queues vetted skill changes for human approval before writing to `~/.otto/skills`), `usage` (provider-token collection, Turso storage, and aggregate analysis), `skill` (discovery, and the experimental automatic contract check), `typesafe` (auxiliary-model clients for skill checks and memory advice), `storage` (the synchronous native Turso adapter), `workspace` (persistent workspace identity and move-compatible session/memory ownership), `fsops` (shared pinned filesystem operations), `log` (the key=value process logger, injected at the composition root), `subagent` (including persistent task records in `~/.otto/tasks.db`), `workflow` (durable TOML DAG runs, approval gates, restart recovery, and event storage), `schedule` (scheduled deliveries, prompt runs, retries and conversation write-back), `server` (HTTP/JSON/SSE and the embedded web UI), `client` (HTTP client of `otto serve` over its Unix socket, used by `otto acp --attach` and `otto --attach`), `acp` (the Agent Client Protocol stdio server behind `otto acp`, and its `--attach` relay to `otto serve`), and `tui` (the terminal UI, and its `--attach` mode as a client of `otto serve`). The frontends (`cli`, `tui`, `acp`, `server`, `client`) build on the other modules, never the reverse; `crates/otto/tests/runtime_boundary.rs` enforces it.
- `crates/otto-web` is the wasm cdylib the browser UI loads. `ui/` (TypeScript) owns the browser frontend and is a client of `crates/otto`'s HTTP API and `crates/otto-web`'s wasm exports only.
- `connect/` is `otto-connect`, the Go chat connector (Telegram, Feishu): an ACP client that drives `otto acp` as a child process or connects directly to `otto serve` over authenticated loopback ACP TCP, and links no otto code. `make connect-check` covers it and runs in `make check` and `make check-linux`. Design: [chat connector](docs/specs/2026-10-02-otto-connect.md).
- `desktop/` is a macOS Tauri 2 shell around `otto serve`, in its own Cargo workspace (not a member of the root workspace, not covered by `make check`/`make check-linux`). See [`desktop/README.md`](desktop/README.md).
- The [architecture contract design](docs/specs/2026-09-05-architecture-contracts.md) records compatibility and ownership rationale. `make rust-wasm-check` enforces the wasm32 boundary: `otto-core` and `otto-web` must stay buildable for `wasm32-unknown-unknown`, which keeps native-only code (sandbox, auth, Turso, process control) out of the shared library.
- Current user behavior belongs in the [README](README.md) and [user manual](docs/user-manual.md). Design documents are historical rationale unless they explicitly say otherwise.

Do not document or implement other providers, and do not list planned
providers or roadmap stages in user-facing docs.

## Agent-friendly architecture

Apply these requirements to every feature, fix, and refactor:

- Keep this file a short navigation map and rulebook. Put detailed contracts next to their owning code or in the development guide, and link to one canonical source instead of copying it across agent instructions.
- Make changes easy to locate: use consistent domain names, cohesive packages, explicit entry points, and adjacent tests. Update the task map when responsibilities move or new packages are introduced.
- Keep dependencies explicit and one-way. Put shared behavior in its owning layer and inject dependencies at the composition root; avoid hidden global state and frontend-specific copies of shared use cases.
- Make contracts readable from types and focused documentation: define valid states, ownership and mutation, concurrency and cancellation, error behavior, and compatibility where relevant. Validate data at trust boundaries.
- Extend existing patterns with the smallest cohesive change. Add interfaces only at real consumer boundaries; do not add speculative frameworks, registries, or configuration for future implementations.
- Turn important invariants into executable checks. Update architecture guards and contract tests when boundaries change; failures should identify the violated rule and where to fix it. Keep focused checks fast, offline, and deterministic, and preserve full acceptance coverage.
- Keep code, schemas, tests, and canonical docs consistent in the same change. Remove stale instructions or mark superseded designs as historical; record durable rationale in the repository rather than relying on chat history.

## Change rules

- Update [CHANGELOG.md](CHANGELOG.md) in the same PR for notable user or contributor changes; follow the [changelog rules](docs/development.md#changelog).
- Ship requested features enabled by default; do not add default-off gates or require an extra enable step.
- After implementation and required verification, open a PR without asking for confirmation again.
- Use a dedicated Git worktree at `.worktree/<name>` under the primary checkout (never a sibling directory) and a development branch for every feature or bug fix; never implement directly on `main`.
- If a design or spec is needed, get explicit approval before writing production code or tests. After approval, implement and verify the agreed scope without repeated confirmation; ask only for material scope or contract changes.
- Track multi-step work in a plan file at `.worktree/<name>.plan.md`, next to its worktree. The path is ignored by Git, so the file is never committed. Record the problem, each decision the user confirmed with its date, the done and open items, the known limitations, and any follow-up work that was agreed. Update the file whenever an item is finished or a decision is made, so the state survives a lost or compacted conversation.
- Feature work and bug fixes ship with tests that check the behavior the change adds or fixes; a test must fail when that behavior breaks. Writing tests before the code is not required. Run the focused tests, then the broader relevant gates. Docs-only changes are exempt.
- The parent owns planning, interfaces, integration review, and acceptance. Delegated work has bounded file ownership; hand off overlapping file ownership and preserve other workers' changes.
- Keep dependencies and abstractions minimal, reuse existing contracts, and preserve append-only session history and compatibility. See the [development guide](docs/development.md) before changing shared contracts.
- Version numbers change only in a release PR made with `make bump VERSION=X.Y.Z`; see [Releases](docs/development.md#releases).
- New features are on by default. Do not add a config switch that defaults to off or an opt-in flag for them. A feature that needs input the user must supply, such as an API key from an environment variable, runs when that input is present and is skipped otherwise.

## Safety

- API keys, chat bot tokens and chat app secrets come only from environment variables; ChatGPT credentials come only from `otto login`. Never place secrets in config, fixtures, logs, docs, tests, or skill files.
- File tools reject workspace and skill-directory escapes after canonical-path and symlink validation.
- `bash` runs through `crates/otto`'s `sandbox` module; Seatbelt is the default and only explicit sandbox `off` is unsandboxed. It still starts in the selected workspace.
- Keep the default test suite offline: no provider credentials, network access, or real interactive terminal.

## Verification

Use the Makefile targets as the canonical commands:

```bash
make check-fast   # the local gate: only the checks for paths changed since origin/main, one worktree at a time
make check        # full macOS lint/UI-build/all-tests/wasm-test/PTY/UI-test gate (no release build)
make check-linux  # the Linux gate: the same minus Seatbelt conformance and the UI
```

Locally, run the focused tests and `make check-fast`; do not run `make check`
or `make check-linux`. CI runs both full gates on every PR. See the
[development guide](docs/development.md#local-verification) for what
`check-fast` selects, test commands, and contract-specific checks.

CI runs `make check` on macOS, split into two parallel jobs (`rust-fmt` with
`rust-lint`, and `check-tests`), and `make check-linux` on Linux, with the
toolchain and action pins in [the workflow](.github/workflows/checks.yml). A
change that touches only Markdown outside `crates/` and `testdata/`, `docs/`,
or `LICENSE` skips every gate; everything else runs them in full.
Full host validation requires macOS 26+ and standalone Command Line Tools;
see the development guide.

Keep README and user-facing docs limited to implemented, tested behavior. Do
not describe `agent_cancel`, `Binding.Observe`,
memory backup/restore/verify, or `allowed-tools` enforcement as working
features.
