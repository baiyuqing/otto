# Architecture

[Development guide](../development.md) · [Code ownership map](../code-organization.md)

## Package boundaries

- `otto-core` owns portable contracts, formats and the agent loop. It must
  remain WASM-safe and I/O-free: no filesystem, network, process, environment,
  stdio or wall-clock access. Provider-specific wire types stay in
  `openaicompat` and `openairesponses`, behind the neutral provider contract.
- `otto` owns native I/O, tools, sandboxing, storage, authentication and
  frontends. `app` owns shared use cases; frontends adapt presentation and
  transport rather than reimplementing domain behavior.
- `runtime/startup.rs` is the shared composition root for CLI, serve and embed.
  Inject domain ports there. CLI-only flag/frontend steps stay in `cli::run`.
- `chat-connector/` is an independent Go ACP client; it links no Otto runtime
  code. The browser UI lives in [otto-plus](https://github.com/baiyuqing/otto-plus).

Use the [ownership map](../code-organization.md) for module entry points rather
than duplicating their inventory here. Dependencies point toward shared
contracts; interfaces belong at actual consumer boundaries, not speculative ones.

File-tool confinement belongs in native `tool`; Bash delegates to `sandbox`
and starts in the selected workspace. Only explicit sandbox `off` uses direct
execution. Provider HTTP/OAuth and credential persistence remain native.

Chat channel metadata uses `app::Channel`, passed through admitted HTTP/ACP
turns. Render fixed guidance for supported channels, never client-authored
system instructions. Per-run context applies to every request/retry without
mutating the configured prompt or session history; absent metadata means no
channel context.

Memory consumers use the service's bound accessors or the controller facade,
never repositories directly. Files are the default memory backend and are not
a storage domain. Model writes become review candidates; human management
follows its own explicit contract. External memory services run as plugins,
not as a memory backend. See the
[memory manual](../manual/memory-and-extensions.md#memory).

## Workspace ownership

`workspace` owns the versioned identity marker, initialized before session
lookup, including serve startup. Read-only lookup derives the same keys when
no marker exists; configured memory IDs are adopted once and conflicts fail.

Use pinned `fsops` descriptors, refuse symlinks, serialize changes with the
workspace lock and publish complete markers atomically. Corruption fails
closed. Historical paths record ownership, not execution authority: tool and
approval boundaries remain path-based. Session candidates still validate their
stable directory, file identity and recorded cwd. Never rewrite transcripts
when paths change. Identity UUIDs are not session IDs or process lease epochs.
Forking leaves old state untouched and requires stopped workspace processes.

## Native storage contracts

`storage/{contracts,sql,files}.rs` define guarantees; drivers implement I/O;
`storage/backends/` owns schemas/codecs. `runtime/storage.rs` validates routes
and capabilities before opening repositories and injects domain-owned ports.
Shipped storage uses local SQLite (bundled through `rusqlite`) and POSIX
session files; no
MySQL, PostgreSQL or cloud service implementation is shipped.

Domain services own validation, state transitions and recovery. Repositories
own atomic mutations, revisions/claims, codecs and schemas. SQL drivers must
validate dialect, immediate serializable transactions, durability and writer
concurrency. An uncertain commit is `CommitUnknown`, not rollback. Never replay
an uncertain external effect. Async session writes preserve append-only Pi v3.

Migration contracts live in `storage/migration/`: bounded snapshots/chunks,
isolated staging, verification, receipts, journal and route cutover. Migration
never executes pending tools. Offline `otto storage transfer` resumes by ID,
keeps original files and publishes destination routes for the same source home.
Abort cannot undo committed or uncertain publication; roots must not overlap.
Use the [storage manual](../manual/memory-and-extensions.md#storage-operations) for operational commands.
The legacy-schema importer (`otto storage migrate`) is separate from route transfer.

### External storage backends

Extension binaries register `RepositoryFactory` implementations with
`runtime::storage::BackendRegistry` before route validation, through
`cli::run::run_with_backends`. Ordinary startup retains local defaults.
Factories bind immutable non-secret options, then validate every namespace
against `RepositoryRequirements`. Version-one profiles mean the whole domain
port contract, not just a SQL dialect.

Endpoints are absolute local paths or opaque service descriptors obtained
from named environment variables. Debug output must redact descriptors;
persisted routes contain instance references and verified namespace identity,
never resolved service addresses. Reject unsupported capabilities before writer
admission; do not simulate a backend with fictitious SQLite PRAGMAs.

Preserve synchronous domain ports. Network adapters may use the bounded
`storage::worker::Worker`; async callers await `call`, while `call_blocking`
requires a multi-thread runtime or a separate blocking thread. Accepted work
is not withdrawn by cancellation; close/drop drain owned work. Lost acknowledgements
retain existing command/receipt and unknown-effect rules.

Import preparation is read-only and returns an isolated stage identity.
Verification reads actual staged logical records through `verification_chunk`,
not destination SQL. Canonical record ordering/digests and portable reference
closure cover cross-domain provenance, aliases, pending approvals and receipts.
The portable verifier enforces its configured materialization bound; larger
backends need another bounded validation strategy. Local transfers retain their streaming
verifier and existing encoding compatibility.

Every backend runs `storage::conformance` in isolated namespaces (default-on
`storage-conformance` feature), plus backend-specific loss, concurrency and
transport tests. An acceptance adapter proves the extension seam, not a live
service's durability. Run `otto_core::conformance::{session,mailbox}` against
every implementation and wrapper, with reopen support for durable state.

## Core contracts

- Use neutral model validation and owned `Clone` values. Both `MemorySession`
  and native `Store` enforce validation and tool-call/result sequencing on
  append. Transient messages may lack IDs/timestamps; Pi codec restrictions
  stay in the session layer. Update ownership tests for new fields.
- `Response::message` owns finish reason and usage. Missing usage differs from
  explicitly reported zero; cached-input presence is separately tracked.
  Preserve both distinctions through events, persistence and aggregates.
- Usage storage contains content-free records only, never prompts, responses,
  tool bodies or opaque context. Logical calls, transport attempts and token
  reports are distinct counts. Frontends query the shared API.
- Typed `ContextMetadata` owns associations; use `task_id`, with text parsing
  only for legacy history. Preserve append-only Pi v3 and optional namespaced
  metadata; never invent missing historical fields.
- Custom entries are not ordinary model context. `append_custom` implementors
  explicitly write or drop entries. Provider-owned compaction is returned only
  to its producer, never projected as UI/model text. Turn and provider-call
  records remain versioned and redacted; their codecs live in
  `otto-core/src/session/{turn,provider_call}.rs`.
- Interrupted calls distinguish possible execution from known nonexecution.
  Recovery uses typed operation facts and tool-owned retry safety, never tool
  names or output text. Automatically replay only genuine read-only effects;
  uncertain effectful operations require reconciliation.
- Inputs flow through the mailbox port. Keep admission, ordering, cancellation,
  steering and wake behavior shared across frontends. Durable inboxes and
  session wrappers must retain the same conformance guarantees.

The executable contracts and tests beside `agent`, `session`, `mailbox` and
native `operation` own detailed record shapes and state machines. Changes to
those contracts update codecs, compatibility tests and consumers together.

## Lifecycle and frontend contracts

Lifecycle and task/authentication capabilities belong in `app`, not UI code.
Session replacement must preserve shared state and admitted work; avoid
frontend-specific execution paths. Keep append-only history and recoverable
operation facts separate from transient presentation.

Approval presentation is not authority: use the shared decision service and
exact request/version checks described in [contracts](contracts.md#unified-approval-contract).
Authentication captures startup credentials; changing those requires restart.
Profile switches refresh backend state even when saving the default fails.
Frontends never persist credentials themselves.

## Embedding API

`otto::embed` builds the shared native runtime in process. Its module rustdoc
is the API contract:

- The compatibility surface is `embed` and the types its signatures name or
  re-export; other native `pub` items have no blanket stability promise.
- Changes are additive even before 1.0: no removals, renames or incompatible
  signatures. Extend non-exhaustive enums and builder methods, not public fields.
- Component switches only disable into an already-supported runtime state;
  introduce that state before exposing a new switch.
- Use one `Otto` per process because the config lock is per-process `fcntl`.
  Construction tests hold the shared lock in `embed.rs`.
- New shared startup steps go in `runtime/startup.rs`, not only in an embed or
  CLI wrapper.
