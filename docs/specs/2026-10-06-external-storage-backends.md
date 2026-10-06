# External storage backend integration

Status: approved and implemented on 2026-10-06.
Verification: `make check-fast` passed (2554 native unit tests, 606 core unit
tests, integration suites, formatting, Clippy and WASM compilation).
MySQL/PostgreSQL implementations remain extension work.
Constraint: preserve existing SQLite/POSIX behavior and compatibility.
Base: `bba3234`. This extends the approved
[state storage contracts](2026-10-06-state-storage-contracts.md).

## Problem and scope

User requirement: remove the integration barriers that prevent `otto-plus`
from adding MySQL and PostgreSQL storage backends.

The domain repositories and `RepositoryFactory` are the extension boundary.
The existing `SqlDriver`, capabilities and `SqlRepositories` implement a
SQLite/Turso profile. MySQL/PostgreSQL implement their own repositories,
queries, schemas and migrations; neither must emulate SQLite, import Turso
internals or require branches in domain services.

Complete Otto's external integration boundary in this change. Implementing
and advertising actual MySQL/PostgreSQL backends remains extension work.
Sessions, workspace files and executable trust retain their POSIX boundaries.
No ORM, SQL translator, dynamic plugin ABI or dependency from Otto to
`otto-plus` is introduced.

## Registration and process composition

- Extract an instance-owned public `BackendRegistry` from the current runtime
  factory maps. Existing SQLite/POSIX registrations remain defaults. Reject
  duplicate names, unsupported domains and incompatible profiles.
- Add `Storage::open_home_with(...)` accepting the registry, resolved storage
  configuration and the captured startup environment. Register all factories
  before reading and validating the persisted route group or admitting writers.
  Keep `open_home` as the default-backend compatibility entry point.
- Add `cli::run::run_with_backends(...)`; existing `run(...)` delegates with the
  default registry. Pass that same registry through normal, serve, ACP, memory,
  workflow and storage-command composition. There is no process-global hook.
- An extension binary in `otto-plus` links Otto and its optional backend crates,
  registers factories, then invokes the public process entry point. Its Cargo
  features select linked extensions. Otto does not depend back on that binary.

## Endpoint configuration and route authority

Add a pure `[storage]` configuration section in `otto-core`. Named backend
instances have a registered backend name and exactly one `path` or
`endpoint_env`; domain routes refer to those instances. The native runtime
resolves `endpoint_env` from its captured environment, never from project
preferences. Backend-specific non-secret options are parsed by the factory.

```toml
[storage.backends.primary]
backend = "postgres"
endpoint_env = "OTTO_POSTGRES_DSN"

[storage.routes]
memory = "primary"
approvals = "primary"
workflows = "primary"
schedule = "primary"
usage = "primary"
tasks = "primary"
reflection = "primary"
skill_checks = "primary"
memory_advice = "primary"
```

This illustrates the extension interface, not shipped PostgreSQL support.

Public endpoint opening distinguishes a local path from an opaque service
descriptor. Resolved descriptors are native-only, non-serializable, and have
redacted debug output. Persist backend-instance references and stable namespace
identities, never resolved DSNs or credentials, in routes and manifests.
Factories must not return provider diagnostics through unredacted public errors.

Storage configuration is user/managed authority only, including for trusted
projects. Integrate it with configuration validation, explain redaction and
the runtime/tool redaction boundary. Storage credentials are not implicitly
passed to tools. Missing variables, empty descriptors, unavailable backends
and unknown options fail before opening writable stores.

Configured authoritative routes open existing namespaces. Selecting an endpoint
does not create a replacement database or authorize changing an active route.
Initialize fresh installations explicitly; refuse bootstrap when existing
authoritative state or a route publication is present. Replacing existing state
requires verified migration and publication. Configuration must agree with the
published route generation and namespace identity. Retain backward decoding
for current local path routes without weakening these checks.

## Capabilities, errors and I/O lifecycle

Put domain requirements above SQL dialect capabilities. Each factory declares
the domains and versioned profiles it implements and validates the actual
namespace/deployment before handing out ports. Profiles specify transaction
scope, conditional revision/claim behavior, durable acknowledgement, ordered
events, query bounds and search semantics. Startup checks cannot consist only
of asserted boolean support flags. SQLite's exact FTS and immediate-transaction
checks remain private to its adapters; other adapters satisfy domain behavior
using their own mechanisms.

Use existing domain error categories where sufficient and supply missing
conflict, incompatible, unavailable, closed and commit-unknown distinctions.
Map deadlocks/serialization failures only when the backend knows the transaction
did not commit. Unknown writes require authoritative receipt reconciliation;
they never cause external tools to run again.

Compatibility refinement after approval: the user explicitly required retaining
current behavior. Preserve the synchronous domain ports and services, and add
awaitable deployment validation, initialization, export/import and an owned
async I/O worker. Network adapters await their client futures on that worker's
real Tokio runtime. Synchronous compatibility calls yield Otto's multi-thread
executor with `block_in_place`; single-thread extension callers use the async
worker entry point or a blocking thread. The original local methods and wire
DTOs remain unchanged. Do not use Turso's synchronous thread-waker bridge for
network futures.

Preserve existing domain command/revision semantics and receipt identities.
Migration commands use (migration ID, domain, cursor): matching retries return
the original receipt and changed arguments conflict. An extension preserves
domain receipts and never weakens a conditional mutation or treats unknown
commit acknowledgement as rollback. This change does not replace established
domain transitions or introduce a new counter into existing records.

Close rejects new work and drains owned work. Cancellation stops new submission;
dropped submitted writes retain a reconcilable outcome. Preserve domain recovery
and the separation of human decisions from application effects. This change
supports coordinated stopped processes on one host; shared deployment writer
authority needs its own backend-enforced implementation, not local PID checks.

## Portable migration and reference validation

Retain the public `SnapshotExport`, `StageImport`, `MigrationJournal`, `Cutover`
and coordinator interfaces. Extend factory composition to supply their backend
implementations and validated route identities. Extend the transfer command to
select named configured destinations while retaining its existing local form.
Abort, resume, freeze and publication use the same registered backend set.

Expose versioned logical record formats beside their domain owners. Move the
existing JSON encoding contracts out of Turso implementation modules while
keeping physical table mappings, queries and SQL validation inside Turso.
Cover all nine SQL domain families, IDs, revisions, receipts, tombstones,
events and recovery facts. Preserve existing version-1 transfer compatibility;
introduce an explicit version if encoding semantics must change. Do not expose
SQL rows, `Connection`, physical table names or positional row arrays as the
extension contract.

Cross-domain verification consumes neutral staged identities/reference facts
supplied by export/import adapters. It must not open SQLite files or execute
queries against destination schemas. Reuse existing invariant checks after
separating data acquisition from validation. Preserve journal aliases, session
IDs, workflow/task references and approval application references.

Before copying, validate the consistent closure of affected domains and freeze
source admissions. Import into a fresh isolated namespace with idempotent chunk
receipts, bounded sizes, digests and durable progress. Reopen and verify staged
repositories before atomically publishing the whole route generation. Retain
source fencing and restart with fresh ownership; never transfer live claims.
Abort restores source admission only before publication. An unknown publication
is reconciled; rollback after publication requires a verified reverse migration.

## External conformance and acceptance

Provide public contract-check helpers behind a `storage-conformance` Cargo
feature. They consume domain ports and controlled fault hooks, not concrete
database types. Extract useful existing behavioral scenarios rather than
inventing another test framework. Cover every implemented port's atomic unit,
revision/claim races, receipt replay, search/filter/pagination behavior,
restart recovery, close and error mapping. A backend's real-service tests are
additional gates; offline helpers cannot establish service durability.

Add an independently compiled test extension crate outside Otto's module
privacy boundary. Its different repository implementation uses only public
contracts and proves registration, service endpoint handling, async opening,
domain service composition, portable import/export and shared contract checks.
It must not wrap Turso stores as evidence of dialect neutrality. Its controlled
storage is not advertised as a production backend.

Acceptance also covers:

- Registration before persisted external-route validation on restart.
- Config and route generation conflicts; missing/unavailable destinations;
  no empty-store fallback; explicit fresh bootstrap versus existing-state transfer.
- Credential redaction through configuration explain, errors, debug output,
  manifests and tool execution environments; project storage overrides rejected.
- Unsupported capabilities rejected before schema or data writes.
- Lost acknowledgement, identical/different command replay, concurrent writers,
  submitted-write cancellation, drain and restart reconciliation.
- Turso export to the independent adapter and reverse transfer; interrupted
  resume, wrong namespace/source, reference/receipt preservation, corrupt chunks,
  unknown publication and abort before/after publication.
- Existing SQLite/POSIX formats, public facades, frontend DTOs and WASM safety.
- No backend imports added to domain services or the migration coordinator.

Implementation order is configuration/registration, owned async I/O,
portable migration/reference validation, then external conformance. Deliver
the complete agreed boundary together, with focused behavior checks followed
by `make check-fast`; full platform checks remain in CI. Update the canonical
code organization, development, user manual and changelog with implemented
behavior, and open the implementation PR after verification.
