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
  - `config`: TOML loading and runtime resolution, including `[inbound.feishu]`, and `config::edit`, the in-place text edits every config write uses: each changes one key or table, keeps every other byte, and returns an error instead of a result that parses to anything besides the intended change
  - `wire`: shared wire DTOs
  - `safetext`: redaction and secret-form detection shared by native and wasm code
- `crates/otto` (native binary):
  - `cli`: composition root, flag parsing, process lifecycle, signal handling, the REPL, and concrete dependency injection
  - `app`: shared lifecycle, turn admission, session replacement, task/authentication capabilities, profile selection, and session info/history access
  - `session`: native JSONL session storage built on `otto_core::session`
  - `config`: the native config path, loading, and the one writer every config change goes through: a compare-and-swap against the bytes the caller read, an atomic temp-file rename, and a timestamped copy of the replaced contents under `backups/`
  - `tool`: native execution of workspace-confined `read`/`grep`/`find`/`ls`/`write`/`edit`/`bash`/`skill`, in-process `remind`, `list_models` (the OpenAI-compatible endpoint's `GET /models`), and the MCP tool adapter (`tool::mcp`) that presents one external server's tools to the model
  - `mcp`: the JSON-RPC codec, the stdio and HTTP transports, OAuth 2.1 sign-in for HTTP servers, and the per-server client that negotiates the protocol era and serves `tools/list`/`tools/call`
  - `sandbox`: sandbox driver contracts, the Seatbelt and direct drivers, environment filtering, and conformance helpers
  - `provider`: native HTTP transports for the two provider implementations
  - `auth`: ChatGPT OAuth sign-in (`otto login`/`otto logout`), credential storage at `~/.otto/auth/chatgpt.json`, and access-token refresh
  - `memory`: neutral memory contracts, validation/secret guards, conservative policy, the `Service` implementation, a null fallback, and the SQLite/FTS5 store and retriever
  - `reflection`: the `/reflect` use case: it reads the part of a session no earlier run covered (from the session file, so compacted entries stay reachable), classifies entries as local or external (`taint`), builds one tool-less request (`prompt`), parses and validates the answer (`output`), verifies quoted evidence in code (`evidence`), and queues survivors as memory candidates through `Service::propose` with `Origin::Extractor`. `store` keeps run rows, per-session watermarks, and skill ownership in `~/.otto/reflection.db`. Skills pass `guard` (structure and the rule table), `review` (a fail-closed second-model call), and only then `skillwrite`, the only file that mutates skill files; it accepts only a `guard::Vetted`, which only `guard::approve` builds. `crates/otto/tests/reflection_boundary.rs` fails if this module ever writes, forgets, or reviews a memory record directly, if a file other than `skillwrite.rs` mutates the filesystem, or if `Vetted` is built outside `guard.rs`. A new scan rule goes in the `RULES` table in `guard.rs` with a positive and a negative test case
  - `usage`: native collection of parent, sub-agent, and compaction token events; append-only SQLite storage; and total/daily aggregate queries consumed by the server
  - `skill`: SKILL.md frontmatter parsing, name/description validation, discovery across configured roots, and rendering of the system-prompt listing
  - `subagent`: child agent construction (`Runner`), task lifecycle, the parent-facing `agent`/`agent_wait`/`agent_status`/`agent_send` tools, the child-only `agent_report` tool, shared task-formatting helpers used by both the REPL and the TUI, and AGENT.md definition discovery
  - `workflow`: workspace-scoped TOML DAG discovery, SQLite run/step/attempt/approval/event state, committed-boundary recovery, and the shared CLI/server workflow controller
  - `inbound`: host adapters that turn external event streams into session inbox notifications. Feishu inbound spawns `lark-cli event consume im.message.receive_v1 --as bot`, parses NDJSON, expands `merge_forward` via `lark-cli im +messages-mget --as bot`, and fans messages out through `Controller::notify`
  - `acp`: the Agent Client Protocol v1 agent server behind `otto acp`: newline-delimited JSON-RPC on stdin/stdout with a single stdout writer, per-session `Controller`s on one `Builder`, the mapping from agent events and stored history to `session/update`, and the after-turn `session/request_permission` round trip for elevated `bash`. Message types come from the `agent-client-protocol-schema` crate; the design is `docs/specs/2026-10-02-acp-agent-server.md`
  - `server`: HTTP/JSON/SSE frontend, wire DTOs, per-session turn buffering, metrics, the Unix-socket and loopback-TCP listeners, bearer-token gating of `/v1/`, and the embedded web UI (`ui/dist`, written by `make ui`)
  - `tui`: the terminal frontend on the alternate screen, transcript rendering, Markdown/tool presentation, key handling, and terminal lifecycle
- `crates/otto-web`: the wasm cdylib the browser UI loads; exports `otto-core`'s wire codecs to JavaScript through `wasm-bindgen`
- `ui/`: the TypeScript browser frontend; a client of `crates/otto`'s HTTP API and `crates/otto-web`'s wasm exports only, with no Rust code of its own and no part in `make check-fast`
- `connect/`: `otto-connect`, the Go chat connector (module `github.com/baiyuqing/otto/connect`). It is an ACP client that starts an ACP agent (by default `otto acp`) as a child process and links no otto code; its only contract with otto is ACP v1 on stdio. `internal/agent` owns the child process and the ACP connection, `internal/bridge` the chat-to-session map, queues, commands, replies, permission requests and admission, `internal/telegram` the Bot API adapter, `internal/config` `connect.toml`, and `internal/state` the state file. Design: [chat connector](specs/2026-10-02-otto-connect.md)

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
always land as pending candidates requiring review. `/reflect` proposes candidates
through `reflection` on demand; automatic extraction (`Binding.Observe`, or a
trigger on compaction or exit) and durability (backup/restore/verify) remain unwired. `/memory review` lists pending
candidates and `/memory review <id> accept|reject` decides one through
`Service::review` (human authority, like `/memory forget`). The TUI dispatches `/memory` and `/remember` to the same
functions the REPL uses (`crates/otto/src/cli/repl_commands.rs`).

Keep `crates/otto`'s `skill` module free of imports from other Otto modules
besides `otto-core`. The skill tool's file reads stay confined to the skill
directory. `/skills` and `/skill` are wired in both frontends and documented
in the user manual; `allowed-tools` enforcement is not, so do not document it
as a working feature.

Keep `crates/otto`'s `subagent` module behind the runner's construction path:
children are built only through it; the agent loop knows tasks only through
its own task registry and never imports `subagent` directly; frontends reach
tasks only through the shared task-lister facade; children never receive
`agent*`, `remember`, `forget`, `memory_search`, or `remind*`; a child's inbox is
not persisted. `agent_send` queues parent task updates in a child's private inbox
and child agents read them only at normal agent-loop notification checkpoints;
it does not interrupt an in-flight provider or tool call. `agent_report` is a
child-only tool that queues one message in the parent's task inbox without
finishing the task; a child uses it only to answer a parent question or to
report a blocker needing a parent decision, never for progress updates, plans,
or interim findings. `REPORT_BUDGET` (`crates/otto/src/subagent/tasks.rs`,
currently 10) caps it at that many calls on the child's own initiative per
task, plus one more per `agent_send` message the parent sends; a call past
the limit returns a tool error and delivers nothing to the parent.
Definitions cannot add tools outside the child tool set; `tools` only
narrows it. `[agents]` is TOML only, like `[skills]`. Do not
document `agent_cancel` as a working feature.

Keep durable workflows separate from ad-hoc sub-agent tasks. Workflow
definitions snapshot their referenced `AGENT.md` bodies, model choices, and
tool allowlists when a run is created; resume uses that snapshot and the
current sandbox. Every attempt has its own append-only Pi v3 transcript.
SQLite is the workflow state source of truth, and a per-workspace advisory
lock permits only one scheduler process. A process loss changes running steps
to `interrupted` and the run to `paused`; retry is always explicit because an
external tool effect may already have happened.

Deadline ownership is layered and monotonic. Native admission converts each
configured duration to one absolute `deadline::Deadline`; children derive the
earlier of their local policy and the parent's remaining budget. `otto-core`
receives only the wasm-safe `operation::OperationControl` capability and must
not read a native clock. Provider retries, compaction, task queueing, and
workflow attempts consume the same logical budget rather than restarting it.
The fixed race rule is completion-first only after the owner has obtained and
validated the result; otherwise the typed stop reason wins. Effectful futures
are never detached with session writers, leases, child processes, or mutable
workflow handles. Bash process groups and shared stdio MCP servers own their
TERM/grace/KILL/reap sequence; HTTP MCP abandons only the timed-out request.
Synchronous filesystem and SQLite boundaries are checked before and after the
call and do not claim a hard wall-clock bound.

Keep `crates/otto`'s `mcp` module behind `crate::mcp`'s client, transport, and
OAuth types; `crate::tool::mcp` (the model-facing tool adapter) and
`Builder::connect_mcp` in `crates/otto/src/cli/wiring.rs` are the only two
callers that construct a server connection. `connect_mcp` connects every
enabled server concurrently and then processes the outcomes in configuration
order, so warnings, `/mcp` rows, and cross-server tool-name deduplication stay
reproducible; a failed or sign-in-required server is reported as a warning and
never blocks the runner from starting, while a disabled server is recorded
silently. Connected servers contribute the compact `mcp_search_tools` /
`mcp_call_tool` router pair to the parent and child registries instead of one
provider-visible schema per remote MCP tool; `tools_for` and
`dedup_cross_server` still run at connect time so router search/call only sees
validated, stable, non-colliding `mcp__<server>__<tool>` names. An interactive
frontend does not wait for that: `cli::run` builds the
first runner with `build_runner_without_mcp_with_trace`, whose `/mcp` rows read
`ServerState::Connecting`, and a background task builds the full runner and
installs it through `Controller::replace_runner_if_current`, which commits only
between turns and only while the session it was built for is still current.
Keep that swap the only hot path into a built runner; headless `--prompt` runs
and `server` still connect before the first turn.
`ServerState::NeedsLogin` marks an HTTP server using OAuth
whose token is missing or cannot be refreshed. `/mcp` (REPL and TUI) and
`otto mcp login|logout` in `crates/otto/src/cli/mcp.rs` are the only sign-in
surfaces; a completed sign-in still requires restarting Otto to pick up the
new token. A stdio server that exits stays disconnected until Otto
restarts; there is no restart policy. A tool name that collides with one
already advertised by an earlier server, or a server whose `env`/`headers`
secrets exceed the redaction limits in `otto_core::safetext`, is skipped
with a warning instead of being made available through the router. Keep MCP transport tests offline: only a
nonexistent stdio command (reaching `ServerState::Failed`) and a disabled
server (reaching `ServerState::Disabled`) exercise the real connect path in
tests; cover the other states through pure functions such as
`format_mcp_report` with hand-built `ServerStatus` fixtures instead of a real
server or OAuth round-trip.

## Core contracts

- Keep dependencies explicit and directed toward shared contracts. Reuse existing helpers and concrete types; add traits at real consumer boundaries, not for hypothetical implementations.
- Use `Message::validate`, `Block::validate`, `ContextMetadata::validate`, and `Usage::validate` in `otto_core::model`. Every type there is an owned value that derives `Clone`; there are no separate deep-copy helpers. Both `otto_core::session::Session` implementations (`MemorySession` in `otto-core` and `Store` in `crates/otto/src/session/store.rs`) enforce neutral validation and tool-call/result sequencing in `append`. Neutral validation permits transient messages without IDs/timestamps; Pi-specific encoding restrictions stay in `otto_core::session`. When adding fields, update ownership tests.
- `otto_core::provider::Response::message` is the single source of finish reason and usage. `Message::usage == None` means unavailable; `Some(Usage::default())` means explicitly reported zero. Preserve explicit presence (`usage_present` on `CompactionResult` and on the task records) through events, task progress, notifications, aggregates, and supported persistence metadata instead of inferring absence from zero counters. Keep legacy Pi normalization in the decoder.
- Keep provider-token persistence in `crates/otto::usage`: collection maps neutral agent events to content-free records, SQLite only appends and aggregates those records, and frontends query through the server API. Never store prompts, response text, tool arguments, or tool output in the usage database.
- Keep context associations in the typed `ContextMetadata`. Prefer the structured `task_id` over notification wording; text parsing is only a legacy-history fallback. Preserve append-only Pi v3 compatibility and namespaced optional details, including the explicit-zero usage marker. Do not rewrite old records or invent missing historical metadata.
- `Session::append_custom` writes a Pi v3 `custom` entry; `pi_entry_to_context_messages` skips `custom` entries, so they never reach a model context. The trait method has no default body: every implementor, including wrappers such as `SharedSession`, states whether it writes or drops the entry. The sub-agent runner writes `otto.task_spec` after the task slot is set and `otto.task_result` after the completion notification is pushed, and drops a write failure. When a session is reopened with unanswered tool calls, the first unanswered call gets `MAY_HAVE_RUN_TOOL_RESULT_TEXT` and the later ones `NOT_EXECUTED_TOOL_RESULT_TEXT` (`otto_core::session::context::missing_tool_results`); tool calls run one at a time, so the answered calls are a prefix.
- `otto_core::tool::ToolResult::persisted_content == None` selects `content`; `Some` selects its value, including `Some("")`; `persisted_text` applies that rule. Preserve redaction and the current-turn full-result overlay. Reuse tool definitions and assembly helpers, including `tool::bash::bash_definition` in `crates/otto`; keep conservative preflight and the `Registry::new` validation.
- Tool arguments decode model output rather than a strict client. An optional list argument deserializes with `tool::empty_as_none`, so an empty list reads as an absent key and a model that sends `[]` beside the arguments it is using is still served; `crates/otto/tests/tool_argument_contract.rs` scans the tool sources for one that does not.
- `Provider` and `Tool`/`ToolExecutor` instances are shared concurrently, so every trait method takes `&self`. Requests and arguments are borrowed for the duration of the call; returned responses, results, and definitions are owned by the caller. Per-call `StreamSink` callbacks are ordered and finish before `Provider::complete` returns; `Event` payloads are owned values, so consumers keep what they need without further copying.
- `serde_json` maps keep insertion order in native workspace builds: `agent-client-protocol-schema` (a `crates/otto` dependency) enables `serde_json/preserve_order`, and Cargo feature unification applies it to every workspace crate. `otto-core` built alone for wasm32 keeps sorted keys. Code and tests must not depend on either order; compare parsed `serde_json::Value`s instead of serialized strings.
- Reuse `otto_core::agent::CompactionResult` as the shared compaction payload. Keep HTTP/SSE DTOs in `crates/otto/src/server` separate from internal structs; update `testdata/server/openapi.yaml` alongside wire changes and preserve existing field meanings.

## Lifecycle and frontend contracts

- Construct `app::Controller` through `Controller::new` (one `Builder`, used by `cli::run`) or `Controller::with_builder` (a shared `Arc<Builder>`, used by `server`); the `Builder` in `crates/otto/src/cli/runtime_builder.rs` is the only session/runtime construction path (`create`, `open`, `resolve_profile`). Do not restore placeholder factories or duplicate runtime construction paths.
- `Controller::request_close` is nonblocking. External lifecycle owners cancel active work as appropriate and call the synchronous `Controller::close` to complete cleanup. An idle close request alone does not release resources. Preserve exactly-once session/runner cleanup, cleanup errors, and post-close `info`/`history` snapshots; reentrancy is tracked by the admission generation (`begin_operation`/`Admission`), not by thread or stack inspection.
- Treat `subagent::tasks::Task` as a query snapshot. Update existing task progress and completion through `Tasks::mark_running`, `record_provider_step`, `record_tool_call`, and `finish`; preserve task identity, terminal states, and notification-before-`wait` ordering. A cancellation request is not proof that execution has stopped. The agent loop sees the registry only as `otto_core::agent::TaskRegistry`.
- Frontends use `Controller::tasks` (`app::TaskView` over the wire-shaped `app::Task`), never the mutable `Tasks` registry or the raw `Inbox`. Host inbound adapters call `Controller::notify` instead of touching `Inbox` directly. Call `Controller::prepare_wake` before publishing a wake turn, then `WakeOperation::run` it once or drop it. Dropping an unstarted claim releases it on cancellation/shutdown. `Tasks::updates` is a coalescing `tokio::sync::watch` signal, not a broadcast subscription; add no competing scheduler. Normal and wake turns share cancellation, event delivery, and compaction accounting; one-shot runs propagate wake failures.
- `otto_core::agent::inbox::Inbox` numbers every item (`Entry::seq`) and runs its persist hook under the inbox lock after every change. `cli::wiring` installs the hook only for a file-backed top-level session; it mirrors the queue to `<id>.inbox.json` through `tool::remind::replace_private` (temporary file, `fsync`, rename, directory `fsync`) and skips the write once `<id>.jsonl` is gone, so an archive is not undone by a late push. `deliver_notifications` removes an item by sequence number only after the session append succeeded, so delivery is at least once; `user_message` items become `Role::User`, while other kinds remain display context. `Inbox::load` runs before any producer holds the inbox and signals the change callback when it restored items; that signal is how a reopened session gets its wake turn. `remind`'s `Inner::fire` pushes the notification and removes the timer under one hold of the `armed` lock.
- Session failover lives in `crates/otto/src/failover`; the approved design is `docs/specs/2026-09-28-session-failover.md`. A session is lease-managed if and only if `<id>.lease/` exists (`failover::lease::is_lease_managed`); `[failover] enabled` only decides whether an open or a new session creates that directory. Every open that takes the session file lock goes through `session::Prepared`, which acquires the lease before it opens the file descriptor, because a takeover replaces `<id>.jsonl`. The `Store` owns the lease: it checks `Lease::check` before every record write and releases the lease on `close` after a successful `fsync`, or on drop. Child transcripts check the parent's lease and never own one. Tool registries carry `failover::CommitGuard` as their `tool::registry::CallGuard`: `before` refuses a call once the lease is lost, and `after` runs `syncfs` on Linux. Dependency direction: `session` → `failover::lease` → `session::fsops` (a leaf), and `failover::recovery` → `subagent::interrupted` → `session`. `Keeper::global` calls `libc::_exit(75)` when a lease goes stale or is superseded, so tests use `Keeper::new_for_test`, or `Lease::acquire` only on leases that stay renewed and are released before the test ends; a stopped holder is a hand-written `epoch-<n>` and `heartbeat`. `SIGTERM` migration: `main.rs` owns the signal and asks `cli::terminate::Terminate::on_signal` what to do, with `failover::lease::holds_lease()` as input; a migration cancels the process token (the `SIGINT` path), and after the frontend returns `cli::run` (or `server::Server::migrate` for `otto serve`) calls `Controller::migrate` before the normal close. `Controller::migrate` holds an admission while it cancels and waits for every sub-agent task (`Tasks::begin_migration`, `Tasks::wait_final`), queues the moved notification (`failover::recovery::notify_moved`), and closes MCP; dropping the admission then closes the session, which releases the lease. In migration mode the sub-agent runner writes `otto.task_result` `interrupted` through the child's own transcript and pushes no completion notification, because a child transcript stays open and locked while its task entry exists, so `subagent::interrupted::mark_interrupted` (which opens a second `Store`) is only for transcripts of another process. Tests never send a signal to the test process; binary tests signal a spawned `otto`.
- Use `auth::Service` (`login`/`logout`/`status`) and `Controller::switch_profile`/`set_default_profile` for shared use cases. Credential paths and concrete services belong in the composition root; OAuth and credential files stay in `crates/otto`'s `auth` module. Frontends own presentation, not credential persistence. Preserve the startup credential snapshot (`cli::login::capture_auth_credentials`) and restart requirement, and refresh backend state after a profile switch even when saving the default fails.

## Development isolation and ownership

Choose the cheapest adequate model for routine work and escalate only when the
task requires it. Delegated work must have bounded file ownership; hand off
overlapping files before another worker edits them and preserve other workers'
changes.

Use a dedicated Git worktree and development branch for every feature or bug
fix. Never implement directly on `main`. If implementation changes are
accidentally made on `main`, restore them before continuing in the worktree.

Put each worktree at `.worktree/<name>` under the primary checkout, not as a
sibling directory of the repository:

```bash
git worktree add .worktree/<name> -b feat/<name>
```

`.worktree/` is gitignored so checkouts are not committed. Cursor also
imports `.gitignore` into the agent workspace; `.cursorignore` un-ignores
`.worktree/` so file tools do not treat a worktree as outside the project.
After the pull request merges, remove that worktree and its local branch
(`git worktree remove .worktree/<name>`, then `git branch -d`).

When work requires a design or spec, finish the discussion and get explicit
approval before writing production code or tests. Once approved, carry the
agreed work through implementation and verification without repeated
confirmation; seek clarification only for material scope or contract changes.

## Rust workflow

Full host validation uses macOS 26+ with standalone Command Line Tools selected.
Linux builds and runs Otto without a sandbox, so its gate is `make check-linux`,
which CI runs on `ubuntu-24.04`: the same format, lint, test, wasm-check and
PTY targets, minus the Seatbelt conformance suite, which is compiled only for
macOS, and minus the release build, the Node wasm tests, and the web UI, which
are platform independent and stay on the macOS job. Keep platform-specific
code behind `cfg` rather than runtime checks, so a target that cannot use it
does not compile it.
A documentation-only change skips both gates. The workflow's `changes` job
classifies the diff first: when every changed path is Markdown outside
`crates/` and `testdata/`, under `docs/`, or `LICENSE`, both gates start and
skip every step, so they still report a status without installing a toolchain.
Anything else — including a workflow edit, `testdata/server/openapi.yaml`, or a
Markdown fixture the skill and agent loaders discover by name — runs the full
gate. An unusual range, such as a new branch or a force push that dropped the
old tip, also runs it.

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
make build          # build the Web UI, cargo build --release, then copy ./otto
make install        # run make build, then install ./otto to ~/.local/bin/otto
make rust-fmt       # cargo fmt --all -- --check
make rust-lint      # cargo clippy --workspace --all-targets -- -D warnings
make rust-test      # cargo test --workspace (offline)
make rust-wasm-check # cargo check -p otto-core and -p otto-web for wasm32-unknown-unknown
make rust-wasm-test  # wasm-pack test --node for otto-core and otto-web
make test-tui       # cargo test -p otto --test tui_pty (needs a real PTY)
make check-linux    # the Linux gate: rust-fmt, rust-lint, rust-test, connect-check, rust-wasm-check, test-tui
make connect-check  # Go connector: gofmt -l, go vet, go test -race (builds target/debug/otto for its end-to-end test)
make connect-build  # build the Go connector to target/otto-connect
```

`check-fast` runs `rustfmt`, `clippy`, and the focused `otto-core` test suite;
`check` adds the Web UI and release build, the full workspace test suite, the
Go connector checks, the wasm32 build check, the wasm tests under Node, the PTY
smoke test, and `make ui-test`.

`connect-check` needs the Go version named in `connect/go.mod`; CI installs it
with `actions/setup-go` from that file. Its end-to-end test drives the real
`otto acp` from `target/debug/otto` against a loopback fake provider, so it is
offline like the Rust tests.

`rust-wasm-test` and `make ui`/`make ui-test` need `wasm-pack` (pinned to
0.15.0 in CI via `cargo install wasm-pack --version 0.15.0 --locked`).
`test-tui` needs a real PTY, which is unavailable in some sandboxed shells;
run it directly with `cargo test -p otto --test tui_pty` on a host that has
one. Keep the default test suite (`cargo test --workspace` without extra
flags) offline: it must not need network access, provider credentials, or a
real interactive terminal.

The Node helpers under `scripts/` are opt-in, but their tests are not: `make
scripts-test` runs every `scripts/*.test.mjs`, and both gates include it. It
needs no build and no network.
`scripts/pi-session-interop.mjs` covers the optional Pi interoperability probe
described in the user manual. `scripts/skill-exec-measure.mjs` reports what the
[sub-agent execution design](specs/2026-09-22-skill-subagent-execution.md) says
must be measured before that feature is trusted: context peak per window, total
tokens, and how often the model honours the `exec="agent"` marking. It reads a
session transcript and `~/.otto/usage.db`, never writes, and needs a real
session to have happened first, so it stays out of the gates even though its
tests are in them. `scripts/trace-viewer.html` is retained from the
Go implementation: the Rust build has no `OTTO_TRACE` writer (the name is only
reserved in the resolution environment), so nothing in this repository produces
the JSONL files the viewer reads. Do not document provider tracing as a working
feature until a writer exists.

## Web UI workflow

`ui/` is a Vite + React + TypeScript project. Its runtime dependencies are
`react`/`react-dom`, `react-markdown` with `remark-gfm`, `remark-math` and
`rehype-katex` plus `katex` for math, `mermaid` for fenced `mermaid` blocks
and the usage chart, and the `otto-web` wasm package built from
`crates/otto-web`. It needs Node 24+ and wasm-pack, and it is exercised by
`make check` through the production build and `make ui-test`.

```bash
make ui       # wasm-pack build, then npm ci && npm run build → ui/dist, embedded by cargo build
make ui-test  # wasm-pack build, then npm ci && npm test (vitest): the SSE frame parser and the transcript reducer
```

`make build` runs `make ui` first. `ui/dist` is a build output: only `.gitkeep`
is tracked, and a direct `cargo build` without a prior `make ui` embeds the
placeholder page. Do not commit built assets.

For development, run `otto serve --listen 127.0.0.1:8787` in one terminal and
`cd ui && OTTO_URL=http://127.0.0.1:8787 npm run dev` in another, then open the
Vite URL with the `?token=` query from the `otto serve` startup line. Vite
proxies `/v1` to the server, so the page stays same-origin and no CORS is
involved. Wire types in `ui/src/types.ts` mirror
[openapi.yaml](../testdata/server/openapi.yaml); update both together.

## Tests

Feature work and bug fixes ship with tests that check the behavior the change
adds or fixes:

1. Each test covers behavior the change defines: an input, a state, or an
   error path, not the structure of the implementation.
2. A test must fail when the behavior it covers breaks. When that is not
   evident from the test, check it once by breaking the behavior locally.
3. Run the smallest relevant `cargo test ...` command, then the broader
   relevant crate or repository gates.

Tests may be written before, during, or after the code. Docs-only and other
non-code changes need no tests.
Keep unit tests next to the module they cover (`#[cfg(test)] mod tests`), and
integration tests in the crate's `tests/` directory. Prefer
`#[test]`/`#[tokio::test]` and `tempfile::TempDir`. TTY-specific coverage must
stay offline and automated, such as the PTY smoke test in
`crates/otto/tests/tui_pty.rs`. Live provider tests are opt-in only and
excluded from the default suite. Tests that spawn subprocesses while the
parent holds files, advisory locks, sockets, or other descriptors must ensure
those descriptors are close-on-exec unless the child intentionally needs them;
otherwise parallel tests can inherit locks and fail unrelated cases as “already
open” or “resource busy.” Add focused regression coverage for descriptor
inheritance when fixing this class of flake. Contract changes need focused
coverage for ownership, invalid states, cancellation, and history/wire compatibility;
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
- The chat connector reads its bot token from the environment variable that `connect.toml` names; a secret key in that file fails the config load. Its errors and logs never contain the token, and it does not log message text.
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
