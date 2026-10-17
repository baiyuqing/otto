# Architecture

Previous: — · [Documentation index](../README.md) · Next: [Development workflow](workflow.md)

## Package boundaries

Keep responsibilities split along the current Rust crate/module layout:

- `crates/otto-core` (I/O-free contracts, formats and agent loop; wasm-safe; no filesystem, network, process, environment, stdio or wall-clock API; see the [layer map](../code-organization.md#rust-layers)):
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
  - `cli`: flag parsing, process lifecycle, signal handling, the REPL, the CLI-only startup steps, and frontend dispatch
  - `runtime::startup`: the startup composition root shared by `cli::run`, `otto serve` and `embed`: environment capture, config, storage, stores, `Shared`, `Builder`, workspace sandbox and memory service, as phase functions the CLI calls with its own steps in between and `startup::start` calls in sequence
  - `embed`: the public in-process API; see [Embedding API](architecture.md#embedding-api)
  - `app`: shared lifecycle, turn admission, session replacement, task/authentication capabilities, profile selection, and session info/history access
  - `session`: native JSONL session storage built on `otto_core::session`
  - `config`: native layer discovery, content-bound executable trust, config paths and loading, and the one writer every config change goes through: a compare-and-swap against the bytes the caller read, an atomic temp-file rename, and a timestamped copy of the replaced contents under `backups/`
  - `tool`: native execution of workspace-confined `read`/`grep`/`find`/`ls`/`write`/`edit`/`bash`/`skill`, in-process `remind`, `list_models` (the OpenAI-compatible endpoint's `GET /models`), and the MCP tool adapter (`tool::mcp`) that presents one external server's tools to the model
  - `mcp`: the JSON-RPC codec, the stdio and HTTP transports, OAuth 2.1 sign-in for HTTP servers, and the per-server client that negotiates the protocol era and serves `tools/list`/`tools/call`
  - `sandbox`: sandbox driver contracts, the Seatbelt and direct drivers, environment filtering, and conformance helpers
  - `provider`: native HTTP transports for the two provider implementations
  - `auth`: ChatGPT OAuth sign-in (`otto login`/`otto logout`), credential storage at `~/.otto/auth/chatgpt.json`, and access-token refresh
  - `memory`: domain contracts, validation/secret guards, conservative policy, repository ports and the `Service` implementation. The file store (`memory/files/`) holds records, tombstones and candidates as files under `~/.otto/memory/`; it is not a storage domain, and `otto memory` commands open the storage route authority only while `identity.json` is absent, to find a published import source. Advice audit SQL lives under `storage/backends/turso/`. A one-time importer reads the earlier Turso memory database. `decision` owns the native typed `Backend`, questions/answers, finite observational confidence, session prompt/parser and Jev transport, with compatibility exports under `memory::advice`. `memory::advice` owns memory-specific advisory review and orchestration (`Advisor` runs a typed audit port). `Advisor::propose` calls `Service::check_proposal` first and queues the candidate without advice on timeout (20 s), error or invalid answer; the session-model backend (`runtime::advice`) is built in `runtime::wiring`, and `TYPESAFE_API_KEY` selects the Jev backend. Advice is stored as `advice.*` candidate metadata and rendered only by `memory::advice::advice_line`
  - `typesafe`: the HTTP client of the TypeSafe auxiliary model (`jev-latest`), shared by `skill::check` and `decision::jev`. `reflection::gate` uses this typed backend only for automatic scheduling when JEV is configured: strict immediate/defer answers, a 20 s outer timeout and cancellation select, fail-open on unavailable/invalid replies, and no watermark write on defer. It sends bounded redacted evidence, emits content-free `reflection_decision` measurements, and cannot bypass existing dynamic-content, memory-review or skill-approval boundaries
  - `reflection`: the `/reflect` use case, also started by the model's `reflect` tool (`tool::reflect` is the port; `app::requested_reflection` binds it to each installed runner and runs it beside the turn with trigger `requested`, sharing the background single-flight flag): it reads the part of a session no earlier run covered (from the session file, so compacted entries stay reachable), classifies entries as local or external (`taint`), builds one tool-less request (`prompt`), parses and validates the answer (`output`), verifies quoted evidence in code (`evidence`), writes a per-run Markdown log, opening with the model's retrospective and listing every proposal and its outcome, to `~/.otto/reflection/runs` (`runlog`, which also lists and reads those logs for `/reflect list|show` by matching directory entry names), and queues survivors as memory candidates through `Service::propose` with `Origin::Extractor`. `repository.rs` owns reflection records/ports; the Turso adapter owns `~/.otto/reflection.db` SQL. Skills pass `guard` (structure and the rule table), `review` (a fail-closed second-model call), and Python syntax validation in `skillwrite`, the only file that mutates skill files; it accepts only a `guard::Vetted`, versions the complete Markdown/Python file set, and publishes staged directories atomically through `fsops::publish_directory`; which only `guard::approve` builds. `crates/otto/tests/reflection_boundary.rs` fails if this module ever writes, forgets, or reviews a memory record directly, if a file other than `skillwrite.rs` mutates the filesystem (except `runlog.rs`, which creates each run log with `OpenOptions` and `create_new`), or if `Vetted` is built outside `guard.rs`. A new scan rule goes in the `RULES` table in `guard.rs` with a positive and a negative test case
  - `usage`: native collection of parent, sub-agent, and compaction token events; a typed repository port; and total/daily aggregate queries consumed by the server. Turso SQL lives in its backend adapter
  - `skill`: SKILL.md frontmatter parsing, name/description validation, discovery across configured roots, and rendering of the system-prompt listing
  - `subagent`: child agent construction (`Runner`), task lifecycle, the non-blocking parent-facing `agent`/`agent_status`/`agent_send`/`agent_cancel` tools, runtime-owned mailbox wake turns, the child-only `agent_report` tool, shared task-formatting helpers used by both the REPL and the TUI, and AGENT.md definition discovery
  - `workflow`: workspace-scoped TOML DAG discovery, typed run/step/attempt/approval/event records and repository port, committed-boundary recovery, and the shared CLI/server controller. Turso persistence lives in its backend adapter
  - `acp`: the Agent Client Protocol v1 agent server behind `otto acp`: newline-delimited JSON-RPC on stdin/stdout with a single stdout writer, per-session `Controller`s on one `Builder`, the mapping from agent events and stored history to `session/update`, and the after-turn `session/request_permission` round trip for elevated `bash` and persistent sandbox read grants. `acp::tcp` is the opt-in `otto serve --acp-listen` transport; it uses the existing serve router in process and shares controllers, turn queues and approvals. Its authentication/lifecycle contract is [ACP TCP](../specs/2026-10-03-acp-tcp.md). `acp::attach` is the `otto acp --attach` relay: the same transport and dispatcher with every session operation sent to `otto serve` through `client`. Message types come from the `agent-client-protocol-schema` crate; the designs are `docs/specs/2026-10-02-acp-agent-server.md` and `docs/specs/2026-10-02-shared-session.md`
  - `client`: the HTTP client of `otto serve` over its Unix socket, used by `acp::attach` and `tui::attach` (sessions, history, queued turns and their SSE streams, the `/v1/status` stream, turn cancel, approval decisions, compaction, context, sub-agent tasks, sandbox reload); it maps a failed connection to `Error::Unreachable` and a non-2xx answer to `Error::Http`
  - `server`: HTTP/JSON/SSE frontend, wire DTOs, per-session turn buffering, metrics, the Unix-socket and loopback-TCP listeners, bearer-token gating of `/v1/`
  - `tui`: the terminal frontend and alternate-screen lifecycle. `features/` owns injected presenters and the command catalog; add a feature by implementing a presenter and registering it there. Presenters project domain data to `View` and return transitions, while `interaction/` owns shared terminal-independent selection, focus, fields and composer state. `backend.rs` owns asynchronous requests and scope tickets; `widgets.rs` and `render.rs` draw views and `App` snapshots without reaching services. Local and `--attach` share composer and feature input; `attach.rs` sends requests through `client`, follows `/v1/status`, and reconnects, while both modes apply turn events as wire frames. `tests/tui_boundary.rs` protects these ownership lines.
- The browser UI and WASM bindings live in [otto-plus](https://github.com/baiyuqing/otto-plus), which consumes the shared core at a pinned revision.
- `chat-connector/`: `otto-connect`, the Go chat connector (module `github.com/baiyuqing/otto/chat-connector`). It is an ACP client that starts an ACP agent (by default `otto acp`) as a child process and links no otto code; its contract with otto is ACP v1 on stdio or authenticated loopback TCP (`[agent].address`). `internal/agent` owns the child process or TCP connection and the ACP handshake, `internal/bridge` the chat-to-session map, queues, commands, replies, permission requests and admission, `internal/telegram` the Bot API adapter (including Markdown rendering to entities), `internal/feishu` the Feishu/Lark adapter over `channel-sdk-go`, `internal/config` `connect.toml`, and `internal/state` the state file. Design: [chat connector](../specs/2026-10-02-otto-connect.md)

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
baseline query) under `STANDING_CONTEXT_PREAMBLE`, then the other in-scope records
(workspace first, newest first, within `max_results` and `recall_tokens`; the
message text is not matched) under the untrusted `MEMORY_CONTEXT_PREAMBLE`. The standing set has its own token
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

The [open Memory service contract](../specs/2026-10-07-open-memory-contract.md)
defines the implemented Rust port in `otto-core::memory` and the native
`memory::client::Client` admission boundary. `BuiltinAdapter` projects the
existing service without changing its native recall/review behavior.
`embed::MemoryExtension` and `cli::run::run_with_extensions` inject another
service through the shared startup phase. External ReviewedChanges model tools
are read-only; trusted Rust hosts use prepared snapshots for inspection/review.
Delegated mutations require an explicit scope/input grant. Native-record
management must never silently fall back to a local store for that service.

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
not persisted. `agent` always returns after registering the child and has no
synchronous wait option. Child reports and completion enter the parent's durable
mailbox; when the parent is idle, frontend wake loops admit an empty-text turn
through the normal session operation gate. The model must not poll
`agent_status` to emulate waiting. `agent_send` queues parent task updates in a
child's private inbox, and child agents read them only at normal agent-loop
notification checkpoints;
it does not interrupt an in-flight provider or tool call. `agent_cancel` is the
parent-only stop for one queued or running task: it cancels the task's token,
which releases a queued task before it starts and stops a running one at its
next checkpoint, and it never detaches an effectful child, which settles before
the task is released. The task ends `canceled` and produces the usual
notification; `/task cancel`, `POST /v1/sessions/{id}/tasks/{task_id}/cancel`
and ACP reach the same registry call. `agent_report` is a
child-only tool that queues one message in the parent's task inbox without
finishing the task; a child uses it only to answer a parent question or to
report a blocker needing a parent decision, never for progress updates, plans,
or interim findings. `REPORT_BUDGET` (`crates/otto/src/subagent/tasks.rs`,
currently 10) caps it at that many calls on the child's own initiative per
task, plus one more per `agent_send` message the parent sends; a call past
the limit returns a tool error and delivers nothing to the parent.
Definitions cannot add tools outside the child tool set; `tools` only
narrows it. `[agents]` is TOML only, like `[skills]`.

A turn whose model stops making progress ends by itself. The turn loop owns a
`ProgressGuard` (`crates/otto-core/src/agent/progress.rs`) configured from
`Options::progress`, and every turn gets one, including each sub-agent's, which
inherits the parent's limits. It compares each tool result with the previous
one under a fingerprint of the tool name and its canonical arguments (object
keys sorted, so two spellings of the same call match) and stops the turn with
`AgentError::NoProgress` when the same call repeats, or when tool results keep
failing, `[agent].no_progress_repeats` or `[agent].no_progress_failures` times
in a row. The whole tool batch runs before the guard is read, so a stopped turn
leaves no assistant tool call without its result. The repetition limit also
stops unchanged 2–4-call cycles containing a failure. The guard holds four
result fingerprints and three match counters, comparing canonical calls and
result text. Only declared read-only calls and operations whose outcome is
`NotStarted` participate; possible mutations or unknown effects reset cycle
tracking. With hooks configured, only consecutive checks run because hook
effects are not covered by tool safety. Longer cycles and successful-only
cycles remain outside this guard.
A turn stopped this way is not a persistence failure, so a frontend continues
the session after reporting it.

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
The ChatGPT transport retries connection/send/body failures, HTTP 429 and
500/502/503/504 responses, a clean EOF before a terminal stream event, and
stream-reported `server_is_overloaded` or `server_error`, including failures
after partial streamed output. Responses error classification uses the first non-empty value from
`response.error.code|type`, event `error.code|type`, then the event's top-level
`code`; an SSE `event:` name supplies a missing JSON `type`, but never replaces
a non-empty one. It makes at most six retries with 1/2/4/8/16/30-second
backoff; a numeric `Retry-After` is preferred and capped at 30 seconds. The
OpenAI-compatible transport retries the classes described next. A retry
repeats only the borrowed provider request, never tools or the agent turn.
Other HTTP, auth, and stream-codec errors are terminal; retries preserve
cancellation, the original deadline, actual send-attempt counts, and unknown
effect certainty on failure. One Responses event's joined `data:` payload is
capped at 1 MiB, and an SSE field line at that cap plus the `data: ` prefix,
because the Responses API sends each event on one line and
`response.completed` echoes `instructions`, `tools` and all output; exceeding
either bound is a terminal codec error. A ChatGPT retry carries
`interrupted_output: true` when the failed attempt emitted a delta. Core
resets pending text/reasoning redaction tails at every retry; compaction and
single-answer calls reset their per-response byte counts. The complete
validated response alone authorizes tool execution and enters model context.
Native TUI/REPL and ACP mark interrupted output before its replacement; the
wire flag gives local/attached TUI and the shared browser transcript reducer
an explicit boundary. Child task progress, HTTP turn summaries and scheduled/
wake reply collectors discard the failed attempt's text, preserving prior
completed model steps. ACP has no stable
retry-countdown update but receives the interruption marker and, on exhaustion,
the normal terminal prompt error.
Normal provider steps that retry or fail append a content-free
`otto.provider_recovery` custom entry after settlement, with operation ID,
attempt count, retry reasons/delays, interruption flags and outcome. These
entries do not enter model context; a crash before settlement may leave no
recovery entry. Error messages classify request/read timeouts and report
retry-budget exhaustion without exposing credentials or raw transport errors.
The OpenAI-compatible transport uses the same policy (4 attempts, 1/2/4 s)
for two failure classes, both of which prove that nothing was visible to the
caller yet. A send whose connection could not be established
(`reqwest::Error::is_connect`, `Retry` reason `connection failed`) was never
dispatched, which the resilience design permits, and the settlement is
`NotStarted`. A dispatched request whose transport failed before any stream
event reached the caller (`Retry` reason `connection interrupted`) is retried
as well: a connection reset, a connection closed before the message
completed, a broken pipe, a stalled read, or a send that produced no response.
Its settlement is `transport_lost` with `Unknown` certainty, because the
endpoint may have started the completion even though no output was visible.
Every HTTP status, every stream-codec error, a local builder or redirect-policy
rejection, and any failure after a reasoning, text or tool-call delta stays
single-attempt.
reqwest's `read_timeout` (60 s) restarts on every received byte, so it does
not bound a body that sends only keepalive bytes. Both transports therefore
keep a per-attempt deadline from the 2xx response head, `MODEL_IDLE_TIMEOUT`
(120 s `chatgpt`, 300 s `openai-compatible`), that restarts only when the
stream assembler emits a delta event; the ChatGPT check runs before
`EventRedactor`, so held-back text counts. Expiry has `Retry` reason
`model output idle timeout`, which diagnostics map to `RetryReason::Timeout`.
ChatGPT retries it like any body failure; OpenAI-compatible handles it as a
transport interruption before the first delta and as a terminal error after
it. Design: `docs/specs/2026-10-11-provider-model-idle-timeout.md`.
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
serializable transactions, commit durability and multi-process writer behavior
before a repository receives a connection. Session
message, compaction, operation-fact, and custom-entry writes cross async
interfaces; the POSIX implementation retains Pi v3 encoding and append-only
history. `otto-core` remains independent of native storage engines.

### External storage backends

`runtime::storage::BackendRegistry` registers external `RepositoryFactory`
implementations before persisted routes are validated. Extension binaries call
`cli::run::run_with_backends`; the ordinary `run` and `Storage::open_home`
retain the existing defaults. Storage init/transfer commands use the same
registry; `otto memory` commands open it only before the first memory
import. Core/browser consumers
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
implement fictitious SQLite PRAGMAs or reuse Turso queries.

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
schemas and approval envelopes are public; table/SQL
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
<migration-id>` composes these ports for a stopped local
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
- `otto_core::provider::Response::message` is the single source of finish reason and token counts. `Message::usage == None` means unavailable; `Some(Usage::default())` means explicitly reported zero. `Response::cached_input_tokens_present` separately records whether the provider supplied that breakdown, including an explicit zero. Preserve both levels of presence through events and aggregates instead of inferring them from zero counters. Keep legacy Pi normalization in the decoder.
- Keep provider-token persistence in `crates/otto::usage`: collection maps neutral agent events to content-free records, Turso only appends and aggregates those records, and frontends query through the server API. Logical provider operations and actual transport attempts are stored separately from usage reports; neither count is inferred from the number of usage rows. Never store prompts, response text, tool arguments, tool output or opaque provider context in the usage database.
- Keep context associations in the typed `ContextMetadata`. Prefer the structured `task_id` over notification wording; text parsing is only a legacy-history fallback. Preserve append-only Pi v3 compatibility and namespaced optional details, including the explicit-zero usage marker. Provider-owned compaction is an invisible `otto.remote_compaction` custom entry; it is returned only to the producing provider and is never projected as model-visible or UI text. Do not rewrite old records or invent missing historical metadata.
- `Session::append_custom` writes a Pi v3 `custom` entry; `pi_entry_to_context_messages` skips `custom` entries, so they never reach a model context. The trait method has no default body: every implementor, including wrappers such as `SharedSession`, states whether it writes or drops the entry. The sub-agent runner writes `otto.task_spec` after the task slot is set and `otto.task_result` after the completion notification is pushed, and drops a write failure. When a session is reopened with unanswered tool calls, the first unanswered call gets `MAY_HAVE_RUN_TOOL_RESULT_TEXT` and the later ones `NOT_EXECUTED_TOOL_RESULT_TEXT` (`otto_core::session::context::missing_tool_results`); tool calls run one at a time, so the answered calls are a prefix.
- Every admitted run (`agent::run_inner`, after the redaction and empty-text checks) first appends an `otto.turn` start record (`otto_core::session::turn`, `TURN_CUSTOM_TYPE`) with `append_custom` and, on every return, an end record whose `outcome` is `completed`, `failed`, or `cancelled` (an error with a control stop reason). Data is `{"event":"started","schemaVersion":1,"turnId":…,"trigger":"user"|"wake"|"recovery","attempt":N,"resumes"?:…}` or `{"event":"ended","schemaVersion":1,"turnId":…,"outcome":…,"error"?:…}`, where `error` appears only on a `failed` end: the turn error's text after `Agent`'s redactor, truncated to `TURN_ERROR_MAX_BYTES` (4096) on a character boundary; the turn id comes from `Options::new_operation_id`. An empty-text run whose session has an `interrupted_turn` (a start without its end on the active path, folded into `ResolvedContext::interrupted_turn`) records `trigger: "recovery"` with that turn's `attempt + 1` and `resumes`; every other run records attempt 1. A failed end write is ignored, since a failed store rejects later writes. `Session::interrupted_turn` has no default body. `Store::from_file` records a `Recovery` (`cause` `TakenOver` or `ProcessLost`) when a lease was taken over, or when the file ends with calls without results or has an interrupted turn; it applies the replay predicate on every open unless the interrupted turn's attempt exceeds `MAX_AUTOMATIC_RESUMES` (1). `failover::recovery::notify` pushes the notice that starts the resuming wake turn, or, when the resume is exhausted, pushes nothing and returns a warning. Each `otto.operation` attempt records the executor's `ToolExecutor::retry_safety` as optional `retrySafety`; `otto_core::session::operation::replay_allowed` is the one replay rule for the store and the agent: the current declaration is `ReadOnly`, at most one attempt, no terminal fact, not corrupt, and a recorded `retrySafety` absent or `ReadOnly`. `otto serve` reports the resuming wake turn with trigger `recovery` (`Controller::resumes_interrupted_turn`). The design is `docs/specs/2026-10-06-crash-recovery-and-turn-resume.md`.
- After each provider operation of a turn or a compaction settles, the agent emits `Event::ProviderApiCall` and appends one `otto.provider_call` custom entry (`otto_core::session::provider_call`, `PROVIDER_CALL_CUSTOM_TYPE`, `schemaVersion` 1; `decode_provider_call` rejects other versions). `agent::provider_call::CallTracker` measures `firstOutputMs` from the operation start to the first reasoning, text or tool-call delta of the attempt that settled (a retry clears it) and records each retry with its reason after the redactor; `request` is the `context_report::sections` estimate, with the recalled-memory message counted as memory and its record ids listed. A one-shot completion (`Agent::complete_text`) and an operation stopped before it was sent append nothing; an append failure is `AgentError::Persist`. A sub-agent's `otto.task_spec` records the spawning tool call as `parentToolCallId` (`subagent::runner::StartRequest::parent_tool_call_id`). `otto_core::session::trace::build` and `span_content` derive the span tree and each span's text from the parent and child entries without I/O; `Controller::trace` and `Controller::trace_span` (`app/trace.rs`) read the session file and the `*.jsonl` files of its children directory through `TraceCache`, which parses a file again only when its length or modification time changed. The TUI `/trace` presenter returns `Presenter::defers_approvals() == true`, so `App` does not push an approval over it. The design is `docs/specs/2026-10-11-trace-view.md`.
- Agent inputs go through the `otto_core::mailbox::Mailbox` port; only `runtime`, `subagent::tasks` and the runner fallback construct the in-memory `Inbox` (`runtime::sidecar_mailbox` tests enforce it). `runtime::sidecar_mailbox::install` makes an `Inbox` durable: it assigns `in_<hex>` ids to notifications without one and mirrors every change to a sidecar with `replace_private_with`; `load` restores it, moving an unreadable file aside. `Mailbox::enqueue` returns `Result<u64, MailboxError>` and skips an id already queued; a producer that can report a failure (`agent_send`, `agent_report`, steering admission) acknowledges the entry and returns an error. Every delivered input carries `Message::input_id` (Otto detail `inputId` on user and context messages; `model::is_input_id`), and delivery uses `Session::append_input_if_absent`, which appends only when the active path has no message with that id; it has no default body. Sidecars: `<id>.inbox.json` (notifications), `<id>.inputs.json` (steering journal: `Steering::set_journal`, entries scoped by `otto.turn` id, restored by `Steering::begin_turn` for the turn that resumes an interrupted one, removed by delivery, withdrawal and `Run` drop; a `Run` drop also hands each undelivered message to `Steering::take_discarded`, which `Runner::report_lost_steering` reports once as a visible `agent_steering` context message plus a notification, quoting the guidance), `<id>.followups.json` (`TaskControl::set_followup_journal`; `begin_resuming` adopts entries for a wake turn that resumes an interrupted turn, `begin` discards them and the caller records `record_unrun_followups`; each step's `agent_followup` message carries `followup:<scope>:<n>` and a step already in the transcript is skipped), and `<child transcript>.inbox.json` (`runner::Config::child_mailbox`, applied by `Tasks::replace_child_inbox` once the transcript path is known). Client idempotency keys are `input_id` (HTTP bodies) and `_meta.otto.inputId` (ACP); the server keeps the last 256 per session (`SessionState::inputs`), checks `Controller::has_input` for older ones, and passes the id to the user message with `Controller::set_next_input_id`. The design is `docs/specs/2026-10-06-durable-inputs.md`.
- Durable turn queue: `app::turn_queue::TurnQueue` wraps the session's `turns.json` mailbox journal (`Builder::session_journal`, installed with `TaskControl::set_turn_queue_journal`; `Controller::turn_queue`). An entry's `id` is the prompt's input id (the client's `input_id`, else `queued:<turn id>` in serve or `queued:<random>` in ACP), `task_id` the serve turn id, and `text` the JSON prompt (text, channel, origin, `imageDropped`); images are never saved. Serve (`Server::start_turn`) and local ACP `/queue` save before admitting (`PROMPT_NOT_PERSISTED`, HTTP `503 input_not_persisted`); the prompt runs with `set_next_input_id(input_id)` and its entry is forgotten after it ran; cancel, `DELETE` and ACP `session/cancel` remove entries, while shutdown and connection loss keep them. `Controller::restorable_prompts` drops entries whose input id the transcript holds; serve holds restored turns in `SessionState::restored` (counted by `busy` but not `busy_for_wake`) until the recovery wake turn ran (`release_restored`), local ACP waits for the slot and pending wake (`restore_queued_prompts`), and the TUI runs them after `run_wake` (`run_restored_prompts`). `announce_restored_prompts` queues a stable-id notice the first restored turn delivers. Archive removes `turns.json`. The design is `docs/specs/2026-10-07-durable-turn-queue.md`.
- Session plan: `otto_core::session::plan` owns the `otto.plan` record (`{"schemaVersion":1,"steps":[{text,status}]}`), its rules (`validate_plan`: at most `MAX_PLAN_STEPS` 20 steps of one line of 1 to `MAX_PLAN_STEP_CHARS` 200 characters, at most one `in_progress`) and its codec; a record that breaks them decodes to `None`. `build_context` folds the latest valid record on the whole active path into `ResolvedContext::plan`; `Session::plan` is required (conformance `session.plan` and the reopen check), and `Store` caches it on open and on each appended record. The parent-only `update_plan` tool (`tool::plan`, `Idempotent`, in `EXCLUDED_CHILD_TOOLS`, `<plan_policy>` in the system prompt) appends the record. `Agent::run_tool_call` emits `Event::PlanUpdated` (wire `plan_updated`, field `steps`) when a call changes `Session::plan`. Compaction appends the current plan as a trailing `<plan>` checklist block (`append_compaction_blocks`), rebuilt each time rather than carried. `app::Info::plan` feeds the HTTP session `plan` field, the TUI plan line and `/plan`; ACP maps `plan_updated` to `SessionUpdate::Plan` and replays the plan after `session/load`. The design is `docs/specs/2026-10-07-session-plan.md`.
- Session forks: `otto_core::session::fork` resolves fork points and plans the copy without I/O. A fork point is a user message (empty `context_type`) that is the first prompt of a `user`-trigger `otto.turn`, or, on a path without turn records, a user message with no unanswered tool call before it; the copy keeps the active path before that turn's start record, or, with no fork point, up to the last entry outside every turn with every call answered. `SessionRepository::fork` (POSIX `storage::backends::posix::session::fork`) copies the kept entries byte for byte with their ids, adds a `session_info` `<name> (fork)` when the source has a name and one `otto.fork` custom record (`{"schemaVersion":1,"sourceSessionId","sourceEntryId"?,"prompt"?}`), sets the header's `parentSession` to the source path, writes it with `fsops::publish_file` in `Exclusive` mode (temporary `<new id>.jsonl.fork-tmp` beside the source, one file sync, an exclusive rename, a directory sync); listing ignores the temporary name. The source is only read. `Store::forked_from` caches the record on open (`fork_origin`), `app::Info::forked_from` carries it, and `Controller::fork_file` / `fork_session` (fork, then `resume_session`) are the entry points for `POST /v1/sessions/{id}/fork`, `GET .../fork-points` and TUI `/fork` (`Reply::Forked` puts the prompt in the editor). Sidecars and sub-agent children are not copied. The design is `docs/specs/2026-10-06-session-fork.md`.
- At the start of each run, after recovery replay and queued-notification delivery and before the run's user or follow-up message, the agent compares its current `otto_core::session::Capabilities` with `Session::observed_capabilities()` (`agent::announce_tool_changes`). A record whose tool names or tool definition digests differ, or whose Otto version differs while `otto_help` is a current tool, or no record on a session with an assistant message or compaction, appends a `Role::Context` message of type `capability_update` (`otto_core::session::CAPABILITY_UPDATE_CONTEXT_TYPE`, `display` true, text without the `[Custom context: ...]` prefix on reload). The agent then writes `otto.capabilities` (`CAPABILITIES_CUSTOM_TYPE`) with `append_custom` if it is absent or differs; the message is written first, so a failed record write repeats the notice instead of losing it. The record is rewritten when any field differs, including the Otto version alone, which produces a notice only when `otto_help` is among the current tools. Record data is `{"tools":["a","b"],"definitions":{name:digest},"version":"X.Y.Z"}`: `tools` is sorted and deduplicated (older releases require it); `definitions` holds the first 16 hex characters of the SHA-256 of each tool's description, a 0x00 byte and the raw parameter bytes (`agent::capabilities::definition_digest`); `version` is `Options::version`. Absent optional fields read as unknown and produce no notice line. Manual section titles must stay unique case-insensitively, because `otto_help` returns the first match; a test enforces it. `build_context` resolves the latest entry on the active path into `ResolvedContext::observed_capabilities`; a malformed entry adds a warning and reads as absent. Notice names pass `otto_core::tool::safe_prompt_tool_name`. Compaction carries summarized notices verbatim in the `<capability-updates>` summary block (`agent::summary_details`, bounds 16 KiB and 8,000 characters per notice). `Session::observed_capabilities` has no default body, like `append_custom`: every implementor, including wrappers, forwards or states its behavior. The design is `docs/specs/2026-10-06-capability-discovery-and-resume-updates.md`.
- `otto_core::tool::ToolResult::persisted_content == None` selects `content`; `Some` selects its value, including `Some("")`; `persisted_text` applies that rule. Preserve redaction and the current-turn full-result overlay. Reuse tool definitions and assembly helpers, including `tool::bash::bash_definition` in `crates/otto`; keep conservative preflight and the `Registry::new` validation.
- Tool arguments decode model output rather than a strict client. An optional list argument deserializes with `tool::empty_as_none`, so an empty list reads as an absent key and a model that sends `[]` beside the arguments it is using is still served; `crates/otto/tests/tool_argument_contract.rs` scans the tool sources for one that does not.
- `Provider` and `Tool`/`ToolExecutor` instances are shared concurrently, so every trait method takes `&self`. Requests and arguments are borrowed for the duration of the call; returned responses, results, and definitions are owned by the caller. Per-call `StreamSink` callbacks are ordered and finish before `Provider::complete` returns; `Event` payloads are owned values, so consumers keep what they need without further copying.
- `serde_json` maps keep insertion order in native workspace builds: `agent-client-protocol-schema` (a `crates/otto` dependency) enables `serde_json/preserve_order`, and Cargo feature unification applies it to every workspace crate. `otto-core` built alone for wasm32 keeps sorted keys. Code and tests must not depend on either order; compare parsed `serde_json::Value`s instead of serialized strings.
- Reuse `otto_core::agent::CompactionResult` as the shared compaction payload. Keep HTTP/SSE DTOs in `crates/otto/src/server` separate from internal structs; update `testdata/server/openapi.yaml` alongside wire changes and preserve existing field meanings.
- Cross-workspace delivery is a dedicated native domain in `crates/otto/src/delivery.rs` and never an ordinary sub-agent. The source session stays bound to its own workspace and file-tool root; only a bounded, hashed handoff packet reaches the target. `delivery::Service` is effect-only and requires an async `Preflight`, and the same preflight runs before the authorization request and again after approval, so the request and its `apply_approval` step always describe the same target, base, branch, worktree path and handoff digest. The durable `ReceiptStore` uses the approval id as its immutable idempotency key, moves `prepared → applying → started` in one direction, refuses to overwrite a binding across a reopen, and converts only `applying → unknown` on recovery: never replay a delivery effect. Worktree creation runs through `WorktreeExecutor`, which accepts only prevalidated fixed `git worktree add` argv through an injected `CommandExecutor`; the target worktree, not its primary checkout, becomes the delivery runner root, and that runner keeps its own session, sandbox and builder. `start_delivery_task` is parent-only, reaches a target only through `DeliveryBroker` (weak `Workspaces`/`Shared`; no generic target executor crosses to the source), and is installed exactly once per process through `ParentToolSlot` before the workflow controller is built, so every later parent runner composes the same tools and no child ever does. Design: `docs/specs/2026-10-06-cross-workspace-delivery-design.md`.

## Lifecycle and frontend contracts

- Construct `app::Controller` through `Controller::new` (one `Builder`, used by `cli::run`) or `Controller::with_builder` (a shared `Arc<Builder>`, used by `server`); the `Builder` in `crates/otto/src/runtime/builder.rs` is the only session/runtime construction path (`create`, `open`, `resolve_profile`). Do not restore placeholder factories or duplicate runtime construction paths.
- `Controller::request_close` is nonblocking. External lifecycle owners cancel active work as appropriate and call the synchronous `Controller::close` to complete cleanup. An idle close request alone does not release resources. Preserve exactly-once session/runner cleanup, cleanup errors, and post-close `info`/`history` snapshots; reentrancy is tracked by the admission generation (`begin_operation`/`Admission`), not by thread or stack inspection.
- Treat `subagent::tasks::Task` as a query snapshot. Update existing task progress and completion through `Tasks::mark_running`, `record_provider_step`, `record_tool_call`, and `finish`; preserve task identity, terminal states, and notification-before-`wait` ordering. A cancellation request is not proof that execution has stopped. The agent loop sees the registry only as `otto_core::agent::TaskRegistry`.
- Frontends use `Controller::tasks` (`app::TaskView` over the wire-shaped `app::Task`), never the mutable `Tasks` registry or the raw mailbox adapter. Call `Controller::prepare_wake` before publishing a wake turn, then `WakeOperation::run` it once or drop it. Dropping an unstarted claim releases it on cancellation/shutdown. `Tasks::updates` is a coalescing `tokio::sync::watch` signal, not a broadcast subscription; add no competing scheduler. Normal and wake turns share cancellation, event delivery, and compaction accounting; one-shot runs propagate wake failures.
- `otto_core::mailbox::Mailbox` is the shared notification port. It supplies ordered snapshots and per-entry acknowledgement: consumers append the session effect before `ack`, so a failed append leaves that entry and later entries visible for at-least-once redelivery. The bundled `InMemoryMailbox` (`agent::inbox::Inbox`) numbers entries (`Entry::seq`), invokes its optional persistence hook under its lock after each mutation, and signals change callbacks after releasing the lock. Native `runtime::wiring` alone installs the top-level session adapter, mirroring `<id>.inbox.json` through `tool::remind::replace_private_with` (temporary file, `fsync`, rename, directory `fsync`) and skipping writes once `<id>.jsonl` is gone, so an archive is not recreated by a late enqueue. It restores before producers are shared; restored entries signal the wake path. Child task mailboxes are intentionally ephemeral. Alternative adapters (including external SQL implementations) implement `Mailbox` and must preserve these ordering and acknowledgement semantics; the core contract does not promise distributed leases, delayed retries, or exactly-once delivery.
- Session failover lives in `crates/otto/src/failover`; the approved design is `docs/specs/2026-09-28-session-failover.md`. A session is lease-managed if and only if `<id>.lease/` exists (`failover::lease::is_lease_managed`); `[failover] enabled` only decides whether an open or a new session creates that directory. Every open that takes the session file lock goes through `session::Prepared`, which acquires the lease before it opens the file descriptor, because a takeover replaces `<id>.jsonl`. The `Store` owns the lease: it checks `Lease::check` before every record write and releases the lease on `close` after a successful `fsync`, or on drop. Child transcripts check the parent's lease and never own one. Tool registries carry `failover::CommitGuard` as their `tool::registry::CallGuard`: `before` refuses a call once the lease is lost, and `after` runs `syncfs` on Linux. Dependency direction: `session` → `failover::lease` → `session::fsops` (a leaf), and `failover::recovery` → `subagent::interrupted` → `session`. `Keeper::global` calls `libc::_exit(75)` when a lease goes stale or is superseded, so tests use `Keeper::new_for_test`, or `Lease::acquire` only on leases that stay renewed and are released before the test ends; a stopped holder is a hand-written `epoch-<n>` and `heartbeat`. `SIGTERM` migration: `main.rs` owns the signal and asks `cli::terminate::Terminate::on_signal` what to do, with `failover::lease::holds_lease()` as input; a migration cancels the process token (the `SIGINT` path), and after the frontend returns `cli::run` (or `server::Server::migrate` for `otto serve`) calls `Controller::migrate` before the normal close. `Controller::migrate` holds an admission while it cancels and waits for every sub-agent task (`Tasks::begin_migration`, `Tasks::wait_final`), queues the moved notification (`failover::recovery::notify_moved`), and closes MCP; dropping the admission then closes the session, which releases the lease. In migration mode the sub-agent runner writes `otto.task_result` `interrupted` through the child's own transcript and pushes no completion notification, because a child transcript stays open and locked while its task entry exists, so `subagent::interrupted::mark_interrupted` (which opens a second `Store`) is only for transcripts of another process. Tests never send a signal to the test process; binary tests signal a spawned `otto`.
- Use `auth::Service` (`login`/`logout`/`status`) and `Controller::switch_profile`/`set_default_profile` for shared use cases. Credential paths and concrete services belong in the composition root; OAuth and credential files stay in `crates/otto`'s `auth` module. Frontends own presentation, not credential persistence. Preserve the startup credential snapshot (`auth::capture_auth_credentials`, re-exported as `cli::login::capture_auth_credentials`) and restart requirement, and refresh backend state after a profile switch even when saving the default fails.

## Embedding API

`otto::embed` lets another Rust program build the runtime `otto acp` builds,
in process, and switch components off at construction. The
[design](../specs/2026-10-07-otto-embedding-api.md) and the module rustdoc are
the contract.

- `runtime/startup.rs` is the only startup composition root. A new startup
  step goes into a phase function there, so `cli::run`, `otto serve` and
  `embed` all run it; do not add a startup step to `run_with_backends` or
  `embed.rs` alone. A CLI-only step (a flag, a frontend) stays in `cli::run`
  between the phase calls.
- The compatibility surface is `otto::embed` and the `otto_core` and `otto`
  types its signatures name or re-export. Other `pub` items in `otto` exist
  for the frontends and tests and carry no stability promise.
- `otto::embed` changes only by addition, including before 1.0: no item is
  removed or renamed and no signature changes incompatibly. `Component`,
  `Input`, `Error` and `ConfigSource` are `#[non_exhaustive]`; `Otto`,
  `OttoBuilder` and `Session` have no public fields; a new option is a new
  builder method.
- A `Component` switch only disables, and only into a state the CLI already
  reaches through a config key, a flag or a failed store open. A new
  component needs such a state first.
- One `Otto` per process: the config lock is a per-process `fcntl` lock.
  Tests that build an `Otto` hold the shared test lock in `embed.rs`.

---

Previous: — · [Documentation index](../README.md) · Next: [Development workflow](workflow.md)
