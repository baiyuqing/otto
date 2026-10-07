# Development guide

[Code organization](code-organization.md) is the repository navigation map,
and the root `AGENTS.md` holds repository-wide rules. Scoped `AGENTS.md`
files in the Rust crates and `docs/` add only their owning layer's constraints. This page holds
the detailed package contracts and development workflow. The wasm32
architecture boundary is enforced by `make rust-wasm-check`, which
requires `crates/otto-core` to stay buildable for
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
  - `config`: raw TOML merging, field authority, provenance, managed constraints and runtime resolution, and `config::edit`, the in-place text edits every config write uses: each changes one key or table, keeps every other byte, and returns an error instead of a result that parses to anything besides the intended change
  - `wire`: shared wire DTOs
  - `safetext`: redaction and secret-form detection shared by native and wasm code
- `crates/otto` (native binary):
  - `cli`: composition root, flag parsing, process lifecycle, signal handling, the REPL, and concrete dependency injection
  - `app`: shared lifecycle, turn admission, session replacement, task/authentication capabilities, profile selection, and session info/history access
  - `session`: native JSONL session storage built on `otto_core::session`
  - `config`: native layer discovery, content-bound executable trust, config paths and loading, and the one writer every config change goes through: a compare-and-swap against the bytes the caller read, an atomic temp-file rename, and a timestamped copy of the replaced contents under `backups/`
  - `tool`: native execution of workspace-confined `read`/`grep`/`find`/`ls`/`write`/`edit`/`bash`/`skill`, in-process `remind`, `list_models` (the OpenAI-compatible endpoint's `GET /models`), and the MCP tool adapter (`tool::mcp`) that presents one external server's tools to the model
  - `mcp`: the JSON-RPC codec, the stdio and HTTP transports, OAuth 2.1 sign-in for HTTP servers, and the per-server client that negotiates the protocol era and serves `tools/list`/`tools/call`
  - `sandbox`: sandbox driver contracts, the Seatbelt and direct drivers, environment filtering, and conformance helpers
  - `provider`: native HTTP transports for the two provider implementations
  - `auth`: ChatGPT OAuth sign-in (`otto login`/`otto logout`), credential storage at `~/.otto/auth/chatgpt.json`, and access-token refresh
  - `memory`: domain contracts, validation/secret guards, conservative policy, repository ports and the `Service` implementation. Turso schemas, FTS search and advice audit SQL live under `storage/backends/turso/`. `memory::advice` owns advisory review contracts and orchestration (`Advisor` runs a typed audit port), the `session` prompt/parser and the `jev` backend. `Advisor::propose` calls `Service::check_proposal` first and queues the candidate without advice on timeout (20 s), error or invalid answer; the session-model backend (`runtime::advice`) is built in `runtime::wiring`, and `TYPESAFE_API_KEY` selects the Jev backend. Advice is stored as `advice.*` candidate metadata and rendered only by `memory::advice::advice_line`
  - `typesafe`: the HTTP client of the TypeSafe auxiliary model (`jev-latest`), shared by `skill::check` and `memory::advice::jev`
  - `reflection`: the `/reflect` use case, also started by the model's `reflect` tool (`tool::reflect` is the port; `app::requested_reflection` binds it to each installed runner and runs it beside the turn with trigger `requested`, sharing the background single-flight flag): it reads the part of a session no earlier run covered (from the session file, so compacted entries stay reachable), classifies entries as local or external (`taint`), builds one tool-less request (`prompt`), parses and validates the answer (`output`), verifies quoted evidence in code (`evidence`), writes a per-run Markdown log, opening with the model's retrospective and listing every proposal and its outcome, to `~/.otto/reflection/runs` (`runlog`, which also lists and reads those logs for `/reflect list|show` by matching directory entry names), and queues survivors as memory candidates through `Service::propose` with `Origin::Extractor`. `repository.rs` owns reflection records/ports; the Turso adapter owns `~/.otto/reflection.db` SQL. Skills pass `guard` (structure and the rule table), `review` (a fail-closed second-model call), and Python syntax validation in `skillwrite`, the only file that mutates skill files; it accepts only a `guard::Vetted`, versions the complete Markdown/Python file set, and publishes staged directories atomically through `fsops::publish_directory`; which only `guard::approve` builds. `crates/otto/tests/reflection_boundary.rs` fails if this module ever writes, forgets, or reviews a memory record directly, if a file other than `skillwrite.rs` mutates the filesystem (except `runlog.rs`, which creates each run log with `OpenOptions` and `create_new`), or if `Vetted` is built outside `guard.rs`. A new scan rule goes in the `RULES` table in `guard.rs` with a positive and a negative test case
  - `usage`: native collection of parent, sub-agent, and compaction token events; a typed repository port; and total/daily aggregate queries consumed by the server. Turso SQL lives in its backend adapter
  - `skill`: SKILL.md frontmatter parsing, name/description validation, discovery across configured roots, and rendering of the system-prompt listing
  - `subagent`: child agent construction (`Runner`), task lifecycle, the parent-facing `agent`/`agent_wait`/`agent_status`/`agent_send` tools, the child-only `agent_report` tool, shared task-formatting helpers used by both the REPL and the TUI, and AGENT.md definition discovery
  - `workflow`: workspace-scoped TOML DAG discovery, typed run/step/attempt/approval/event records and repository port, committed-boundary recovery, and the shared CLI/server controller. Turso persistence lives in its backend adapter
  - `acp`: the Agent Client Protocol v1 agent server behind `otto acp`: newline-delimited JSON-RPC on stdin/stdout with a single stdout writer, per-session `Controller`s on one `Builder`, the mapping from agent events and stored history to `session/update`, and the after-turn `session/request_permission` round trip for elevated `bash` and persistent sandbox read grants. `acp::tcp` is the opt-in `otto serve --acp-listen` transport; it uses the existing serve router in process and shares controllers, turn queues and approvals. Its authentication/lifecycle contract is [ACP TCP](specs/2026-10-03-acp-tcp.md). `acp::attach` is the `otto acp --attach` relay: the same transport and dispatcher with every session operation sent to `otto serve` through `client`. Message types come from the `agent-client-protocol-schema` crate; the designs are `docs/specs/2026-10-02-acp-agent-server.md` and `docs/specs/2026-10-02-shared-session.md`
  - `client`: the HTTP client of `otto serve` over its Unix socket, used by `acp::attach` and `tui::attach` (sessions, history, queued turns and their SSE streams, the `/v1/status` stream, turn cancel, approval decisions, compaction, context, sub-agent tasks, sandbox reload); it maps a failed connection to `Error::Unreachable` and a non-2xx answer to `Error::Http`
  - `server`: HTTP/JSON/SSE frontend, wire DTOs, per-session turn buffering, metrics, the Unix-socket and loopback-TCP listeners, bearer-token gating of `/v1/`
  - `tui`: the terminal frontend and alternate-screen lifecycle. `features/` owns injected presenters and the command catalog; add a feature by implementing a presenter and registering it there. Presenters project domain data to `View` and return transitions, while `interaction/` owns shared terminal-independent selection, focus, fields and composer state. `backend.rs` owns asynchronous requests and scope tickets; `widgets.rs` and `render.rs` draw views and `App` snapshots without reaching services. Local and `--attach` share composer and feature input; `attach.rs` sends requests through `client`, follows `/v1/status`, and reconnects, while both modes apply turn events as wire frames. `tests/tui_boundary.rs` protects these ownership lines.
- The browser UI and WASM bindings live in [otto-plus](https://github.com/baiyuqing/otto-plus), which consumes the shared core at a pinned revision.
- `chat-connector/`: `otto-connect`, the Go chat connector (module `github.com/baiyuqing/otto/chat-connector`). It is an ACP client that starts an ACP agent (by default `otto acp`) as a child process and links no otto code; its contract with otto is ACP v1 on stdio or authenticated loopback TCP (`[agent].address`). `internal/agent` owns the child process or TCP connection and the ACP handshake, `internal/bridge` the chat-to-session map, queues, commands, replies, permission requests and admission, `internal/telegram` the Bot API adapter (including Markdown rendering to entities), `internal/feishu` the Feishu/Lark adapter over `channel-sdk-go`, `internal/config` `connect.toml`, and `internal/state` the state file. Design: [chat connector](specs/2026-10-02-otto-connect.md)

Keep provider-specific wire structs inside the two provider implementation
modules. Keep file-tool workspace enforcement inside `crates/otto`'s `tool`
module, session persistence append-only, and `bash` delegated through
`crates/otto`'s `sandbox` module; only explicit sandbox `off` may use direct
execution, and it still starts in the selected workspace.

Chat channel metadata belongs to the native `app::Channel` contract. ACP
accepts `_meta.otto.channel`, and HTTP turns carry the same optional enum
through queue admission to execution. Render only fixed guidance for supported
channels; never accept client-authored system instructions. The core agent's
per-run system context applies to every ordinary provider request, including
tool rounds and overflow retries, without mutating the configured prompt or
session history. Approval retries retain the current channel; later turns
without metadata have no channel context.

Keep `crates/otto`'s `memory` module behind its neutral contracts. The agent
loop, tools, and frontends must never reach a store directly: use the
`Service`'s bound accessors or the `Controller` memory facade. Per-turn recall
and explicit management (`memory_search`/`remember`/`forget` tools,
`/memory`/`/remember` in the REPL, and `otto memory status|forget`) are wired
end to end via `[memory]` TOML config. Model-originated writes always land as
pending candidates requiring review; human `/remember` and `/memory forget`
apply immediately. Recall renders two groups (`otto-core`'s
`render_memory_context`): the standing set (active user-scope
`preference`/`instruction` records with a key, flagged by the retriever's
baseline query) under `STANDING_CONTEXT_PREAMBLE`, then the query matches under
the untrusted `MEMORY_CONTEXT_PREAMBLE`. The standing set has its own token
budget, `MAX_STANDING_TOKENS`; `Service::remember` and an accept in
`Service::review` reject a write that would exceed it, and validation rejects a
keyless user-scope `preference`/`instruction` create. On each session open
`Controller::notify_memory_state` queues one notice for pending candidates and
one for standing records left out over the budget.
`reflection` proposes candidates
on `/reflect` and, by default, in the background after a completed turn with
at least ten distinct successful tool operations or a compaction
(`app::auto_reflection`); `Binding.Observe` and durability (backup/restore/verify) remain unwired. `/memory review` lists pending
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
Turso is the workflow state source of truth, and a per-workspace advisory
lock permits only one scheduler process. A process loss changes running steps
to `interrupted` and the run to `paused`; retry is always explicit because an
external tool effect may already have happened.

Deadline ownership is layered and monotonic. Native admission converts each
configured duration to one absolute `deadline::Deadline`; children derive the
earlier of their local policy and the parent's remaining budget. `otto-core`
receives only the wasm-safe `operation::OperationControl` capability and must
not read a native clock. Provider retries, compaction, task queueing, and
workflow attempts consume the same logical budget rather than restarting it.
The ChatGPT transport retries only connection/send/body failures before any
streamed output, at most three times with 1/2/4-second backoff. It repeats only
the borrowed provider request, never tools or the agent turn. HTTP, auth, and
stream-codec errors are terminal; retries preserve cancellation, the original
deadline, actual send-attempt counts, and unknown effect certainty on failure.
The OpenAI-compatible transport retries only a send whose connection could not
be established (`reqwest::Error::is_connect`), with the same policy (4
attempts, 1/2/4 s, `Retry` reason `connection failed`): such a request was
never dispatched, which the resilience design permits, and the settlement is
`NotStarted`. Any failure after connecting, every HTTP status, and stream
errors stay single-attempt.
Retry progress uses the existing provider event; core owns countdown formatting
and native/browser frontends supply elapsed time.
The fixed race rule is completion-first only after the owner has obtained and
validated the result; otherwise the typed stop reason wins. Effectful futures
are never detached with session writers, leases, child processes, or mutable
workflow handles. Bash process groups and shared stdio MCP servers own their
TERM/grace/KILL/reap sequence; HTTP MCP abandons only the timed-out request.
Synchronous filesystem and Turso boundaries are checked before and after the
call and do not claim a hard wall-clock bound.

Keep `crates/otto`'s `mcp` module behind `crate::mcp`'s client, transport, and
OAuth types; `crate::tool::mcp` (the model-facing tool adapter) and
`Builder::connect_mcp` in `crates/otto/src/runtime/wiring.rs` are the only two
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

## Workspace ownership

`crates/otto::workspace` owns the local versioned identity marker. CLI startup
initializes it before session discovery, including workspaces opened through
`otto serve`. Session lookup and workspace memory binding read its adopted
keys. Missing markers in read-only lookups retain legacy path-derived behavior.
Configured memory IDs are adopted once; conflicts are errors.

Marker reads and writes reuse `crates/otto::fsops` pinned directory descriptors
and refuse symlinks. Session-specific key and Pi entry-ID helpers remain in
`session::fsops`, so workspace identity does not depend on the session store.
A directory lock serializes initialization, move-history updates and explicit
forks; atomic rename publishes a synced complete marker. Corrupt markers fail
closed. `paths` records historical session ownership, never execution authority;
actual tool and approval boundaries stay path-based. Session candidate checks
still validate the stable session directory, file identity and recorded cwd.
Do not rewrite append-only transcripts to update workspace paths. Identity UUIDs
are distinct from session IDs and lease epochs; keep process ownership in the
existing lease implementation.

Behavioral tests live in `workspace`, `runtime::wiring`, `session`, and the file-tool
workspace boundary. Forking changes workspace ownership, leaves old state
untouched, and must be performed with workspace processes stopped. No new
storage engine, per-workspace database, or cloud synchronization is involved.

## Native storage contracts

`crates/otto/src/storage/` keeps backend concerns below domain-owned typed
ports. `contracts.rs`, `sql.rs`, and `files.rs` describe capability guarantees;
`drivers/{turso,posix}.rs` implement local I/O; `backends/turso/` owns SQL
schemas and codecs, while `backends/posix/` owns confined file repositories.
`runtime/storage.rs` is the composition boundary: it validates routes and
driver capabilities before opening repositories and injects only domain ports.
The shipped defaults are local Turso-compatible SQL storage and confined POSIX
session files. No MySQL, PostgreSQL or cloud service implementation is shipped.
Statically linked extension binaries can register their own factories and
select validated domain routes through user/managed storage configuration.

Domain services own validation, state transitions and recovery policy.
Repositories expose typed domain operations and own atomicity, revision/claim
checks, codecs and schemas. The shared SQL driver checks dialect, immediate
serializable transactions, commit durability, multi-process writer behavior,
and the memory FTS profile before a repository receives a connection. Session
message, compaction, operation-fact, and custom-entry writes cross async
interfaces; the POSIX implementation retains Pi v3 encoding and append-only
history. `otto-core` remains independent of native storage engines.

### External storage backends

`runtime::storage::BackendRegistry` registers external `RepositoryFactory`
implementations before persisted routes are validated. Extension binaries call
`cli::run::run_with_backends`; the ordinary `run` and `Storage::open_home`
retain the existing defaults. Memory commands, including `--config`, and
storage init/transfer commands use the same registry. Core/browser consumers
have no native SDK dependency.

`StorageEndpoint` distinguishes an absolute local path from an opaque service
descriptor resolved from a named environment variable. Its debug output and
`Endpoint` debug output redact the address. Persisted routes carry instance
references and verified namespace identities, never resolved service descriptors.
Factories bind immutable non-secret options with `configured`, then validate
each actual namespace against `RepositoryRequirements` before writer admission.
Version-one profiles mean the existing domain port's complete atomic mutation,
snapshot/revision/claim, durability, query and recovery semantics; they are not
a SQL dialect declaration. Unsupported options/profiles and namespace changes
fail closed. Project configuration cannot supply storage preferences, even with
executable trust. Credential names and values enter the existing redaction
boundary and stay withheld from tools.

The SQL compatibility driver and `SqlRepositories` remain SQLite/Turso profile
interfaces. A MySQL/PostgreSQL extension supplies its own schemas, migrations,
queries, error mapping and domain repository implementations. It does not
implement fictitious SQLite PRAGMAs or reuse Turso FTS queries.

Keep existing synchronous domain ports compatible. A network adapter can own
`storage::worker::Worker<T>` and await client futures on its real Tokio runtime.
`call().await` supports async callers; `call_blocking` yields Otto's multi-thread
executor with `block_in_place`. Single-thread hosts must use the async entry
point or a blocking thread. The queue is bounded, cancellation never withdraws
an accepted operation, and close/drop drain owned work. Preserve each domain's
existing command/receipt and unknown-effect recovery rules; never translate a
lost commit acknowledgement into rollback or retry an external tool.

`RepositoryFactory::prepare_import` and `export` compose external migration
adapters with the existing coordinator. Preparation is read-only and returns
an isolated stage's stable namespace identity. Domain-owned version-one record
schemas, approval envelopes and canonical memory codecs are public; table/SQL
mappings remain backend-owned. `migration::record` supplies canonical ordering
and digests. The existing transfer encoding and legacy local routes remain
readable. `StageImport::verification_chunk` returns actual staged logical data;
`PortableReferenceClosure` checks cross-domain provenance, session aliases,
pending approvals and receipts without querying destination SQL. The portable verifier
fails above its declared 64 MiB materialization bound; choose another bounded
validation strategy before supporting larger external reference sets. Transfers
using only the default local backends retain their existing streaming verifier
and limits.

Run public `storage::conformance` checks in isolated backend test namespaces.
The `storage-conformance` Cargo feature is enabled by default and can be disabled
in deployment builds. Helpers cover existing port behavior and staged command
receipts; backend-specific process/host loss, concurrency and transport fault
tests are additional requirements. Passing the independent JSON acceptance
adapter proves the extension boundary, not MySQL/PostgreSQL support or a live
service's durability. Actual backend extensions require those integration gates.

Run the public `otto_core::conformance::session` and
`otto_core::conformance::mailbox` checks (default-on `conformance` feature in
`otto-core`) against every `Session` and `Mailbox` implementation, wrappers
included, passing a `reopen` function where the state is durable. A new
implementation or wrapper adds such a test; a failure names the broken check,
such as `session.input_id` or `mailbox.fifo`. Existing runs:
`crates/otto/tests/session_conformance.rs` (POSIX `Store`, `SharedSession`,
`MemoryHandle`), the `SharedTranscript` and sidecar mailbox unit tests, and
the `otto-core` tests for `MemorySession` and `Inbox`.

`storage/migration/` contains versioned snapshot, bounded chunk, stage import,
verification, journal and cutover contracts. Domain codecs and backend staging
implementations live under `storage/backends/`. The generic coordinator
sequences an offline resumable migration, reconciles receipts, verifies the
staged snapshot, and publishes a new local route group through the route
authority; it never executes pending tools. `otto storage transfer <source-home> <destination-root>
<migration-id> [--memory-db <path>]` composes these ports for a stopped local
installation. On success it publishes the destination as the active route group
for the source home; restart Otto from that same home to use it. Repeating the
same command and ID resumes its manifest. Use `otto storage abort <source-home>
<migration-id>` to restore source admission before publication; abort retains
isolated staging bytes and marks that ID unusable. It refuses if publication is
committed or uncertain. The source and destination roots must not overlap.
Original database/session files remain in place; no cleanup is automatic. This
command does not provide a remote/cloud backend.

The existing `otto storage migrate <legacy.db> <new.db>` command remains a
separate Python-assisted importer for recognized legacy SQLite databases into
a new local Turso database. It keeps the source untouched and validates
recognized rows/schema before success; it does not invoke the logical transfer
coordinator. See the user manual for both command forms and source details.

## Core contracts

- Keep dependencies explicit and directed toward shared contracts. Reuse existing helpers and concrete types; add traits at real consumer boundaries, not for hypothetical implementations.
- Use `Message::validate`, `Block::validate`, `ContextMetadata::validate`, and `Usage::validate` in `otto_core::model`. Every type there is an owned value that derives `Clone`; there are no separate deep-copy helpers. Both `otto_core::session::Session` implementations (`MemorySession` in `otto-core` and `Store` in `crates/otto/src/session/store.rs`) enforce neutral validation and tool-call/result sequencing in `append`. Neutral validation permits transient messages without IDs/timestamps; Pi-specific encoding restrictions stay in `otto_core::session`. When adding fields, update ownership tests.
- `otto_core::provider::Response::message` is the single source of finish reason and usage. `Message::usage == None` means unavailable; `Some(Usage::default())` means explicitly reported zero. Preserve explicit presence (`usage_present` on `CompactionResult` and on the task records) through events, task progress, notifications, aggregates, and supported persistence metadata instead of inferring absence from zero counters. Keep legacy Pi normalization in the decoder.
- Keep provider-token persistence in `crates/otto::usage`: collection maps neutral agent events to content-free records, Turso only appends and aggregates those records, and frontends query through the server API. Never store prompts, response text, tool arguments, or tool output in the usage database.
- Keep context associations in the typed `ContextMetadata`. Prefer the structured `task_id` over notification wording; text parsing is only a legacy-history fallback. Preserve append-only Pi v3 compatibility and namespaced optional details, including the explicit-zero usage marker. Do not rewrite old records or invent missing historical metadata.
- `Session::append_custom` writes a Pi v3 `custom` entry; `pi_entry_to_context_messages` skips `custom` entries, so they never reach a model context. The trait method has no default body: every implementor, including wrappers such as `SharedSession`, states whether it writes or drops the entry. The sub-agent runner writes `otto.task_spec` after the task slot is set and `otto.task_result` after the completion notification is pushed, and drops a write failure. When a session is reopened with unanswered tool calls, the first unanswered call gets `MAY_HAVE_RUN_TOOL_RESULT_TEXT` and the later ones `NOT_EXECUTED_TOOL_RESULT_TEXT` (`otto_core::session::context::missing_tool_results`); tool calls run one at a time, so the answered calls are a prefix.
- Every admitted run (`agent::run_inner`, after the redaction and empty-text checks) first appends an `otto.turn` start record (`otto_core::session::turn`, `TURN_CUSTOM_TYPE`) with `append_custom` and, on every return, an end record whose `outcome` is `completed`, `failed`, or `cancelled` (an error with a control stop reason). Data is `{"event":"started","schemaVersion":1,"turnId":…,"trigger":"user"|"wake"|"recovery","attempt":N,"resumes"?:…}` or `{"event":"ended","schemaVersion":1,"turnId":…,"outcome":…}`; the turn id comes from `Options::new_operation_id`. An empty-text run whose session has an `interrupted_turn` (a start without its end on the active path, folded into `ResolvedContext::interrupted_turn`) records `trigger: "recovery"` with that turn's `attempt + 1` and `resumes`; every other run records attempt 1. A failed end write is ignored, since a failed store rejects later writes. `Session::interrupted_turn` has no default body. `Store::from_file` records a `Recovery` (`cause` `TakenOver` or `ProcessLost`) when a lease was taken over, or when the file ends with calls without results or has an interrupted turn; it applies the replay predicate on every open unless the interrupted turn's attempt exceeds `MAX_AUTOMATIC_RESUMES` (1). `failover::recovery::notify` pushes the notice that starts the resuming wake turn, or, when the resume is exhausted, pushes nothing and returns a warning. Each `otto.operation` attempt records the executor's `ToolExecutor::retry_safety` as optional `retrySafety`; `otto_core::session::operation::replay_allowed` is the one replay rule for the store and the agent: the current declaration is `ReadOnly`, at most one attempt, no terminal fact, not corrupt, and a recorded `retrySafety` absent or `ReadOnly`. `otto serve` reports the resuming wake turn with trigger `recovery` (`Controller::resumes_interrupted_turn`). The design is `docs/specs/2026-10-06-crash-recovery-and-turn-resume.md`.
- Agent inputs go through the `otto_core::mailbox::Mailbox` port; only `runtime`, `subagent::tasks` and the runner fallback construct the in-memory `Inbox` (`runtime::sidecar_mailbox` tests enforce it). `runtime::sidecar_mailbox::install` makes an `Inbox` durable: it assigns `in_<hex>` ids to notifications without one and mirrors every change to a sidecar with `replace_private`; `load` restores it, moving an unreadable file aside. `Mailbox::enqueue` returns `Result<u64, MailboxError>` and skips an id already queued; a producer that can report a failure (`agent_send`, `agent_report`, steering admission) acknowledges the entry and returns an error. Every delivered input carries `Message::input_id` (Otto detail `inputId` on user and context messages; `model::is_input_id`), and delivery uses `Session::append_input_if_absent`, which appends only when the active path has no message with that id; it has no default body. Sidecars: `<id>.inbox.json` (notifications), `<id>.inputs.json` (steering journal: `Steering::set_journal`, entries scoped by `otto.turn` id, restored by `Steering::begin_turn` for the turn that resumes an interrupted one, removed by delivery, withdrawal and `Run` drop), `<id>.followups.json` (`TaskControl::set_followup_journal`; `begin_resuming` adopts entries for a wake turn that resumes an interrupted turn, `begin` discards them and the caller records `record_unrun_followups`; each step's `agent_followup` message carries `followup:<scope>:<n>` and a step already in the transcript is skipped), and `<child transcript>.inbox.json` (`runner::Config::child_mailbox`, applied by `Tasks::replace_child_inbox` once the transcript path is known). Client idempotency keys are `input_id` (HTTP bodies) and `_meta.otto.inputId` (ACP); the server keeps the last 256 per session (`SessionState::inputs`), checks `Controller::has_input` for older ones, and passes the id to the user message with `Controller::set_next_input_id`. The design is `docs/specs/2026-10-06-durable-inputs.md`.
- Session forks: `otto_core::session::fork` resolves fork points and plans the copy without I/O. A fork point is a user message (empty `context_type`) that is the first prompt of a `user`-trigger `otto.turn`, or, on a path without turn records, a user message with no unanswered tool call before it; the copy keeps the active path before that turn's start record, or, with no fork point, up to the last entry outside every turn with every call answered. `SessionRepository::fork` (POSIX `storage::backends::posix::session::fork`) copies the kept entries byte for byte with their ids, adds a `session_info` `<name> (fork)` when the source has a name and one `otto.fork` custom record (`{"schemaVersion":1,"sourceSessionId","sourceEntryId"?,"prompt"?}`), sets the header's `parentSession` to the source path, writes `<new id>.jsonl.fork-tmp` beside the source with one sync and publishes it with `rename_excl`; listing ignores the temporary name. The source is only read. `Store::forked_from` caches the record on open (`fork_origin`), `app::Info::forked_from` carries it, and `Controller::fork_file` / `fork_session` (fork, then `resume_session`) are the entry points for `POST /v1/sessions/{id}/fork`, `GET .../fork-points` and TUI `/fork` (`Reply::Forked` puts the prompt in the editor). Sidecars and sub-agent children are not copied. The design is `docs/specs/2026-10-06-session-fork.md`.
- At the start of each run, after recovery replay and queued-notification delivery and before the run's user or follow-up message, the agent compares its current `otto_core::session::Capabilities` with `Session::observed_capabilities()` (`agent::announce_tool_changes`). A record whose tool names or tool definition digests differ, or whose Otto version differs while `otto_help` is a current tool, or no record on a session with an assistant message or compaction, appends a `Role::Context` message of type `capability_update` (`otto_core::session::CAPABILITY_UPDATE_CONTEXT_TYPE`, `display` true, text without the `[Custom context: ...]` prefix on reload). The agent then writes `otto.capabilities` (`CAPABILITIES_CUSTOM_TYPE`) with `append_custom` if it is absent or differs; the message is written first, so a failed record write repeats the notice instead of losing it. The record is rewritten when any field differs, including the Otto version alone, which produces a notice only when `otto_help` is among the current tools. Record data is `{"tools":["a","b"],"definitions":{name:digest},"version":"X.Y.Z"}`: `tools` is sorted and deduplicated (older releases require it); `definitions` holds the first 16 hex characters of the SHA-256 of each tool's description, a 0x00 byte and the raw parameter bytes (`agent::capabilities::definition_digest`); `version` is `Options::version`. Absent optional fields read as unknown and produce no notice line. Manual section titles must stay unique case-insensitively, because `otto_help` returns the first match; a test enforces it. `build_context` resolves the latest entry on the active path into `ResolvedContext::observed_capabilities`; a malformed entry adds a warning and reads as absent. Notice names pass `otto_core::tool::safe_prompt_tool_name`. Compaction carries summarized notices verbatim in the `<capability-updates>` summary block (`agent::summary_details`, bounds 16 KiB and 8,000 characters per notice). `Session::observed_capabilities` has no default body, like `append_custom`: every implementor, including wrappers, forwards or states its behavior. The design is `docs/specs/2026-10-06-capability-discovery-and-resume-updates.md`.
- `otto_core::tool::ToolResult::persisted_content == None` selects `content`; `Some` selects its value, including `Some("")`; `persisted_text` applies that rule. Preserve redaction and the current-turn full-result overlay. Reuse tool definitions and assembly helpers, including `tool::bash::bash_definition` in `crates/otto`; keep conservative preflight and the `Registry::new` validation.
- Tool arguments decode model output rather than a strict client. An optional list argument deserializes with `tool::empty_as_none`, so an empty list reads as an absent key and a model that sends `[]` beside the arguments it is using is still served; `crates/otto/tests/tool_argument_contract.rs` scans the tool sources for one that does not.
- `Provider` and `Tool`/`ToolExecutor` instances are shared concurrently, so every trait method takes `&self`. Requests and arguments are borrowed for the duration of the call; returned responses, results, and definitions are owned by the caller. Per-call `StreamSink` callbacks are ordered and finish before `Provider::complete` returns; `Event` payloads are owned values, so consumers keep what they need without further copying.
- `serde_json` maps keep insertion order in native workspace builds: `agent-client-protocol-schema` (a `crates/otto` dependency) enables `serde_json/preserve_order`, and Cargo feature unification applies it to every workspace crate. `otto-core` built alone for wasm32 keeps sorted keys. Code and tests must not depend on either order; compare parsed `serde_json::Value`s instead of serialized strings.
- Reuse `otto_core::agent::CompactionResult` as the shared compaction payload. Keep HTTP/SSE DTOs in `crates/otto/src/server` separate from internal structs; update `testdata/server/openapi.yaml` alongside wire changes and preserve existing field meanings.

## Lifecycle and frontend contracts

- Construct `app::Controller` through `Controller::new` (one `Builder`, used by `cli::run`) or `Controller::with_builder` (a shared `Arc<Builder>`, used by `server`); the `Builder` in `crates/otto/src/runtime/builder.rs` is the only session/runtime construction path (`create`, `open`, `resolve_profile`). Do not restore placeholder factories or duplicate runtime construction paths.
- `Controller::request_close` is nonblocking. External lifecycle owners cancel active work as appropriate and call the synchronous `Controller::close` to complete cleanup. An idle close request alone does not release resources. Preserve exactly-once session/runner cleanup, cleanup errors, and post-close `info`/`history` snapshots; reentrancy is tracked by the admission generation (`begin_operation`/`Admission`), not by thread or stack inspection.
- Treat `subagent::tasks::Task` as a query snapshot. Update existing task progress and completion through `Tasks::mark_running`, `record_provider_step`, `record_tool_call`, and `finish`; preserve task identity, terminal states, and notification-before-`wait` ordering. A cancellation request is not proof that execution has stopped. The agent loop sees the registry only as `otto_core::agent::TaskRegistry`.
- Frontends use `Controller::tasks` (`app::TaskView` over the wire-shaped `app::Task`), never the mutable `Tasks` registry or the raw mailbox adapter. Call `Controller::prepare_wake` before publishing a wake turn, then `WakeOperation::run` it once or drop it. Dropping an unstarted claim releases it on cancellation/shutdown. `Tasks::updates` is a coalescing `tokio::sync::watch` signal, not a broadcast subscription; add no competing scheduler. Normal and wake turns share cancellation, event delivery, and compaction accounting; one-shot runs propagate wake failures.
- `otto_core::mailbox::Mailbox` is the shared notification port. It supplies ordered snapshots and per-entry acknowledgement: consumers append the session effect before `ack`, so a failed append leaves that entry and later entries visible for at-least-once redelivery. The bundled `InMemoryMailbox` (`agent::inbox::Inbox`) numbers entries (`Entry::seq`), invokes its optional persistence hook under its lock after each mutation, and signals change callbacks after releasing the lock. Native `runtime::wiring` alone installs the top-level session adapter, mirroring `<id>.inbox.json` through `tool::remind::replace_private` (temporary file, `fsync`, rename, directory `fsync`) and skipping writes once `<id>.jsonl` is gone, so an archive is not recreated by a late enqueue. It restores before producers are shared; restored entries signal the wake path. Child task mailboxes are intentionally ephemeral. Alternative adapters (including external SQL implementations) implement `Mailbox` and must preserve these ordering and acknowledgement semantics; the core contract does not promise distributed leases, delayed retries, or exactly-once delivery.
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
On macOS, CI runs `make check` as two parallel jobs: `macOS lint` runs
`rust-fmt` and `rust-lint`, and `macOS acceptance` runs `make check-tests`,
which is everything else in `check`. Clippy compiles the workspace a second
time, so running it beside the tests takes it off the critical path; the lint
job keeps its own dependency cache, since the tests' cache holds no
check-mode metadata.
A documentation-only change skips every gate. The workflow's `changes` job
classifies the diff first: when every changed path is Markdown outside
`crates/` and `testdata/`, under `docs/`, or `LICENSE`, the gates start and
skip every step, so they still report a status without installing a toolchain.
Anything else — including a workflow edit, `testdata/server/openapi.yaml`, or a
Markdown fixture the skill and agent loaders discover by name — runs the full
gate. An unusual range, such as a new branch or a force push that dropped the
old tip, also runs it.

### CI build cache

Most of a CI run is compilation, so the workflow keeps it down in three ways:

- It sets `CARGO_INCREMENTAL=0` and `line-tables-only` debuginfo for the dev
  and test profiles. A CI build never reuses incremental state, and line
  tables are enough for a failing test's backtrace. Local builds keep Cargo's
  defaults.
- The cargo cache holds the registry and the third-party part of `target/`,
  keyed by the root `Cargo.toml` (which holds the profiles), `Cargo.lock` and
  `rust-toolchain.toml`. A run saves it only when no
  cache matched the key exactly, after `node scripts/ci-cache.mjs prune`
  removes the workspace crates' own output, which changes with every commit
  and is most of `target/`. A failed gate still saves once
  `cargo test --workspace --no-run` finishes the build, since the key is
  saved only once and a failing test would otherwise leave every later run
  building cold. A push to `main` restores only an exact match, so
  the cache it saves never accumulates artifacts of older lock files; a pull
  request may fall back to `main`'s previous cache. Bump the `v2-` key prefix
  when a workflow change makes the cached artifacts unusable, such as the
  `CARGO_PROFILE_*` variables.
- `wasm-pack` and `wasm-bindgen-cli` are cached by version, saved right
  after they are installed. wasm-pack uses the
  `wasm-bindgen` on `PATH` when it matches the version `Cargo.lock` pins
  (`node scripts/ci-cache.mjs lock-version wasm-bindgen`); without it,
  wasm-pack compiles its own copy on every run.

Each gate builds `crates/otto` once per feature set, so its targets must not
ask Cargo for a different one; see `connect-check` below. The root
`Cargo.toml` builds the Turso crates at `opt-level = 2` in the dev profile:
the stores' tests spend most of their time in Turso, and an optimized Turso
runs them about three times faster at a compile cost paid once per Turso
version.

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
make check-fast     # the local gate: only the checks for paths changed since origin/main, one worktree at a time
make check          # full macOS gate: rust-fmt, rust-lint, all tests (PTY tests included), core wasm tests (no release build)
make check-tests    # `check` without rust-fmt and rust-lint (CI's macOS acceptance job)
make build          # the release build: cargo build --release, then copy ./otto
make install        # build and install otto, otto-connect and bundled skills
make rust-fmt       # cargo fmt --all -- --check
make rust-lint      # cargo clippy --workspace --all-targets -- -D warnings
make rust-test      # cargo test --workspace (offline)
make rust-wasm-check # cargo check -p otto-core for wasm32-unknown-unknown
make rust-wasm-test  # wasm-pack test --node for otto-core
make test-tui       # only the PTY tests: cargo test -p otto --test tui_pty --test tui_attach_pty
make check-linux    # the Linux gate: rust-fmt, rust-lint, rust-test, connect-check, rust-wasm-check, scripts-test, license-check
make license-check  # fail if a shipped Rust crate uses a license outside the allowlist
make connect-check  # Go connector: gofmt -l, go vet, go test -race (builds target/debug/otto for its end-to-end test)
make connect-build  # build the Go connector to target/otto-connect
```

`check-fast` is described under [Local verification](#local-verification).
`check` runs `rust-fmt` and `rust-lint`, then runs the full workspace test suite (which
includes the `otto-core` tests and the PTY tests), the Go connector
checks, and the core wasm tests under Node. It does not run `cargo build
--release` or `rust-wasm-check`; `make build` is the release build, and
`make check-linux` runs `rust-wasm-check`.

`connect-check` needs the Go version named in `chat-connector/go.mod`; CI installs it
with `actions/setup-go`. It gets `target/debug/otto` from
`cargo test --workspace --no-run`, which resolves the same features as
`rust-test` and so reuses its build; `cargo build -p otto` would leave the
dev-dependency features off and recompile most of the dependency tree. Its end-to-end tests drive the real
`otto acp`, and `otto serve` with `otto acp --attach`, from `target/debug/otto`
against a loopback fake provider, so they are offline like the Rust tests.

`rust-wasm-test` needs `wasm-pack` (pinned to
0.15.0 in CI via `cargo install wasm-pack --version 0.15.0 --locked`).
The PTY tests (`tui_pty`, `tui_attach_pty`) open a pseudo-terminal, which is
unavailable in some sandboxed shells; `cargo test --workspace` runs them, and
`make test-tui` runs only them. Neither gate runs `test-tui` separately:
`cargo test -p otto` resolves features differently from `--workspace` and
would compile `crates/otto` again just to repeat the same tests. Keep the default test suite (`cargo test --workspace` without extra
flags) offline: it must not need network access, provider credentials, or a
real interactive terminal.

The Node helpers under `scripts/` are opt-in, but their tests are not: `make
scripts-test` runs every `scripts/*.test.mjs`, and `check`, `check-tests` and `check-linux` include it. It
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

### Dependency licenses

`make license-check` runs `scripts/license-check.mjs`, and `make check-linux`
runs it. It reads the `license` field of every Rust crate in the non-dev
dependency graph (`cargo metadata --locked`, all targets), and fails when a license expression is not
satisfied by the allowlist `ALLOWED`: MIT, Apache-2.0, BSD-2-Clause,
BSD-3-Clause, ISC, Zlib, Unicode-3.0 and Unlicense. An `OR` expression passes
when one alternative is allowed; `AND` requires all. The check needs network
access in CI only if `cargo metadata` has to fetch crates. `cargo metadata`
resolves features for all targets together with dev-dependencies, so the
checked set also contains optional crates that a release build does not
compile (36 of 479 on 2026-10-05); such a crate can fail the check without
shipping.

Dev-only Rust crates are not checked because they do
not ship. The Go module under `chat-connector/` is not checked because
`go.mod` carries no license metadata.

When the check fails, read the package's license. If it is acceptable, add its
identifier to `ALLOWED`. For a package without a license field, read its
license file and add a `name@version` entry to `CLARIFIED`; the version is part
of the key, so a new version is reviewed again.

Otto's binaries are not distributed, so no third-party notice file ships. If
binaries are distributed, the MIT, BSD and Apache-2.0 terms require one.

### Local verification

Locally, run the focused tests for the change and then `make check-fast`. Do
not run `make check` or `make check-linux`; CI runs both on every PR. A full
gate compiles the workspace in test mode and again in check mode for clippy,
checks core WASM compatibility and runs the Go race tests. Every
worktree has its own `target/`, so two worktrees running a full gate at the
same time each compile the whole dependency tree on every core.

`make check-fast` first takes the lock file `otto-check-fast.lock` in the
common git directory (`lockf` on macOS, `flock` on Linux). All worktrees of the
checkout share that directory, so `check-fast` runs in one worktree at a time;
a second invocation waits until the first exits, and the lock is released when
the process exits, including on a crash. Then `scripts/check-changed.mjs`
lists the paths changed since `git merge-base HEAD origin/main`, including
uncommitted and untracked files, and runs only the commands those paths need:

| Changed path | Commands |
|---|---|
| any `*.rs` file | `cargo fmt --all -- --check` |
| anything under `crates/` or `testdata/`; root `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml` | `cargo clippy --workspace --all-targets -- -D warnings` |
| `crates/otto/src/<module>/…` or `crates/otto/src/<module>.rs` | `cargo test --workspace --lib -- <module>::`, one run for all changed modules |
| `crates/otto/tests/<name>.rs` | `cargo test --workspace --test <name>` |
| `crates/otto-core/`, `testdata/`, root manifests, `crates/otto/` `Cargo.toml`, `build.rs`, `src/lib.rs`, `src/main.rs`, `tests/common/`, `tests/fixtures/`, `tests/testdata/`, and any other path under `crates/`  | `cargo test --workspace`, in place of the two rows above |
| `crates/otto-core/`, root manifests | `make rust-wasm-check` |
| `chat-connector/` | `make connect-check` |
| `scripts/` | `make scripts-test` |
| root `Cargo.toml`, `Cargo.lock`, `crates/*/Cargo.toml`, `scripts/license-check.mjs` | `make license-check` |
| `docs/`, `LICENSE`, Markdown outside `crates/` and `testdata/` | nothing |
| anything else, such as `.github/`, `Makefile` | nothing; printed as not checked |

It ends with `git diff --check` against the same base.

The selection follows Cargo's units. A crate compiles as a whole, so any change
in `crates/otto` recompiles all of it; the selection only limits which test
binaries are linked and which tests run. The module filter is a libtest name
substring, so `memory::` also runs tests whose path contains `memory::` under
another module. A change in one module runs that module's unit tests only;
other modules' tests and the integration tests run in CI. Clippy with
`--all-targets` still compiles every target, so a compile error anywhere is
reported locally. When a change touches what an integration test covers, run
it by name, for example `cargo test --workspace --test tui_pty --test
tui_attach_pty` for `tui`. Use `--workspace` in focused Cargo commands too:
`-p <crate>` resolves features differently and recompiles dependencies.

`make check` takes no lock. To cap `check-fast`'s CPU use further, set
`CARGO_BUILD_JOBS` in the environment; the script passes the environment to
every command.

### ACP integration-test navigation

`crates/otto/tests/acp_stdio.rs` and `acp_attach.rs` retain the two ACP test
binaries and their process/JSON-RPC harnesses. Their scenario modules keep the
suite navigable without changing the focused command:

```bash
cargo test --workspace --test acp_stdio --test acp_attach
```

- `acp_stdio/{protocol,memory,approval,commands,tasks}.rs` covers the local
  stdio server's wire contract, memory review, permission dialogue, commands,
  cancellation, task queueing, signals, and wake turns.
- `acp_attach/{relay,approval,commands}.rs` covers the relay through
  `otto serve`: session parity/queueing/failure, relay permissions, and
  commands, reflection cancellation, and wake turns.

When changing one scenario, use its fully qualified libtest name to narrow the
existing target, for example `cargo test --workspace --test acp_stdio
memory::` or `cargo test --workspace --test acp_attach relay::`. Run both
targets after shared ACP protocol, session, approval, or serve changes.

## Web UI workflow

The browser frontend and WASM bindings live in
[otto-plus](https://github.com/baiyuqing/otto-plus). Build and test them there.
Otto builds independently, serves authenticated HTTP APIs, and keeps its core
WASM conformance checks. Coordinate API/schema changes with the frontend's
pinned core revision and wire declarations; use its same-origin proxy rather
than adding CORS or static assets to Otto.

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

## Docker acceptance

After changing `Dockerfile`, `.dockerignore` or `compose.yaml`, build the
image with `docker build -t otto:local .`, then run
`node scripts/docker-smoke.mjs`. This opt-in test requires Docker Compose
and a free localhost port 8787. It uses a disposable project, workspace and
state volume; it calls no live provider and deletes its own resources.
It checks the real setup flow across recreation, non-root execution,
API token enforcement and rotation, session persistence,
workspace writes and graceful SIGTERM shutdown. It does not run as part of
the default offline script tests. Check Docker Desktop on each Mac
architecture separately; Linux execution cannot verify macOS file sharing.

For managed environments with an inspection proxy, supply a trusted combined
CA bundle:

```bash
docker build --secret id=proxy_ca,src=/path/to/ca-bundle.pem -t otto:local .
```

The build uses this optional secret only during networked
toolchain and dependency installation; it is not copied into the image.

## Secret scanning

Install Gitleaks (`brew install gitleaks` on macOS), then run
`make setup-git-hooks` once per clone. The native pre-commit hook scans staged
changes and blocks commits on findings or scanner errors, including a missing
Gitleaks binary. The hook is shared with this clone's worktrees; installation
refuses to overwrite a different hook. Other clones need their own setup.

The independent **Secret scan** CI job scans full Git history on every PR and
push to `main`, including documentation-only changes. It uses Gitleaks 8.30.1
with a pinned download checksum and redacted output. Configure this job as a
required status check for `main`. GitHub Secret Scanning and Push Protection
provide complementary server-side detection for supported secret types.

For a manual scan, run `gitleaks git -v --redact --ignore-gitleaks-allow` from
the repository root. Inline `gitleaks:allow` comments do not bypass the hook
or CI. The root `.gitleaksignore` lists reviewed historical false positives
by exact fingerprint, with a reason for each group. All files and new commits
remain scanned. Before adding an entry, inspect the finding in its original
commit and confirm its provenance; do not ignore whole test directories,
rules, or commits. Changes to ignore lists or scanner configuration need the
same careful review as security-sensitive code.

Hooks can be bypassed and CI runs after upload; neither guarantees that secrets
never reach the remote. Keep real credentials outside tracked files. If a real
secret is uploaded, revoke or rotate it immediately; deleting it in a later
commit leaves it in history.

## Changelog

[CHANGELOG.md](../CHANGELOG.md) is the curated record of notable user and
contributor changes. Update it in the same PR as features, fixes, compatibility
changes, security changes, performance improvements, or build/CI changes.
Documentation changes that affect how people use or develop Otto also belong
there; typo-only edits, internal refactors with no observable effect, and
changelog maintenance itself need no entry.

- Add pending entries under `Unreleased`. Use these categories in order:
  `Usage and compatibility`, `Features`, `Fixes`, `Security`, `Performance`,
  `Build and CI`, and `Documentation`. Omit empty categories.
- Describe the resulting behavior and its practical impact in concise English,
  with a PR link (or commit link when there is no PR). Group related changes;
  do not copy the commit log. Add the link once the PR number is known. The
  complete historical index is a fixed initial backfill, not a per-PR ledger.
- Put breaking changes, changed defaults, migration steps, and required user
  action in `Usage and compatibility`. Link the manual for detailed setup.
  Use other categories for distinct aspects of the same change without
  repeating the same explanation. Do not claim unmeasured speedups.
- In a release PR, move the pending entries into `## X.Y.Z - YYYY-MM-DD`
  using the intended release date and leave `## Unreleased` above it.
  Keep releases newest first and preserve prior release entries and the fixed
  historical index. `make bump`
  updates version files only; this changelog step is manual.

## Releases

The version is `[workspace.package] version` in `Cargo.toml`, shared by `otto` and `otto-core`. The `otto` and `otto-core` entries of `Cargo.lock` must carry the same value; a test fails `make check` when they differ.

Versioning follows SemVer in the 0.x range: a new feature or an incompatible change raises the minor version (0.1.0 to 0.2.0); only fixes raise the patch version (0.1.0 to 0.1.1). Moving to 1.0.0 is a maintainer decision. Version numbers change only in a release PR.

1. On a branch, run `make bump VERSION=X.Y.Z`. `scripts/release.mjs` requires X.Y.Z to be a plain version greater than the current one, rewrites `Cargo.toml` and `Cargo.lock` by exact text replacement (no other line changes, no network), and exits with an error if a location is missing or matches more than once.
2. Archive the `Unreleased` changelog entries as described above. Commit, open a PR titled `Release vX.Y.Z`, and merge it.
3. On an up-to-date `main`, run `make release-tag`. It checks that HEAD equals `origin/main`, the working tree is clean and the tag does not exist locally or on origin, then creates the annotated tag `vX.Y.Z`, pushes it and runs `gh release create` (needs `gh`).

Release notes are generated by GitHub from the PR titles merged since the previous tag. The curated, categorized history lives in [CHANGELOG.md](../CHANGELOG.md); review it alongside the generated notes.

### Build metadata

`crates/otto/build.rs` embeds the output of `git rev-parse --short=12 HEAD`, plus `.dirty` when tracked files were modified when the build script last ran. It reruns when HEAD, the index, the current ref, `packed-refs` or anything under `crates/` changes. When git is unavailable it uses the `OTTO_GIT_HASH` environment variable, then `unknown`. The result is `otto::VERSION` (`0.1.0+a1b2c3d4e5f6`), printed by `otto --version` and reported in the ACP agent info, the MCP client info and the `otto_help` output.

## Secrets, safety, and documentation

- Never add `--api-key`; API keys come only from environment variables, and ChatGPT credentials only from `otto login`.
- Never put raw API keys, OAuth tokens, or auth headers in TOML, JSONL session fixtures, logs, docs, or tests. Redact sample values in errors and examples.
- `read`, `grep`, `find`, `ls`, `write`, `edit`, and `skill` must reject workspace/skill-directory escapes after canonical path and symlink validation.
- Do not describe `bash` as always unsandboxed. Otto defaults to macOS Seatbelt; only explicit sandbox `off` is unsandboxed.
- The chat connector reads its Telegram bot token and Feishu app secret from the environment variables that `connect.toml` names; a secret key in that file fails the config load. Its errors and logs never contain those secrets, and it does not log message text (the Feishu adapter drops the SDK's debug and info logs, which carry event payloads and the connection URL).
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

### Unified approval contract

`otto-core::approval` owns neutral requests, choices, scopes and separate
human/application states. `otto::approval` stores them in `approvals.db`; the
same decision authority is injected into the parent and child native registries,
HTTP, ACP and terminal surfaces. Models discover `request_approval` in actual
runner definitions and system guidance, and choose when conversational consent
is needed. Models can query/withdraw but cannot decide an approval. Session-owned requests
default to automatic decisions under the persisted AUTO policy; user-only
`/auto off` selects manual review. Creation and decision remain distinct
journal events. See [session automatic approval](specs/2026-10-05-session-auto-mode.md).

Authorization is prepared before tool dispatch, bound to the logical operation,
exact arguments and relevant domain version. Decisions/events commit together.
An application claim has a process identity; maintenance must not take over a
live owner's work. Domain handlers reconcile memory candidates, workflow gates,
config previews and vetted skill artifacts through their canonical receipts.
External actions with an uncertain effect are not replayed automatically.

Confirmation/rejection forces a new provider step: later calls in the same
assistant batch receive `NotStarted` results. Skill proposals never write when
approval service composition is missing. Memory review text stays in the
candidate store; the approval journal retains references and digests only.

HTTP exposes `/v1/approvals`, per-request decisions/withdrawal and resumable
changes/events. ACP uses standard permission requests for tool authorization,
negotiated elicitation for confirmation, and `_otto/approvals/*` for durable
queues. Connector cards and commands bind the exact request/version; feedback
returns to the waiting model. MCP calls pass the same native authorization
boundary; unsupported inbound elicitation modes are not advertised and cannot
be interpreted as authorization.

Design: [unified approval](specs/2026-10-04-unified-approval.md).

### Agent self-help and approval conversations

`tool::otto` reads installed help from the embedded canonical user manual and
uses current runner definitions for capabilities. `app::approval_control`
reuses the provider with an isolated memory transcript and only help/query/
withdraw/queue tools while the original approval waits. It never grants or
runs task tools, and only the original runner writes dialogue context from
its inbox. A dialogue is bound to one session and original request ID;
withdrawal cannot affect a newer request. Protocol and frontend contracts:
[self-help and approval control](specs/2026-10-03-agent-self-help-and-approval-control.md).

### Sandbox read approvals

Bash's optional `sandbox_read_path` requests persistent read access to one
absolute existing path, with a justification and `use_default` permissions.
`BashApprovals` binds its canonical path and exact command to the session's
latest approval ID, separately from unsandboxed command grants. The controller
applies the existing backed-up config amendment and sandbox reload before
granting a single confined retry. Reload failure rolls back the config; denial
and cancellation discard the pending request. A changed canonical path requires
a new request. The optional `read_path` on `approval_requested` distinguishes
read grants from elevation for attached clients. ACP advertises `allow_always`
and discloses persistence in the title; the connector routes it through its
existing cards and callback admission checks.

## Plugin execution contracts

`otto-core::config::plugins` owns package/config schemas; `agent::hooks` owns
an optional async hook port and session lifecycle state. Core invokes hooks
before tool dispatch so argument rewrites reach child write policy, tool
validation and exact-argument authorization. Native `plugin/` owns pinned
package discovery and immutable resource digests; `plugin/hooks.rs` owns
command execution and offline JSON Schema validation. Runtime wiring composes
existing skill/MCP paths and
injects the same plugin runtime into parent and child agents.

Before hooks may deny but cannot grant rights. Commands use the native sandbox
executor with bounded stdin, output and process-group cleanup. Project roots,
hook declarations and resources participate in executable trust; recheck at
effects and cancel running package commands when source or authority changes.
Preserve original operation facts across output transformations and append
argument/original-result audit entries before exposing rewritten results.
Hook effects make even read-only tool attempts ineligible for automatic
crash replay. Session hook state belongs to the shared session, never a
replacement runner. Graceful close owners await session-end hooks where the
frontend exposes asynchronous shutdown; synchronous close starts bounded
cleanup, and a process crash makes no delivery guarantee.

Protocol and examples: [Hooks and custom tools](user-manual.md#hooks-and-custom-tools).

## Agent implementation contracts

Apply these requirements to every feature, fix, and refactor:

- Keep agent instruction files short navigation maps and rulebooks. Put detailed contracts next to their owning code or in this development guide, and link to one canonical source instead of copying it across agent instructions.
- Make changes easy to locate: use consistent domain names, cohesive packages, explicit entry points, and adjacent tests. Update the task map when responsibilities move or new packages are introduced.
- Keep dependencies explicit and one-way. Put shared behavior in its owning layer and inject dependencies at the composition root; avoid hidden global state and frontend-specific copies of shared use cases.
- Make contracts readable from types and focused documentation: define valid states, ownership and mutation, concurrency and cancellation, error behavior, and compatibility where relevant. Validate data at trust boundaries.
- Extend existing patterns with the smallest cohesive change. Add interfaces only at real consumer boundaries; do not add speculative frameworks, registries, or configuration for future implementations.
- Turn important invariants into executable checks. Update architecture guards and contract tests when boundaries change; failures should identify the violated rule and where to fix it. Keep focused checks fast, offline, and deterministic, and preserve full acceptance coverage.
- Keep code, schemas, tests, and canonical docs consistent in the same change. Remove stale instructions or mark superseded designs as historical; record durable rationale in the repository rather than relying on chat history.

### Capability descriptions

The model learns what Otto can do only from these sources. Do not add a
separate capability catalog.

- Written by hand: each tool's `description` and input-schema field
  descriptions; the policy sections in `system_prompt_for`
  (`crates/otto/src/runtime/prompt.rs`), each rendered only when its tool is
  registered; the user manual, which `otto_help` serves to the model; and
  `EXCLUDED_CHILD_TOOLS` (`crates/otto/src/subagent/runner.rs`).
- Generated from the registry, never edited by hand: `<available_tools>`, the
  `otto_help` `capabilities` topic, and `capability_update` notices.

A description is short and has one reading. It states:

- the capability: what the tool does, and when to use it instead of a related
  tool;
- inputs: each parameter's meaning, type, allowed values or range, default,
  and whether it is required;
- outputs: what a successful call returns and what a failed call returns;
- limits and effects: counts, sizes, durations, persistence, and effects that
  need approval.

Use one term per concept and concrete values ("at most 8 timers, 1 to 3600
seconds"), not adjectives ("a few", "short"). Leave out text that repeats the
schema or does not change how the model calls the tool.

Adding or changing a capability updates, in the same PR:

1. the tool's description and schema field descriptions;
2. its policy section in `system_prompt_for` when its use depends on other
   tools, ordering, approval, or the session lifecycle, with a prompt test
   that the section is present when the tool is registered and absent when it
   is not;
3. the descriptions and policy sections of related tools that should name it
   (for example, `remind` names `schedule`);
4. `EXCLUDED_CHILD_TOOLS`, when sub-agents must not have it;
5. the user manual.

The tool name must pass `otto_core::tool::safe_prompt_tool_name`; otherwise it
is left out of `<available_tools>` and of `capability_update` notices.

Removing a capability deletes, in the same PR, its policy section and every
mention of its name in other tools' descriptions and schemas, other policy
sections, `EXCLUDED_CHILD_TOOLS`, and the user manual, including its example
agent definitions. A remaining mention directs the model to a tool that does
not exist. Check with `git grep -w <name> -- crates docs`: each remaining
match is a historical spec, a test that asserts the tool's absence, session
compatibility code, or an unrelated use of the same word. Renaming is a
removal plus an addition.

The PR body lists the added, changed, and removed tool names and the updates
made for each. A PR that changes a capability without these updates is not
merged.
