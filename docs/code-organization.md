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
| `crates/otto/` | Native Otto executable, runtime, persistence, integrations, and terminal/server frontends | `src/main.rs`, then `src/cli/` |
| `chat-connector/` | Independent Go Telegram and Feishu connector that communicates with Otto through ACP | `chat-connector/cmd/otto-connect/` and `chat-connector/internal/` |
| `crates/otto/src/fsops.rs` | Shared pinned filesystem descriptors and atomic operations | Native session and workspace callers |
| `crates/otto/src/workspace.rs` | Persistent workspace identity, move history and state ownership | `initialize`, `read`, `fork` |
| `crates/otto/src/storage/` | Native SQL/file capability contracts, drivers, backend repositories and logical migration ports | `runtime/storage.rs` composition and route registry |
| `docs/` | User manual, contributor documentation, and historical design rationale | `user-manual.md`, `development.md`, and `specs/` |
| `testdata/` | Shared test fixtures and wire-contract inputs | The adjacent test that consumes a fixture |
| `scripts/` | Build and development helpers invoked by the Makefile; `release.mjs` implements `make bump` and `make release-tag`, `license-check.mjs` implements `make license-check`, `otto.sh` is the `make run` launcher that turns `otto.env` into command-line options, and `otto-tcp.sh` supervises the ACP TCP pair (`otto serve --acp-listen` plus `otto-connect`) | `Makefile` |

The root Cargo workspace contains `otto-core` and `otto` only.
`chat-connector/` has its own build definition and is not a root workspace crate.

## Rust layers

```text
CLI / TUI / HTTP server / ACP
              │
              ▼
       crates/otto: app and native services
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
- `view_tree`: the summary-tree context view: log messages from session
  history, tree nodes and build scheduling, due-ordered view selection, zoom
  lookup, compressor prompts and the cache cost model. Its only native caller
  today is `otto eval view-tree` (`crates/otto/src/cli/eval_command/`).
- `agent`: provider/tool turn orchestration and emitted events.
- `mailbox`: WASM-safe FIFO notification contract, delivery acknowledgement semantics, and the default in-memory adapter. Native runtime code composes persistence or external implementations.
- `memory`: service-neutral capabilities, scoped requests, context projections,
  prepared changes and receipts; native `memory::client::Client` admits calls,
  and `memory::adapter::BuiltinAdapter` projects the existing domain service.
- `config`, `wire`, and `safetext`: shared configuration resolution, frontend
  DTOs, and secret-safe text handling.

`config/layered.rs` owns presence-preserving merge, field authority,
provenance and managed constraints. Native `config/layered.rs` discovers
workspace sources and content-bound executable trust; `cli/config_command.rs`
composes offline inspection/scoped editing, and `tool/project_guard.rs`
rechecks authorization at tool effects and requests cancellation on revocation.

Six module names exist in both crates. In each pair `otto-core` holds the
contract or format and `otto` holds the implementation:

| Module | `otto-core` | `otto` |
| --- | --- | --- |
| `session` | `Session` trait, `MemorySession`, Pi v3 codec, context, compaction and fork planning, trace building (`session::trace`) | `SessionHandle`; the JSONL `Store` in `storage/backends/posix/session/`, which `session/` re-exports |
| `config` | configuration schema, layered merge, provenance, managed constraints, resolution, `config::edit` | source discovery, executable trust, the compare-and-swap writer |
| `memory` | `MemoryService` open contract, scoped requests, context projections | built-in domain `Service`, validation, `client::Client`, `adapter::BuiltinAdapter`; file store in `memory/files/` |
| `approval` | approval requests, choices, modes and lifecycle values | durable approval authority `Service`, policy, repository port; SQLite store under `storage/` |
| `provider` | `Provider` trait; wire codecs in `openaicompat` and `openairesponses` | HTTP transports in `provider/openaicompat.rs` and `provider/chatgpt.rs` |
| `tool` | tool definitions, schema assembly, `ToolExecutor` trait | `Tool` trait, `Registry` (implements `ToolExecutor`), native tool execution, MCP tool adapter |

Keep provider transport and credential behavior out of this crate.

### `crates/otto`: native application

`src/main.rs` starts the executable. `src/cli/` parses commands, runs the
REPL and owns process lifecycle; it is the primary starting point for a new
command or startup behavior. `src/runtime/` assembles one runnable agent from
resolved configuration (`builder.rs` holds `Builder` and `Runner`,
`wiring.rs` connects memory, MCP and sub-agents, `prompt.rs` builds the system
prompt). `runtime/startup.rs` holds the startup composition, from environment
capture to the memory service, as phase functions; `cli::run`, `otto serve`
(`load_workspace`) and `embed` all call it. `runtime/workspace_admission.rs`
admits a workspace path for `otto serve`, delivery and `embed`. `src/app/`
exposes the shared application lifecycle used by frontends. `src/embed.rs` is
the public in-process API (`otto::embed::Otto`) that builds the `otto acp`
runtime for another Rust program; see the
[embedding API contract](development/architecture.md#embedding-api).

The frontends (`cli/`, `tui/`, `acp/`, `server/` and `client/`) build on every
other module, and no other module may import a frontend:
`crates/otto/tests/runtime_boundary.rs` fails when one does.
`crates/otto/tests/storage_boundary.rs` checks storage ownership, dependency
direction, and that no `otto` source re-exports an `otto-core` item. `crates/otto/tests/faults_boundary.rs` keeps `faults` out of
production code. See [package boundaries](development/architecture.md#package-boundaries).

The other modules group native responsibilities:

- **User entry points:** `cli/`, `tui/`, `server/`, and `acp/`.
- **Remote and external integrations:** `provider/`, `mcp/`, `plugin/`, `auth/`, and
  `client/` (the client for a running `otto serve`).
- **Local state and reliability:** `approval/` (shared authority and domain adapters), `session/`, `config/`, `memory/`,
  `typesafe/`, `storage/` (drivers, backend repositories, and migration ports),
  `usage/`, `failover/`, `workflow/`, and `schedule/` (typed store port, tools, worker).
- **Plugin packages:** `plugin/` owns snapshot installation, the user registry,
  package validation and Skills/MCP adaptation with content-bound trust.
  `plugin/hooks.rs` adapts authorized `dev.otto` hooks/tools through the native
  sandbox; `otto-core/src/agent/hooks.rs` is the neutral lifecycle port.
  `cli/plugin.rs` exposes management commands; shared runner construction loads
  packages for every native frontend.
- **Agent capabilities:** `tool/`, `sandbox/`, `skill/`, and `subagent/`.
  `subagent/tools.rs` owns the nonblocking `agent_yield` capability;
  `otto-core/src/agent/mod.rs` settles batches and yields safely, while
  `agent/progress.rs` owns the bounded per-turn edit-batching advisory.
- **Runtime assembly:** `runtime/`, and `metrics.rs` (the HTTP and turn
  metrics `server/` exposes and the schedule worker records).
- **Test-only fault injection:** `faults/` (compiled under `cfg(test)` or the
  `fault-injection` feature; `faults/http.rs` is the scripted loopback HTTP
  server; see [Fault injection](development/testing.md#fault-injection)).
- **Cross-cutting native helpers:** `deadline.rs`, `retry.rs`, `gourl.rs`,
  `urlprivacy.rs`, and `log.rs` (the process logger: one instance built at the
  composition root and passed to its consumers). `log/diagnostics.rs` owns the
  private rotating execution files; `otto-core/src/diagnostics.rs` supplies the
  content-free observation boundary used by provider and agent operations. Event names and fields are in
  [diagnostics guide](manual/servers-and-connectors.md#logs-metrics-and-request-correlation).

### Persistent state and storage ownership

Domain directories own typed repository ports and state semantics. Concrete
schema, physical row mappings, SQL and file operations live under
`crates/otto/src/storage/backends/`; the small capability contracts and
SQLite/POSIX drivers live beside them in `storage/`. `runtime/storage.rs`
validates routes and capabilities, opens concrete repositories, and injects
domain ports into consumers. Portable migration record definitions live beside
their domain owners in `migration.rs` (`skill/check_migration.rs` for skill
audits); approval envelopes also belong to their domains. Backend packages retain their physical mappings and I/O.

| State | Domain owner | Current adapter |
| --- | --- | --- |
| Approval authority and isolated smart review | `approval/{contracts,repository,service,reviewer}.rs`, `otto-core/src/approval/` | `storage/backends/sqlite/approval.rs` |
| Memory and review advice | `memory/{repository,service}.rs`, `memory/advice/audit_contracts.rs` | `memory/files/` (records, tombstones, candidates) and `storage/backends/sqlite/memory_advice.rs` (advice audit) |
| Session transcripts | `otto-core/src/session/`, native `session/repository.rs` | `storage/backends/posix/session/` |
| Schedules and workflows | `schedule/store.rs`, `workflow/{contracts,repository,service}.rs` | `storage/backends/sqlite/{schedule,workflow}.rs` |
| Tasks, usage, skill checks | Their domain repository modules | `storage/backends/sqlite/{tasks,usage,skill_checks}.rs` |

The default registry uses the local SQLite driver for SQL-backed
state and the confined POSIX file driver for sessions. The repository registry
has a public extension entry point in `runtime/storage/backend.rs` and
`cli::run::run_with_backends`. Linked extensions implement `RepositoryFactory`;
`config/storage.rs` in core supplies pure configuration. The shipped binary
includes only its existing local backends. `storage/worker.rs` owns optional
async adapter I/O, `storage/conformance.rs` exposes contract checks, and
`tests/external_storage_backend.rs` is an independently compiled acceptance
crate using a different JSON repository. See the [storage contract](development/architecture.md#native-storage-contracts)
for capability, migration, and compatibility boundaries.

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

### Browser and chat clients

- [otto-plus](https://github.com/baiyuqing/otto-plus) owns the browser UI and
  thin WASM bindings. It consumes `otto-core` at a pinned revision and calls
  Otto HTTP APIs; Otto does not depend on it.
- `chat-connector/` does not link Otto code. It starts `otto acp` over stdio or
  connects directly to `otto serve` over authenticated loopback ACP TCP.
  Both use ACP v1; see [ACP guide](manual/servers-and-connectors.md#acp-agent-server).

## Common change paths

| If you are changing… | Start with… | Then check… |
| --- | --- | --- |
| A CLI command, flag, REPL command, or process lifecycle behavior | `crates/otto/src/cli/` | `app/` and the relevant frontend tests |
| Shared agent turn behavior, messages, tool schemas, or session encoding | `crates/otto-core/src/agent/`, `model.rs`, `tool.rs`, or `session/` | Native callers and WASM compatibility |
| Provider HTTP behavior | `crates/otto/src/provider/` | The matching wire module in `otto-core` |
| Shared approvals and model-initiated consent | `approval/`, `otto-core/src/approval.rs`, `tool/approval.rs` | Domain adapters, TUI, HTTP, ACP and chat connector |
| Agent self-help and pending approval dialogue | `tool/otto.rs`, `app/approval_control.rs` | `runtime/builder.rs`, ACP, HTTP, chat and TUI input paths |
| Active-task guidance and model follow-ups | `otto-core/src/agent/steering.rs`, `app/task_control.rs`, `tool/task_control.rs` | runtime tool/prompt wiring, HTTP/ACP and connector routing |
| Per-turn chat channel context | `app/channel.rs`, then ACP prompt parsing | Core per-run system context, HTTP turn queue, `client/`, and `chat-connector/` |
| Web fetch and search tools, destination policy | `tool/web/` (`policy.rs` is the SSRF boundary) | `runtime/builder/runner_entry.rs` (`web_tools`) |
| Native tool execution or sandbox behavior | `crates/otto/src/tool/` or `sandbox/` | Workspace and sandbox contract tests |
| Bash command parsing for approval rules, review facts and effect classification | `crates/otto/src/tool/shell.rs` | `tool/bash.rs`, `approval/policy.rs`, `otto-core/src/approval/rules.rs` |
| Web API or browser behavior | `crates/otto/src/server/` or external otto-plus | `otto-core` for shared wire codecs |
| ACP integrations | `crates/otto/src/acp/` | `client/` for attach mode; `chat-connector/` for chat connector behavior |
| Wake-turn replies sent outside a prompt | `app/approval.rs` (`run_with_channel`), `acp/mod.rs` wake loop | the `wake-replies` HTTP route, `client/`, `chat-connector/internal/bridge` |
| Scheduled deliveries (`schedule` tools, due runs, delivery and write-back) | `crates/otto/src/schedule/`, `app/scheduled.rs` | turn origin in `app/approval.rs`, the worker spawn in `cli/run/process.rs` and `cli/serve.rs`, HTTP schedule routes, ACP `_otto/schedule/deliver`, `chat-connector/internal/bridge`, external otto-plus |
| Persistent memories, sessions, workflow runs, or token aggregates | The matching native state module | Its store and lifecycle tests |
| Cross-workspace delivery tasks (target admission, approval, worktree setup, handoff) | `crates/otto/src/delivery.rs`, then `cli/serve/delivery.rs` (`DeliveryManager`, `DeliveryBroker`, `DeliveryToolFactory`) | [user manual](manual/servers-and-connectors.md#cross-workspace-deliveries), `runtime/workspace_admission.rs`, `ParentToolSlot` in `runtime/builder.rs`, the receipt/task ledger tests |

## Dependency and documentation rules

- Keep dependencies pointed toward shared contracts: frontends and native
  integrations depend on `otto-core`; `otto-core` does not depend on native
  services or browser UI code.
- Keep HTTP/SSE DTOs in `crates/otto/src/server` separate from internal types;
  share only the wire types that genuinely cross frontend boundaries.
- Treat `docs/specs/` as design rationale unless a document says otherwise.
  Current behavior belongs in the README and user manual; implementation
  contracts belong in the development guide.
- When moving a module or changing ownership, update this map, `AGENTS.md`,
  and the relevant detailed contract together.

## Verification

For documentation-only changes, check links and review the rendered Markdown.
For code organization changes, the canonical repository gates are described
in the [development guide](development/workflow.md#local-verification).
