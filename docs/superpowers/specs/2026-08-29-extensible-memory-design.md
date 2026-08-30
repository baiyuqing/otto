# Extensible Memory Subsystem Design

**Status:** Chat-approved; awaiting written-spec review  
**Date:** 2026-08-29  
**Target:** Otto Stage 1 on macOS

## 1. Goal

Add durable, cross-session memory without coupling the agent loop to a storage engine, search technology, extraction model, or remote service protocol.

The agent loop must depend on a small, stable API. The first implementation will use a shared local SQLite database with FTS5, while the architecture permits compile-time replacement of storage, retrieval, extraction, and policy components. Runtime-loaded Go plugins are not supported.

Memory is separate from session history:

- Pi v3 JSONL sessions remain the source of truth for a conversation.
- Compaction remains responsible for fitting conversation history into a model context window.
- Memory stores curated cross-session preferences and project knowledge.
- Recalled memory is request-local and is never appended to a Pi session.

## 2. Scope

The complete design includes:

- Stable agent-facing and management APIs.
- Extensible user, workspace, agent, and future custom scopes.
- Structured text records with optional stable keys and provenance.
- Explicit memory management plus policy-controlled automatic candidates.
- Bounded automatic recall plus a deeper `memory_search` tool.
- A process-shared SQLite/FTS5 backend.
- Multi-process concurrency and optimistic revision checks.
- Backup, verification, restore, retention, and strong-delete behavior.
- TUI, REPL, and standalone maintenance commands.
- A runtime-scoped, tool-free LLM extractor that is disabled by default.

## 3. Non-goals

The first implementation does not include:

- Anthropic, Codex subscription, or any provider beyond the existing OpenAI-compatible runtime.
- Runtime-loaded Go plugins.
- A vector database or embedding implementation.
- Team synchronization or a hosted memory service.
- Application-level encryption or SQLCipher.
- Automatic internet discovery of memory backends.
- Copying automatically recalled or searched stored-memory text into Pi JSONL, compaction summaries, logs, or session metadata.
- Backup files as a human-editable import/export format.
- Destructive restore from inside a running TUI.

A future remote adapter may satisfy the same contracts, but it is not a working Stage 1 backend.

## 4. Design principles and invariants

1. **Stable loop boundary.** The agent loop sees `Recall` and `Observe`, not SQL, embeddings, backend scores, or migration state.
2. **Separate policy from persistence.** Storage, retrieval, extraction, and candidate policy are independently replaceable.
3. **Human authority.** Model tool calls can propose mutations but cannot directly activate, update, or delete durable memory.
4. **Fail soft for augmentation.** Recall and automatic observation failures do not invalidate an otherwise successful turn.
5. **Fail clearly for explicit operations.** User-requested management operations return typed errors and never pretend to succeed.
6. **Bound everything.** Inputs, outputs, pages, labels, metadata, candidates, recalled records, and token budgets have hard ceilings.
7. **No secret persistence.** Credentials, authorization data, endpoint userinfo, private keys, and redacted placeholders are rejected.
8. **No stale session pollution.** Recalled records are ephemeral request context and are reused consistently within one turn.
9. **Multi-process correctness.** Sharing one local database must not cause lost updates, duplicate observations, unsafe migrations, or unsafe restore.
10. **Recoverability.** Backups are consistent, verified, checksummed, atomically published, and restorable without raw WAL copying.

## 5. Architecture

```text
cmd/otto configuration and factories
                 │
                 ├────────────── Memory Maintenance CLI
                 │                         │
                 ▼                         ▼
       per-runtime TurnMemory       Maintenance interface
                 │                         │
                 └──────────┬──────────────┘
                            ▼
                    Shared Memory Core
              ┌─────────────┼─────────────┐
              ▼             ▼             ▼
            Store       Retriever       Policy
              ▲             ▲
              └──── backend Components ──┘
                            │
                    SQLite + FTS5 first

per-runtime TurnMemory = Shared Core + runtime-scoped Extractor
```

### 5.1 Package boundaries

- `internal/memory`: neutral records, requests, results, interfaces, validation, core service, policy, rendering, and null implementations.
- `internal/memory/sqlite`: SQLite schema, migrations, CRUD, FTS5 retrieval, locks, backup, and restore.
- `internal/agent`: turn-level orchestration only; it imports the narrow memory contract and never a backend package.
- `internal/app`: frontend-safe memory management facade and lifecycle serialization.
- `internal/tool`: `memory_search`, `remember`, and `forget` tool adapters. Search depends on the `Reader` surface; model mutations depend on `memory.Proposer`. No tool accesses a Store or the human-authorized mutation methods.
- `internal/config`: strict TOML schema and runtime resolution for memory.
- `cmd/otto`: backend registry, construction, standalone `otto memory` commands, close ordering, and warnings.
- `internal/tui` and `internal/repl`: presentation and explicit human management commands.

Provider-specific HTTP or JSON must remain under `internal/provider/openaicompat`. The LLM extractor uses the neutral provider contract and exposes no tools.

## 6. Stable service contracts

The exact request and result structs are versioned inside `internal/memory`; callers must not depend on backend-specific fields.

### 6.1 Agent-facing contract

```go
type TurnMemory interface {
    Recall(context.Context, RecallRequest) (RecallResult, error)
    Observe(context.Context, Observation) (ObserveResult, error)
}
```

`Recall` returns a bounded, ordered snapshot for one user turn. `Observe` is idempotent by `Observation.ID` and may create candidates, but never silently activates automatically extracted content under the default policy.

### 6.2 Management contract

```go
type Reader interface {
    Get(context.Context, RecordRef) (Record, error)
    GetByKey(context.Context, RecordKey) (Record, error)
    GetTombstone(context.Context, RecordRef) (Tombstone, error)
    GetCandidate(context.Context, CandidateRef) (Candidate, error)
    Search(context.Context, SearchRequest) (SearchResult, error)
}

type Manager interface {
    Reader
    Remember(context.Context, RememberRequest) (Record, error)
    Forget(context.Context, ForgetRequest) (ForgetResult, error)
    Review(context.Context, ReviewRequest) (ReviewResult, error)
}

type Proposer interface {
    Propose(context.Context, ProposeRequest) (CandidateBatch, error)
}

type Binding interface {
    TurnMemory
    Close() error
}

type Service interface {
    Manager
    Proposer
    Bind(context.Context, BindOptions) (Binding, error)
    Close() error
}
```

The process-level Service owns the shared Store, Retriever, Policy, and maintenance worker. `Bind` creates an immutable per-runner Binding containing the allowed user/workspace scopes, default automatic-write scope, runtime-scoped Extractor, redactor, and token estimator. The Agent Loop does not choose arbitrary scope IDs; Binding injects and enforces its resolved scopes and normalized UTC clock, so the Agent cannot make expired records eligible. Closing a Binding cannot close the shared Store; closing the Service first prevents new bindings, waits for released bindings, then closes shared components.

Reader.Get/GetByKey/GetTombstone/GetCandidate provide exact scope-bound reads for keyed CAS updates and explicit stale-candidate refresh; app/frontends never reach into Store for them. The management API distinguishes human-authorized calls from model-originated proposals by using different methods. Frontends and standalone human commands receive `Manager`; read-only model search receives `Reader`; model mutation tools receive only `Proposer`. Human authority is therefore created by the application call path, and no model-supplied boolean or string can grant it.

### 6.3 Maintenance contract

```go
type Maintenance interface {
    Backup(context.Context, BackupRequest) (BackupInfo, error)
    ListBackups(context.Context) ([]BackupInfo, error)
    VerifyBackup(context.Context, string) (BackupInfo, error)
    Restore(context.Context, RestoreRequest) error
    PurgeForgotten(context.Context, PurgeForgottenRequest) (PurgeForgottenResult, error)
}
```

Maintenance is never exposed as an agent tool. Backends without a maintenance implementation return typed `ErrUnsupported`.

### 6.4 Internal component contracts

The core service is composed from four narrow components:

```go
type Store interface {
    Identity(context.Context) (StoreIdentity, error)
    Get(context.Context, RecordRef) (Record, error)
    GetByKey(context.Context, RecordKey) (Record, error)
    GetTombstone(context.Context, RecordRef) (Tombstone, error)
    GetCandidate(context.Context, CandidateRef) (Candidate, error)
    List(context.Context, ListRequest) (RecordPage, error)
    ListTombstones(context.Context, TombstoneListRequest) (TombstonePage, error)
    ListCandidates(context.Context, CandidateListRequest) (CandidatePage, error)
    Upsert(context.Context, UpsertRequest) (Record, error)
    Forget(context.Context, StoreForgetRequest) (Tombstone, error)
    Propose(context.Context, ProposalBatch) (CandidateBatch, error)
    GetObservationReceipt(context.Context, string) (ObservationReceipt, error)
    CommitObservation(context.Context, ObservationCommit) (ObservationReceipt, error)
    Review(context.Context, StoreReviewRequest) (ReviewResult, error)
    Close() error
}

type Retriever interface {
    Retrieve(context.Context, RetrievalRequest) (RetrievalResult, error)
}

type Extractor interface {
    Extract(context.Context, ExtractRequest) ([]Proposal, error)
}

type Policy interface {
    Decide(context.Context, PolicyRequest) (PolicyDecision, error)
}
```

Retriever has neutral opaque pagination. Turn recall sets `IncludeBaseline=true`, `IncludeExpired=false`, and the Binding's runtime estimator. Process-level management Search has no Binding, so it always uses the fixed conservative byte estimator; textual search sets `IncludeBaseline=false` and may request expired records, while empty search uses Store.List so it can enumerate complete active data rather than recall baselines.

Store implementations never invoke caller-supplied guards, token estimators, clocks, or ID generators while SQL rows, transactions, borrowed connections, or lifecycle mutexes are held. Reads release bounded projections before callbacks; writes guard snapshots first and transactionally verify their digest/revision before mutation.

Each Store mutation is atomic. Cancellation before the commit point rolls back; once SQLite commit starts, a successful commit is reported even if the context is subsequently canceled, while a commit error returns content-free `ErrCommitUnknown` and bounded entity IDs for reconciliation through Reader/Store before retry. Observation IDs remain the only automatic replay key; other mutations use caller-generated IDs plus explicit reconciliation rather than pretending commit ambiguity cannot occur. `CommitObservation` atomically records the idempotency receipt and all candidates. `Review` atomically checks the candidate and target revisions, applies an accepted mutation, and records the decision. All pages use opaque cursors with deterministic ordering.

A backend factory returns components rather than a monolithic service:

```go
type Factory interface {
    Open(context.Context) (Components, error)
}

type Components struct {
    Store        Store
    Retriever    Retriever
    Maintenance  Maintenance
    Capabilities Capabilities
}
```

Factories are registered at compile time by a stable backend name. `cmd/otto` validates configuration and constructs a backend-specific Factory with typed options before calling `Open`; the neutral Factory API therefore does not carry an untyped configuration map. The first registry contains `sqlite`; `NullService` is constructed by the application rather than registered as persistent storage. A returned Maintenance implementation is non-nil and returns `ErrUnsupported` for unsupported operations.

Required persistence semantics are not optional capabilities. A backend must round-trip core fields, enforce conditional revisions, provide deterministic pagination, and atomically commit observation receipts with their candidate mutations. Capabilities only describe optional features such as semantic search, online backup, or encryption at rest.

## 7. Data model

### 7.1 Scope

```go
type Scope struct {
    Namespace string
    ID        string
}
```

`Namespace` is validated but not a closed enum. Stage 1 recognizes `user` and `workspace`; `agent` and custom namespaces can be introduced without changing the record shape.

- The local user scope uses an installation-local opaque ID stored in the shared database.
- The default workspace ID is a SHA-256 digest of the canonical workspace path.
- A canonical-path-to-stable-ID mapping or an explicit per-invocation override provides identity across directory moves without collapsing unrelated workspaces.
- Git remotes are never used to derive identity because they may contain userinfo, differ across forks, or be absent.
- Otto never writes a project ID file into the workspace automatically.

### 7.2 Active record

```go
type Record struct {
    ID         string
    Scope      Scope
    Kind       string
    Key        string
    Text       string
    Labels     []string
    Metadata   map[string]string
    Source     Provenance
    Confidence float64
    Revision   uint64
    CreatedAt  time.Time
    UpdatedAt  time.Time
    ExpiresAt  *time.Time
}
```

Rules:

- `ID` is generated above the backend and is never a SQLite row ID.
- A non-empty `Key` is unique within `(Scope, Kind, Key)` and represents stable semantic identity.
- Remembering the same non-empty key is a conditional update, not an append.
- An empty key creates an independent record.
- `Revision` starts at one and increments on every accepted mutation.
- `Confidence` is finite and in `[0, 1]`.
- Metadata values are strings so adapters can round-trip them without backend JSON semantics.
- Labels and metadata render in deterministic sorted order.
- Expired, tombstoned, pending, rejected, or malformed data is never recalled.

Initial hard limits are:

- Record text: 8 KiB.
- Namespace and kind: 32 bytes each.
- Record ID: 64 bytes; scope ID: 128 bytes; semantic key: 256 bytes.
- Labels: at most 32; each at most 64 bytes.
- Metadata: at most 32 entries, 64-byte keys, 512-byte values, and 4 KiB serialized in total.
- Candidate reason: 2 KiB.
- Provenance message IDs: at most 32 and 128 bytes each.

These are hard safety ceilings. Config may choose lower recall limits but cannot raise safety ceilings.

A forgotten row decodes only to a content-free Tombstone:

```go
type Tombstone struct {
    ID          string
    Scope       Scope
    Revision    uint64
    CreatedAt   time.Time
    UpdatedAt   time.Time
    ForgottenAt time.Time
}
```

### 7.3 Provenance

```go
type Provenance struct {
    Origin         Origin
    SessionID      string
    MessageIDs     []string
    ObservationID  string
    DecisionAt     *time.Time
    DecisionSource Origin
}
```

Provenance records only bounded identifiers and origin metadata:

- Origin: human command, model proposal, automatic extractor, import, or migration.
- Session ID and bounded message IDs when applicable.
- Observation ID when applicable.
- Decision time and decision source for reviewed candidates.

It does not copy raw prompts, tool outputs, endpoint URLs, filesystem paths, or authorization data.

### 7.4 Candidate

```go
type Candidate struct {
    ID           string
    Proposed     Record
    Action       CandidateAction
    TargetID     string
    BaseRevision uint64
    Reason       string
    State        CandidateState
    CreatedAt      time.Time
    DecidedAt      *time.Time
    DecisionSource Origin
    ResultRecordID string
    ResultRevision uint64
}
```

Actions are `create`, `update`, and `forget`. States are `pending`, `accepted`, and `rejected`.

- Pending candidates never participate in recall.
- Update and forget candidates include the target revision observed during extraction.
- Review atomically checks that revision; stale candidates return `ErrConflict` and remain pending. After explicitly refreshing the active Record, only a human/migration review path may submit that current target revision to rebase the same Candidate; update rebases also require non-nil full edited content constructed from the refreshed target, preventing stale proposals from overwriting intervening fields; model/extractor/import paths cannot.
- Accepted candidates link to the resulting record revision.
- Accepted and rejected Proposed content—including kind, key, text, labels, metadata, provenance, confidence, expiry, and proposal timestamps—and reason are cleared immediately after the decision. Only scope, action/target/result identifiers and revisions, state, and decision metadata remain for 90 days.
- Pending candidates older than 30 days are deleted by bounded maintenance work.

## 8. Turn data flow

### 8.1 Recall timing

For each non-empty user turn:

1. Validate and redact the user text using the current runtime redactor.
2. Persist the redacted user message to the current session.
3. Call `Recall` once with the query, count limit, and token budget; the immutable Binding injects its resolved user/workspace scopes.
4. Render selected records into one request-local context message.
5. Build the ordinary provider request from the memory context followed by active session messages.

The memory snapshot is stored in the per-turn dispatch state. Tool-loop provider steps, proactive compaction rebuilds, overflow compaction retries, and the one overflow retry reuse the same snapshot. A turn does not observe memory changes made midway through that turn; successful tool mutations become recallable on the next user turn.

The rendered memory block:

- Is marked as untrusted reference material, not system authority.
- Uses deterministic delimiters and includes record IDs, scope, kind, key, and text.
- Escapes delimiter-like content.
- Is redacted again before dispatch.
- Is counted by request sizing and trimmed within the memory budget.
- Is never appended to the session, displayed as a transcript entry, or included in a compaction summary.

If recall fails, the agent emits a bounded memory warning and proceeds with an empty snapshot.

### 8.2 Retrieval policy

The initial retriever combines:

1. A bounded baseline of active, keyed user `preference` and `instruction` records.
2. FTS5 matches from active user and workspace records for the current query.

It then:

- Resolves the same `(Kind, Key)` in favor of the workspace scope.
- Deduplicates by record ID and normalized content fingerprint.
- Removes expired and ineligible records.
- Applies deterministic scope, kind, relevance, update-time, and ID tie-breaking.
- Enforces configured result and token budgets plus immutable hard ceilings.

A dedicated runtime token estimator is injected when available; it is separate from the provider's serialized request-byte sizer. Otherwise the existing conservative token estimate is used. Backend relevance scores are not stable API and are not interpreted by the agent loop.

The initial SQLite retriever accepts exactly one user scope and at most one workspace scope; custom namespaces remain valid Store data but are not recall-eligible until a precedence policy is defined. It treats user input as terms, not raw FTS syntax, constructs parameterized FTS queries, and returns opaque generation-bound pages. A future vector or hybrid retriever can replace it without changing the loop or record store.

### 8.3 Agent tools

- `memory_search` performs a bounded read of active records.
- `remember` from the model creates a pending create/update candidate.
- `forget` from the model creates a pending forget candidate.

The model cannot claim human authority in tool arguments. `tool.Result` gains an explicit persisted-content field: existing tools default it to their ordinary content, while `memory_search` supplies a protocol-valid placeholder containing only bounded record IDs and counts. The full redacted search text is held in a per-turn overlay and substituted only when constructing provider requests. The overlay survives tool-loop rebuilds, compaction rebuilds, and overflow retry, then is discarded when the turn ends. Consequently, searching memory does not copy stored memory text into Pi JSONL or later compaction summaries. TUI events may show the bounded full result during the live turn, while resumed history shows only the placeholder.

Remember/forget results contain IDs and decision state rather than record text. Observation excludes `memory_search` output from `ToolFacts`, preventing recalled content from being proposed as new memory.

### 8.4 Observation

After the final ordinary assistant message has been durably appended and before `EventAgentFinished`, the runner may call:

```go
Observe(ctx, Observation{
    ID:            stableObservationID,
    UserText:      redactedUserText,
    AssistantText: redactedFinalText,
    ToolFacts:     boundedToolFacts,
    SessionID:     sessionID,
    MessageIDs:    boundedMessageIDs,
})
```

- Automatic observation is disabled by default.
- `Observation.ID` is derived from stable session/final-message identity and extractor version; Binding injects and enforces its resolved default write scope rather than accepting one from the Agent Loop.
- Binding probes the Store's content-free `GetObservationReceipt` before extractor invocation; a hit returns the original receipt without calling the extractor.
- On a miss, the Store commits the observation receipt and generated candidates atomically; `CommitObservation` rechecks inside its write transaction so concurrent retries converge on the original receipt and cannot duplicate candidates.
- A failed observation emits a warning and does not change the successful turn outcome.
- Cancellation or timeout leaves no partial candidate set and permits an idempotent later retry.

Observation input is capped at 64 KiB. Individual copied tool facts are capped at 2 KiB, and the extractor can propose at most eight candidates per turn.

## 9. Extractor and policy

### 9.1 Runtime-scoped extractor

The database, retriever, and policy are shared for the process. Each candidate runner receives a `TurnMemory` facade with an extractor bound to that runner's resolved provider/model and redactor.

Startup, `/new`, and `/resume` build the candidate extractor before replacing the current runner. Failed replacement leaves the previous runner and memory facade intact.

The first implementation provides:

- `NoopExtractor`, used when automatic extraction is disabled.
- `LLMExtractor`, using the existing neutral provider contract and current OpenAI-compatible runtime.

The LLM extractor:

- Exposes no tools.
- Treats all transcript and tool content as untrusted data.
- Receives only redacted, bounded observation fields and bounded relevant existing records needed for update detection.
- Requests a strict structured proposal array.
- Enforces a bounded response, strict decoding, field validation, duplicate rejection, and post-decode redaction/sensitivity checks.
- Does not contribute usage to ordinary session token totals.
- Runs synchronously with a configurable bounded timeout while the turn remains active.

No provider credentials are stored in Memory config, records, candidates, errors, manifests, or logs.

### 9.2 Default policy

The policy decision is one of `accept`, `pending`, or `reject`. PolicyRequest carries origin/action, scope, kind, confidence, and bounded provenance so replacement policies can implement configured rules without Store/backend access.

Default behavior:

- An explicit frontend or standalone human command can write an active record immediately after validation.
- Model tools, automatic extraction, and future imports produce pending candidates.
- Internal schema/data migrations preserve accepted records without user review but still pass validation and sensitivity checks.
- Sensitive or malformed proposals are rejected before persistence.
- Pending candidates are not recalled.

The Policy interface supports compile-time replacement and future configured rules by scope, kind, source, and minimum confidence. Stage 1 ships only the conservative default; it does not silently auto-accept an extractor proposal.

## 10. SQLite backend

### 10.1 Location and permissions

The default layout follows Otto's existing home directory convention:

```text
~/.otto/memory/
├── memory.db
├── memory.lock
├── backup.lock
├── forget-ledger
└── backups/
```

The directory is `0700`; database, lock, ledger, backup, and manifest files are `0600`. Custom locations undergo canonical parent validation and reject symlinked final paths or unsafe writable parent directories.

The first implementation uses a pure-Go SQLite driver with the required SQLite and FTS5 features so normal Otto builds do not require an external SQLite CLI or runtime daemon.

### 10.2 Database behavior

Required settings include:

- WAL journal mode.
- Foreign keys enabled.
- A bounded busy timeout.
- Durable synchronous mode.
- Explicit, short write transactions.
- A process-local writer mutex to avoid self-contention.
- Parameterized SQL and explicit FTS updates in the same transaction as record mutations.

Model calls, rendering, redaction, backup verification, and filesystem operations never run while a record write transaction is held.

SQLite permits concurrent readers and one writer. `SQLITE_BUSY` receives only bounded jittered retries; other errors are not retried blindly.

### 10.3 Record consistency

- A partial unique index enforces non-empty `(scope_namespace, scope_id, kind, key)` identity.
- Conditional updates and deletes include the expected revision.
- Observation IDs are unique.
- Candidate decision and resulting record mutation are one transaction.
- FTS rows are updated atomically with active record state.
- Forget clears all content-bearing fields from the live row and leaves a content-free ID/scope/revision/timestamp tombstone sufficient to reject stale mutations of that record ID. A later proposal for the same semantic key is a new pending Candidate and cannot become active without an explicit human review; deliberate re-remembering is allowed and is not stale resurrection.

### 10.4 Forget ledger

`forget-ledger` is an append-only, content-free record of committed tombstones used when restoring an older database snapshot. Entries contain format version, database ID, record ID, tombstone revision, forget time, and a checksum; they contain no scope text, key, metadata, or record content. Appends are serialized across processes and fsynced before a forget operation reports full success.

The live SQLite tombstone is committed first. If the ledger append then fails, the live record remains forgotten but the operation returns `ErrIncompleteForget`. Startup reconciliation repairs missing ledger entries from live tombstones before recall is enabled. Restore fails closed on unrecoverable ledger corruption rather than silently resurrecting records. Each backup bundle includes a verified ledger snapshot, and restore merges it with newer entries from the current installation before applying tombstones.

No mechanism can retrofit a tombstone into an unmanaged backup copied away before the forget occurred. Strong forget addresses only backups still controlled by Otto and explicitly discloses this boundary.

### 10.5 Multi-process schema safety

This section becomes normative in the Durability phase. Memory Core advertises `ConcurrentProcesses: false`; its subprocess tests are groundwork, not a support claim.

Once enabled, every normal process holds a shared advisory lock on `memory.lock` for the lifetime of its Store. Migration and restore require the exclusive lock.

On open:

1. Acquire the appropriate lock.
2. Read and validate the schema version.
3. If migration is required, acquire the exclusive lock and recheck.
4. Create a verified pre-migration backup.
5. Apply the migration transactionally.
6. Verify the resulting schema and integrity before reopening normal access.

A process using an older schema blocks migration until it exits. A binary that encounters a schema newer than it supports returns `ErrIncompatibleSchema`; it never rewrites or guesses at the database. The lifetime lock makes the validated schema stable while a process is using it.

The shared SQLite file is supported only on a local filesystem on one machine. NFS, SMB, iCloud Drive, Dropbox, and equivalent synchronized folders are rejected or explicitly unsupported.

## 11. Backup and restore

### 11.1 Consistent backup

Raw copying of `memory.db`, `-wal`, or `-shm` is forbidden. The first SQLite adapter uses the engine's `VACUUM INTO` command to create a transactionally consistent standalone snapshot without an external CLI. Driver and concurrency tests must prove the selected SQLite build's WAL behavior before implementation is accepted.

Backup publication is:

1. Acquire the backup coordination lock so only one process publishes a backup.
2. Create unique temporary database and ledger snapshots inside the private backup directory.
3. Run `VACUUM INTO` while normal WAL-safe access may continue.
4. Copy a stable, content-free forget-ledger snapshot under its append lock.
5. Run SQLite integrity verification on the database snapshot.
6. Compute SHA-256 for both snapshots.
7. Write and fsync a manifest.
8. fsync the database and ledger snapshots.
9. Atomically rename the complete bundle into final names.
10. fsync the backup directory.
11. Only then update backup catalog state and rotate older bundles.

A manifest contains backup ID, creation time, schema version, Otto version, database ID, record/candidate counts, backup class, and both SHA-256 values. It contains no record text, raw path, endpoint, or credential.

### 11.2 Automatic backup and retention

Defaults:

- Automatic backup enabled.
- A backup is due when the database generation changed and the latest verified regular backup is older than 24 hours.
- Seven daily, four weekly, and three migration/safety backups are retained.
- Total retained backup bytes are capped at 1 GiB by default.
- At least the newest verified compatible backup is always retained.
- Pre-migration and pre-restore backup failure aborts the destructive operation.
- Routine scheduled backup failure emits a warning and leaves prior verified backups untouched.

A single bounded maintenance worker performs due checks and stops before Store close. Multiple processes coordinate through `backup.lock`; a process that loses the race skips duplicate work.

### 11.3 Restore

Restore is a standalone maintenance operation and never runs inside an active agent process.

1. Resolve every file in the selected backup bundle without following symlinks.
2. Verify manifest structure, both SHA-256 values, file permissions, ledger integrity, SQLite integrity, database ID warning, and schema compatibility.
3. Acquire the exclusive `memory.lock`; active Otto processes cause `ErrMemoryInUse`.
4. Create and verify a safety backup of the current database and ledger.
5. Close database connections and freeze ledger appends.
6. Merge the backup ledger with newer valid entries from the current installation.
7. Copy the source database and merged ledger to `0600` staging files in the live directory.
8. Write and fsync a restore-intent marker naming only backup IDs and expected hashes.
9. Fsync staging, then atomically rename each staged file while the exclusive lock prevents observers.
10. Remove stale WAL/SHM only while holding the exclusive lock.
11. Reapply merged tombstones to the restored database.
12. Reopen, migrate only if explicitly allowed, verify integrity, fsync the directory, and remove the intent marker.
13. On ordinary failure, restore the safety bundle and report both the primary and rollback errors. On process crash, the next maintenance open uses the intent marker and safety bundle to finish or roll back before normal access.

Restore requires an interactive confirmation or an explicit headless confirmation flag. The confirmation shows backup time, schema version, database ID mismatch, and whether records newer than the snapshot will be lost.

### 11.4 Forget and backup privacy

Two explicit semantics exist:

- Normal forget removes live content and records a tombstone. Historical backups can retain the old bytes until normal retention removes them. Restore in the same installation reapplies newer tombstones by default.
- Strong forget (`--purge-backups`) removes live content, identifies and deletes every managed backup that may contain the record, fsyncs the backup directory, and creates a new verified clean backup.

Strong forget is irreversible and requires a second confirmation. The neutral Service forgets live data first, then calls `Maintenance.PurgeForgotten` with the content-free Tombstone; this keeps backup/ledger coordination backend-neutral and makes partial-purge retry explicit. If deletion of any contaminated backup fails, the live forget remains effective but the command returns a typed incomplete-purge error naming only backup IDs, not paths or content. Retrying is idempotent.

Otto cannot erase unmanaged copies that a user previously copied outside the managed backup directory; the confirmation states this limitation.

## 12. Security and privacy

### 12.1 Secret rejection

Before extraction, inputs pass through the current runtime redactor. Before any candidate, receipt, or record is persisted, every caller-controlled persisted string—including opaque IDs—passes through validation and a required non-nil composite ContentGuard. The guard combines generic detection with a hash-only exact matcher for resolved credentials/endpoints, retains no raw forbidden value, and is injectable at SQLite construction before runtime config wiring exists.

Rejected classes include:

- Resolved runtime secret values and encoded variants.
- API keys and OAuth/access/refresh tokens.
- Authorization and Cookie headers.
- URI userinfo.
- Private keys and known credential blocks.
- Redaction markers or values changed by redaction.

Decoded Store/Retriever content is revalidated and guarded before return so externally corrupted rows cannot bypass this boundary. Sensitivity rejection is fail-closed and returns `ErrSensitiveMemory`. Samples and errors never echo the rejected value.

### 12.2 Prompt injection and poisoning

- Recalled content is rendered under a fixed statement that it is untrusted reference material.
- Delimiter-like record content is escaped.
- Model tool requests cannot assert human authority.
- Automatic extraction cannot activate a record under the default policy.
- Search results retain provenance and record IDs.
- Memory records cannot modify system prompts, tool definitions, approval policy, or workspace boundaries.
- Extractor summary calls expose no tools and treat observation content as data rather than instructions.

### 12.3 At-rest protection

The Stage 1 SQLite backend advertises `EncryptionAtRest: false`. It relies on `0700` directories, `0600` files, macOS account isolation, and optional FileVault. Backups receive the same permissions.

If config requires encryption and the selected backend does not advertise it, startup fails. It never silently downgrades. A future encrypted backend must separately specify key creation, Keychain integration, backup key recovery, rotation, and lost-key behavior.

### 12.4 Logging and diagnostics

Errors and structured events may include operation, backend name, scope namespace, record ID, candidate ID, duration, and typed category. They never include memory text, metadata values, queries, tool facts, raw paths, prompts, endpoint userinfo, or credentials.

This feature does not implement the separately planned runtime error-log subsystem.

## 13. Configuration and defaults

The Stage 1 strict TOML shape is:

```toml
[memory]
enabled = true
backend = "sqlite"
required = false
recall_tokens = 2000
max_results = 12
require_encryption = false

[memory.workspace_ids]
"/canonical/old/project/path" = "stable-project-id"
"/canonical/new/project/path" = "stable-project-id"

[memory.extraction]
auto = false
timeout = "30s"
max_candidates = 8

[memory.backup]
auto = true
interval = "24h"
keep_daily = 7
keep_weekly = 4
keep_safety = 3
max_bytes = 1073741824

[memory.sqlite]
path = ""
busy_timeout = "5s"
```

Defaults:

- Local SQLite Store, recall, explicit frontend commands, and model memory tools are enabled.
- Automatic extractor calls are disabled.
- Empty SQLite path resolves to `~/.otto/memory/memory.db`.
- The current canonical path is looked up in `memory.workspace_ids`; an unmatched path uses its canonical-path digest.
- A future moved path can map to the same stable ID without changing or merging unrelated workspace records.
- A per-invocation `--memory-workspace-id` override takes precedence over the map and is intended for automation with a dedicated project identity.
- `required = false` permits a transient backend open failure to degrade to `NullService` with a warning.
- Unknown backend, malformed config, unsafe path, invalid bounds, and an explicit unmet encryption requirement fail startup.
- Corruption never causes automatic replacement with an empty database; the original file is preserved for verification or restore.
- `--no-session` disables Pi session persistence only. Memory remains enabled unless separately disabled.

Future remote backend authentication must use environment variables. TOML cannot contain raw keys or tokens.

## 14. User experience

### 14.1 TUI and REPL

Commands:

- `/memory`: open/list active and pending memory.
- `/memory search <query>`: bounded search.
- `/remember [--scope user|workspace] [--kind KIND] [--key KEY] <text>`: human-authorized explicit remember.
- `/memory forget <record-id>`: confirmed live forget.

`/remember` defaults to the current workspace scope, kind `fact`, and an empty key. User preferences use `--scope user --kind preference` and should use a stable key when they are intended to replace an older preference.
- Candidate review supports accept, reject, edit-and-accept, refresh after conflict, and bounded batch operations.

The TUI uses bounded typed messages. Only Bubble Tea `Update` mutates presentation state. Destructive actions use confirmation views. Narrow layouts remain bounded and mouse-wheel behavior remains unchanged.

REPL provides equivalent textual commands. `RunOnce` remains prompt-only and never interprets slash commands.

### 14.2 Standalone maintenance CLI

The CLI dispatches `otto memory ...` before normal agent flag parsing:

```text
otto memory status
otto memory backup
otto memory backups
otto memory verify <backup-id-or-path>
otto memory restore <backup-id-or-path>
otto memory forget <record-id> [--purge-backups]
```

Maintenance commands do not resolve provider credentials or start an agent. Restore and strong forget require explicit confirmation; no `--api-key` flag is introduced.

### 14.3 App lifecycle

`internal/app` exposes a memory management facade rather than a Store. User-initiated memory mutations are serialized against prompt, compaction, new-session, resume, and close operations. Read-only list/search may use immutable snapshots while a turn is active. Model memory tools execute inside the already serialized prompt operation.

`/new` and `/resume` preserve the shared core. Failed session replacement cannot publish a candidate runtime extractor. Close order is:

1. Stop accepting frontend operations.
2. Complete/cancel the controller operation.
3. Close the current runner facade.
4. Stop memory maintenance work.
5. Close Store connections and release locks.

## 15. Error and degradation semantics

Typed errors include:

- `ErrDisabled`
- `ErrUnavailable`
- `ErrConflict`
- `ErrSensitiveMemory`
- `ErrUnsupported`
- `ErrMemoryInUse`
- `ErrPersistenceDisabled`
- `ErrBusy`
- `ErrCommitUnknown`
- `ErrCorrupt`
- `ErrIncompatibleSchema`
- `ErrInvalidRecord`
- `ErrInvalidRequest`
- `ErrNotFound`
- `ErrClosed`
- `ErrInvalidCursor`
- `ErrIncompleteForget`
- `ErrIncompletePurge`

Behavior:

- Recall failure: emit one bounded warning for the turn and continue without memory.
- Automatic Observe failure: emit a warning, commit no partial candidates, and leave the turn successful.
- Explicit remember/search/forget/review failure: surface the typed error to the human or tool result.
- Interactive agent startup with transient unavailability and `required=false`: warn and use `NullService` for the process.
- Standalone explicit maintenance or management commands never substitute `NullService`; backend open failure is returned.
- Invalid configuration, unsafe path, unknown backend, or unmet encryption requirement: fail startup.
- Corrupt/incompatible database: preserve the file, disable memory only when policy permits, and direct the user to verify/restore commands.
- Close errors are joined and redacted without leaking content.

Warnings are rate-limited by category so an unavailable backend cannot flood the TUI.

## 16. Testing strategy

All default tests are offline and credential-free.

### 16.1 Contract and validation

- Compile-time interface assertions.
- Table tests for every record, scope, candidate, request, result, and size boundary.
- Clone/alias tests for slices and maps crossing API boundaries.
- Store conformance suite reusable by every backend.
- Retriever conformance tests for bounds, scope precedence, expiry, deterministic order, and pagination.
- Policy tests proving model and extractor origins cannot acquire human authority.

### 16.2 SQLite and multi-process behavior

- Real SQLite files under `t.TempDir`; no mocked SQL expectations.
- Concurrent readers and writers across goroutines and spawned helper processes.
- Revision conflicts, keyed uniqueness, observation idempotency, and atomic candidate resolution.
- Busy retry bounds and non-busy error behavior.
- Migration lock contention, interrupted migration, newer schema refusal, and integrity failures.
- Symlink, permission, unsafe directory, and unsupported-filesystem tests where deterministically detectable.
- Race detector coverage for service, lifecycle, and close paths.

### 16.3 Backup and restore

- Backups during concurrent WAL writes produce a coherent point-in-time snapshot.
- Fault injection at snapshot, checksum, manifest, fsync, rename, rotation, restore, reopen, and rollback boundaries.
- Corrupt database, corrupt manifest, checksum mismatch, unknown schema, database ID mismatch, and active-process lock cases.
- Retention never deletes the newest verified backup.
- Forget-ledger partial writes, reconciliation, checksum failure, and cross-process appends are fail-closed and recoverable.
- Normal forget tombstones survive restore.
- Restore-intent crash recovery either completes or rolls back before normal access.
- Strong forget removes every managed contaminated backup and reports partial deletion safely.

### 16.4 Agent integration

- Recall occurs once per user turn.
- Tool loops, threshold compaction, and overflow retries reuse the same snapshot.
- Memory context is included in request sizing but absent from Pi JSONL, history rendering, and compaction summaries.
- Full `memory_search` results remain in the per-turn overlay; only protocol-valid ID/count placeholders reach Pi JSONL, including across tool loops and compaction/overflow rebuilds.
- Recall and Observe failures do not change successful ordinary turn durability.
- LLM extraction receives no tools, bounded redacted input, and strict structured-output validation.
- Extractor usage does not alter ordinary session token totals.
- New/resume replacement publishes the correct runtime-scoped extractor transactionally.

### 16.5 Frontends and CLI

- TUI review, confirmation, cancellation, stale revision, narrow layout, and typed-message bounds.
- REPL command behavior and unchanged `RunOnce` prompt-only behavior.
- Standalone maintenance commands do not initialize a provider.
- PTY lifecycle and terminal restoration remain offline and automated.

Final gates remain:

```bash
go build -trimpath -o <temp>/otto ./cmd/otto
go test -count=1 ./...
go test -race -count=1 ./...
go test ./cmd/otto -run 'Test(TUIPseudoTerminalResumeLifecycle|TUICompactCommandCompletionCancelAndTerminalRestore|TUIPseudoTerminalLifecycle)$' -count=10
go vet ./...
go run honnef.co/go/tools/cmd/staticcheck@latest ./...
test -z "$(gofmt -l .)"
git diff --check
```

## 17. Delivery plan boundaries

Implementation is split into independently reviewed changes:

1. **Memory core:** contracts, validation, scopes, records, candidates, policy, null service, SQLite CRUD/CAS/FTS schema, and conformance tests. It keeps `ConcurrentProcesses: false`; subprocess tests establish baseline SQLite behavior but do not claim supported deployment.
2. **Durability:** local-filesystem validation, shared lifetime/exclusive locks, migrations, automatic/manual backup, restore, strong forget, and maintenance CLI. Only this phase enables `ConcurrentProcesses: true`.
3. **Recall and explicit management:** config/runtime construction, request-local recall, tools, app facade, TUI/REPL management, and session non-persistence tests.
4. **Automatic extraction:** runtime-scoped extractor, observation idempotency, candidate policy/review flow, events, and usage isolation.

Each phase uses its own isolated worktree and focused commits. No README claim is added before the corresponding behavior is implemented and verified.

## 18. Acceptance criteria

The feature is complete when:

- Different Store/Retriever/Extractor/Policy implementations can be injected without modifying the agent loop.
- Multiple local Otto processes safely share one database.
- User and workspace memory scopes resolve predictably and support stable override IDs.
- Explicit records persist across sessions and are recalled within hard budgets.
- Model-originated and automatic mutations remain pending under the default policy.
- Recalled memory never enters Pi session persistence or compaction summaries.
- Memory outages do not break normal turns, while explicit operations report truthful errors.
- Secrets and redacted placeholders cannot be persisted.
- Backups are consistent and verified; restore is atomic and lock-safe.
- Ordinary and strong forget semantics are visible, tested, and non-deceptive.
- Existing Stage 1 provider, session, REPL-safe redirected use, TUI lifecycle, and offline test guarantees remain intact.
