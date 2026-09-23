# Repository Guidelines

Read this file first. It is the canonical short rulebook; the [development
guide](docs/development.md) contains the detailed package contracts and
workflow.

## Scope and task map

- Kite supports only the `openai-compatible` and `chatgpt` providers. macOS is the supported platform and the only one with a sandbox (Seatbelt). Kite also builds and runs on Linux, where there is no confined driver: `bash` is unavailable unless `--sandbox off` is passed explicitly, and everything else (file tools, sessions, memory, MCP, server, TUI) works. Keep platform-specific code behind `cfg`, and keep a new platform's claims limited to what its tests actually cover.
- `crates/kite-core` is the wasm-safe library: `model`, `provider` (neutral contract), `openaicompat`/`openairesponses` (wire formats), `tool` (definitions), `session` (Pi v3 codec), `agent` (provider/tool turn loop), `config`, `wire`, and `safetext`.
- `crates/kite` is the native binary: `cli` (composition, flags, REPL, process lifecycle), `app` (shared lifecycle and frontend capabilities), `session` (store), `config` (path resolution and the backed-up config writer), `tool` (native execution), `mcp` (stdio/HTTP client, JSON-RPC codec, OAuth sign-in for external tool servers), `sandbox` (Seatbelt/direct drivers), `provider` (HTTP transports), `auth` (ChatGPT OAuth), `memory` (SQLite/FTS5 store and service), `usage` (provider-token collection, SQLite storage, and aggregate analysis), `skill`, `subagent`, `workflow` (durable TOML DAG runs, approval gates, restart recovery, and event storage), `inbound` (host `lark-cli` Feishu consumer into the session inbox), `server` (HTTP/JSON/SSE and the embedded web UI), and `tui`.
- `crates/kite-web` is the wasm cdylib the browser UI loads. `ui/` (TypeScript) owns the browser frontend and is a client of `crates/kite`'s HTTP API and `crates/kite-web`'s wasm exports only.
- The [architecture contract design](docs/specs/2026-09-05-architecture-contracts.md) records compatibility and ownership rationale. `make rust-wasm-check` enforces the wasm32 boundary: `kite-core` and `kite-web` must stay buildable for `wasm32-unknown-unknown`, which keeps native-only code (sandbox, auth, SQLite, process control) out of the shared library.
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

- Use a dedicated Git worktree at `.worktree/<name>` under the primary checkout (never a sibling directory) and a development branch for every feature or bug fix; never implement directly on `main`.
- If a design or spec is needed, get explicit approval before writing production code or tests. After approval, implement and verify the agreed scope without repeated confirmation; ask only for material scope or contract changes.
- TDD is required for feature work and bug fixes: failing test first, smallest RED run, minimal change, focused GREEN run, then broader relevant gates. Docs-only changes are exempt.
- The parent owns planning, interfaces, integration review, and acceptance. Delegated work has bounded file ownership; hand off overlapping file ownership and preserve other workers' changes.
- Keep dependencies and abstractions minimal, reuse existing contracts, and preserve append-only session history and compatibility. See the [development guide](docs/development.md) before changing shared contracts.

## Safety

- API keys come only from environment variables; ChatGPT credentials come only from `kite login`. Never place secrets in config, fixtures, logs, docs, tests, or skill files.
- File tools reject workspace and skill-directory escapes after canonical-path and symlink validation.
- `bash` runs through `crates/kite`'s `sandbox` module; Seatbelt is the default and only explicit sandbox `off` is unsandboxed. It still starts in the selected workspace.
- Keep the default test suite offline: no provider credentials, network access, or real interactive terminal.

## Verification

Use the Makefile targets as the canonical commands:

```bash
make check-fast   # rustfmt, clippy, focused kite-core tests, git diff --check
make check        # full macOS build/lint/all-tests/wasm/PTY/UI gate
make check-linux  # the Linux gate: the same minus Seatbelt conformance and the UI
```

See the [development guide](docs/development.md)
for the focused package set, test commands, and contract-specific checks.

CI runs `make check` on macOS and `make check-linux` on Linux, with the
toolchain and action pins in [the workflow](.github/workflows/checks.yml). A
change that touches only Markdown outside `crates/` and `testdata/`, `docs/`,
or `LICENSE` skips both gates; everything else runs them in full.
Full host validation requires macOS 26+ and standalone Command Line Tools;
see the development guide.

Keep README and user-facing docs limited to implemented, tested behavior. Do
not describe `agent_send`, `agent_cancel`, `agent_report`, automatic memory
extraction, memory backup/restore/verify, or `allowed-tools` enforcement as
working features.
