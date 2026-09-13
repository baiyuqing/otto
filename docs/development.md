# Development guide

`AGENTS.md` is the short entry point and canonical repository rulebook. This
page holds the detailed package contracts and development workflow. The
wasm32 architecture boundary is enforced by `make rust-wasm-check`, which
requires `crates/otto-core` and `crates/otto-web` to stay buildable for
`wasm32-unknown-unknown`; the rationale and compatibility details are in the
[architecture contract design](specs/2026-09-05-architecture-contracts.md).

## Package boundaries

Keep responsibilities split along the current Rust crate/module layout:

- `crates/otto-core` (wasm-safe library; no filesystem, network, process, or wall-clock API):
  - `model`: provider-neutral message/tool types, message/block validation, and shared deep-copy helpers
  - `provider`: neutral provider contract
  - `openaicompat`: all OpenAI-compatible Chat Completions HTTP/JSON/SSE wire code
  - `openairesponses`: all ChatGPT Responses API HTTP/JSON/SSE wire code
  - `tool`: tool definitions and schema assembly
  - `session`: the Pi v3 JSONL codec, compaction, and context association types
  - `agent`: provider/tool orchestration and event emission
  - `config`: TOML loading and runtime resolution
  - `wire`: shared wire DTOs
  - `safetext`: redaction and secret-form detection shared by native and wasm code
- `crates/otto` (native binary):
  - `cli`: composition root, flag parsing, process lifecycle, signal handling, the REPL, and concrete dependency injection
  - `app`: shared lifecycle, turn admission, session replacement, task/authentication capabilities, profile selection, and session info/history access
  - `session`: native JSONL session storage built on `otto_core::session`
  - `tool`: native execution of `read`/`grep`/`find`/`ls`/`write`/`edit`/`bash`/`skill`, with workspace validation
  - `sandbox`: sandbox driver contracts, the Seatbelt and direct drivers, environment filtering, and conformance helpers
  - `provider`: native HTTP transports for the two provider implementations
  - `auth`: ChatGPT OAuth sign-in (`otto login`/`otto logout`), credential storage at `~/.otto/auth/chatgpt.json`, and access-token refresh
  - `memory`: neutral memory contracts, validation/secret guards, conservative policy, the `Service` implementation, a null fallback, and the SQLite/FTS5 store and retriever
  - `skill`: SKILL.md frontmatter parsing, name/description validation, discovery across configured roots, and rendering of the system-prompt listing
  - `subagent`: child agent construction (`Runner`), task lifecycle, the `agent`/`agent_wait`/`agent_status` tools, shared task-formatting helpers used by both the REPL and the TUI, and AGENT.md definition discovery
  - `server`: HTTP/JSON/SSE frontend, wire DTOs, per-session turn buffering, metrics, the Unix-socket and loopback-TCP listeners, bearer-token gating of `/v1/`, and the embedded web UI (`ui/dist`, written by `make ui`)
  - `tui`: the terminal frontend on the alternate screen, transcript rendering, Markdown/tool presentation, key handling, and terminal lifecycle
- `crates/otto-web`: the wasm cdylib the browser UI loads; exports `otto-core`'s wire codecs to JavaScript through `wasm-bindgen`
- `ui/`: the TypeScript browser frontend; a client of `crates/otto`'s HTTP API and `crates/otto-web`'s wasm exports only, with no Rust code of its own and no part in `make check-fast`

Keep provider-specific wire structs inside the two provider implementation
modules. Keep file-tool workspace enforcement inside `crates/otto`'s `tool`
module, session persistence append-only, and `bash` delegated through
`crates/otto`'s `sandbox` module; only explicit sandbox `off` may use direct
execution, and it still starts in the selected workspace.

Keep `crates/otto`'s `memory` module behind its neutral contracts. The agent
loop, tools, and frontends must never reach a store directly: use the
`Service`'s bound accessors or the `Controller` memory facade. Per-turn recall
and explicit management (`memory_search`/`remember`/`forget` tools,
`/memory`/`/remember` in the REPL, and `otto memory status|forget`) are wired
end to end via `[memory]` TOML config. Model- and human-originated writes
always land as pending candidates requiring review. Automatic extraction and
durability (backup/restore/verify) remain unwired. `/memory` and `/remember`
are not yet wired into the TUI; `crates/otto/src/tui/app.rs`'s `UNPORTED`
table names them and the dispatcher answers with a "not yet ported" line
until they land.

Keep `crates/otto`'s `skill` module free of imports from other Otto modules
besides `otto-core`. The skill tool's file reads stay confined to the skill
directory. Do not document `/skills`, `/skill`, or `allowed-tools` enforcement
as working features.

Keep `crates/otto`'s `subagent` module behind the runner's construction path:
children are built only through it; the agent loop knows tasks only through
its own task registry and never imports `subagent` directly; frontends reach
tasks only through the shared task-lister facade; children never receive
`agent*`, `remember`, `forget`, or `memory_search`; child transcripts are not
persisted. Definitions cannot add tools outside the child tool set; `tools`
only narrows it. `[agents]` is TOML only, like `[skills]`. Do not document
`agent_send`/`agent_cancel`/`agent_report` as working features.

## Core contracts

- Keep dependencies explicit and directed toward shared contracts. Reuse existing helpers and concrete types; add interfaces at real consumer boundaries, not for hypothetical implementations.
- Use `model.Message.Validate`, `model.Block.Validate`, and `model.CloneMessage`/`CloneMessages`/`CloneUsage`. Both Session implementations enforce neutral validation and tool-call/result sequencing on append. Neutral validation permits transient messages without IDs/timestamps; Pi-specific encoding restrictions stay in `session`. When adding reference fields, update deep copies and ownership tests.
- `provider.Response.Message` is the single source of finish reason and usage. `Usage == nil` means unavailable; a non-nil zero usage means explicitly reported zero. Preserve explicit presence through events, task progress, notifications, aggregates, and supported persistence metadata instead of inferring absence from zero counters. Keep legacy Pi normalization in the decoder.
- Keep context associations in typed `ContextMetadata`. Prefer structured TaskID over notification wording; text parsing is only a legacy-history fallback. Preserve append-only Pi v3 compatibility and namespaced optional details, including the explicit-zero usage marker. Do not rewrite old records or invent missing historical metadata.
- `tool.Result.PersistedContent == nil` selects `Content`; a non-nil pointer selects its value, including empty text. Preserve redaction and the current-turn full-result overlay. Reuse tool definitions and assembly helpers, including `tool.BashDefinition`; keep conservative preflight and final registry validation.
- Provider and Tool instances may be shared concurrently. Respect borrowed read-only request/argument data and caller-owned returned data/schema. Per-call provider callbacks are ordered and finish before `Complete` returns; event consumers must copy reference fields before retaining mutable payloads.
- Reuse the shared compaction result payload. Keep HTTP/SSE DTOs separate from internal structs; update `testdata/server/openapi.yaml` alongside wire changes and preserve existing field meanings.

## Lifecycle and frontend contracts

- Construct `app.New` with a ready `SessionReplacement`; use `NewSessionBuilder` for new sessions. Do not restore placeholder factories or duplicate runtime construction paths.
- Controller callbacks use nonblocking `RequestClose()`. External lifecycle owners cancel active work as appropriate and call synchronous `Close()` to complete cleanup. An idle close request alone does not release resources. Preserve exactly-once session/runner cleanup, cleanup errors, and post-close Info/History snapshots; do not inspect goroutine IDs or stack text to detect reentrancy.
- Treat `agent.Task` as a query snapshot. Update existing task progress and completion through `MarkRunning`, `RecordProviderStep`, `RecordToolCall`, and `Finish`; preserve task identity, terminal states, and notification-before-Wait ordering. A cancellation request is not proof that execution has stopped.
- Frontends use `app.TaskLister`/`TaskView`, never the mutable registry or raw Inbox. Call `PrepareWake` before publishing a wake turn, then run the claim once or cancel it. Release abandoned claims on cancellation/shutdown. `Updates` is a coalescing single-consumer signal, not a broadcast subscription; add no competing scheduler. Normal and wake turns share cancellation, event delivery, and compaction accounting; one-shot runs propagate wake failures.
- Use `app.Authentication` and `app.SelectProfile` for shared use cases. Credential paths and concrete services belong in the composition root; OAuth and credential files stay in `auth`. Frontends own presentation, not credential persistence. Preserve the startup credential snapshot/restart requirement and refresh backend state after a profile switch even when saving the default fails.

## Development isolation and ownership

Choose the cheapest adequate model for routine work and escalate only when the
task requires it. Delegated work must have bounded file ownership; hand off
overlapping files before another worker edits them and preserve other workers'
changes.

Use a dedicated Git worktree and development branch for every feature or bug
fix. Never implement directly on `main`. If implementation changes are
accidentally made on `main`, restore them before continuing in the worktree.

When work requires a design or spec, finish the discussion and get explicit
approval before writing production code or tests. Once approved, carry the
agreed work through implementation and verification without repeated
confirmation; seek clarification only for material scope or contract changes.

## Rust workflow

Full host validation uses macOS 26+ with standalone Command Line Tools selected.
The copied Apple broker fixtures are ad-hoc-signed arm64e executables, which
require the third-party arm64e support introduced in macOS 26. CI selects
`/Library/Developer/CommandLineTools` so Git and Clang use the existing reviewed
developer read root. See [the workflow](../.github/workflows/checks.yml) for pins.

`rust-toolchain.toml` pins the exact channel (1.98.0) and the
`wasm32-unknown-unknown` target; `rustup toolchain install` in a repository
checkout installs that toolchain without asking. Workspace dependencies in
`Cargo.toml` are exact-pinned (`=x.y.z`), so `cargo update` never silently
changes a dependency version.

The canonical Make targets are:

```bash
make check-fast     # rustfmt --check, clippy -D warnings, focused otto-core tests, git diff --check
make check          # full macOS gate: check-fast, build, all tests, wasm check+test, PTY test, UI test
make build          # cargo build --release, then copy the binary to ./otto
make rust-fmt       # cargo fmt --all -- --check
make rust-lint      # cargo clippy --workspace --all-targets -- -D warnings
make rust-test      # cargo test --workspace (offline)
make rust-wasm-check # cargo check -p otto-core and -p otto-web for wasm32-unknown-unknown
make rust-wasm-test  # wasm-pack test --node for otto-core and otto-web
make test-tui       # cargo test -p otto --test tui_pty (needs a real PTY)
```

`check-fast` runs `rustfmt`, `clippy`, and the focused `otto-core` test suite;
`check` adds the release build, the full workspace test suite, the wasm32
build check, the wasm tests under Node, the PTY smoke test, and `make
ui-test`.

`rust-wasm-test` and `make ui`/`make ui-test` need `wasm-pack` (pinned to
0.15.0 in CI via `cargo install wasm-pack --version 0.15.0 --locked`).
`test-tui` needs a real PTY, which is unavailable in some sandboxed shells;
run it directly with `cargo test -p otto --test tui_pty` on a host that has
one. Keep the default test suite (`cargo test --workspace` without extra
flags) offline: it must not need network access, provider credentials, or a
real interactive terminal.

## Web UI workflow

`ui/` is a Vite + React + TypeScript project with `react-markdown` and
`remark-gfm` as its only runtime dependencies, plus the `otto-web` wasm
package built from `crates/otto-web`. It needs Node 24+ and wasm-pack, and it
is exercised by `make check` through `make ui-test`.

```bash
make ui       # wasm-pack build, then npm ci && npm run build → ui/dist, embedded by cargo build
make ui-test  # wasm-pack build, then npm ci && npm test (vitest): the SSE frame parser and the transcript reducer
```

`ui/dist` is a build output: only `.gitkeep` is tracked, and a `cargo build`
without a prior `make ui` embeds the placeholder page. Do not commit built
assets.

For development, run `otto serve --listen 127.0.0.1:8787` in one terminal and
`cd ui && OTTO_URL=http://127.0.0.1:8787 npm run dev` in another, then open the
Vite URL with the `?token=` query from the `otto serve` startup line. Vite
proxies `/v1` to the server, so the page stays same-origin and no CORS is
involved. Wire types in `ui/src/types.ts` mirror
[openapi.yaml](../testdata/server/openapi.yaml); update both together.

## Test-driven development

TDD is required for feature work and bug fixes:

1. Write or update the failing test first.
2. Run the smallest relevant `cargo test ...` command and watch it fail for the expected reason.
3. Make the minimal code change.
4. Re-run the focused test.
5. Re-run the broader relevant crate or repository gates.

Do not add production behavior without a failing test first unless the user
explicitly approves an exception for docs-only work or another non-code change.
Keep unit tests next to the module they cover (`#[cfg(test)] mod tests`), and
integration tests in the crate's `tests/` directory. Prefer
`#[test]`/`#[tokio::test]` and `tempfile::TempDir`. TTY-specific coverage must
stay offline and automated, such as the PTY smoke test in
`crates/otto/tests/tui_pty.rs`. Live provider tests are opt-in only and
excluded from the default suite. Contract changes need focused coverage for
ownership, invalid states, cancellation, and history/wire compatibility;
verify both `Session` implementations (`MemorySession` and `Store`) where they
share a contract. Report failing gates and reruns accurately, check the
baseline for unchanged failures, and never weaken validation or safety checks
to obtain a pass. Preserve unrelated behavioral assertions when migrating test
fixtures.

## Secrets, safety, and documentation

- Never add `--api-key`; API keys come only from environment variables, and ChatGPT credentials only from `otto login`.
- Never put raw API keys, OAuth tokens, or auth headers in TOML, JSONL session fixtures, logs, docs, or tests. Redact sample values in errors and examples.
- `read`, `grep`, `find`, `ls`, `write`, `edit`, and `skill` must reject workspace/skill-directory escapes after canonical path and symlink validation.
- Do not describe `bash` as always unsandboxed. Otto defaults to macOS Seatbelt; only explicit sandbox `off` is unsandboxed.
- Never put secrets in skill files; skill content is user- or repository-provided instruction text of the same class as `AGENTS.md` and `CLAUDE.md`.

Keep README limited to implemented, tested behavior; list unsupported behavior
under Limitations. Do not list roadmap stages or planned providers. Keep command
examples aligned with the actual CLI flags in `crates/otto/src/cli/flags.rs`.
Document the config, session, and safety behavior that tests enforce today,
not aspirational behavior.

Use small, focused commits with imperative subjects, for example
`feat: add OpenAI-compatible streaming` or `docs: document the ChatGPT sign-in
flow`. Before committing, run the relevant Rust gates and confirm that the
working tree contains only intentional changes.
