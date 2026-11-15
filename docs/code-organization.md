# Code organization

This page is a map for contributors: where a behavior lives, which layer owns
it, and which entry point to start from. It complements the detailed contracts
in the [development guide](development.md); that guide is the canonical source
for invariants and implementation constraints.

Root `AGENTS.md` carries cross-repository rules. The Rust crates and `docs/`
have scoped `AGENTS.md` files; read the ones on the path being edited.
Detailed inventories and contracts stay on this page and in the development
guide rather than being copied into those instruction files.

## Repository map

| Path | Owns | Start here |
| --- | --- | --- |
| `crates/otto-core/` | WebAssembly-safe shared domain logic and protocol codecs | `src/lib.rs` |
| `crates/otto/` | Native runtime library: services, persistence, integrations, tools, sandbox, `embed` and the terminal UI | `src/lib.rs`, `src/runtime/`, `src/app/` |
| `crates/otto-server/` | `otto serve` HTTP frontend: routes, listeners, TLS, workspaces, tasks, timers, workflows | `src/lib.rs` (`otto_server::Server`) |
| `crates/otto-acp/` | ACP stdio and attach frontends | `src/lib.rs` |
| `crates/otto-cli/` | The `otto` executable: command parsing, process lifecycle and composition root | `src/main.rs`, then `otto_cli::run` in `src/run.rs` |
| `chat-connector/` | Independent Go Telegram connector that communicates with Otto through ACP | `chat-connector/cmd/otto-connect/` and `chat-connector/internal/` |
| `crates/otto/src/fsops/` | Shared pinned filesystem descriptors and atomic operations (`posix.rs`), the `FileDriver` journal contract (`files.rs`) | Native session and workspace callers |
| `crates/otto/src/child_fence.rs` | Process-wide registry of child processes killed when a session lease is lost or a panic aborts | `Children::global`, `Children::register` |
| `crates/otto/src/workspace.rs` | Persistent workspace identity, move history and state ownership | `initialize`, `read`, `fork` |
| `crates/otto/src/storage/` | Native SQL/file capability contracts, drivers, backend repositories | `runtime/storage.rs` composition |
| `docs/` | User manual, contributor contracts, and retained design decisions | `user-manual.md`, `development.md`, and `specs/` |
| `testdata/` | Shared test fixtures and wire-contract inputs | The adjacent test that consumes a fixture |
| `scripts/` | Build and development helpers invoked by the Makefile; `release.mjs` implements `make bump` and `make release-tag`, `license-check.mjs` implements `make license-check`, `otto.sh` is the `make run` launcher that turns `otto.env` into command-line options, and `otto-serve-connect.sh` supervises `otto serve` and an `otto-connect` that reaches it through `otto acp --attach` | `Makefile` |

The root Cargo workspace contains `otto-core`, `otto`, `otto-server`,
`otto-acp` and `otto-cli`.
`chat-connector/` has its own build definition and is not a root workspace crate.

## Rust layers

```text
crates/otto-cli: `otto` binary, composition root
              │
              ▼
crates/otto-acp: ACP stdio and attach frontends
              │
              ▼
crates/otto-server: HTTP serve frontend
              │
              ▼
crates/otto: app, native services, TUI, embed
              │
              ▼
 crates/otto-core: I/O-free contracts, formats and agent loop
              ▲
              │
otto-plus (separate repository): browser UI and WASM bindings
              ▲
              │
        React/TypeScript browser client
```

`otto-core` holds I/O-free contracts (traits and types), formats (Pi v3
session, provider wire formats, wire DTOs, configuration schema and layered
merge) and the agent loop. `otto` holds the implementations: domain services,
filesystem, process, network and clock access, persistence, authentication,
the sandbox, startup composition and frontends. A domain service stays in
`otto` even where a part of it performs no I/O. `otto` imports core items as
`otto_core::...` and does not re-export them. The
[otto-core rules](../crates/otto-core/AGENTS.md) state the rule and the source
guards that check it; `otto-core` also remains buildable for
`wasm32-unknown-unknown`.

### `crates/otto-core`: I/O-free contracts, formats and agent loop

`crates/otto-core` contains no native runtime integration. Its principal
modules are:

- `approval`: frontend-neutral approval requests, choices and lifecycle values.
- `model`: provider-neutral messages, blocks, validation, and usage types.
- `provider`: the provider-neutral completion contract.
- `openaicompat` and `openairesponses`: provider wire formats and streaming
  codecs.
- `tool`: model-visible tool definitions and schema assembly.
- `session`: Pi v3 session codec, compaction, and context association types.
- `agent`: provider/tool turn orchestration and emitted events.
- `mailbox`: WASM-safe FIFO notification contract, delivery acknowledgement semantics, and the default in-memory adapter. Native runtime code composes persistence or external implementations.
- `memory`: service-neutral capabilities, scoped requests, context projections,
  prepared changes and receipts; native `memory::client::Client` admits calls,
  and `memory::adapter::BuiltinAdapter` projects the existing domain service.
- `config`, `wire`, and `safetext`: shared configuration resolution, frontend
  DTOs, and secret-safe text handling.

`config/layered.rs` owns presence-preserving merge, field authority,
provenance and managed constraints. Native `config/layered.rs` discovers
workspace sources and content-bound executable trust; `otto-cli/src/config_command.rs`
composes offline inspection/scoped editing, and `tool/project_guard.rs`
rechecks authorization at tool effects and requests cancellation on revocation.

Six module names exist in both crates. In each pair `otto-core` holds the
contract or format and `otto` holds the implementation:

| Module | `otto-core` | `otto` |
| --- | --- | --- |
| `session` | `Session` trait, `MemorySession`, Pi v3 codec, context, compaction and fork planning, trace building (`session::trace`) | `SessionHandle`; the JSONL `Store`, `PosixSessionRepository` and the writer lease in `crates/otto/src/session/` (`store.rs`, `posix_repository.rs`, `lease.rs`) |
| `config` | configuration schema, layered merge, provenance, managed constraints, resolution, `config::edit` | source discovery, executable trust, the compare-and-swap writer |
| `memory` | `MemoryService` open contract, scoped requests, context projections | built-in domain `Service`, validation, `client::Client`, `adapter::BuiltinAdapter`; file store in `memory/files/` |
| `approval` | approval requests, choices, modes and lifecycle values | durable approval authority `Service`, policy, repository port; SQLite store under `storage/` |
| `provider` | `Provider` trait; wire codecs in `openaicompat` and `openairesponses` | HTTP transports in `provider/openaicompat.rs` and `provider/chatgpt.rs` |
| `tool` | tool definitions, schema assembly, `ToolExecutor` trait | `Tool` trait, `Registry` (implements `ToolExecutor`), native tool execution, MCP tool adapter |

Keep provider transport and credential behavior out of this crate.

### `crates/otto`: native runtime library

`crates/otto` is a library. Its dependency chain is one-way:
`otto-core` <- `otto` <- `otto-server` <- `otto-acp` <- `otto-cli`.
`src/runtime/` assembles one runnable agent from
resolved configuration (`builder.rs` holds `Builder` and `Runner`,
`wiring.rs` connects memory, MCP and sub-agents, `prompt.rs` builds the system
prompt, `session_resources.rs` holds what a session owns across runner
replacement: sub-agent registry, inbox, timers, bash output logs, MCP servers
and workflow delivery worker). A `Runner` borrows these resources and never
stops them; `SharedSession::close` and `close_resources` do. `runtime/startup.rs` holds the startup composition, from environment
capture to the memory service, as phase functions; `otto_cli::run`, `otto serve`
(`load_workspace`) and `embed` all call it. `runtime/workspace_admission.rs`
admits a workspace path for `otto serve`, delivery and `embed`. `src/app/`
exposes the shared application lifecycle used by frontends. `src/embed.rs` is
the public in-process API (`otto::embed::Otto`) that builds the `otto acp`
runtime for another Rust program; see the
[embedding API contract](development/architecture.md#embedding-api).

`src/cli/` keeps only `login`, `repl` and `repl_commands`, because `tui/`
uses them; `cli/mod.rs` also re-exports `app` as `controller` and
`runtime::sandbox_switch`. The `tui/` and `client/` frontends stay in `otto`.
Items that `otto-server`, `otto-acp` and `otto-cli` call are `pub` in `otto`
(for example `runtime::startup`, `app::{management, task_control,
turn_queue, scheduled, notices}`, `fsops`, `hex`); widening visibility is the
mechanism, and new frontend code goes into the frontend crates, not into `otto`.

`otto` does not depend on the frontend crates. Within `otto`, every top-level
module belongs to a layer (platform, persistence, capability, orchestration,
assembly, application, frontend) and imports only its own or a lower layer;
see [layers inside `otto`](development/architecture.md#layers-inside-otto).
`crates/otto/tests/layer_boundary.rs` fails on an import that breaks the
layer order. `crates/otto/tests/storage_boundary.rs` checks storage
ownership, dependency direction, and that no `otto` source re-exports an
`otto-core` item. `crates/otto/tests/faults_boundary.rs` keeps `faults` out
of production code. See [package boundaries](development/architecture.md#package-boundaries).

Integration tests that need no frontend stay in `crates/otto/tests/`:
`definition_parity`, `fault_*`, `faults_boundary`, `herdr_report`,
`layer_boundary`, `mcp_stdio`, `memory_plugin_source`,
`memory_review_boundary`, `memory_service_conformance`, `session_conformance`,
`smart_approval`, `sqlite_storage`, `storage_boundary`,
`tool_argument_contract`, `tui_boundary`, and `testdata/`.

### `crates/otto-server`: HTTP serve frontend

`src/lib.rs` defines `otto_server::Server` and its `Options`; the modules
beside it hold routes (`session_routes.rs`, `turn_routes.rs`,
`config_routes.rs`), `listen.rs`, `tls.rs`, `workspaces.rs`, `tasks.rs`,
`timers.rs`, `workflows.rs`, `schedule.rs` and middleware. It depends on `otto`
and `otto-core`. HTTP/SSE DTOs stay here, separate from internal types.
Its tests are in the crate (`src/tests/`).

### `crates/otto-acp`: ACP frontends

`src/lib.rs` implements ACP over stdio and TCP (`serve`); `attach.rs` is the
attach client mode, `dispatcher.rs` and `command.rs` route requests, and
`schedule/` handles `_otto/schedule/deliver`. It depends on `otto`,
`otto-server` and `otto-core`. Its tests are in the crate (`channel_tests.rs`).

### `crates/otto-cli`: the `otto` executable

`src/main.rs` starts the process and calls `otto_cli::run` (`src/run.rs`),
the composition root. Command modules: `acp`, `config_command`,
`eval_command/`, `flags/`, `mcp/`, `memory_command/`, `memory_eval_command/`,
`panic_cleanup`, `plugin`, `run/`, `sandbox_setup`, `serve/`, `setup`, `storage`,
`terminate`, `trust`, `workflow`, `workspace_command`.
The crate depends on all four other crates and owns the `[[bin]] name = "otto"`
target.

Its integration tests in `crates/otto-cli/tests/` drive the built binary or
`otto_cli::run`: `acp_attach`, `acp_stdio`, `binary_e2e`,
`binary_e2e_unconfined`, `serve_listen`, `sigkill`, `sigterm`,
`tui_attach_pty`, `tui_pty`, `version`, `embed_parity`, `plugin_cli`, with
support code in `binary_harness/`. Run one with
`cargo test -p otto-cli --test <name>`.

The other modules group native responsibilities:

- **User entry points in `otto`:** `tui/`, `cli/{login,repl,repl_commands}` and `embed`; the binary, HTTP server and ACP frontends are the other three crates.
- **Remote and external integrations:** `provider/`, `mcp/`, `plugin/`, `auth/`, and
  `client/` (the client for a running `otto serve`).
- **Local state and reliability:** `approval/` (shared authority and domain adapters), `session/`, `config/`, `memory/`,
  `typesafe/`, `storage/` (drivers and backend repositories),
  `usage/`, `failover/`, `workflow/`, and `schedule/` (typed store port, tools, worker).
- **Plugin packages:** `plugin/` owns snapshot installation, the user registry,
  package validation and Skills/MCP adaptation with content-bound trust.
  `plugin/hooks.rs` adapts authorized `dev.otto` hooks/tools through the native
  sandbox or built-in pure-computation Lua handlers (`hooks/lua_engine.rs`,
  `hooks/lua_handler.rs`; [security contract](development/lua-plugin-security.md));
  `plugin/claude_hooks/` reads the Claude Code hook format
  (manifest, matcher, payload, response and tool-name translation) and
  `plugin/hooks/claude_binding.rs` runs those hooks through the shared
  sandboxed process runner; `otto-core/src/agent/hooks.rs` is the neutral
  lifecycle port.
  `otto-cli/src/plugin.rs` exposes management commands; shared runner construction loads
  packages for every native frontend.
- **Agent capabilities:** `tool/`, `sandbox/`, `skill/`, and `subagent/`.
  `sandbox/read_denylist.rs` owns the built-in read denylist constants and
  `ReadDenylist`, which decides whether a path is covered; the Seatbelt profile
  module renders the read denylist and the `read_paths` exceptions
  (`sandbox/seatbelt/profile/deny_roots.rs` builds the deny set and filters the
  exceptions) instead of discovering read roots.
  `runtime/sandbox_runtime/child_environment.rs` resolves the host and child
  environment of a sandboxed command (real `HOME`, `ZDOTDIR`, `CARGO_HOME`).
  `subagent/tools.rs` owns the nonblocking `agent_yield` capability;
  `otto-core/src/agent/mod.rs` settles batches and yields safely, while
  `agent/progress.rs` owns the bounded per-turn edit-batching advisory.
- **Runtime assembly:** `runtime/`, and `metrics.rs` (the HTTP and turn
  metrics `otto-server` exposes and the schedule worker records).
- **Test-only fault injection:** `faults/` (compiled under `cfg(test)` or the
  `fault-injection` feature; `faults/http.rs` is the scripted loopback HTTP
  server; see [Fault injection](development/testing.md#fault-injection)).
- **Test support:** `fixtures.rs` (compiled under `cfg(test)` or the
  `test-support` feature, which implies `fault-injection`): the offline
  two-provider configuration, controllers on temporary directories and
  `fixtures/scripted_provider.rs`, the scripted OpenAI-compatible origin
  server on `faults/http.rs` that the binary, `embed` and controller tests
  talk to. Dependent crates enable `test-support` through a dev-dependency.
  It also holds `embed_harness`, `plugin_package` and `subagent_doubles`, and
  exposes test-only constructors the dependent crates use (`Runner::scripted`,
  `Controller::push_notice`, `tool::testutil`, `memory::advice::testing`,
  `memory::files::testsupport`, in-memory SQLite store constructors,
  `workflow::Catalog::from_definitions`). Such items are public and gated
  `#[cfg(any(test, feature = "test-support"))]`, not `#[cfg(test)]`.
- **Cross-cutting native helpers:** `deadline.rs`, `retry.rs`, `gourl.rs`,
  `urlprivacy.rs`, `tls_provider.rs` (installs `ring` as the process-wide
  rustls crypto provider; every reqwest client is built through its
  `http_client_builder()` or `http_client()`, and `otto-server/src/tls.rs` takes its
  provider from `install()`), and `log.rs` (the process logger: one instance
  built at the composition root and passed to its consumers). `log/diagnostics.rs` owns the
  private rotating execution files; `otto-core/src/diagnostics.rs` supplies the
  content-free observation boundary used by provider and agent operations. Event names and fields are in
  [diagnostics guide](manual/servers-and-connectors.md#logs-metrics-and-request-correlation).

### Persistent state and storage ownership

Domain directories own typed repository ports and state semantics. Concrete
schema, physical row mappings, SQL and file operations live under
`crates/otto/src/storage/backends/`, except the session store and the usage
store, which live in `session/` and `usage/`; the small capability contracts
and the SQLite driver live beside them in `storage/`. The POSIX file driver
and the `FileDriver` contract are in `fsops/`. `runtime/storage.rs`
opens concrete repositories at their local paths and injects domain ports into
consumers. Backend packages retain their physical mappings and I/O.

| State | Domain owner | Current adapter |
| --- | --- | --- |
| Approval authority and isolated smart review | `approval/{contracts,repository,service,reviewer}.rs`, `otto-core/src/approval/` | `storage/backends/sqlite/approval.rs` |
| Memory and review advice | `memory/{repository,service}.rs`, `memory/advice/audit_contracts.rs` | `memory/files/` (records, tombstones, candidates) and `storage/backends/sqlite/memory_advice.rs` (advice audit) |
| Session transcripts | `otto-core/src/session/`, native `session/repository.rs` | `session/{store,prepared,list,fork,lease}.rs` |
| Schedules and workflows | `schedule/store.rs`, `workflow/{contracts,repository,service}.rs` | `storage/backends/sqlite/{schedule,workflow}.rs` |
| Tasks, skill checks | Their domain repository modules | `storage/backends/sqlite/{tasks,skill_checks}.rs` |
| Usage | `usage/{mod,repository}.rs` | `usage/store.rs` |

`runtime::storage::Storage` uses the local SQLite driver for SQL-backed
state and the confined POSIX file driver for sessions. `storage/worker.rs` owns
optional async adapter I/O. See the
[storage contract](development/architecture.md#native-storage-contracts) for
capability and compatibility boundaries.

`subagent/reports.rs` owns persistent workspace research reports and automatic
final-answer overflow capture; `subagent/scratch.rs` owns temporary downloads
that are removed when the child exits. Both use pinned workspace handles.

Place native tool execution and workspace enforcement in `tool/`; `bash`
execution goes through `sandbox/`. Keep session persistence append-only.

The TUI separates terminal interaction from feature behavior. `tui/features/`
contains injected presenters and the command catalog; `tui/interaction.rs`
and its composer submodule hold shared terminal-independent controls;
`tui/backend.rs` owns asynchronous requests and scope tickets;
`tui/widgets.rs` and `tui/render.rs` draw views and application snapshots.
Add an interactive feature by implementing a presenter and registering it in
the catalog. Keep local and `--attach` input on the same composer, and return
scoped replies through the backend contract. `crates/otto/tests/tui_boundary.rs`
guards these dependency directions.

Local TUI status projections (including input recovery, command receipts and
task/skill lists) use the backend's blocking worker, with at most one status
request in flight and a 500 ms polling interval in idle and active turns.
Drawing and terminal input use the current UI snapshot without session-history
queries. Status replies retain session/generation scope; turn transitions and
command receipts invalidate older replies, and streamed usage stays authoritative
while a turn is active.

`tui/frame_gate.rs` owns redraw pacing shared by local idle, local active-turn
and `--attach` loops. Timer ticks and unchanged local status do not draw;
background request completions draw when they update their visible projection.
Consecutive streamed text/reasoning events are applied in order but share
terminal frames; input, approvals, interruption, errors and lifecycle boundaries
draw immediately. The terminal reader batches ready wheel steps in one direction,
collapses ready resize redraws and keeps only the latest consecutive drag-extend
coordinate. Direction changes, keys, paste, and drag start/end retain exact order.

### Browser and chat clients

- [otto-plus](https://github.com/baiyuqing/otto-plus) owns the browser UI and
  thin WASM bindings. It consumes `otto-core` at a pinned revision and calls
  Otto HTTP APIs; Otto does not depend on it.
- `chat-connector/` does not link Otto code. It starts `otto acp` over stdio,
  either a local agent or `otto acp --attach` to reach `otto serve` through its
  Unix socket. Both use ACP v1; see [ACP guide](manual/servers-and-connectors.md#acp-agent-server).

## Common change paths

| If you are changing… | Start with… | Then check… |
| --- | --- | --- |
| A CLI command, flag, REPL command, or process lifecycle behavior | `crates/otto-cli/src/` | `app/` and the relevant frontend tests |
| Shared agent turn behavior, messages, tool schemas, or session encoding | `crates/otto-core/src/agent/`, `model.rs`, `tool.rs`, or `session/` | Native callers and WASM compatibility |
| Provider HTTP behavior | `crates/otto/src/provider/` | The matching wire module in `otto-core` |
| Shared approvals and model-initiated consent | `approval/`, `otto-core/src/approval.rs`, `tool/approval.rs` | Domain adapters, TUI, HTTP, ACP and chat connector |
| Agent self-help and pending approval dialogue | `tool/otto.rs`, `app/approval_control.rs` | `runtime/builder.rs`, ACP, HTTP, chat and TUI input paths |
| Active-task guidance and model follow-ups | `otto-core/src/agent/steering.rs`, `app/task_control.rs`, `tool/task_control.rs` | runtime tool/prompt wiring, HTTP/ACP and connector routing |
| Per-turn chat channel context | `app/channel.rs`, then ACP prompt parsing | Core per-run system context, HTTP turn queue, `client/`, and `chat-connector/` |
| Web fetch and search tools, destination policy | `tool/web/` (`policy.rs` is the SSRF boundary) | `runtime/builder/runner_entry.rs` (`web_tools`) |
| Native tool execution or sandbox behavior | `crates/otto/src/tool/` or `sandbox/` | Workspace and sandbox contract tests |
| Bash command parsing for approval rules, review facts and effect classification | `crates/otto/src/tool/shell.rs` | `tool/bash.rs`, `approval/policy.rs`, `otto-core/src/approval/rules.rs` |
| Web API or browser behavior | `crates/otto-server/src/` or external otto-plus | `otto-core` for shared wire codecs |
| ACP integrations | `crates/otto-acp/src/` | `client/` for attach mode; `chat-connector/` for chat connector behavior |
| Wake-turn replies sent outside a prompt | `app/approval.rs` (`run_with_channel`), `crates/otto-acp/src/lib.rs` wake loop | the `wake-replies` HTTP route, `client/`, `chat-connector/internal/bridge` |
| Scheduled deliveries (`schedule` tools, due runs, delivery and write-back) | `crates/otto/src/schedule/`, `app/scheduled.rs` | turn origin in `app/approval.rs`, the worker spawn in `otto-cli/src/run/process.rs` and `serve.rs`, HTTP schedule routes, ACP `_otto/schedule/deliver`, `chat-connector/internal/bridge`, external otto-plus |
| Persistent memories, sessions, workflow runs, or token aggregates | The matching native state module | Its store and lifecycle tests |
| Cross-workspace delivery tasks (target admission, approval, worktree setup, handoff) | `crates/otto/src/delivery.rs`, then `crates/otto-cli/src/serve/delivery.rs` (`DeliveryManager`, `DeliveryBroker`, `DeliveryToolFactory`) | [user manual](manual/servers-and-connectors.md#cross-workspace-deliveries), `runtime/workspace_admission.rs`, `ParentToolSlot` in `runtime/builder.rs`, the receipt/task ledger tests |

## Dependency and documentation rules

- Keep dependencies pointed toward shared contracts: frontends and native
  integrations depend on `otto-core`; `otto-core` does not depend on native
  services or browser UI code.
- Keep HTTP/SSE DTOs in `crates/otto-server/src` separate from internal types;
  share only the wire types that genuinely cross frontend boundaries.
- `docs/specs/` retains only still-useful contracts, rationale and decision gates;
  each note states its status. Current use and contributor workflows belong in
  the manual and development guide, not historical implementation plans.
- When moving a module or changing ownership, update this map, `AGENTS.md`,
  and the relevant detailed contract together.

## Verification

For documentation-only changes, check links and review the rendered Markdown.
For code organization changes, the canonical repository gates are described
in the [development guide](development/workflow.md#local-verification).
